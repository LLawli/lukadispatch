//! O miolo: tudo que o frontend e o socket mandam fazer passa por aqui.
//!
//! Regra de convivência entre os dois lados: o frontend nunca fala com o hospedeiro direto e o
//! hook nunca fala com o frontend direto. Os dois chamam método deste tipo, que é quem conhece o
//! estado. Assim existe um só lugar onde "sessão morreu" quer dizer as quatro coisas que ela
//! precisa querer dizer (matar a sessão, apagar o canal, fechar as perguntas, marcar no banco).
//!
//! O `App` não sabe o que é Telegram, WhatsApp ou tmux: ele fala com as portas
//! ([`Frontend`], [`Transcritor`], [`Divisores`], [`Hospedeiro`]), reunidas em [`Portas`]. Ver
//! `docs/decisoes/0002-portas-e-adaptadores.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use ld_core::ask::{Answer, Ask};
use ld_core::config::{Config, Project};
use ld_core::paths;
use ld_core::proto::{
    EventKind, RegisterSession, Response, SessionEvent, SessionSummary, StopReport,
};
use ld_core::state::{Session, Store, Worktree};
use tracing::{info, warn};

use crate::agente::{
    Agente, DescricaoDoChat, Guardado, Invocacao, Memoria, PartidaDaMemoria, PedidoDePartida,
};
use crate::cards::{Acao, Card, Cards, Efeito};
use crate::divisor::Divisores;
use crate::frontend::formato::escapa;
use crate::frontend::{Botao, Canal, Frontend, Midia, MsgId};
use crate::hub::{Hub, Incoming};
use crate::panel::Panel;
use crate::sessions::{self, Hospedeiro, Situacao};
use crate::status::{Ctx, StatusBoard};
use crate::transcritor::Transcritor;

/// Por que o portão não abriu card.
///
/// Vira erro de propósito: o caminho de "não abri card" já existia (sessão sem canal, sessão
/// desconhecida), e o socket sabe traduzir. O que muda é a resposta ao Claude Code: liberar é uma
/// decisão, e no modo remoto ela precisa ser dita, porque o `dontAsk` por baixo nega o silêncio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemCard {
    Libera,
}

impl std::fmt::Display for SemCard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Libera => write!(f, "ferramenta liberada sem perguntar"),
        }
    }
}

impl std::error::Error for SemCard {}

/// De onde veio o pedido de encerrar a sessão.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fim {
    /// Você mandou (`/kill`, `lukadispatch kill`).
    Explicito,
    /// O hook `SessionEnd` avisou que o processo acabou.
    Hook,
}

/// Depois de tantas cobranças seguidas de re-arme sem sucesso, o daemon para de insistir e
/// avisa. Insistir para sempre prenderia a sessão num laço de acordar-e-não-resolver.
const TETO_REARME: u32 = 3;

/// As quatro portas que o `App` precisa para trabalhar. Reunidas num tipo só porque nascem juntas
/// (a partida monta as quatro antes de subir o `App`) e porque um teste de fluxo troca as quatro
/// de uma vez pelos dublês.
pub struct Portas {
    pub frontend: Arc<dyn Frontend>,
    pub agente: Arc<dyn Agente>,
    pub memoria: Arc<dyn Memoria>,
    /// `None` desliga a transcrição: áudio ainda chega como arquivo, só não vira card.
    pub transcritor: Option<Arc<dyn Transcritor>>,
    pub divisores: Divisores,
    pub hospedeiro: Arc<dyn Hospedeiro>,
}

/// O que [`App::monta_e_lanca`] precisa para montar uma partida, seja de sessão nova ou de
/// relançamento. Reunido num tipo só porque a função já tinha oito parâmetros, um por campo do
/// [`PedidoDePartida`] do agente mais o id da sessão.
struct PedidoDeLancamento<'a> {
    projeto: &'a Project,
    permission_mode: &'a str,
    model: Option<&'a str>,
    effort: Option<&'a str>,
    resume: Option<&'a str>,
    retomada: bool,
    session_id: &'a str,
}

pub struct App {
    pub cfg: Config,
    pub store: Arc<Store>,
    pub frontend: Arc<dyn Frontend>,
    pub agente: Arc<dyn Agente>,
    pub memoria: Arc<dyn Memoria>,
    pub transcritor: Option<Arc<dyn Transcritor>>,
    pub divisores: Divisores,
    pub hospedeiro: Arc<dyn Hospedeiro>,
    /// Raiz de todas as pastas de sessão, onde anexo recebido e parte de arquivo grande moram.
    /// Padrão [`paths::arquivos_base`]; um teste troca por um tempdir com [`App::com_raiz_arquivos`].
    pub raiz_arquivos: PathBuf,
    /// Raiz de todas as pastas de sessão do agente (script de partida, prompt, log do painel).
    /// Padrão `paths::state_dir().join("sessions")`; um teste troca por um tempdir com
    /// [`App::com_raiz_sessoes`]. Cada sessão vive em `raiz_sessoes/<id>/`.
    pub raiz_sessoes: PathBuf,
    /// Raiz de todas as worktrees das sessões. Padrão [`crate::worktree::base`]; um teste troca
    /// por um tempdir com [`App::com_raiz_worktrees`].
    pub raiz_worktrees: PathBuf,
    pub hub: Hub,
    pub status: StatusBoard,
    pub cards: Cards,
    /// Transcrições esperando seu aval antes de virarem mensagem para a sessão.
    pub confirmacoes: crate::confirmacao::Confirmacoes,
    pub panel: Panel,
    /// As escolhas em aberto do `/new` e do `/kill`.
    pub novo: crate::novo::Estado,
    /// Último aviso de ociosidade ("Claude is waiting for your input") por sessão.
    ///
    /// Ele é útil quando chega, e vira lixo assim que você responde: some na próxima mensagem
    /// entregue à sessão, para o canal não acumular uma fila deles.
    avisos: Arc<Mutex<HashMap<String, MsgId>>>,
    /// Sessões que estão trocando de modelo agora, com a hora em que a troca começou.
    ///
    /// Relançar exige matar o processo, e matar dispara o hook `SessionEnd`. Sem esta marca o
    /// daemon trataria a troca como fim de sessão: apagaria o canal e encerraria tudo no meio
    /// do caminho. A janela é por tempo, e não por evento, porque o hook é `async` e pode chegar
    /// depois de a sessão nova já estar de pé.
    relancando: Mutex<HashMap<String, std::time::Instant>>,
    /// Uma reconciliação por vez. Além do relógio de um minuto, o fim de um processo por fora
    /// agenda uma volta, e duas ao mesmo tempo relançariam a mesma sessão duas vezes.
    reconciliando: tokio::sync::Mutex<()>,
    /// Quanto a reconciliação espera depois de um processo acabar por fora (ver
    /// [`App::end_session_por_hook`]). O bastante para um restart do herdr restaurar os panes; o
    /// `situacao` do hospedeiro sobe o servidor se ele ainda estiver fora do ar. Um teste troca
    /// com [`App::com_espera_depois_do_fim`].
    espera_depois_do_fim: std::time::Duration,
    /// Quanto a partida espera a memória de uma worktree ser solta por uma partida anterior
    /// que caiu sem soltá-la, antes de subir com uma memória só dela. Um teste troca com
    /// [`App::com_espera_da_memoria`].
    espera_da_memoria: std::time::Duration,
    /// A hora do último `SessionStart` de cada sessão. É o sinal de que ela já passou pelo
    /// início, e de que o que a memória tirou do caminho antes da partida pode voltar.
    inicios: Arc<Mutex<HashMap<String, tokio::time::Instant>>>,
    /// Última mensagem entregue a cada sessão pelo frontend, com a hora.
    ///
    /// O hook `UserPromptSubmit` não distingue o que você digitou no PC do que chegou pelo
    /// celular, e republicar o segundo no canal seria eco. A comparação é por texto e por
    /// tempo: só o que acabou de sair daqui é descartado.
    entregues: Mutex<HashMap<String, (String, std::time::Instant)>>,
}

impl App {
    /// Precisa rodar dentro de um runtime tokio: o painel sobe a tarefa dele aqui.
    pub fn new(cfg: Config, store: Store, portas: Portas) -> Self {
        let store = Arc::new(store);
        let panel = Panel::start(
            portas.frontend.clone(),
            store.clone(),
            portas.agente.clone(),
        );
        Self {
            cfg,
            store,
            frontend: portas.frontend,
            agente: portas.agente,
            memoria: portas.memoria,
            transcritor: portas.transcritor,
            divisores: portas.divisores,
            hospedeiro: portas.hospedeiro,
            raiz_arquivos: paths::arquivos_base(),
            raiz_sessoes: paths::state_dir().join("sessions"),
            raiz_worktrees: crate::worktree::base(),
            hub: Hub::new(),
            status: StatusBoard::new(),
            cards: Cards::new(),
            confirmacoes: Default::default(),
            panel,
            novo: Default::default(),
            avisos: Arc::new(Mutex::new(HashMap::new())),
            relancando: Mutex::new(HashMap::new()),
            reconciliando: tokio::sync::Mutex::new(()),
            espera_depois_do_fim: std::time::Duration::from_secs(5),
            espera_da_memoria: std::time::Duration::from_secs(100),
            inicios: Arc::new(Mutex::new(HashMap::new())),
            entregues: Mutex::new(HashMap::new()),
        }
    }

    /// Troca a raiz de arquivos padrão por outra (um tempdir de teste, tipicamente).
    pub fn com_raiz_arquivos(mut self, raiz: PathBuf) -> Self {
        self.raiz_arquivos = raiz;
        self
    }

    /// Troca a espera entre o fim de um processo por fora e a reconciliação que ele agenda.
    pub fn com_espera_depois_do_fim(mut self, espera: std::time::Duration) -> Self {
        self.espera_depois_do_fim = espera;
        self
    }

    /// Troca a raiz das worktrees por outra (um tempdir de teste, tipicamente).
    pub fn com_raiz_worktrees(mut self, raiz: PathBuf) -> Self {
        self.raiz_worktrees = raiz;
        self
    }

    /// Troca quanto a partida espera a memória de uma worktree ser solta.
    pub fn com_espera_da_memoria(mut self, espera: std::time::Duration) -> Self {
        self.espera_da_memoria = espera;
        self
    }

    /// Troca a raiz de sessões padrão por outra (um tempdir de teste, tipicamente). Cada sessão
    /// mora em `raiz/<id>/`.
    pub fn com_raiz_sessoes(mut self, raiz: PathBuf) -> Self {
        self.raiz_sessoes = raiz;
        self
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            frontend: self.frontend.clone(),
            store: self.store.clone(),
        }
    }

    /// Monta a partida (pede ao agente a invocação, embrulha na memória, escreve o script) e
    /// pede ao hospedeiro para subir. Usado tanto para abrir sessão nova quanto para relançar
    /// uma existente com `--resume`.
    ///
    /// Em volta da partida vai o que a memória precisa: o que ela tira do caminho antes volta
    /// depois que a sessão passou pelo início (ou na hora, se a partida falhou).
    async fn monta_e_lanca(&self, p: PedidoDeLancamento<'_>) -> Result<sessions::Launched> {
        let dir = self.raiz_sessoes.join(p.session_id);
        std::fs::create_dir_all(&dir).with_context(|| format!("criando {}", dir.display()))?;
        let worktree = self.store.worktree_em(&p.projeto.path).ok().flatten();
        let base = PartidaDaMemoria {
            session_id: p.session_id,
            cwd: Path::new(&p.projeto.path),
            worktree: worktree.as_ref(),
            isolada: false,
        };
        let guardado = self
            .memoria
            .antes_da_partida(&base)
            .await
            .unwrap_or_else(|e| {
                warn!(sessao = %p.session_id, erro = %format!("{e:#}"), "a memória não se preparou para a partida");
                None
            });
        let desde = tokio::time::Instant::now();
        let r = self
            .lanca_esperando_a_memoria(&p, &dir, worktree.as_ref())
            .await;
        if let Some(g) = guardado {
            self.devolve_depois_do_inicio(p.session_id, g, desde, r.is_ok());
        }
        r
    }

    /// Sobe a partida. Se ela morrer porque a memória ainda está presa a uma partida anterior
    /// (um processo que caiu sem soltá-la), espera e tenta de novo; passado o prazo, sobe com
    /// uma memória só dela, em vez de deixar você esperando mais.
    async fn lanca_esperando_a_memoria(
        &self,
        p: &PedidoDeLancamento<'_>,
        dir: &Path,
        worktree: Option<&Worktree>,
    ) -> Result<sessions::Launched> {
        let chat = DescricaoDoChat {
            plataforma: self.frontend.plataforma().into(),
            onde: self
                .frontend
                .onde(&nome_do_canal(&p.projeto.name, worktree)),
            teto_envio: self.frontend.limites().enviar,
            renderiza_markdown: self.frontend.renderiza_markdown(),
        };
        let raiz = worktree.map_or(p.projeto.path.as_str(), |w| w.raiz.as_str());
        let comeco = tokio::time::Instant::now();
        let mut isolada = false;
        loop {
            let memoria = PartidaDaMemoria {
                session_id: p.session_id,
                cwd: Path::new(&p.projeto.path),
                worktree,
                isolada,
            };
            let instrucoes = self.memoria.instrucoes(&memoria);
            let pedido = PedidoDePartida {
                projeto: p.projeto,
                raiz,
                permission_mode: p.permission_mode,
                model: p.model,
                effort: p.effort,
                resume: p.resume,
                retomada: p.retomada,
                wrap_mcp: self.cfg.wrap_mcp,
                chat: &chat,
                instrucoes: instrucoes.as_deref(),
            };
            let invocacao = self.agente.invocacao(&pedido, p.session_id, dir)?;
            let argv = self.memoria.embrulha(&memoria, invocacao.argv).await?;
            let partida = crate::agente::escreve_partida(
                dir,
                p.session_id,
                Invocacao {
                    argv,
                    prompt: invocacao.prompt,
                },
            )?;
            // O espelho do painel é só desta tentativa: o erro de uma anterior não pode decidir
            // por esta.
            let _ = std::fs::remove_file(&partida.log);
            let erro = match self.hospedeiro.lanca(&partida, p.projeto).await {
                Ok(l) => return Ok(l),
                Err(e) => e,
            };
            let saida = format!(
                "{}\n{erro:#}",
                std::fs::read_to_string(&partida.log).unwrap_or_default()
            );
            if isolada || !self.memoria.ocupada(&saida) {
                return Err(erro);
            }
            if comeco.elapsed() < self.espera_da_memoria {
                info!(sessao = %p.session_id, "a memória ainda está presa a uma partida anterior; esperando");
                tokio::time::sleep(self.espera_da_memoria / 10).await;
            } else {
                warn!(sessao = %p.session_id, "a memória não foi solta a tempo; esta partida usa uma só dela");
                isolada = true;
            }
        }
    }

    /// Devolve o que a memória tirou do caminho, depois que a sessão passou pelo início (o
    /// `SessionStart` dela chegou), ou na hora se ela não subiu. Com teto: sessão que não avisa
    /// o início em um minuto não segura o que foi tirado para sempre.
    fn devolve_depois_do_inicio(
        &self,
        session_id: &str,
        guardado: Guardado,
        desde: tokio::time::Instant,
        subiu: bool,
    ) {
        let memoria = self.memoria.clone();
        let inicios = self.inicios.clone();
        let id = session_id.to_string();
        tokio::spawn(async move {
            if subiu {
                let prazo = desde + std::time::Duration::from_secs(60);
                while tokio::time::Instant::now() < prazo {
                    let iniciou = inicios
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&id)
                        .is_some_and(|quando| *quando >= desde);
                    if iniciou {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                // Os hooks do início rodam juntos: o nosso pode ter chegado antes do que lê a
                // memória.
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
            if let Err(e) = memoria.devolve(guardado).await {
                warn!(sessao = %id, erro = %format!("{e:#}"), "não consegui devolver o que a memória tirou do caminho");
            }
        });
    }

    /// Para a sessão pedindo ao agente que saia, antes de o hospedeiro derrubar tudo. Quem
    /// embrulha o agente (a memória) termina o trabalho dele quando o agente sai: derrubado junto,
    /// ele deixaria a memória presa e sem o fim da conversa.
    async fn para_com_calma(&self, hospedagem: &str) {
        let Some(pid) = self.hospedeiro.pid(hospedagem).await else {
            return;
        };
        let alvos = self.memoria.a_parar(pid);
        for alvo in &alvos {
            let _ = tokio::process::Command::new("kill")
                .args(["-TERM", &alvo.to_string()])
                .status()
                .await;
        }
        for _ in 0..40 {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        warn!(
            pid,
            "a sessão não saiu com calma em 10 s; o hospedeiro derruba"
        );
    }

    // ---------------------------------------------------------------- ciclo de vida

    /// Cria o canal, sobe a sessão e devolve o id dela.
    ///
    /// Ordem importa: o canal vem antes do hospedeiro para a sessão já nascer com para onde
    /// falar. Se a sessão falhar ao subir, o canal recém-criado é apagado, senão sobra canal
    /// órfão a cada tentativa.
    pub async fn create_session(
        &self,
        projeto: &Project,
        model: Option<&str>,
        effort: Option<&str>,
        retomar: Option<&str>,
    ) -> Result<String> {
        let worktree = self.store.worktree_em(&projeto.path).ok().flatten();
        let canal = self
            .frontend
            .cria_canal(&nome_do_canal(&projeto.name, worktree.as_ref()))
            .await?;

        // Antes de subir: a pasta precisa estar confiada, senão o agente para num diálogo que só
        // dá para responder no teclado do PC, e do celular a sessão parece muda.
        if self.cfg.trust_projects {
            match self.agente.confia(Path::new(&projeto.path)) {
                Ok(true) => info!(projeto = %projeto.path, "pasta marcada como confiada"),
                Ok(false) => {}
                // Sem a marca, a sessão sobe e para no diálogo: o log é a única pista disso.
                Err(e) => warn!(projeto = %projeto.path, erro = %format!("{e:#}"),
                    "não consegui marcar a pasta como confiada; a sessão pode parar no diálogo"),
            }
        }

        // A regra de permissão é do projeto: numa worktree, a do repositório dela.
        let raiz = worktree
            .as_ref()
            .map_or(projeto.path.as_str(), |w| w.raiz.as_str());
        let modo = self.cfg.permission_mode_for(raiz);
        let id = match retomar {
            Some(r) => r.to_string(),
            None => self.agente.novo_id(),
        };
        let lancada = match self
            .monta_e_lanca(PedidoDeLancamento {
                projeto,
                permission_mode: &modo,
                model,
                effort,
                resume: retomar,
                retomada: true,
                session_id: &id,
            })
            .await
        {
            Ok(l) => l,
            Err(e) => {
                let _ = self.frontend.apaga_canal(&canal).await;
                return Err(e);
            }
        };

        self.store.upsert(&Session {
            session_id: lancada.session_id.clone(),
            project: projeto.name.clone(),
            cwd: projeto.path.clone(),
            transcript_path: None,
            hospedagem: Some(lancada.hospedagem.clone()),
            canal_id: Some(canal.as_str().to_string()),
            status: "iniciando".into(),
            status_msg_id: None,
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
            permission_mode: Some(modo.clone()),
            created_at: 0,
            ended_at: None,
        })?;

        let _ = self
            .frontend
            .envia(
                Some(&canal),
                &format!(
                    "🟢 <b>{}</b>\n<code>{}</code>\n{}\n\nPode falar. Para fechar, mande /kill.",
                    escapa(&projeto.name),
                    escapa(&projeto.path),
                    escapa(&ficha(
                        model,
                        effort,
                        &modo,
                        &self.hospedeiro.descreve(&lancada.hospedagem)
                    )),
                ),
                &[],
                None,
            )
            .await;

        self.panel.refresh();
        // Retomando: o canal nasce com o que já foi conversado, senão você continua às cegas.
        if retomar.is_some() {
            self.publica_historico(&canal, &lancada.session_id).await;
        }

        info!(sessao = %lancada.session_id, canal = %canal, projeto = %projeto.name, retomada = retomar.is_some(), "sessão criada");
        Ok(lancada.session_id)
    }

    /// Encerra a sessão: mata a sessão no hospedeiro, fecha as perguntas abertas, apaga o canal e
    /// marca no banco. Idempotente de propósito, porque dois caminhos chegam aqui (o /kill do
    /// chat e o hook SessionEnd de quando você fecha o Claude no PC).
    pub async fn end_session(&self, session_id: &str, apagar_canal: bool) -> Result<()> {
        self.encerra(session_id, apagar_canal, Fim::Explicito).await
    }

    /// Fim vindo do hook `SessionEnd`, que respeita a janela de relançamento.
    ///
    /// Com o motivo `other`, que é o que o Claude Code manda quando o processo morre por fora (o
    /// hospedeiro caiu ou reiniciou, o pane foi fechado), a sessão do bot não encerra aqui: um
    /// restart do herdr devolve o lugar dela, e quem decide entre relançar e encerrar é a
    /// reconciliação. Encerrar agora apagaria o canal antes disso. A volta vem daqui a poucos
    /// segundos, e não na próxima varredura, para o canal não ficar até um
    /// minuto falando com ninguém. Os outros motivos (`/exit`, `/clear`, logout) são você
    /// fechando a sessão, e encerram na hora.
    pub async fn end_session_por_hook(
        self: &Arc<Self>,
        session_id: &str,
        motivo: &str,
    ) -> Result<()> {
        let do_bot = self
            .store
            .get(session_id)?
            .is_some_and(|s| s.owned_by_bot() && s.ended_at.is_none());
        if motivo != "other" || !do_bot {
            return self.encerra(session_id, true, Fim::Hook).await;
        }
        info!(sessao = %session_id, "o processo da sessão acabou por fora; a reconciliação decide");
        let app = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(app.espera_depois_do_fim).await;
            if let Err(e) = app.reconcile().await {
                warn!(erro = %e, "reconciliação falhou");
            }
        });
        Ok(())
    }

    async fn encerra(&self, session_id: &str, apagar_canal: bool, quem: Fim) -> Result<()> {
        let Some(s) = self.store.get(session_id)? else {
            return Ok(());
        };
        if s.ended_at.is_some() {
            return Ok(());
        }
        // Troca de modelo em andamento: o fim VINDO DO HOOK é do processo velho, não da sessão.
        // Um pedido explícito seu passa por cima: você mandou fechar, fecha.
        if quem == Fim::Hook && self.em_relancamento(session_id) {
            info!(sessao = %session_id, "fim ignorado: a sessão está sendo relançada");
            return Ok(());
        }
        if quem == Fim::Explicito {
            self.relancando
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(session_id);
        }

        for ask in self.cards.da_sessao(session_id) {
            // Sessão morrendo: a pergunta aberta não tem mais quem responda.
            self.cleanup_ask(&ask, None).await;
        }
        for ask in self.hub.asks_of(session_id) {
            self.hub.close_ask(&ask);
        }
        self.hub.unlisten_qualquer(session_id);
        self.status.forget(session_id);

        if let Some(hospedagem) = &s.hospedagem
            && self.hospedeiro.vive(hospedagem).await
        {
            self.para_com_calma(hospedagem).await;
            if let Err(e) = self.hospedeiro.mata(hospedagem).await {
                warn!(sessao = %session_id, erro = %e, "não consegui encerrar a sessão no hospedeiro");
            }
        }

        if apagar_canal && let Some(canal) = canal_da_sessao(&s) {
            match self.frontend.apaga_canal(&canal).await {
                // Só esquece o canal depois de apagá-lo: enquanto ele estiver no banco, a
                // varredura de canal vazado sabe que ainda há o que limpar.
                crate::frontend::Resolvido::Apagado => self.store.clear_canal(session_id)?,
                crate::frontend::Resolvido::JaNaoExiste => self.store.clear_canal(session_id)?,
                crate::frontend::Resolvido::TenteDepois => {
                    warn!(sessao = %session_id, canal = %canal, "não consegui apagar o canal");
                }
            }
        }

        // Os anexos morrem com a sessão, como o canal: foi a escolha de guardar o mínimo, e
        // vale para o caso comum. Se um arquivo precisa sobreviver, ele sai daqui pela sessão,
        // que grava onde você mandar.
        crate::arquivos::limpa(&self.raiz_arquivos, session_id).await;
        // Card de transcrição de uma sessão que acabou não pode sobreviver a ela: o canal vai
        // embora junto, mas um card órfão ainda responderia a toques até o daemon reiniciar.
        for p in self.confirmacoes.limpa_sessao(session_id) {
            if let Some(m) = &p.msg {
                self.frontend.apaga(m).await;
            }
        }

        self.store.end(session_id)?;
        self.panel.refresh();
        info!(sessao = %session_id, "sessão encerrada");
        Ok(())
    }

    /// Uma troca de modelo em andamento silencia o fim da sessão antiga por esta janela.
    const JANELA_RELANCAMENTO: std::time::Duration = std::time::Duration::from_secs(90);

    /// Registra que alguém pediu alguma coisa a esta sessão.
    fn marca_pedido(&self, session_id: &str) {
        if let Err(e) = self.store.marca_pedido(session_id) {
            warn!(sessao = %session_id, erro = %e, "não consegui marcar o pedido");
        }
    }

    /// Consome a marca: devolve `true` uma vez só, no `Stop` daquele turno.
    fn tinha_pedido(&self, session_id: &str) -> bool {
        self.store.tira_pedido(session_id).unwrap_or(false)
    }

    fn marca_relancamento(&self, session_id: &str) {
        self.relancando
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), std::time::Instant::now());
    }

    /// `true` enquanto a sessão está no meio de uma troca de modelo.
    pub fn em_relancamento(&self, session_id: &str) -> bool {
        let mut mapa = self.relancando.lock().unwrap_or_else(|e| e.into_inner());
        mapa.retain(|_, quando| quando.elapsed() < Self::JANELA_RELANCAMENTO);
        mapa.contains_key(session_id)
    }

    /// Catálogo de modelos que o agente oferece. Pode ser caro (o do Claude Code varre o
    /// binário); a implementação guarda em cache, não o `App`.
    pub fn modelos(&self) -> Vec<ld_core::models::Modelo> {
        self.agente.modelos()
    }

    /// Troca modelo ou esforço de uma sessão viva, sem perder a conversa.
    ///
    /// `/model` e `/effort` são comandos do frontend do Claude Code: nenhum evento consegue
    /// dispará-los, e digitar no terminal está fora de questão neste projeto. O que dá para
    /// fazer sem trapaça é reiniciar o processo com `--resume <id>`, que volta com o mesmo
    /// transcript e o mesmo id, só que com a flag nova. A conversa continua; o que se perde é o
    /// Monitor, e o prompt de re-arme cuida disso.
    /// Troca só o modo de permissão, pela mesma mecânica do modelo. Quem sabe quais modos
    /// existem e quais fazem sentido é o agente; aqui só se aplica a decisão dele.
    pub async fn relaunch_modo(&self, session_id: &str, modo: &str) -> Result<()> {
        self.agente.valida_modo(modo)?;
        self.store.set_permission_mode(session_id, modo)?;
        self.relaunch(session_id, None, None).await
    }

    pub async fn relaunch(
        &self,
        session_id: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<()> {
        let Some(s) = self.store.get(session_id)? else {
            bail!("sessão desconhecida");
        };
        if !s.owned_by_bot() {
            bail!("esta sessão foi aberta no terminal; troque por lá");
        }
        // Reiniciar no meio de um turno jogaria fora o trabalho em andamento sem aviso.
        if matches!(s.status.as_str(), "pensando" | "ferramenta" | "perguntando") {
            bail!("a sessão está trabalhando; espere o turno acabar e mande de novo");
        }

        let ficha = self.relanca(&s, model, effort).await?;
        if let Some(canal) = canal_da_sessao(&s) {
            let _ = self
                .frontend
                .envia(
                    Some(&canal),
                    &format!(
                        "♻️ Sessão reiniciada com o contexto inteiro.\n{}",
                        escapa(&ficha)
                    ),
                    &[],
                    None,
                )
                .await;
        }
        self.panel.refresh();
        info!(sessao = %session_id, modelo = ?model, esforco = ?effort, "sessão relançada");
        Ok(())
    }

    /// Mata o processo da sessão, se houver, e a sobe de novo com `--resume`: mesmo id, mesmo
    /// canal, e o que não foi pedido agora continua valendo. Grava onde ela passou a rodar e
    /// devolve a ficha dela, para o aviso no canal. Não confere se a sessão está no meio de um
    /// turno: isso é de quem chama.
    async fn relanca(
        &self,
        s: &Session,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<String> {
        let session_id = s.session_id.as_str();
        let projeto = Project {
            name: s.project.clone(),
            path: s.cwd.clone(),
            permission_mode: None,
            model: None,
            effort: None,
        };
        // O modo guardado é o que vale: ele pode ter sido trocado por /mode depois da criação.
        let modo = s.permission_mode.clone().unwrap_or_else(|| {
            let worktree = self.store.worktree_em(&s.cwd).ok().flatten();
            self.cfg
                .permission_mode_for(worktree.as_ref().map_or(s.cwd.as_str(), |w| &w.raiz))
        });
        // Trocar só o esforço não derruba o modelo.
        let model_final = model.map(str::to_string).or_else(|| s.model.clone());
        let effort_final = effort.map(str::to_string).or_else(|| s.effort.clone());

        self.marca_relancamento(session_id);
        if let Some(hospedagem) = &s.hospedagem {
            self.para_com_calma(hospedagem).await;
            self.hospedeiro.mata(hospedagem).await?;
            // Sem esta pausa o `--resume` pode esbarrar no processo anterior ainda saindo.
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        self.hub.unlisten_qualquer(session_id);
        self.status.forget(session_id);

        let lancada = self
            .monta_e_lanca(PedidoDeLancamento {
                projeto: &projeto,
                permission_mode: &modo,
                model: model_final.as_deref(),
                effort: effort_final.as_deref(),
                resume: Some(session_id),
                retomada: false,
                session_id,
            })
            .await?;

        self.store.set_hospedagem(session_id, &lancada.hospedagem)?;
        self.store
            .set_model(session_id, model_final.as_deref(), effort_final.as_deref())?;
        self.store.set_status(session_id, "iniciando")?;
        Ok(ficha(
            model_final.as_deref(),
            effort_final.as_deref(),
            &modo,
            &self.hospedeiro.descreve(&lancada.hospedagem),
        ))
    }

    /// Relança, no mesmo canal, a sessão que caiu junto com o hospedeiro e cujo lugar ele
    /// devolveu ao voltar ([`Situacao::Restaurada`]). O turno em andamento se perdeu com o
    /// processo, então a guarda de "trabalhando" do [`App::relaunch`] não vale aqui.
    async fn readota(&self, s: &Session) -> Result<()> {
        let session_id = s.session_id.as_str();
        // As perguntas abertas eram do processo que morreu: ninguém mais espera a resposta.
        for ask in self.cards.da_sessao(session_id) {
            self.cleanup_ask(&ask, None).await;
        }
        for ask in self.hub.asks_of(session_id) {
            self.hub.close_ask(&ask);
        }
        let ficha = self.relanca(s, None, None).await?;
        if let Some(canal) = canal_da_sessao(s) {
            let _ = self
                .frontend
                .envia(
                    Some(&canal),
                    &format!(
                        "♻️ A sessão caiu junto com o hospedeiro, que reiniciou, e voltou com o \
                         contexto inteiro. O turno que estava em andamento se perdeu.\n{}",
                        escapa(&ficha)
                    ),
                    &[],
                    None,
                )
                .await;
        }
        self.panel.refresh();
        Ok(())
    }

    /// Despeja no canal as últimas falas da conversa retomada.
    ///
    /// Vai em mensagens separadas por papel, e não num bloco só, porque no celular um muro de
    /// texto misturando pergunta e resposta não se lê. O que não é diálogo (ferramenta,
    /// raciocínio) fica de fora: aqui interessa o fio da conversa.
    async fn publica_historico(&self, canal: &Canal, session_id: &str) {
        let Some(s) = self.store.get(session_id).ok().flatten() else {
            return;
        };
        let falas = self.agente.historico(&s, self.cfg.history_lines);
        info!(
            sessao = %session_id,
            falas = falas.len(),
            transcript = ?s.transcript_path,
            "histórico publicado no canal"
        );
        if falas.is_empty() {
            return;
        }

        let _ = self
            .frontend
            .envia(
                Some(canal),
                &format!(
                    "📜 <b>Retomando a conversa</b> <i>(últimas {} falas)</i>",
                    falas.len()
                ),
                &[],
                None,
            )
            .await;
        for f in falas {
            let (marca, texto) = match f.papel {
                ld_core::transcript::Papel::Usuario => ("👤", f.texto),
                ld_core::transcript::Papel::Assistente => ("🤖", f.texto),
            };
            let corpo = corta(&texto, 1200);
            let _ = self
                .frontend
                .envia(
                    Some(canal),
                    &format!("{marca} {}", escapa(&corpo)),
                    &[],
                    None,
                )
                .await;
        }
        let _ = self
            .frontend
            .envia(
                Some(canal),
                "— <i>fim do histórico; pode continuar daqui</i>",
                &[],
                None,
            )
            .await;
    }

    /// Encerra as sessões cujo hospedeiro não existe mais.
    ///
    /// Duas coisas deixam esse lixo para trás: a sessão morre sozinha (crash, `exit` digitado no
    /// PC) com o daemon fora do ar, e o lançamento falha depois que o registro já foi gravado.
    /// Sem isto elas ficam no painel para sempre, e o canal delas vira um canal que não responde.
    pub async fn reconcile(&self) -> Result<usize> {
        let _vez = self.reconciliando.lock().await;
        let mut mortas = 0;
        for s in self.store.live()? {
            let Some(hospedagem) = &s.hospedagem else {
                continue; // sessão do terminal: quem cuida dela é o hook SessionEnd.
            };
            if self.em_relancamento(&s.session_id) {
                continue;
            }
            match self.hospedeiro.situacao(hospedagem).await {
                Situacao::Viva => {}
                Situacao::Mudou(nova) => {
                    info!(sessao = %s.session_id, de = %hospedagem, para = %nova, "a sessão continua viva com outra hospedagem");
                    self.store.set_hospedagem(&s.session_id, &nova)?;
                }
                Situacao::Restaurada => match self.readota(&s).await {
                    Ok(()) => {
                        info!(sessao = %s.session_id, "sessão relançada no lugar que o hospedeiro restaurou");
                    }
                    // Uma tentativa só: a sessão que não volta encerra, em vez de ficar num laço
                    // de relançar a cada volta. O canal dela vai embora, então o aviso vai no
                    // principal.
                    Err(e) => {
                        warn!(sessao = %s.session_id, erro = %format!("{e:#}"), "não consegui relançar a sessão restaurada; encerrando");
                        let _ = self
                            .frontend
                            .envia(
                                None,
                                &format!(
                                    "⚠️ A sessão de <b>{}</b> caiu junto com o hospedeiro e não \
                                     consegui trazê-la de volta: {}",
                                    escapa(&s.project),
                                    escapa(&format!("{e:#}"))
                                ),
                                &[],
                                None,
                            )
                            .await;
                        self.end_session(&s.session_id, true).await?;
                        mortas += 1;
                    }
                },
                Situacao::Morta => {
                    warn!(sessao = %s.session_id, hospedagem = %hospedagem, "sessão sumiu no hospedeiro; encerrando");
                    self.end_session(&s.session_id, true).await?;
                    mortas += 1;
                }
            }
        }

        // Canal de sessão encerrada que sobrou (daemon caiu no meio do fechamento, adaptador
        // fora do ar na hora): vira um canal que não responde a ninguém. A regra é a mesma para
        // qualquer canal, numérico ou não: quem decide se ele existe é o adaptador, não aqui.
        for (sessao, canal_id) in self.store.canais_vazados().unwrap_or_default() {
            if self.em_relancamento(&sessao) {
                continue;
            }
            let canal = Canal::new(canal_id);
            match self.frontend.apaga_canal(&canal).await {
                crate::frontend::Resolvido::Apagado => {
                    warn!(canal = %canal, sessao = %sessao, "canal vazado; apagado");
                    let _ = self.store.clear_canal(&sessao);
                }
                // Já não existe: o objetivo era não ter esse canal, e ele não está lá.
                crate::frontend::Resolvido::JaNaoExiste => {
                    let _ = self.store.clear_canal(&sessao);
                }
                crate::frontend::Resolvido::TenteDepois => {}
            }
        }

        // Anexo de sessão que já não existe (o `/clear` troca o id sem encerrar nada, e o
        // daemon pode ter morrido no meio de um fechamento) fica em disco sem dono.
        let vivas: std::collections::HashSet<String> = self
            .store
            .live()?
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        let apagados = crate::arquivos::varre_orfaos(&self.raiz_arquivos, &vivas).await;
        if apagados > 0 {
            info!(apagados, "arquivos de sessões mortas removidos");
        }

        // Áudio guardado tem prazo: ele existe para conferir uma transcrição estranha, e isso
        // ninguém faz semanas depois.
        let velhos = crate::arquivos::varre_audio_velho(
            &self.raiz_arquivos,
            self.cfg.transcricao.guardar_audio_dias,
        )
        .await;
        if velhos > 0 {
            info!(
                velhos,
                dias = self.cfg.transcricao.guardar_audio_dias,
                "áudios expirados removidos"
            );
        }

        // O contrário também acontece: o hospedeiro ficou com uma sessão viva já encerrada no
        // banco (um relançamento interrompido no meio, por exemplo). Ninguém mais fala com ela,
        // e o canal dela já foi apagado, então é lixo que só consome memória.
        for rotulo in self.hospedeiro.nossas().await {
            if self.store.rotulo_de_sessao_morta(&rotulo).unwrap_or(false) {
                warn!(rotulo = %rotulo, "sessão órfã de sessão encerrada; matando");
                let _ = self.hospedeiro.mata(&rotulo).await;
            }
        }
        Ok(mortas)
    }

    // ---------------------------------------------------------------- vindo do chat

    /// Mensagem sua num canal de sessão.
    pub async fn on_incoming(&self, canal: &Canal, texto: &str, de: &str) -> Result<()> {
        self.on_incoming_com_arquivos(canal, texto, de, Vec::new())
            .await
    }

    /// O mesmo, com anexos já baixados: a sessão recebe os caminhos na própria linha.
    pub async fn on_incoming_com_arquivos(
        &self,
        canal: &Canal,
        texto: &str,
        de: &str,
        files: Vec<String>,
    ) -> Result<()> {
        let Some(s) = self.store.by_canal(canal.as_str())? else {
            bail!("canal {canal} não tem sessão viva");
        };

        let msg = Incoming {
            text: texto.to_string(),
            from: de.to_string(),
            at: agora(),
            files: files.clone(),
        };
        if !self.hub.deliver(&s.session_id, msg) {
            // Sem monitor armado: guarda para entregar assim que ele voltar, e diz isso, senão
            // parece que a mensagem sumiu. O anexo já está em disco, então o que espera na fila
            // é só o caminho dele.
            self.store.enqueue(&s.session_id, texto, de, &files)?;
            let _ = self
                .frontend
                .envia(
                    Some(canal),
                    "⏳ <i>a sessão está sem monitor armado; guardei a mensagem e ela entra assim que ele voltar</i>",
                    &[],
                    None,
                )
                .await;
            return Ok(());
        }

        self.entregues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                s.session_id.clone(),
                (texto.to_string(), std::time::Instant::now()),
            );

        // Você respondeu: o aviso de "esperando a sua resposta" virou passado.
        if let Some(id) = self.tira_aviso(&s.session_id) {
            self.frontend.apaga(&id).await;
        }

        self.marca_pedido(&s.session_id);
        self.store.set_status(&s.session_id, "pensando")?;
        self.status
            .set(&self.ctx(), &s.session_id, canal, "Pensando...".into());
        Ok(())
    }

    /// A sessão devolvendo um arquivo pelo canal dela.
    ///
    /// Devolve a linha que o agente vê no terminal: ele não enxerga o chat, então o retorno
    /// precisa dizer o que saiu e como.
    pub async fn send_file(
        &self,
        session_id: &str,
        caminho: &str,
        legenda: Option<&str>,
        como_arquivo: bool,
    ) -> Result<String> {
        let Some(s) = self.store.get(session_id)? else {
            bail!("não conheço a sessão {session_id}");
        };
        let Some(canal) = canal_da_sessao(&s) else {
            bail!("esta sessão não tem canal");
        };

        let limites = self.frontend.limites();
        let pronto = crate::arquivos::para_enviar(
            std::path::Path::new(caminho),
            como_arquivo,
            &limites,
            &self.divisores,
        )?;
        let nome = pronto
            .caminho
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| caminho.to_string());
        let tamanho = crate::arquivos::humano_u64(pronto.tamanho);

        // Maior que o teto do frontend: vai em volumes, e não na frente do fim de turno.
        // Dividir e subir um arquivo grande leva minutos, e o hook Stop desiste em 60 segundos;
        // segurar o turno por causa disso deixaria a sessão parada e o texto preso junto.
        if pronto.precisa_dividir {
            self.envia_em_partes(
                &s.session_id,
                canal,
                pronto.caminho.clone(),
                nome.clone(),
                legenda.map(str::to_string),
            );
            return Ok(format!(
                "{nome} ({tamanho}) passa do teto do frontend; mandando em partes"
            ));
        }

        if pronto.como_foto {
            match self
                .frontend
                .envia_arquivo(Some(&canal), &pronto.caminho, legenda, Midia::Foto)
                .await
            {
                Ok(_) => {
                    info!(sessao = %session_id, arquivo = %pronto.caminho.display(), "foto enviada");
                    return Ok(format!("{nome} ({tamanho}) enviado como foto"));
                }
                // O frontend recusa foto por dimensão, proporção e formato que ele não
                // reconhece. Cair para documento entrega o arquivo do mesmo jeito, que é o que
                // foi pedido.
                Err(e) => warn!(erro = %e, "envio como foto recusado; mando como documento"),
            }
        }

        self.frontend
            .envia_arquivo(Some(&canal), &pronto.caminho, legenda, Midia::Documento)
            .await?;
        info!(sessao = %session_id, arquivo = %pronto.caminho.display(), "documento enviado");
        Ok(format!("{nome} ({tamanho}) enviado como documento"))
    }

    /// `true` quando este prompt é o que o daemon acabou de entregar pelo frontend.
    fn e_eco(&self, session_id: &str, texto: &str) -> bool {
        const JANELA: std::time::Duration = std::time::Duration::from_secs(120);
        let mut mapa = self.entregues.lock().unwrap_or_else(|e| e.into_inner());
        mapa.retain(|_, (_, quando)| quando.elapsed() < JANELA);
        match mapa.get(session_id) {
            Some((entregue, _)) => entregue.trim() == texto.trim(),
            None => false,
        }
    }

    fn tira_aviso(&self, session_id: &str) -> Option<MsgId> {
        self.avisos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id)
    }

    fn avisos_handle(&self) -> Arc<Mutex<HashMap<String, MsgId>>> {
        // O mapa é consultado de dentro de uma tarefa que sobrevive a esta chamada, então ele
        // precisa ser compartilhado por Arc, e não emprestado.
        self.avisos.clone()
    }

    /// Entrega uma mensagem direto a uma sessão, sem passar pelo chat.
    pub fn inject(&self, session_id: &str, texto: &str) -> Result<bool> {
        let entregue = self
            .hub
            .deliver(session_id, Incoming::texto(texto, "pc", agora()));
        if entregue {
            self.marca_pedido(session_id);
        } else {
            self.store.enqueue(session_id, texto, "pc", &[])?;
        }
        Ok(entregue)
    }

    // ---------------------------------------------------------------- vindo dos hooks

    pub fn on_register(&self, r: &RegisterSession) -> Result<()> {
        self.inicios
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(r.session_id.clone(), tokio::time::Instant::now());
        // Sessão que o bot criou: só falta o caminho do transcript.
        if let Some(existente) = self.store.get(&r.session_id)? {
            if existente.transcript_path.as_deref() != Some(r.transcript_path.as_str()) {
                self.store
                    .set_transcript(&r.session_id, &r.transcript_path)?;
            }
            return Ok(());
        }

        // `/clear` troca o id da sessão sem trocar o terminal. Sem isto o canal ficaria falando
        // com um id morto, que é exatamente o bug que derrubava a ponte antiga.
        if r.reason == "clear"
            && let Some(anterior) = self.store.live_by_cwd(&r.cwd, &r.session_id)?
        {
            self.store.upsert(&nova_sessao(r))?;
            self.store.rekey(&anterior.session_id, &r.session_id)?;
            self.hub.unlisten_qualquer(&anterior.session_id);
            self.status.forget(&anterior.session_id);
            info!(de = %anterior.session_id, para = %r.session_id, "sessão remapeada depois de /clear");
            return Ok(());
        }

        // Sessão que você abriu no terminal: entra no painel, mas o bot não a controla.
        self.store.upsert(&nova_sessao(r))?;
        self.panel.refresh();
        Ok(())
    }

    pub fn on_event(&self, ev: &SessionEvent) -> Result<()> {
        let Some(s) = self.store.get(&ev.session_id)? else {
            return Ok(());
        };
        // Troca de modelo conta para o painel mesmo em sessão sem canal (as do seu terminal).
        if let EventKind::ModelSwitch { model } = &ev.event {
            self.store.set_model(&ev.session_id, Some(model), None)?;
            self.panel.refresh();
            return Ok(());
        }
        let Some(canal) = canal_da_sessao(&s) else {
            // Sessão de terminal: conta para o painel, não tem onde escrever.
            return Ok(());
        };

        match &ev.event {
            EventKind::ToolStart { label, effort, .. } => {
                // Só escreve quando muda: isto roda a cada ferramenta.
                if effort.is_some() && effort.as_deref() != s.effort.as_deref() {
                    self.store
                        .set_model(&ev.session_id, None, effort.as_deref())?;
                    self.panel.refresh();
                }
                self.store.set_status(&ev.session_id, "ferramenta")?;
                self.status
                    .set(&self.ctx(), &ev.session_id, &canal, label.clone());
            }
            EventKind::ToolEnd { .. } => {
                self.store.set_status(&ev.session_id, "pensando")?;
                self.status
                    .set(&self.ctx(), &ev.session_id, &canal, "Pensando...".into());
            }
            EventKind::Streaming => {
                self.status
                    .set(&self.ctx(), &ev.session_id, &canal, "Escrevendo...".into());
            }
            EventKind::UserPrompt { text } => {
                // Guarda repetida de propósito: o hook já filtra, mas um binário velho no PATH
                // mandaria encanamento para o canal e ninguém veria o erro.
                if !self.agente.e_fala_digitada(text) || self.e_eco(&ev.session_id, text) {
                    return Ok(());
                }
                info!(sessao = %ev.session_id, "prompt digitado no PC, espelhado no canal");
                self.marca_pedido(&ev.session_id);
                self.store.set_status(&ev.session_id, "pensando")?;
                let corpo = format!("👤 <i>do PC</i>\n{}", escapa(&corta(text, 1200)));
                let frontend = self.frontend.clone();
                tokio::spawn(async move {
                    let _ = frontend.envia(Some(&canal), &corpo, &[], None).await;
                });
            }
            EventKind::Elicitation { servidor, pedido } => {
                let onde = match s.hospedagem.as_deref() {
                    Some(h) => format!(
                        "Responda no PC:</i>\n<code>{}</code>",
                        escapa(&self.hospedeiro.como_anexar(h))
                    ),
                    None => "Responda no terminal onde a sessão está aberta.</i>".into(),
                };
                let corpo = format!(
                    "🧩 <b>{}</b> está pedindo confirmação:\n{}\n\n<i>Este diálogo é do próprio \
                     servidor MCP, fora do sistema de permissões do Claude Code, e não dá para \
                     responder daqui. {onde}",
                    escapa(servidor),
                    escapa(&corta(pedido, 600)),
                );
                let frontend = self.frontend.clone();
                let anterior = self.tira_aviso(&ev.session_id);
                let sessao = ev.session_id.clone();
                let avisos = self.avisos_handle();
                tokio::spawn(async move {
                    if let Some(id) = anterior {
                        frontend.apaga(&id).await;
                    }
                    if let Ok(id) = frontend.envia(Some(&canal), &corpo, &[], None).await {
                        avisos
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(sessao, id);
                    }
                });
            }
            // Respondido no PC: o aviso já cumpriu o papel e vira ruído.
            EventKind::ElicitationFim => {
                if let Some(id) = self.tira_aviso(&ev.session_id) {
                    let frontend = self.frontend.clone();
                    tokio::spawn(async move { frontend.apaga(&id).await });
                }
            }
            EventKind::ModelSwitch { model } => {
                self.store.set_model(&ev.session_id, Some(model), None)?;
                self.panel.refresh();
            }
            EventKind::Notification { text } | EventKind::Failure { text } => {
                let texto = text.clone();
                let frontend = self.frontend.clone();
                let anterior = self.tira_aviso(&ev.session_id);
                let sessao = ev.session_id.clone();
                let app_avisos = self.avisos_handle();
                tokio::spawn(async move {
                    // Dois avisos seguidos não se acumulam: o novo substitui o velho.
                    if let Some(id) = anterior {
                        frontend.apaga(&id).await;
                    }
                    if let Ok(id) = frontend
                        .envia(Some(&canal), &format!("⚠️ {}", escapa(&texto)), &[], None)
                        .await
                    {
                        app_avisos
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(sessao, id);
                    }
                });
            }
        }
        Ok(())
    }

    /// Fim de turno: entrega a resposta e diz ao hook se o monitor precisa voltar.
    pub async fn on_stop(&self, r: &StopReport) -> Result<Response> {
        let Some(s) = self.store.get(&r.session_id)? else {
            return Ok(Response::Listener {
                alive: true,
                rearm_attempts: 0,
                rearm_command: None,
            });
        };

        if let Some(t) = &r.transcript_path
            && s.transcript_path.as_deref() != Some(t.as_str())
        {
            self.store.set_transcript(&r.session_id, t)?;
        }
        self.store.set_status(&r.session_id, "ocioso")?;
        // O fim do turno é quando o contexto realmente mudou: é a hora certa de redesenhar.
        self.panel.refresh();

        if let Some(canal) = canal_da_sessao(&s) {
            self.status.clear(&self.ctx(), &r.session_id, &canal);
            let pedida = self.tinha_pedido(&r.session_id);
            let origem = r
                .transcript_path
                .as_deref()
                .or(s.transcript_path.as_deref())
                .and_then(|t| ld_core::transcript::origem_do_turno(std::path::Path::new(t)));
            // Trabalho que a própria sessão deixou rodando (um CI, um monitor dela) acorda um
            // turno sem mensagem nova, mas ele continua o que foi pedido: a resposta é de quem
            // pediu. O que fica de fora é o re-arme do canal e os prompts do daemon.
            let de_fundo = origem == Some(ld_core::transcript::Origem::TarefaDeFundo);
            let resposta = self.resposta_do_turno(r, &s);
            info!(
                sessao = %r.session_id,
                pedida,
                ?origem,
                tem_texto = resposta.is_some(),
                "fim de turno"
            );
            if (pedida || de_fundo)
                && let Some(texto) = resposta
            {
                // Texto e arquivo saem na ordem em que o agente os escreveu. Uma resposta que
                // explica, mostra o gráfico, explica de novo e mostra o log só funciona nessa
                // sequência: agrupar os arquivos num bloco separaria cada imagem do parágrafo
                // que fala dela.
                for pedaco in crate::arquivos::divide_resposta(&texto) {
                    match pedaco {
                        crate::arquivos::Pedaco::Envio(envio) => {
                            self.envia_marcado(&r.session_id, &canal, &envio).await;
                        }
                        crate::arquivos::Pedaco::Texto(t) => {
                            self.frontend.envia_texto(Some(&canal), &t).await?;
                        }
                    }
                }
            }
        }

        // Só sessão do bot com canal precisa de monitor: a do terminal fala pelo teclado.
        let precisa_monitor = s.owned_by_bot() && canal_da_sessao(&s).is_some();
        if !precisa_monitor || self.hub.has_listener(&r.session_id) {
            return Ok(Response::Listener {
                alive: true,
                rearm_attempts: 0,
                rearm_command: None,
            });
        }

        let tentativas = self.hub.bump_rearm(&r.session_id);
        if tentativas > TETO_REARME {
            if let Some(canal) = canal_da_sessao(&s) {
                // `precisa_monitor` garante que a sessão é do bot, então a hospedagem existe.
                let anexar = s
                    .hospedagem
                    .as_deref()
                    .map(|h| self.hospedeiro.como_anexar(h))
                    .unwrap_or_default();
                let _ = self.frontend.envia(
                    Some(&canal),
                    &format!(
                        "🔇 <b>Sessão surda.</b> O monitor não voltou depois de três lembretes, \
                         então parei de insistir. Mande /kill e abra outra, ou reative no PC:\n\
                         <code>{}</code>",
                        escapa(&anexar)
                    ),
                    &[],
                    None,
                ).await;
            }
            return Ok(Response::Listener {
                alive: true,
                rearm_attempts: tentativas,
                rearm_command: None,
            });
        }

        Ok(Response::Listener {
            alive: false,
            rearm_attempts: tentativas,
            rearm_command: Some(format!("lukadispatch listen --session {}", r.session_id)),
        })
    }

    /// Divide um arquivo grande e manda os volumes, em segundo plano.
    ///
    /// Em segundo plano porque isto demora: o que a sessão recebe de volta é "vai chegar aí", e
    /// quem acompanha o progresso é o canal, que ganha uma parte de cada vez.
    fn envia_em_partes(
        &self,
        session_id: &str,
        canal: Canal,
        caminho: std::path::PathBuf,
        nome: String,
        legenda: Option<String>,
    ) {
        let frontend = self.frontend.clone();
        let divisores = self.divisores.clone();
        let dir = crate::arquivos::dir_partes(&self.raiz_arquivos, session_id);
        let sessao = session_id.to_string();
        let teto = frontend.limites().enviar;
        tokio::spawn(async move {
            let anuncio = divisores
                .candidatos(&caminho)
                .first()
                .map(|d| d.anuncio(teto))
                .unwrap_or_else(|| "dividindo o arquivo".to_string());
            let _ = frontend.envia(Some(&canal), &anuncio, &[], None).await;

            let partes = match divisores.divide(&caminho, dir.clone(), teto).await {
                Ok(p) => p,
                Err(e) => {
                    warn!(sessao = %sessao, erro = %e, "não consegui dividir o arquivo");
                    let _ = frontend
                        .envia(
                            Some(&canal),
                            &format!("⚠️ {}", escapa(&format!("{e:#}"))),
                            &[],
                            None,
                        )
                        .await;
                    return;
                }
            };

            let total = partes.arquivos.len();
            let mut enviadas = 0;
            for (i, parte) in partes.arquivos.iter().enumerate() {
                let rotulo = match &legenda {
                    Some(l) => format!("{nome} · parte {}/{total} · {l}", i + 1),
                    None => format!("{nome} · parte {}/{total}", i + 1),
                };
                let enviou = match frontend
                    .envia_arquivo(Some(&canal), parte, Some(&rotulo), partes.midia)
                    .await
                {
                    Ok(id) => Ok(id),
                    // Vídeo recusado (formato, container) ainda entrega como documento; o resto
                    // do fluxo não distingue os dois.
                    Err(e) if partes.midia == Midia::Video => {
                        warn!(erro = %e, "envio como vídeo recusado; mando como documento");
                        frontend
                            .envia_arquivo(Some(&canal), parte, Some(&rotulo), Midia::Documento)
                            .await
                    }
                    Err(e) => Err(e),
                };
                match enviou {
                    Ok(_) => enviadas += 1,
                    Err(e) => {
                        warn!(sessao = %sessao, erro = %e, parte = %parte.display(), "parte não subiu");
                        let _ = frontend
                            .envia(
                                Some(&canal),
                                &format!(
                                    "⚠️ a parte {}/{total} não subiu: {}",
                                    i + 1,
                                    escapa(&format!("{e:#}"))
                                ),
                                &[],
                                None,
                            )
                            .await;
                        break;
                    }
                }
            }

            // Sem a instrução de juntar, um punhado de partes no celular é só lixo.
            if enviadas == total {
                let _ = frontend
                    .envia(Some(&canal), &partes.como_juntar, &[], None)
                    .await;
                info!(sessao = %sessao, arquivo = %caminho.display(), partes = total, "arquivo grande enviado em partes");
            }
            partes.limpa().await;
        });
    }

    /// Manda um arquivo que o agente marcou na resposta.
    ///
    /// Falha aqui não derruba o fim de turno: o texto ainda tem que chegar. O que não pode é o
    /// arquivo sumir calado, então o motivo vai para o canal, com o caminho que falhou.
    async fn envia_marcado(
        &self,
        session_id: &str,
        canal: &Canal,
        envio: &crate::arquivos::Marcado,
    ) {
        match self
            .send_file(
                session_id,
                &envio.caminho,
                envio.legenda.as_deref(),
                envio.como_arquivo,
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                warn!(sessao = %session_id, erro = %e, "marcador de arquivo falhou");
                let _ = self
                    .frontend
                    .envia(
                        Some(canal),
                        &format!("⚠️ {}", escapa(&format!("{e:#}"))),
                        &[],
                        None,
                    )
                    .await;
            }
        }
    }

    // ---------------------------------------------------------------- perguntas

    /// Abre o card de pergunta no canal da sessão e devolve por onde a resposta chega.
    ///
    /// O hook chama isto e fica esperando. Quem responde primeiro (aqui ou na janela do PC)
    /// resolve o mesmo `oneshot`, e o segundo a chegar encontra a pendência já fechada.
    pub async fn start_ask(
        &self,
        session_id: &str,
        ask: Ask,
    ) -> Result<(String, tokio::sync::oneshot::Receiver<String>)> {
        let Some(s) = self.store.get(session_id)? else {
            bail!("sessão desconhecida");
        };
        let Some(canal) = canal_da_sessao(&s) else {
            // Sessão de terminal: o menu nativo do Claude Code é melhor do que um card no
            // celular para quem já está na frente do teclado.
            bail!("sessão sem canal");
        };
        if ask.is_empty() {
            bail!("pergunta vazia");
        }

        let (ask_id, rx) = self.hub.open_ask(session_id);
        let mut card = Card::nova_pergunta(
            ask_id.clone(),
            session_id.to_string(),
            canal.clone(),
            MsgId::new(""),
            ask,
        );
        let Efeito::Redesenhar(texto, botoes) = card.desenhar() else {
            self.hub.close_ask(&ask_id);
            bail!("não consegui desenhar o card");
        };
        let msg = self
            .frontend
            .envia(Some(&canal), &texto, &botoes, None)
            .await?;
        card.msg = msg;
        self.cards.abrir(&ask_id, card);

        self.store.set_status(session_id, "perguntando")?;
        self.status
            .set(&self.ctx(), session_id, &canal, "Perguntando...".into());
        Ok((ask_id, rx))
    }

    /// Card de permissão: dois botões, sem máquina de estados.
    pub async fn start_permission(
        &self,
        session_id: &str,
        ferramenta: &str,
        entrada: &serde_json::Value,
    ) -> Result<(String, tokio::sync::oneshot::Receiver<String>)> {
        let Some(s) = self.store.get(session_id)? else {
            bail!("sessão desconhecida");
        };
        let Some(canal) = canal_da_sessao(&s) else {
            bail!("sessão sem canal");
        };
        // O portão dispara para TODA ferramenta; a política mora aqui.
        //
        // Fora do modo remoto: "não decidi", e a sessão segue o caminho normal do Claude Code.
        if s.permission_mode.as_deref() != Some("perguntar") {
            bail!("sessão não está no modo de perguntar");
        }
        // No modo remoto, a sessão roda em `dontAsk`, que nega por padrão. Por isso o que não
        // merece pergunta precisa ser LIBERADO aqui, e não deixado passar: deixar passar seria
        // negar em silêncio.
        if !self.cfg.pergunta_por(ferramenta) {
            info!(sessao = %session_id, ferramenta, "portão liberou sem perguntar");
            return Err(SemCard::Libera.into());
        }
        info!(sessao = %session_id, ferramenta, "portão pedindo decisão no celular");

        let (ask_id, rx) = self.hub.open_ask(session_id);
        let detalhe = ld_core::labels::label_for_tool(ferramenta, entrada);
        let texto = format!(
            "🔐 <b>Permissão</b>\n{}\n<code>{}</code>",
            escapa(ferramenta),
            escapa(&detalhe)
        );
        let botoes = vec![
            Botao::new("✅ Permitir", format!("p:{ask_id}:a")),
            Botao::new("⛔ Negar", format!("p:{ask_id}:d")),
        ];
        let msg = self
            .frontend
            .envia(Some(&canal), &texto, &botoes, None)
            .await?;
        self.cards.abrir(
            &ask_id,
            Card::nova_permissao(ask_id.clone(), session_id.to_string(), canal.clone(), msg),
        );
        self.store.set_status(session_id, "permissão")?;
        self.status.set(
            &self.ctx(),
            session_id,
            &canal,
            "Esperando você liberar...".into(),
        );
        Ok((ask_id, rx))
    }

    /// Fecha o card. É o único ponto de limpeza: quem espera a resposta chama isto ao terminar,
    /// tanto no caminho feliz quanto no timeout.
    ///
    /// Com resposta, o card **não some**: ele vira o registro do que foi perguntado e do que foi
    /// respondido, sem botões. Apagar deixava a sua resposta escrita no canal sem a pergunta ao
    /// lado, e quem lesse depois não saberia do que se tratava. Sem resposta (timeout, sessão
    /// morta), aí sim ele some: pergunta que ninguém respondeu e ninguém mais pode responder é só
    /// ruído.
    pub async fn cleanup_ask(&self, ask_id: &str, resposta: Option<&str>) {
        self.hub.close_ask(ask_id);
        let Some(card) = self.cards.fechar(ask_id) else {
            return;
        };
        match resposta {
            Some(resumo) => {
                // Quem responde a um card está no turno: a resposta final dele é para essa
                // pessoa, mesmo num turno que ninguém abriu pelo chat (um que o fim de um CI
                // acordou, por exemplo). Sem isto, "faço o deploy?" era respondido no celular e
                // o resultado do deploy nunca chegava lá.
                self.marca_pedido(&card.session_id);
                if self.frontend.edita(&card.msg, resumo, &[]).await.is_err() {
                    // Mensagem sumiu (apagada na mão): manda o registro como mensagem nova.
                    let _ = self
                        .frontend
                        .envia(Some(&card.canal), resumo, &[], None)
                        .await;
                }
            }
            None => self.frontend.apaga(&card.msg).await,
        }
        // A sessão volta a trabalhar: deixar "Perguntando..." parado seria mentira na tela.
        let _ = self.store.set_status(&card.session_id, "pensando");
        self.status.set(
            &self.ctx(),
            &card.session_id,
            &card.canal,
            "Pensando...".into(),
        );
    }

    /// Como a pergunta respondida fica no canal.
    pub fn resumo_respondido(&self, bruta: &str) -> String {
        match serde_json::from_str::<Answer>(bruta) {
            Ok(a) => {
                let mut s = String::from("✅ <b>Respondido</b>");
                for item in &a.items {
                    s.push_str(&format!(
                        "\n\n<b>{}</b>\n{}",
                        escapa(&item.question),
                        escapa(&item.answers.join(", "))
                    ));
                }
                s
            }
            Err(_) => format!("✅ <b>Respondido</b>\n{}", escapa(bruta)),
        }
    }

    /// Idem, para o card de permissão.
    pub fn resumo_permissao(&self, ferramenta: &str, permitido: bool) -> String {
        let decisao = if permitido {
            "✅ Permitido"
        } else {
            "⛔ Negado"
        };
        format!("🔐 <b>{}</b>\n{decisao}", escapa(ferramenta))
    }

    /// Responde o card aberto com o texto que você escreveu no canal.
    ///
    /// Devolve `true` quando havia card esperando. Enquanto ele existe, a sessão está parada
    /// dentro da ferramenta de pergunta: mandar a mensagem para lá seria jogá-la num processo que
    /// não vai lê-la tão cedo.
    pub async fn on_card_text(&self, session_id: &str, texto: &str) -> Result<bool> {
        let Some((ask_id, kind)) = self.cards.aberto_da_sessao(session_id) else {
            return Ok(false);
        };

        if kind == crate::cards::Kind::Permissao {
            // Permissão é binária: só o que for claramente sim ou não conta, e o resto continua
            // sendo mensagem para a sessão (que a lerá quando a permissão for resolvida).
            let baixo = texto.trim().to_lowercase();
            let decisao = match baixo.as_str() {
                "sim" | "s" | "permitir" | "pode" | "ok" => Some("allow"),
                "não" | "nao" | "n" | "negar" => Some("deny"),
                _ => None,
            };
            let Some(decisao) = decisao else {
                return Ok(false);
            };
            self.hub.answer(&ask_id, decisao.to_string());
            return Ok(true);
        }

        match self.cards.tocar(&ask_id, Acao::Texto(texto.to_string())) {
            Efeito::Redesenhar(corpo, botoes) => {
                if let Some(msg) = self.cards.msg(&ask_id) {
                    self.frontend.edita(&msg, &corpo, &botoes).await?;
                }
                Ok(true)
            }
            Efeito::Pronto(resposta) => {
                let payload = serde_json::to_string(&resposta).unwrap_or_default();
                self.hub.answer(&ask_id, payload);
                Ok(true)
            }
            Efeito::Ignorar => Ok(false),
        }
    }

    /// Toque em botão de card, vindo do chat.
    pub async fn on_card_touch(&self, dado: &str) -> Result<()> {
        let mut partes = dado.split(':');
        let tipo = partes.next().unwrap_or("");
        let ask_id = partes.next().unwrap_or("").to_string();
        let acao = partes.next().unwrap_or("");

        match tipo {
            "p" => {
                let decisao = if acao == "a" { "allow" } else { "deny" };
                self.hub.answer(&ask_id, decisao.to_string());
            }
            "a" => {
                let acao = if acao == "c" {
                    Acao::Confirmar
                } else {
                    match acao.parse::<usize>() {
                        Ok(i) => Acao::Opcao(i),
                        Err(_) => return Ok(()),
                    }
                };
                match self.cards.tocar(&ask_id, acao) {
                    Efeito::Redesenhar(texto, botoes) => {
                        if let Some(msg) = self.cards.msg(&ask_id) {
                            self.frontend.edita(&msg, &texto, &botoes).await?;
                        }
                    }
                    Efeito::Pronto(resposta) => {
                        let payload = serde_json::to_string(&resposta)
                            .unwrap_or_else(|_| "{\"items\":[]}".into());
                        self.hub.answer(&ask_id, payload);
                    }
                    Efeito::Ignorar => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Traduz o que veio do canal vencedor para o texto que o Claude recebe.
    pub fn resposta_para_claude(&self, bruta: &str) -> String {
        match serde_json::from_str::<Answer>(bruta) {
            Ok(a) => a.to_claude(),
            // Veio da janela do PC em formato livre: repassa como está, avisando que é resposta.
            Err(_) => format!("O usuário respondeu pelo lukadispatch: {bruta}"),
        }
    }

    /// A fala que de fato responde ao turno.
    ///
    /// O hook entrega a ÚLTIMA mensagem do assistente, e o agente costuma continuar falando
    /// depois de responder: entrega o resultado e anuncia que re-armou o monitor. Quando a
    /// última é só esse anúncio, quem sabe achar a resposta anterior do mesmo turno é o agente
    /// (a sua conversa gravada tem um formato só ele conhece).
    fn resposta_do_turno(&self, r: &StopReport, s: &Session) -> Option<String> {
        let caminho = r
            .transcript_path
            .as_deref()
            .or(s.transcript_path.as_deref())
            .map(std::path::Path::new);
        self.agente
            .resposta_do_turno(r.last_assistant_message.as_deref(), caminho)
    }

    // ---------------------------------------------------------------- painel

    pub fn summaries(&self) -> Result<Vec<SessionSummary>> {
        Ok(self
            .store
            .live()?
            .into_iter()
            .map(|s| {
                let ctx = self.agente.contexto(&s);
                s.summary(ctx)
            })
            .collect())
    }

    pub async fn session_for_canal(&self, canal: &Canal) -> Result<Option<Session>> {
        self.store
            .by_canal(canal.as_str())
            .context("consultando canal")
    }
}

/// O canal opaco guardado no banco, como o tipo que o resto do domínio entende.
/// Como o canal da sessão se chama: o projeto, e a branch quando ela roda numa worktree, para
/// duas sessões do mesmo projeto não terem canais de mesmo nome.
pub fn nome_do_canal(projeto: &str, worktree: Option<&Worktree>) -> String {
    match worktree {
        Some(w) => format!("{projeto} · {}", w.branch),
        None => projeto.to_string(),
    }
}

fn canal_da_sessao(s: &Session) -> Option<Canal> {
    s.canal_id.as_deref().map(Canal::new)
}

/// Linha de identificação da sessão: modelo, esforço, modo de permissão e onde ela roda.
fn ficha(model: Option<&str>, effort: Option<&str>, modo: &str, onde: &str) -> String {
    format!(
        "modelo: {} · esforço: {} · permissão: {modo} · {onde}",
        model.unwrap_or("padrão"),
        effort.unwrap_or("padrão"),
    )
}

fn nova_sessao(r: &RegisterSession) -> Session {
    Session {
        session_id: r.session_id.clone(),
        project: nome_do_cwd(&r.cwd),
        cwd: r.cwd.clone(),
        transcript_path: Some(r.transcript_path.clone()),
        hospedagem: None,
        canal_id: None,
        status: "ocioso".into(),
        status_msg_id: None,
        model: r.model.clone(),
        effort: None,
        permission_mode: None,
        created_at: 0,
        ended_at: None,
    }
}

/// Corta preservando o começo, que é onde está o assunto da fala.
fn corta(texto: &str, teto: usize) -> String {
    if texto.chars().count() <= teto {
        return texto.to_string();
    }
    format!(
        "{}\n[…]",
        texto.chars().take(teto).collect::<String>().trim_end()
    )
}

fn nome_do_cwd(cwd: &str) -> String {
    cwd.rsplit('/')
        .find(|p| !p.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

fn agora() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nome_do_cwd_ignora_barra_final() {
        assert_eq!(nome_do_cwd("/home/luka/Personal/proj/"), "proj");
        assert_eq!(nome_do_cwd("/home/luka/Personal/proj"), "proj");
    }
}
