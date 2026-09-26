//! Os fluxos do daemon de ponta a ponta, com todas as portas trocadas por dublês.
//!
//! Prova que o `App` e o roteador falam só com as traits (`Frontend`, `Agente`, `Memoria`,
//! `Hospedeiro`, `Transcritor`, `Divisor`), e nunca com uma implementação por baixo.
//!
//! O frontend é o [`Memoria`], que registra tudo que o daemon mandou, editou e apagou. Se um
//! fluxo precisar de algo que não passa pela trait `Frontend`, não há como escrevê-lo aqui, e é
//! exatamente isso que se quer provar: o domínio não sabe que o Telegram existe.
//!
//! Nada aqui toca a máquina de verdade: sem tmux (hospedeiro de mentira), sem Claude Code
//! (agente de mentira, com outros esforços, outros modos e outra linha de comando), sem modelo de
//! voz (transcritor de mentira), sem 7z (divisor de mentira), e sem `~/.local` (raízes de arquivos
//! e de sessões num tempdir).
//!
//! O agente de mentira é a prova de que o domínio não depende do Claude Code: se o `/effort`
//! mostra os níveis dele, e não os do Claude Code, é porque o roteador pergunta à trait.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use ld_core::config::{Config, Project};
use ld_core::context::ContextUsage;
use ld_core::models::Modelo;
use ld_core::proto::{EventKind, RegisterSession, SessionEvent, StopReport};
use ld_core::state::{Session, Store, Worktree};
use ld_core::transcript::{Fala, SessaoAnterior};
use ld_core::usage::{SessionTokens, Windows};
use ld_daemon::agente::claude_code::{ClaudeCode, Locais};
use ld_daemon::agente::{
    Agente, AiMemory, Guardado, Invocacao, Modo, Partida, PartidaDaMemoria, PedidoDePartida,
    SemMemoria,
};
use ld_daemon::app::{App, Portas};
use ld_daemon::divisor::{Divisor, Divisores, Partes};
use ld_daemon::frontend::memoria::{Chamada, Memoria};
use ld_daemon::frontend::nulo::Nulo;
use ld_daemon::frontend::{
    Anexo, Autor, Canal, Evento, Frontend, Limites, Midia, MsgId, TipoAnexo,
};
use ld_daemon::hub::Aviso;
use ld_daemon::roteador;
use ld_daemon::sessions::{Hospedeiro, Launched, Situacao};
use ld_daemon::transcritor::{Transcrito, Transcritor};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;

// ------------------------------------------------------------------ dublês das portas

struct TranscritorFalso(&'static str);

#[async_trait]
impl Transcritor for TranscritorFalso {
    fn nome(&self) -> &str {
        "falso"
    }
    async fn transcreve(&self, _audio: &Path) -> Result<Transcrito> {
        Ok(Transcrito {
            texto: self.0.to_string(),
            duracao: Duration::ZERO,
        })
    }
}

/// Guarda cada partida que recebeu, para os testes lerem o script que o daemon montou.
#[derive(Default)]
struct HospedeiroFalso {
    vivas: Mutex<HashSet<String>>,
    /// Hospedagem que continua viva com outro nome, como o terminal num live handoff do herdr.
    mudou: Mutex<HashMap<String, String>>,
    /// Hospedagem cujo processo morreu e cujo lugar o hospedeiro devolveu ao reiniciar.
    restauradas: Mutex<HashSet<String>>,
    /// Recusa os próximos lançamentos, como um hospedeiro que não consegue subir a sessão.
    recusa: Mutex<bool>,
    /// Quantos dos próximos lançamentos morrem ao subir com a memória presa a uma partida
    /// anterior, deixando no painel o que o `ai-memory run` diz nesse caso.
    presa: Mutex<u32>,
    /// O processo que o painel diz rodar.
    pid: Mutex<Option<u32>>,
    lancadas: Mutex<u32>,
    partidas: Mutex<Vec<Partida>>,
    /// `mata:<nome>` e `lanca:<id>`, na ordem em que aconteceram.
    eventos: Mutex<Vec<String>>,
}

impl HospedeiroFalso {
    /// O script e o prompt da última partida.
    fn ultima_partida(&self) -> (String, String) {
        let p = self
            .partidas
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("nenhuma partida");
        let script = std::fs::read_to_string(&p.script).expect("script de partida");
        let prompt =
            std::fs::read_to_string(p.script.with_file_name("prompt.txt")).unwrap_or_default();
        (script, prompt)
    }
}

#[async_trait]
impl Hospedeiro for HospedeiroFalso {
    async fn lanca(&self, partida: &Partida, projeto: &Project) -> Result<Launched> {
        if *self.recusa.lock().unwrap() {
            anyhow::bail!("o hospedeiro recusou");
        }
        {
            let mut presa = self.presa.lock().unwrap();
            if *presa > 0 {
                *presa -= 1;
                std::fs::write(
                    &partida.log,
                    "Error: opening managed workstream\nCaused by: workstream is already active: x",
                )
                .unwrap();
                anyhow::bail!("a sessão morreu ao subir: Error: opening managed workstream");
            }
        }
        *self.lancadas.lock().unwrap() += 1;
        self.eventos
            .lock()
            .unwrap()
            .push(format!("lanca:{}", partida.session_id));
        assert!(
            partida.script.exists(),
            "o hospedeiro recebeu script que não existe"
        );
        self.partidas.lock().unwrap().push(partida.clone());
        // Cada lançamento ganha uma hospedagem nova, como o terminal do herdr: quem relança
        // tem de gravar a nova, senão a reconciliação procura pela velha.
        let n = *self.lancadas.lock().unwrap();
        let hospedagem = format!("ld-{}-{}@{n}", projeto.name, partida.session_id);
        self.vivas.lock().unwrap().insert(hospedagem.clone());
        Ok(Launched {
            session_id: partida.session_id.clone(),
            hospedagem,
        })
    }
    async fn vive(&self, nome: &str) -> bool {
        self.vivas.lock().unwrap().contains(nome)
    }
    async fn situacao(&self, nome: &str) -> Situacao {
        if let Some(nova) = self.mudou.lock().unwrap().get(nome) {
            return Situacao::Mudou(nova.clone());
        }
        if self.restauradas.lock().unwrap().contains(nome) {
            return Situacao::Restaurada;
        }
        if self.vive(nome).await {
            Situacao::Viva
        } else {
            Situacao::Morta
        }
    }
    /// Aceita a hospedagem ou só o rótulo, como o contrato pede.
    async fn mata(&self, nome: &str) -> Result<()> {
        self.eventos.lock().unwrap().push(format!("mata:{nome}"));
        self.vivas
            .lock()
            .unwrap()
            .retain(|h| h != nome && rotulo(h) != nome);
        self.restauradas.lock().unwrap().remove(nome);
        Ok(())
    }
    async fn pid(&self, _nome: &str) -> Option<u32> {
        *self.pid.lock().unwrap()
    }
    async fn nossas(&self) -> Vec<String> {
        self.vivas
            .lock()
            .unwrap()
            .iter()
            .map(|h| rotulo(h).to_string())
            .collect()
    }
    fn descreve(&self, nome: &str) -> String {
        format!("falso: {nome}")
    }
    fn como_anexar(&self, nome: &str) -> String {
        format!("falso-anexa {nome}")
    }
}

/// O começo da hospedagem, antes do `@`.
fn rotulo(hospedagem: &str) -> &str {
    hospedagem.split('@').next().unwrap_or_default()
}

/// Um agente que não é o Claude Code: outros níveis de esforço, um modo só, outro apelido de
/// modelo e outra linha de comando. Nada do domínio pode assumir o Claude Code sem que um teste
/// daqui perceba.
struct AgenteFalso;

const MODOS_FALSOS: [Modo; 1] = [Modo {
    id: "livre",
    rotulo: "🕊 livre",
    no_menu: true,
}];

impl Agente for AgenteFalso {
    fn nome(&self) -> &'static str {
        "falso"
    }
    fn prepara(&self) -> Result<()> {
        Ok(())
    }
    fn confia(&self, _pasta: &Path) -> Result<bool> {
        Ok(false)
    }
    fn novo_id(&self) -> String {
        "id-falso-0001".into()
    }
    fn invocacao(
        &self,
        pedido: &PedidoDePartida<'_>,
        session_id: &str,
        _dir: &Path,
    ) -> Result<Invocacao> {
        let mut argv = vec!["agente-falso".to_string(), "--id".into(), session_id.into()];
        if let Some(r) = pedido.resume {
            argv.extend(["--continua".into(), r.into()]);
        }
        if let Some(m) = pedido.model {
            argv.extend(["--cerebro".into(), m.into()]);
        }
        if let Some(e) = pedido.effort {
            argv.extend(["--forca".into(), e.into()]);
        }
        Ok(Invocacao {
            argv,
            prompt: Some(format!(
                "fale com o Luka por {}; arquivo até {} bytes{}",
                pedido.chat.onde,
                pedido.chat.teto_envio,
                pedido
                    .instrucoes
                    .map(|i| format!("\n{i}"))
                    .unwrap_or_default()
            )),
        })
    }
    fn modelos(&self) -> Vec<Modelo> {
        Vec::new()
    }
    fn e_nome_de_modelo(&self, palavra: &str) -> bool {
        palavra == "cerebro-grande"
    }
    fn esforcos(&self) -> &'static [&'static str] {
        &["pouco", "muito"]
    }
    fn modos(&self) -> &'static [Modo] {
        &MODOS_FALSOS
    }
    fn valida_modo(&self, modo: &str) -> Result<()> {
        if modo == "livre" {
            Ok(())
        } else {
            anyhow::bail!("o agente falso só conhece o modo livre")
        }
    }
    fn historico(&self, _sessao: &Session, _limite: usize) -> Vec<Fala> {
        Vec::new()
    }
    fn resposta_do_turno(&self, ultima: Option<&str>, _t: Option<&Path>) -> Option<String> {
        ultima.map(str::to_string)
    }
    fn e_fala_digitada(&self, _texto: &str) -> bool {
        true
    }
    fn ultima_sessao(&self, _cwd: &str) -> Option<SessaoAnterior> {
        None
    }
    fn contexto(&self, _sessao: &Session) -> Option<ContextUsage> {
        None
    }
    fn modelo_da_sessao(&self, _sessao: &Session) -> Option<String> {
        None
    }
    fn uso(&self) -> Windows {
        Windows::default()
    }
    fn tokens_da_sessao(&self, _session_id: &str) -> Option<SessionTokens> {
        None
    }
}

/// Parte qualquer arquivo em três pedaços, e diz como juntar.
struct DivisorFalso;

#[async_trait]
impl Divisor for DivisorFalso {
    fn nome(&self) -> &str {
        "falso"
    }
    fn disponivel(&self) -> bool {
        true
    }
    fn aceita(&self, _caminho: &Path) -> bool {
        true
    }
    fn anuncio(&self, _teto: u64) -> String {
        "estou partindo em pedaços".into()
    }
    async fn divide(&self, _caminho: &Path, dir: PathBuf, _teto: u64) -> Result<Partes> {
        tokio::fs::create_dir_all(&dir).await?;
        let mut arquivos = Vec::new();
        for i in 1..=3 {
            let p = dir.join(format!("pedaco.{i:03}"));
            tokio::fs::write(&p, b"x").await?;
            arquivos.push(p);
        }
        Ok(Partes::new(
            dir,
            arquivos,
            Midia::Documento,
            "junte tudo".into(),
        ))
    }
}

// ------------------------------------------------------------------ montagem

const SESSAO: &str = "s1";
const TMUX: &str = "ld-proj-s1";

struct Cena {
    app: Arc<App>,
    fe: Arc<Memoria>,
    hospedeiro: Arc<HospedeiroFalso>,
    canal: Canal,
    raiz: TempDir,
}

fn luka() -> Autor {
    Autor {
        id: "42".into(),
        nome: "Luka".into(),
    }
}

async fn cena_com(limites: Limites) -> Cena {
    cena_montada(limites, Arc::new(SemMemoria), |_, _| {}).await
}

async fn cena_montada(
    limites: Limites,
    memoria: Arc<dyn ld_daemon::agente::Memoria>,
    ajusta: impl FnOnce(&mut Config, &Path),
) -> Cena {
    let fe = Memoria::com_limites(limites);
    let canal = fe.cria_canal("proj").await.unwrap();
    fe.limpa_registro();

    let store = Store::open_memory().unwrap();
    store
        .upsert(&Session {
            session_id: SESSAO.into(),
            project: "proj".into(),
            cwd: "/tmp/proj".into(),
            transcript_path: None,
            hospedagem: Some(TMUX.into()),
            canal_id: Some(canal.as_str().to_string()),
            status: "ocioso".into(),
            status_msg_id: None,
            model: None,
            effort: None,
            permission_mode: Some("auto".into()),
            created_at: 0,
            ended_at: None,
        })
        .unwrap();

    let hospedeiro = Arc::new(HospedeiroFalso::default());
    hospedeiro.vivas.lock().unwrap().insert(TMUX.into());

    let raiz = tempfile::tempdir().unwrap();
    let cfg = Config {
        trust_projects: false,
        projects: vec![Project {
            name: "outro".into(),
            path: "/tmp/outro".into(),
            permission_mode: None,
            model: None,
            effort: None,
        }],
        scan: ld_core::config::Scan {
            enabled: false,
            ..Default::default()
        },
        ..Config::default()
    };
    let mut cfg = cfg;
    ajusta(&mut cfg, raiz.path());
    let app = App::new(
        cfg,
        store,
        Portas {
            frontend: fe.clone(),
            agente: Arc::new(AgenteFalso),
            memoria,
            transcritor: Some(Arc::new(TranscritorFalso("roda os testes"))),
            divisores: Divisores::new(vec![Arc::new(DivisorFalso)]),
            hospedeiro: hospedeiro.clone(),
        },
    )
    .com_raiz_arquivos(raiz.path().join("arquivos"))
    .com_raiz_sessoes(raiz.path().join("sessoes"))
    .com_espera_depois_do_fim(Duration::from_millis(50))
    .com_espera_da_memoria(Duration::from_secs(100))
    .com_raiz_worktrees(raiz.path().join("wt"));

    Cena {
        app: Arc::new(app),
        fe,
        hospedeiro,
        canal,
        raiz,
    }
}

async fn cena() -> Cena {
    cena_com(Limites::default()).await
}

impl Cena {
    fn mensagem(&self, msg: &str, texto: &str, responde_a: Option<&str>) -> Evento {
        Evento::Mensagem {
            autor: luka(),
            canal: Some(self.canal.clone()),
            msg: MsgId::new(msg),
            texto: texto.into(),
            responde_a: responde_a.map(MsgId::new),
            anexos: vec![],
        }
    }

    fn voz(&self, msg: &str) -> Evento {
        self.fe.guarda_anexo("v1", "voice/file_1.oga", b"OggS");
        Evento::Mensagem {
            autor: luka(),
            canal: Some(self.canal.clone()),
            msg: MsgId::new(msg),
            texto: String::new(),
            responde_a: None,
            anexos: vec![Anexo {
                id: "v1".into(),
                tamanho: 4,
                nome: None,
                tipo: TipoAnexo::Voz,
            }],
        }
    }

    fn toque(&self, msg: &MsgId, dado: &str) -> Evento {
        Evento::Toque {
            autor: luka(),
            canal: Some(self.canal.clone()),
            msg: Some(msg.clone()),
            dado: dado.into(),
        }
    }

    async fn trata(&self, ev: Evento) {
        roteador::trata(self.app.clone(), ev).await.unwrap();
    }

    /// O card de transcrição na tela: a mensagem e o dado dos dois botões.
    async fn espera_card(&self) -> (MsgId, String, String, Option<MsgId>) {
        let fe = self.fe.clone();
        espera("card de transcrição", move || {
            fe.chamadas().into_iter().find_map(|c| match c {
                Chamada::Envia {
                    botoes,
                    msg,
                    responde_a,
                    rico,
                    ..
                } if botoes.iter().any(|b| b.dado.starts_with("t:ok:")) => {
                    assert!(rico.contains("roda os testes"), "{rico}");
                    let ok = botoes.iter().find(|b| b.dado.starts_with("t:ok:"))?;
                    let no = botoes.iter().find(|b| b.dado.starts_with("t:no:"))?;
                    Some((msg, ok.dado.clone(), no.dado.clone(), responde_a))
                }
                _ => None,
            })
        })
        .await
    }
}

/// Espera até `f` devolver algo, ou falha dizendo o que não aconteceu.
async fn espera<T>(o_que: &str, mut f: impl FnMut() -> Option<T>) -> T {
    for _ in 0..250 {
        if let Some(v) = f() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("não aconteceu em 5 s: {o_que}");
}

/// A próxima mensagem que a sessão recebeu, com prazo.
async fn recebe(rx: &mut UnboundedReceiver<Aviso>) -> ld_daemon::hub::Incoming {
    match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
        Ok(Some(Aviso::Mensagem(m))) => m,
        outro => panic!("a sessão não recebeu mensagem: {outro:?}"),
    }
}

/// A sessão NÃO pode ter recebido nada.
fn nada_chegou(rx: &mut UnboundedReceiver<Aviso>) {
    if let Ok(a) = rx.try_recv() {
        panic!("a sessão recebeu o que não devia: {a:?}");
    }
}

fn apagou(fe: &Memoria, msg: &str) -> bool {
    fe.chamadas().contains(&Chamada::Apaga(MsgId::new(msg)))
}

// ------------------------------------------------------------------ ciclo de vida

#[tokio::test(flavor = "multi_thread")]
async fn criar_sessao_abre_canal_e_apresenta_a_sessao() {
    let c = cena().await;
    let projeto = Project {
        name: "outro".into(),
        path: "/tmp/outro".into(),
        permission_mode: None,
        model: None,
        effort: None,
    };
    let id = c
        .app
        .create_session(&projeto, Some("opus"), None, None)
        .await
        .unwrap();

    let canal =
        c.fe.chamadas()
            .into_iter()
            .find_map(|ch| match ch {
                Chamada::CriaCanal { nome, canal } if nome == "outro" => Some(canal),
                _ => None,
            })
            .expect("o canal da sessão não foi criado");
    let s = c.app.store.get(&id).unwrap().unwrap();
    assert_eq!(s.canal_id.as_deref(), Some(canal.as_str()));
    assert_eq!(*c.hospedeiro.lancadas.lock().unwrap(), 1);
    assert!(
        c.fe.chamadas().iter().any(|ch| matches!(ch,
            Chamada::Envia { canal: Some(k), rico, .. }
                if *k == canal && rico.contains("outro") && rico.contains("/kill"))),
        "a sessão nasce se apresentando no canal dela: {:?}",
        c.fe.textos()
    );
    // Onde ela roda vem do hospedeiro, não de um "tmux:" fixo no domínio.
    assert!(
        c.fe.textos()
            .iter()
            .any(|t| t.contains(&format!("falso: ld-outro-{id}"))),
        "{:?}",
        c.fe.textos()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_no_canal_encerra_mata_e_apaga_o_canal() {
    let c = cena().await;
    c.trata(c.mensagem("50", "/kill", None)).await;
    assert!(c.app.store.get(SESSAO).unwrap().unwrap().ended_at.is_some());
    assert!(
        !c.hospedeiro.vive(TMUX).await,
        "o hospedeiro não foi mandado matar"
    );
    assert!(
        c.fe.chamadas()
            .contains(&Chamada::ApagaCanal(c.canal.clone())),
        "o canal ficou para trás"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reconciliacao_encerra_sessao_cujo_hospedeiro_morreu() {
    let c = cena().await;
    c.hospedeiro.mata(TMUX).await.unwrap();
    assert_eq!(c.app.reconcile().await.unwrap(), 1);
    assert!(c.app.store.get(SESSAO).unwrap().unwrap().ended_at.is_some());
    assert!(
        c.fe.chamadas()
            .contains(&Chamada::ApagaCanal(c.canal.clone()))
    );
}

/// O hospedeiro reiniciou: o processo da sessão morreu, e o lugar dela voltou sem ele.
fn restart_do_hospedeiro(c: &Cena) {
    c.hospedeiro.vivas.lock().unwrap().remove(TMUX);
    c.hospedeiro.restauradas.lock().unwrap().insert(TMUX.into());
}

#[tokio::test(flavor = "multi_thread")]
async fn reconciliacao_relanca_no_mesmo_canal_a_sessao_que_o_hospedeiro_restaurou() {
    let c = cena().await;
    // Caiu no meio de um turno: o turno se perdeu, e a guarda do /model não vale aqui.
    c.app.store.set_status(SESSAO, "pensando").unwrap();
    restart_do_hospedeiro(&c);

    assert_eq!(c.app.reconcile().await.unwrap(), 0);

    let s = c.app.store.get(SESSAO).unwrap().unwrap();
    assert!(s.ended_at.is_none(), "a sessão restaurada foi encerrada");
    assert_eq!(s.canal_id.as_deref(), Some(c.canal.as_str()));
    assert_eq!(s.hospedagem.as_deref(), Some("ld-proj-s1@1"));
    assert_eq!(
        *c.hospedeiro.eventos.lock().unwrap(),
        [format!("mata:{TMUX}"), format!("lanca:{SESSAO}")],
        "o lugar restaurado tem de fechar antes de a sessão subir de novo"
    );
    let (script, _) = c.hospedeiro.ultima_partida();
    assert!(
        script.contains(&format!("'--continua' '{SESSAO}'")),
        "{script}"
    );
    assert!(
        !c.fe
            .chamadas()
            .contains(&Chamada::ApagaCanal(c.canal.clone()))
    );
    assert!(
        c.fe.chamadas().iter().any(|ch| matches!(ch,
            Chamada::Envia { canal: Some(k), rico, .. } if *k == c.canal && rico.contains("caiu junto"))),
        "o canal não soube por que a sessão voltou: {:?}",
        c.fe.textos()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sessao_restaurada_que_nao_volta_encerra_e_avisa_no_principal() {
    let c = cena().await;
    restart_do_hospedeiro(&c);
    *c.hospedeiro.recusa.lock().unwrap() = true;

    assert_eq!(c.app.reconcile().await.unwrap(), 1);

    assert!(c.app.store.get(SESSAO).unwrap().unwrap().ended_at.is_some());
    assert!(
        c.fe.chamadas()
            .contains(&Chamada::ApagaCanal(c.canal.clone()))
    );
    assert!(
        c.fe.chamadas().iter().any(|ch| matches!(ch,
            Chamada::Envia { canal: None, rico, .. } if rico.contains("o hospedeiro recusou"))),
        "o motivo não chegou ao canal principal: {:?}",
        c.fe.textos()
    );
    // Uma tentativa só: a próxima volta não relança de novo.
    *c.hospedeiro.recusa.lock().unwrap() = false;
    c.app.reconcile().await.unwrap();
    assert_eq!(*c.hospedeiro.lancadas.lock().unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn processo_do_bot_que_acaba_por_fora_nao_encerra_antes_da_reconciliacao() {
    let c = cena().await;
    restart_do_hospedeiro(&c);
    // O Claude Code manda `other` quando o processo morre por fora, como num restart do herdr.
    c.app.end_session_por_hook(SESSAO, "other").await.unwrap();
    assert!(
        !c.fe
            .chamadas()
            .contains(&Chamada::ApagaCanal(c.canal.clone())),
        "o canal foi apagado antes de a reconciliação decidir"
    );

    // A reconciliação agendada relança a sessão restaurada.
    let hospedeiro = c.hospedeiro.clone();
    espera(
        "a reconciliação agendada relançar a sessão",
        move || (*hospedeiro.lancadas.lock().unwrap() == 1).then_some(()),
    )
    .await;
    assert!(c.app.store.get(SESSAO).unwrap().unwrap().ended_at.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn fim_pedido_por_voce_encerra_na_hora() {
    let c = cena().await;
    c.app
        .end_session_por_hook(SESSAO, "prompt_input_exit")
        .await
        .unwrap();
    assert!(c.app.store.get(SESSAO).unwrap().unwrap().ended_at.is_some());
    assert!(
        c.fe.chamadas()
            .contains(&Chamada::ApagaCanal(c.canal.clone()))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reconciliacao_segue_a_sessao_viva_que_mudou_de_hospedagem() {
    let c = cena().await;
    c.hospedeiro
        .mudou
        .lock()
        .unwrap()
        .insert(TMUX.into(), format!("{TMUX}@novo"));
    assert_eq!(c.app.reconcile().await.unwrap(), 0);
    let s = c.app.store.get(SESSAO).unwrap().unwrap();
    assert!(s.ended_at.is_none(), "a sessão viva foi encerrada");
    assert_eq!(s.hospedagem.as_deref(), Some(&*format!("{TMUX}@novo")));
    assert!(
        !c.fe
            .chamadas()
            .contains(&Chamada::ApagaCanal(c.canal.clone()))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reconciliacao_mata_pelo_rotulo_o_que_sobrou_de_sessao_encerrada() {
    let c = cena().await;
    c.app
        .store
        .set_hospedagem(SESSAO, &format!("{TMUX}@velho"))
        .unwrap();
    c.app.store.end(SESSAO).unwrap();
    // O hospedeiro reiniciou e devolveu o lugar da sessão com outro nome: o rótulo é o mesmo.
    c.hospedeiro.vivas.lock().unwrap().clear();
    c.hospedeiro
        .vivas
        .lock()
        .unwrap()
        .insert(format!("{TMUX}@restaurado"));
    c.app.reconcile().await.unwrap();
    assert!(
        c.hospedeiro.vivas.lock().unwrap().is_empty(),
        "sobrou o pane de uma sessão encerrada"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn o_frontend_nulo_sustenta_o_daemon_inteiro() {
    // O modo offline deixou de ser um `bool` espalhado: é só outro frontend.
    let raiz_nulo = tempfile::tempdir().unwrap();
    let app = App::new(
        Config {
            trust_projects: false,
            ..Config::default()
        },
        Store::open_memory().unwrap(),
        Portas {
            frontend: Arc::new(Nulo::default()),
            agente: Arc::new(AgenteFalso),
            memoria: Arc::new(SemMemoria),
            transcritor: None,
            divisores: Divisores::new(vec![]),
            hospedeiro: Arc::new(HospedeiroFalso::default()),
        },
    )
    .com_raiz_sessoes(raiz_nulo.path().to_path_buf());
    let projeto = Project {
        name: "p".into(),
        path: "/tmp/p".into(),
        permission_mode: None,
        model: None,
        effort: None,
    };
    let id = app
        .create_session(&projeto, None, None, None)
        .await
        .unwrap();
    let s = app.store.get(&id).unwrap().unwrap();
    assert!(s.canal_id.unwrap().starts_with("nulo-"));
}

// ------------------------------------------------------------------ conversa

#[tokio::test(flavor = "multi_thread")]
async fn mensagem_no_canal_chega_na_sessao_que_escuta() {
    let c = cena().await;
    let (_t, mut rx) = c.app.hub.listen(SESSAO);
    c.trata(c.mensagem("10", "oi", None)).await;
    let m = recebe(&mut rx).await;
    assert_eq!((m.text.as_str(), m.from.as_str()), ("oi", "Luka"));
}

#[tokio::test(flavor = "multi_thread")]
async fn sem_monitor_a_mensagem_espera_na_fila_e_avisa() {
    let c = cena().await;
    c.trata(c.mensagem("10", "guarda isso", None)).await;
    let fila = c.app.store.drain(SESSAO).unwrap();
    assert_eq!(fila.len(), 1);
    assert!(
        c.fe.textos().iter().any(|t| t.contains("sem monitor")),
        "sem aviso a mensagem parece ter sumido: {:?}",
        c.fe.textos()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn comando_no_canal_principal_some_junto_com_a_resposta() {
    let c = cena().await;
    c.trata(Evento::Mensagem {
        autor: luka(),
        canal: None,
        msg: MsgId::new("77"),
        texto: "/ls".into(),
        responde_a: None,
        anexos: vec![],
    })
    .await;
    assert!(apagou(&c.fe, "77"), "o comando ficou no painel");
}

// ------------------------------------------------------------------ voz e o card de aval

#[tokio::test(flavor = "multi_thread")]
async fn voz_vira_card_que_responde_ao_audio_e_so_vai_com_aval() {
    let c = cena().await;
    let (_t, mut rx) = c.app.hub.listen(SESSAO);
    c.trata(c.voz("900")).await;

    let (card, ok, _no, responde_a) = c.espera_card().await;
    assert_eq!(
        responde_a,
        Some(MsgId::new("900")),
        "o card tem de apontar para o áudio"
    );
    nada_chegou(&mut rx);

    c.trata(c.toque(&card, &ok)).await;
    let m = recebe(&mut rx).await;
    assert_eq!(m.text, "roda os testes");
    assert_eq!(m.files.len(), 1);
    let audio = Path::new(&m.files[0]);
    assert!(
        audio.starts_with(c.raiz.path()),
        "o áudio tem de cair na raiz de arquivos: {audio:?}"
    );
    assert_eq!(std::fs::read(audio).unwrap(), b"OggS");
    assert!(
        c.fe.chamadas().iter().any(|ch| matches!(ch,
            Chamada::Edita { msg, botoes, rico } if *msg == card && botoes.is_empty()
                && rico.contains("roda os testes"))),
        "o card vira registro, sem botões"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn texto_solto_com_card_aberto_e_segurado() {
    let c = cena().await;
    let (_t, mut rx) = c.app.hub.listen(SESSAO);
    c.trata(c.voz("900")).await;
    c.espera_card().await;

    c.trata(c.mensagem("901", "outra coisa", None)).await;
    assert!(apagou(&c.fe, "901"), "a mensagem solta ficou no canal");
    assert!(
        c.fe.textos()
            .iter()
            .any(|t| t.contains("esperando") && t.contains("outra coisa")),
        "o aviso tem de devolver o que você escreveu: {:?}",
        c.fe.textos()
    );
    // Responder a outra mensagem qualquer também não fura a guarda.
    c.trata(c.mensagem("902", "e isso", Some("555"))).await;
    assert!(apagou(&c.fe, "902"));
    nada_chegou(&mut rx);
}

#[tokio::test(flavor = "multi_thread")]
async fn responder_ao_card_manda_a_transcricao_com_a_correcao() {
    let c = cena().await;
    let (_t, mut rx) = c.app.hub.listen(SESSAO);
    c.trata(c.voz("900")).await;
    let (card, ..) = c.espera_card().await;

    c.trata(c.mensagem("903", "é o módulo transcritor", Some(card.as_str())))
        .await;
    let m = recebe(&mut rx).await;
    assert!(
        m.text.contains("[transcrição do áudio] roda os testes"),
        "{}",
        m.text
    );
    assert!(m.text.contains("é o módulo transcritor"), "{}", m.text);
    assert!(
        apagou(&c.fe, "903"),
        "a correção vive no card, não solta no canal"
    );
    assert!(
        c.fe.chamadas().iter().any(|ch| matches!(ch,
            Chamada::Edita { msg, rico, .. } if *msg == card && rico.contains("Ratificação"))),
        "o card registra os dois textos"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn descartar_some_sem_rastro() {
    let c = cena().await;
    let (_t, mut rx) = c.app.hub.listen(SESSAO);
    c.trata(c.voz("900")).await;
    let (card, _ok, no, _) = c.espera_card().await;
    c.trata(c.toque(&card, &no)).await;
    assert!(c.fe.chamadas().contains(&Chamada::Apaga(card)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    nada_chegou(&mut rx);
}

#[tokio::test(flavor = "multi_thread")]
async fn comando_passa_mesmo_com_card_aberto() {
    // `/kill` é a válvula de escape para card preso; sem ela o canal vira sala trancada.
    let c = cena().await;
    c.trata(c.voz("900")).await;
    let (card, ..) = c.espera_card().await;
    c.trata(c.mensagem("904", "/kill", None)).await;
    assert!(c.app.store.get(SESSAO).unwrap().unwrap().ended_at.is_some());
    assert!(
        c.fe.chamadas().contains(&Chamada::Apaga(card)),
        "card de sessão morta não pode sobrar"
    );
}

// ------------------------------------------------------------------ permissão

#[tokio::test(flavor = "multi_thread")]
async fn toque_no_card_de_permissao_decide() {
    let c = cena().await;
    c.app
        .store
        .set_permission_mode(SESSAO, "perguntar")
        .unwrap();
    let (ask, rx) = c
        .app
        .start_permission(SESSAO, "Bash", &serde_json::json!({"command": "ls"}))
        .await
        .unwrap();
    let card = espera("card de permissão", || {
        c.fe.chamadas().into_iter().find_map(|ch| match ch {
            Chamada::Envia { msg, botoes, .. }
                if botoes.iter().any(|b| b.dado == format!("p:{ask}:a")) =>
            {
                Some(msg)
            }
            _ => None,
        })
    })
    .await;
    c.trata(c.toque(&card, &format!("p:{ask}:a"))).await;
    let decisao = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decisao, "allow");
}

// ------------------------------------------------------------------ arquivos de volta

fn stop(texto: &str) -> StopReport {
    StopReport {
        at_ms: None,
        session_id: SESSAO.into(),
        transcript_path: None,
        last_assistant_message: Some(texto.into()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn resposta_com_marcador_sai_na_ordem_em_que_foi_escrita() {
    let c = cena().await;
    let png = c.raiz.path().join("g.png");
    std::fs::write(&png, b"png").unwrap();
    c.app.store.marca_pedido(SESSAO).unwrap();

    c.app
        .on_stop(&stop(&format!(
            "antes\n@arquivo: {} | o gráfico\ndepois",
            png.display()
        )))
        .await
        .unwrap();

    let saida: Vec<Chamada> =
        c.fe.chamadas()
            .into_iter()
            .filter(|ch| matches!(ch, Chamada::Texto { .. } | Chamada::Arquivo { .. }))
            .collect();
    assert_eq!(saida.len(), 3, "{saida:?}");
    assert!(matches!(&saida[0], Chamada::Texto { texto, .. } if texto == "antes"));
    assert!(matches!(&saida[1],
        Chamada::Arquivo { caminho, como: Midia::Foto, legenda, .. }
            if *caminho == png && legenda.as_deref() == Some("o gráfico")));
    assert!(matches!(&saida[2], Chamada::Texto { texto, .. } if texto == "depois"));
}

#[tokio::test(flavor = "multi_thread")]
async fn turno_que_ninguem_pediu_nao_vai_para_o_canal() {
    let c = cena().await;
    c.app.on_stop(&stop("Monitor rearmado")).await.unwrap();
    assert!(
        !c.fe
            .chamadas()
            .iter()
            .any(|ch| matches!(ch, Chamada::Texto { .. })),
        "{:?}",
        c.fe.chamadas()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dialogo_de_mcp_diz_como_anexar_pelo_hospedeiro_em_uso() {
    // O diálogo só se responde no teclado, então o aviso precisa do comando do hospedeiro que
    // está de fato rodando a sessão: com o herdr, um "tmux attach" mandaria você para o lugar
    // errado.
    let c = cena().await;
    c.app
        .on_event(&SessionEvent {
            at_ms: None,
            session_id: SESSAO.into(),
            event: EventKind::Elicitation {
                servidor: "github".into(),
                pedido: "autorizar?".into(),
            },
        })
        .unwrap();
    let aviso = espera("o aviso do diálogo", || {
        c.fe.textos().into_iter().find(|t| t.contains("github"))
    })
    .await;
    assert!(aviso.contains(&format!("falso-anexa {TMUX}")), "{aviso}");
    assert!(!aviso.contains("tmux attach"), "{aviso}");
}

fn textos(c: &Cena) -> Vec<String> {
    c.fe.chamadas()
        .into_iter()
        .filter_map(|ch| match ch {
            Chamada::Texto { texto, .. } => Some(texto),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn responder_um_card_faz_a_resposta_do_turno_ir_para_o_canal() {
    // O caso real: o turno foi acordado pelo CI que a sessão deixou rodando (sem pedido), ela
    // perguntou "faço o deploy?", você respondeu no card, e a resposta final sumiu.
    let c = cena().await;
    c.app
        .store
        .set_permission_mode(SESSAO, "perguntar")
        .unwrap();
    let (ask, rx) = c
        .app
        .start_permission(SESSAO, "Bash", &serde_json::json!({"command": "deploy"}))
        .await
        .unwrap();
    c.app.hub.answer(&ask, "allow".into());
    rx.await.unwrap();
    c.app.cleanup_ask(&ask, Some("✅ Permitido")).await;

    c.app.on_stop(&stop("O deploy não saiu")).await.unwrap();
    assert!(
        textos(&c).iter().any(|t| t == "O deploy não saiu"),
        "{:?}",
        c.fe.chamadas()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn card_que_expira_sem_resposta_nao_conta_como_pedido() {
    let c = cena().await;
    c.app
        .store
        .set_permission_mode(SESSAO, "perguntar")
        .unwrap();
    let (ask, _rx) = c
        .app
        .start_permission(SESSAO, "Bash", &serde_json::json!({"command": "ls"}))
        .await
        .unwrap();
    c.app.cleanup_ask(&ask, None).await;
    c.app.on_stop(&stop("Monitor rearmado")).await.unwrap();
    assert!(textos(&c).is_empty(), "{:?}", c.fe.chamadas());
}

/// Um transcript cujo último turno foi aberto por `gatilho`.
fn transcript_aberto_por(c: &Cena, gatilho: &str) -> String {
    let fala = "resposta";
    let caminho = c.raiz.path().join("turno.jsonl");
    let linha = |tipo: &str, conteudo: serde_json::Value| {
        serde_json::json!({"type": tipo, "message": {"role": tipo, "content": conteudo}})
            .to_string()
    };
    let linhas = [
        linha("user", serde_json::json!(gatilho)),
        linha(
            "assistant",
            serde_json::json!([{"type": "text", "text": fala}]),
        ),
    ];
    std::fs::write(&caminho, linhas.join("\n")).unwrap();
    caminho.to_string_lossy().into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn turno_aberto_por_tarefa_de_fundo_da_sessao_vai_para_o_canal() {
    let c = cena().await;
    let transcript = transcript_aberto_por(
        &c,
        "<task-notification>\n<task-id>b5tudmugw</task-id>\n<status>completed</status>\n<summary>Background command \"Wait for the first MarkChatRead after deploy\" completed (exit code 0)</summary>\n</task-notification>",
    );
    let mut r = stop("Pronto: a correção funcionou em produção.");
    r.transcript_path = Some(transcript);
    c.app.on_stop(&r).await.unwrap();
    assert!(
        textos(&c)
            .iter()
            .any(|t| t.starts_with("Pronto: a correção")),
        "{:?}",
        c.fe.chamadas()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn turno_aberto_pelo_canal_expirando_continua_fora_do_canal() {
    let c = cena().await;
    let transcript = transcript_aberto_por(
        &c,
        "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n<summary>Monitor \"mensagens do Telegram\" stream ended</summary>\n</task-notification>",
    );
    let mut r = stop("Monitor rearmado");
    r.transcript_path = Some(transcript);
    c.app.on_stop(&r).await.unwrap();
    assert!(textos(&c).is_empty(), "{:?}", c.fe.chamadas());
}

#[tokio::test(flavor = "multi_thread")]
async fn foto_recusada_cai_para_documento() {
    let c = cena().await;
    c.fe.recusa(Midia::Foto);
    let png = c.raiz.path().join("g.png");
    std::fs::write(&png, b"png").unwrap();
    let r = c
        .app
        .send_file(SESSAO, png.to_str().unwrap(), None, false)
        .await
        .unwrap();
    assert!(r.contains("documento"), "{r}");
    assert!(c.fe.chamadas().iter().any(|ch| matches!(ch,
        Chamada::Arquivo { como: Midia::Documento, caminho, .. } if *caminho == png)));
}

#[tokio::test(flavor = "multi_thread")]
async fn arquivo_acima_do_teto_do_frontend_vai_em_partes() {
    // O teto vem do frontend, não de constante: com 1000 bytes de limite, 5000 não cabem.
    let c = cena_com(Limites {
        enviar: 1000,
        ..Limites::default()
    })
    .await;
    let grande = c.raiz.path().join("grande.bin");
    std::fs::write(&grande, vec![7u8; 5000]).unwrap();

    let r = c
        .app
        .send_file(SESSAO, grande.to_str().unwrap(), Some("o dump"), true)
        .await
        .unwrap();
    assert!(
        r.contains("partes"),
        "a sessão tem de saber que vai em partes: {r}"
    );

    let fe = c.fe.clone();
    espera("instrução de juntar", move || {
        fe.textos()
            .iter()
            .any(|t| t.contains("junte tudo"))
            .then_some(())
    })
    .await;
    let arquivos: Vec<Chamada> =
        c.fe.chamadas()
            .into_iter()
            .filter(|ch| matches!(ch, Chamada::Arquivo { .. }))
            .collect();
    assert_eq!(arquivos.len(), 3, "{arquivos:?}");
    assert!(
        arquivos.iter().all(|ch| matches!(ch,
        Chamada::Arquivo { como: Midia::Documento, legenda: Some(l), .. } if l.contains("o dump")))
    );
    assert!(
        c.fe.textos()
            .iter()
            .any(|t| t.contains("estou partindo em pedaços")),
        "o anúncio do divisor tem de sair antes"
    );
}

// ------------------------------------------------------------------ o agente é uma porta

/// Um comando no canal da sessão, e o teclado que ele devolveu.
async fn botoes_de(c: &Cena, comando: &str) -> Vec<String> {
    c.fe.limpa_registro();
    c.trata(c.mensagem("60", comando, None)).await;
    c.fe.chamadas()
        .into_iter()
        .find_map(|ch| match ch {
            Chamada::Envia { botoes, .. } if !botoes.is_empty() => {
                Some(botoes.into_iter().map(|b| b.dado).collect())
            }
            _ => None,
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn o_teclado_de_esforco_e_o_do_agente_em_uso() {
    let c = cena().await;
    assert_eq!(botoes_de(&c, "/effort").await, ["ef:pouco", "ef:muito"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn o_teclado_de_modo_e_o_do_agente_em_uso() {
    let c = cena().await;
    assert_eq!(botoes_de(&c, "/mode").await, ["pm:livre"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn modo_recusado_pelo_agente_volta_com_o_motivo_dele() {
    let c = cena().await;
    c.trata(c.mensagem("61", "/mode plan", None)).await;
    assert!(
        c.fe.textos()
            .iter()
            .any(|t| t.contains("só conhece o modo livre")),
        "{:?}",
        c.fe.textos()
    );
    assert_eq!(
        *c.hospedeiro.lancadas.lock().unwrap(),
        0,
        "modo recusado não pode relançar"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_partida_sai_do_agente_e_o_prompt_descreve_o_chat_em_uso() {
    let c = cena().await;
    let projeto = Project {
        name: "outro".into(),
        path: "/tmp/outro".into(),
        permission_mode: None,
        model: None,
        effort: None,
    };
    let id = c
        .app
        .create_session(&projeto, None, None, None)
        .await
        .unwrap();
    assert_eq!(id, "id-falso-0001", "o id da sessão nova vem do agente");

    let (script, prompt) = c.hospedeiro.ultima_partida();
    assert!(
        script.contains("'agente-falso' '--id' 'id-falso-0001'"),
        "{script}"
    );
    assert!(
        prompt.contains(&c.fe.onde("outro")),
        "o prompt fala do chat em uso: {prompt}"
    );
    assert!(
        prompt.contains(&c.fe.limites().enviar.to_string()),
        "e do teto dele: {prompt}"
    );
    assert!(
        c.raiz
            .path()
            .join("sessoes")
            .join(&id)
            .join("launch.sh")
            .exists(),
        "o script mora na raiz de sessões"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn new_com_apelido_e_esforco_do_agente_abre_ja_configurada() {
    // "cerebro-grande" e "pouco" só significam algo para o agente de mentira: separar o nome do
    // projeto das flags é pergunta que o roteador faz a ele.
    let c = cena().await;
    c.trata(Evento::Mensagem {
        autor: luka(),
        canal: None,
        msg: MsgId::new("70"),
        texto: "/new outro cerebro-grande pouco".into(),
        responde_a: None,
        anexos: vec![],
    })
    .await;
    let (script, _) = c.hospedeiro.ultima_partida();
    assert!(script.contains("'--cerebro' 'cerebro-grande'"), "{script}");
    assert!(script.contains("'--forca' 'pouco'"), "{script}");
}

#[tokio::test(flavor = "multi_thread")]
async fn trocar_esforco_relanca_continuando_a_mesma_sessao() {
    let c = cena().await;
    c.trata(c.mensagem("62", "/effort muito", None)).await;
    let (script, _) = c.hospedeiro.ultima_partida();
    assert!(
        script.contains(&format!("'--continua' '{SESSAO}'")),
        "{script}"
    );
    assert!(script.contains("'--forca' 'muito'"), "{script}");
    // O nome do processo é o mesmo antes e depois (ele sai do id, que o relançamento mantém):
    // o que prova a troca é o velho morrer ANTES de o novo subir.
    assert_eq!(
        *c.hospedeiro.eventos.lock().unwrap(),
        [format!("mata:{TMUX}"), format!("lanca:{SESSAO}")],
        "o processo velho tem de morrer antes do novo subir"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn relancar_grava_a_hospedagem_nova_e_a_reconciliacao_nao_encerra() {
    let c = cena().await;
    c.trata(c.mensagem("63", "/effort muito", None)).await;
    let s = c.app.store.get(SESSAO).unwrap().unwrap();
    assert_eq!(s.hospedagem.as_deref(), Some("ld-proj-s1@1"));
    assert_eq!(c.app.reconcile().await.unwrap(), 0);
    assert!(c.app.store.get(SESSAO).unwrap().unwrap().ended_at.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn o_claude_code_sobe_dentro_do_ai_memory() {
    // A composição de verdade, com o agente e a memória reais: é a linha que roda no tmux.
    let c = cena().await;
    let raiz = tempfile::tempdir().unwrap();
    let locais = Locais {
        cli: "/opt/ld/lukadispatch".into(),
        mcp_proxy: "/opt/ld/lukadispatch-mcp".into(),
        settings: raiz.path().join("bot-settings.json"),
        claude_json: raiz.path().join("claude.json"),
        claude_dir: raiz.path().join("claude"),
        uso_db: raiz.path().join("uso.db"),
    };
    let hospedeiro = Arc::new(HospedeiroFalso::default());
    let app = App::new(
        Config {
            trust_projects: false,
            wrap_mcp: false,
            ..Config::default()
        },
        Store::open_memory().unwrap(),
        Portas {
            frontend: c.fe.clone(),
            agente: Arc::new(ClaudeCode::new(locais, None)),
            memoria: Arc::new(AiMemory),
            transcritor: None,
            divisores: Divisores::new(vec![]),
            hospedeiro: hospedeiro.clone(),
        },
    )
    .com_raiz_sessoes(raiz.path().join("sessoes"));
    let projeto = Project {
        name: "outro".into(),
        path: "/tmp/outro".into(),
        permission_mode: None,
        model: Some("opus".into()),
        effort: None,
    };
    let id = app
        .create_session(&projeto, Some("opus"), None, None)
        .await
        .unwrap();

    let (script, prompt) = hospedeiro.ultima_partida();
    assert!(
        script.contains("exec 'ai-memory' 'run' '--new'"),
        "{script}"
    );
    assert!(
        script.contains(&format!("'claude' '--session-id' '{id}'")),
        "{script}"
    );
    assert!(script.contains("'--model' 'opus'"), "{script}");
    assert!(prompt.contains("select:Monitor"), "{prompt}");
    assert!(prompt.contains(&c.fe.onde("outro")), "{prompt}");
}

// ------------------------------------------------------------------ memória de longo prazo

/// Uma memória que registra o que o domínio pediu a ela.
#[derive(Default)]
struct MemoriaFalsa {
    /// Para cada embrulho: a worktree que veio (a branch) e se a partida era isolada.
    embrulhos: Mutex<Vec<(Option<String>, bool)>>,
    /// `tira` e `devolve`, na ordem em que aconteceram.
    eventos: Mutex<Vec<String>>,
}

#[async_trait]
impl ld_daemon::agente::Memoria for MemoriaFalsa {
    fn nome(&self) -> &'static str {
        "falsa"
    }
    async fn embrulha(&self, p: &PartidaDaMemoria<'_>, argv: Vec<String>) -> Result<Vec<String>> {
        self.embrulhos
            .lock()
            .unwrap()
            .push((p.worktree.map(|w| w.branch.clone()), p.isolada));
        Ok(argv)
    }
    fn instrucoes(&self, p: &PartidaDaMemoria<'_>) -> Option<String> {
        p.worktree
            .map(|w| format!("INSTRUÇÃO DA BRANCH {}", w.branch))
    }
    async fn antes_da_partida(&self, _: &PartidaDaMemoria<'_>) -> Result<Option<Guardado>> {
        self.eventos.lock().unwrap().push("tira".into());
        Ok(Some(Guardado(Box::new(()))))
    }
    async fn devolve(&self, _: Guardado) -> Result<()> {
        self.eventos.lock().unwrap().push("devolve".into());
        Ok(())
    }
    fn ocupada(&self, saida: &str) -> bool {
        saida.contains("workstream is already active")
    }
    fn segura_a_partida(&self, p: &PartidaDaMemoria<'_>) -> Duration {
        if p.worktree.is_some() {
            Duration::from_secs(7)
        } else {
            Duration::ZERO
        }
    }
}

/// Uma cena com a memória falsa e um projeto numa worktree da branch `feat`.
async fn cena_de_worktree() -> (Cena, Arc<MemoriaFalsa>, Project) {
    let memoria = Arc::new(MemoriaFalsa::default());
    let c = cena_montada(Limites::default(), memoria.clone(), |_, _| {}).await;
    let caminho = c.raiz.path().join("wt/outro/feat");
    std::fs::create_dir_all(&caminho).unwrap();
    let caminho = caminho.to_string_lossy().into_owned();
    c.app
        .store
        .registra_worktree(&Worktree {
            caminho: caminho.clone(),
            projeto: "outro".into(),
            raiz: "/tmp/outro".into(),
            branch: "feat".into(),
            criada_em: 0,
            usada_em: 0,
        })
        .unwrap();
    let projeto = Project {
        name: "outro".into(),
        path: caminho,
        permission_mode: None,
        model: None,
        effort: None,
    };
    (c, memoria, projeto)
}

#[tokio::test(flavor = "multi_thread")]
async fn sessao_em_worktree_tem_canal_com_a_branch_e_a_memoria_sabe_dela() {
    let (c, memoria, projeto) = cena_de_worktree().await;
    c.app
        .create_session(&projeto, None, None, None)
        .await
        .unwrap();

    assert!(
        c.fe.chamadas().iter().any(|ch| matches!(ch,
            Chamada::CriaCanal { nome, .. } if nome == "outro · feat")),
        "duas sessões do mesmo projeto não podem ter canais de mesmo nome"
    );
    assert_eq!(
        *memoria.embrulhos.lock().unwrap(),
        [(Some("feat".to_string()), false)]
    );
    let (_, prompt) = c.hospedeiro.ultima_partida();
    assert!(prompt.contains("INSTRUÇÃO DA BRANCH feat"), "{prompt}");
    assert_eq!(
        c.hospedeiro
            .partidas
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .espera_ao_subir,
        Duration::from_secs(7),
        "o hospedeiro espera o que a memória pode segurar a partida"
    );
}

#[tokio::test(start_paused = true)]
async fn memoria_presa_espera_e_tenta_de_novo() {
    let (c, memoria, projeto) = cena_de_worktree().await;
    *c.hospedeiro.presa.lock().unwrap() = 2;
    c.app
        .create_session(&projeto, None, None, None)
        .await
        .unwrap();
    assert_eq!(
        *memoria.embrulhos.lock().unwrap(),
        [
            (Some("feat".to_string()), false),
            (Some("feat".to_string()), false),
            (Some("feat".to_string()), false),
        ],
        "dentro do prazo, a mesma memória: é ela que tem o registro da branch"
    );
    assert_eq!(*c.hospedeiro.lancadas.lock().unwrap(), 1);
}

#[tokio::test(start_paused = true)]
async fn memoria_que_nao_solta_no_prazo_vira_memoria_isolada() {
    let (c, memoria, projeto) = cena_de_worktree().await;
    // Com 100 s de prazo e uma volta a cada 10 s, são onze tentativas na mesma memória: a
    // décima segunda, já fora do prazo, é a isolada, e é essa que sobe.
    *c.hospedeiro.presa.lock().unwrap() = 11;
    c.app
        .create_session(&projeto, None, None, None)
        .await
        .unwrap();
    let embrulhos = memoria.embrulhos.lock().unwrap().clone();
    assert_eq!(embrulhos.len(), 12, "{embrulhos:?}");
    assert_eq!(embrulhos.last(), Some(&(Some("feat".to_string()), true)));
    assert!(embrulhos[..11].iter().all(|(_, isolada)| !isolada));
}

#[tokio::test(start_paused = true)]
async fn o_que_a_memoria_tirou_so_volta_depois_do_inicio_da_sessao() {
    let (c, memoria, projeto) = cena_de_worktree().await;
    let id = c
        .app
        .create_session(&projeto, None, None, None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(
        *memoria.eventos.lock().unwrap(),
        ["tira"],
        "sem o início da sessão, o que foi tirado continua fora"
    );

    c.app
        .on_register(&RegisterSession {
            session_id: id,
            cwd: projeto.path.clone(),
            transcript_path: "/tmp/t.jsonl".into(),
            reason: "startup".into(),
            model: None,
        })
        .unwrap();
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(*memoria.eventos.lock().unwrap(), ["tira", "devolve"]);
}

#[tokio::test(start_paused = true)]
async fn partida_que_falha_devolve_na_hora() {
    let (c, memoria, projeto) = cena_de_worktree().await;
    *c.hospedeiro.recusa.lock().unwrap() = true;
    assert!(
        c.app
            .create_session(&projeto, None, None, None)
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(*memoria.eventos.lock().unwrap(), ["tira", "devolve"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn relancar_pede_ao_processo_que_saia_antes_de_derrubar_o_painel() {
    let c = cena().await;
    let mut processo = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .unwrap();
    *c.hospedeiro.pid.lock().unwrap() = Some(processo.id());
    // Sem recolher o filho, o pid dele fica como zumbi e parece vivo para sempre.
    let recolhe = tokio::task::spawn_blocking(move || processo.wait().unwrap());

    c.app.relaunch(SESSAO, Some("opus"), None).await.unwrap();

    let status = tokio::time::timeout(Duration::from_secs(5), recolhe)
        .await
        .expect("o processo não recebeu o pedido de saída")
        .unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(15),
        "saiu por SIGTERM, e não derrubado"
    );
    assert!(
        c.hospedeiro
            .eventos
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.starts_with("mata:")),
        "o hospedeiro ainda derruba o painel depois"
    );
}

// ------------------------------------------------------------------ /new com worktree

/// Uma cena com um repositório git de verdade, `repo`, fixado no config, e uma raiz de projetos
/// vazia (`projetos/`) para o `/new new-project`.
async fn cena_git() -> (Cena, PathBuf) {
    // O commit inicial do projeto novo precisa de identidade, e a máquina de CI não tem uma.
    // SAFETY: todo teste que lê estas variáveis quer o mesmo valor.
    unsafe {
        for (k, v) in [
            ("GIT_AUTHOR_NAME", "Teste"),
            ("GIT_AUTHOR_EMAIL", "teste@exemplo"),
            ("GIT_COMMITTER_NAME", "Teste"),
            ("GIT_COMMITTER_EMAIL", "teste@exemplo"),
        ] {
            std::env::set_var(k, v);
        }
    }
    let mut repo = PathBuf::new();
    let c = cena_montada(Limites::default(), Arc::new(SemMemoria), |cfg, raiz| {
        repo = raiz.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            &["init", "--quiet", "--initial-branch=master"][..],
            &["config", "commit.gpgsign", "false"],
            &["commit", "--quiet", "--allow-empty", "-m", "um"],
        ] {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?}");
        }
        std::fs::create_dir_all(raiz.join("projetos")).unwrap();
        cfg.projects = vec![Project {
            name: "repo".into(),
            path: repo.to_string_lossy().into_owned(),
            permission_mode: None,
            model: None,
            effort: None,
        }];
        cfg.scan.roots = vec![raiz.join("projetos").to_string_lossy().into_owned()];
    })
    .await;
    (c, repo)
}

impl Cena {
    async fn no_principal(&self, texto: &str) {
        self.trata(Evento::Mensagem {
            autor: luka(),
            canal: None,
            msg: MsgId::new(format!("p-{}", self.fe.chamadas().len())),
            texto: texto.into(),
            responde_a: None,
            anexos: vec![],
        })
        .await;
    }

    /// Toca o botão cujo rótulo contém `rotulo`, na última mensagem que o trouxe.
    async fn toca(&self, rotulo: &str) {
        let (msg, dado) = self
            .fe
            .chamadas()
            .into_iter()
            .rev()
            .find_map(|c| match c {
                Chamada::Envia { botoes, msg, .. } => botoes
                    .into_iter()
                    .find(|b| b.rotulo.contains(rotulo))
                    .map(|b| (msg, b.dado)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("nenhum botão com {rotulo:?}: {:?}", self.fe.textos()));
        self.trata(Evento::Toque {
            autor: luka(),
            canal: None,
            msg: Some(msg),
            dado,
        })
        .await;
    }

    fn falou(&self, trecho: &str) -> bool {
        self.fe.textos().iter().any(|t| t.contains(trecho))
    }

    /// A sessão viva que roda em `cwd`.
    fn sessao_em(&self, cwd: &Path) -> Option<Session> {
        self.app
            .store
            .live()
            .unwrap()
            .into_iter()
            .find(|s| Path::new(&s.cwd) == cwd)
    }
}

fn existe_branch(repo: &Path, nome: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{nome}"),
        ])
        .status()
        .unwrap()
        .success()
}

#[tokio::test(flavor = "multi_thread")]
async fn new_com_branch_que_nao_existe_cria_a_worktree_e_abre_nela() {
    let (c, repo) = cena_git().await;
    c.no_principal("/new repo feat").await;

    let w = c
        .app
        .store
        .worktree_da_branch(&repo.to_string_lossy(), "feat")
        .unwrap()
        .expect("a worktree não foi registrada");
    assert!(Path::new(&w.caminho).join(".git").is_file());
    assert!(existe_branch(&repo, "feat"));
    assert!(
        c.sessao_em(Path::new(&w.caminho)).is_some(),
        "{:?}",
        c.fe.textos()
    );
    assert!(c.fe.chamadas().iter().any(|ch| matches!(ch,
        Chamada::CriaCanal { nome, .. } if nome == "repo · feat")));

    // A mesma branch de novo não abre outra sessão: aponta a que existe.
    c.no_principal("/new repo feat").await;
    assert_eq!(*c.hospedeiro.lancadas.lock().unwrap(), 1);
    assert!(c.falou("Já há uma sessão aberta"), "{:?}", c.fe.textos());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_principal_nunca_abre_direto_e_o_nome_digitado_vira_a_branch() {
    let (c, repo) = cena_git().await;
    c.no_principal("/new repo master").await;
    assert!(c.falou("nome da branch nova"), "{:?}", c.fe.textos());
    assert_eq!(*c.hospedeiro.lancadas.lock().unwrap(), 0);

    c.no_principal("com espaço").await;
    assert!(c.falou("não serve para nome de branch"));
    c.no_principal("fix/x").await;

    let w = c
        .app
        .store
        .worktree_da_branch(&repo.to_string_lossy(), "fix/x")
        .unwrap()
        .expect("a branch nova não virou worktree");
    assert!(c.sessao_em(Path::new(&w.caminho)).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn o_seletor_leva_da_pasta_a_branch() {
    let (c, repo) = cena_git().await;
    c.no_principal("/new").await;
    // Só a pasta dos fixados tem projeto: ela é pulada, e o seletor já mostra os projetos.
    c.toca("repo").await;
    c.toca("Nova branch a partir de master").await;
    c.toca("Gerar um nome").await;
    let w = c.app.store.worktrees_de(&repo.to_string_lossy()).unwrap();
    assert_eq!(w.len(), 1);
    assert!(w[0].branch.starts_with("ld/"), "{}", w[0].branch);
    assert!(c.sessao_em(Path::new(&w[0].caminho)).is_some());
}

/// Abre `feat` e devolve a worktree e o canal da sessão.
async fn abre_feat(c: &Cena, repo: &Path) -> (Worktree, Canal) {
    c.no_principal("/new repo feat").await;
    let w = c
        .app
        .store
        .worktree_da_branch(&repo.to_string_lossy(), "feat")
        .unwrap()
        .unwrap();
    let s = c.sessao_em(Path::new(&w.caminho)).unwrap();
    (w, Canal::new(s.canal_id.unwrap()))
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_em_worktree_limpa_pergunta_e_apaga_tudo() {
    let (c, repo) = cena_git().await;
    let (w, canal) = abre_feat(&c, &repo).await;
    c.trata(Evento::Mensagem {
        autor: luka(),
        canal: Some(canal),
        msg: MsgId::new("k1"),
        texto: "/kill".into(),
        responde_a: None,
        anexos: vec![],
    })
    .await;
    assert!(
        c.sessao_em(Path::new(&w.caminho)).is_some(),
        "pergunta antes de fechar"
    );

    c.toca("apagar worktree e branch").await;
    assert!(c.sessao_em(Path::new(&w.caminho)).is_none());
    assert!(!Path::new(&w.caminho).exists());
    assert!(!existe_branch(&repo, "feat"));
    assert!(c.app.store.worktree_em(&w.caminho).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn trabalho_nao_salvo_pede_confirmacao_antes_de_apagar() {
    let (c, repo) = cena_git().await;
    let (w, canal) = abre_feat(&c, &repo).await;
    std::fs::write(Path::new(&w.caminho).join("rascunho.txt"), "x").unwrap();
    c.trata(Evento::Mensagem {
        autor: luka(),
        canal: Some(canal),
        msg: MsgId::new("k1"),
        texto: "/kill".into(),
        responde_a: None,
        anexos: vec![],
    })
    .await;
    c.toca("apagar worktree e branch").await;
    assert!(
        c.falou("1 arquivo(s) mudado(s) sem commit"),
        "{:?}",
        c.fe.textos()
    );
    assert!(Path::new(&w.caminho).exists(), "ainda não apagou");

    c.toca("Apagar mesmo assim").await;
    assert!(!Path::new(&w.caminho).exists());
    assert!(!existe_branch(&repo, "feat"));
}

#[tokio::test(flavor = "multi_thread")]
async fn manter_fecha_a_sessao_e_a_worktree_volta_no_new() {
    let (c, repo) = cena_git().await;
    let (w, canal) = abre_feat(&c, &repo).await;
    c.trata(Evento::Mensagem {
        autor: luka(),
        canal: Some(canal),
        msg: MsgId::new("k1"),
        texto: "/kill".into(),
        responde_a: None,
        anexos: vec![],
    })
    .await;
    c.toca("manter a worktree").await;
    assert!(c.sessao_em(Path::new(&w.caminho)).is_none());
    assert!(Path::new(&w.caminho).exists());

    // Reabrir cai na mesma pasta: é por ela que o agente acha a conversa anterior.
    c.no_principal("/new repo feat").await;
    assert!(c.sessao_em(Path::new(&w.caminho)).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn new_project_cria_o_repositorio_e_abre_a_sessao_nele() {
    let (c, _) = cena_git().await;
    c.no_principal("/new new-project").await;
    c.toca("projetos").await;
    c.no_principal("novo-app").await;

    let pasta = c.raiz.path().join("projetos/novo-app");
    assert!(pasta.join(".git").is_dir(), "{:?}", c.fe.textos());
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(&pasta)
            .args(["rev-parse", "--verify", "HEAD"])
            .output()
            .unwrap()
            .status
            .success(),
        "nasce com commit, senão não sai worktree dele"
    );
    assert!(c.sessao_em(&pasta).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn projeto_de_mesmo_nome_em_duas_pastas_pergunta_qual_ou_segue_a_pasta_dita() {
    // Duas pastas de projetos, cada uma com um repositório `api`.
    let c = cena_montada(Limites::default(), Arc::new(SemMemoria), |cfg, raiz| {
        for pasta in ["Personal", "Trabalho"] {
            let repo = raiz.join(pasta).join("api");
            std::fs::create_dir_all(&repo).unwrap();
            for args in [
                &["init", "--quiet", "--initial-branch=master"][..],
                &["config", "commit.gpgsign", "false"],
                &["commit", "--quiet", "--allow-empty", "-m", "um"],
            ] {
                assert!(
                    std::process::Command::new("git")
                        .arg("-C")
                        .arg(&repo)
                        .args(args)
                        .status()
                        .unwrap()
                        .success()
                );
            }
        }
        cfg.projects = Vec::new();
        cfg.scan.enabled = true;
        cfg.scan.roots = vec![
            raiz.join("Personal").to_string_lossy().into_owned(),
            raiz.join("Trabalho").to_string_lossy().into_owned(),
        ];
    })
    .await;

    c.no_principal("/new api feat").await;
    assert!(c.falou("Há mais de um"), "{:?}", c.fe.textos());
    assert_eq!(*c.hospedeiro.lancadas.lock().unwrap(), 0);
    c.toca("api · Trabalho").await;
    let trabalho = c.raiz.path().join("Trabalho/api");
    let w = c
        .app
        .store
        .worktree_da_branch(&trabalho.to_string_lossy(), "feat")
        .unwrap()
        .expect("a escolha não levou a branch junto");
    assert!(c.sessao_em(Path::new(&w.caminho)).is_some());

    // Com a pasta na frente, não pergunta.
    c.no_principal("/new personal api outra").await;
    let personal = c.raiz.path().join("Personal/api");
    assert!(
        c.app
            .store
            .worktree_da_branch(&personal.to_string_lossy(), "outra")
            .unwrap()
            .is_some(),
        "{:?}",
        c.fe.textos()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn o_cli_abre_na_branch_sem_perguntar_e_recusa_a_principal() {
    let (c, repo) = cena_git().await;
    let p = Project {
        name: "repo".into(),
        path: repo.to_string_lossy().into_owned(),
        permission_mode: None,
        model: None,
        effort: None,
    };
    let e = ld_daemon::novo::abre_na_branch(&c.app, p.clone(), "master", false)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("principal"), "{e:#}");

    ld_daemon::novo::abre_na_branch(&c.app, p.clone(), "cli", false)
        .await
        .unwrap();
    let w = c
        .app
        .store
        .worktree_da_branch(&p.path, "cli")
        .unwrap()
        .unwrap();
    assert!(c.sessao_em(Path::new(&w.caminho)).is_some());
    assert!(
        ld_daemon::novo::abre_na_branch(&c.app, p, "cli", false)
            .await
            .is_err(),
        "a branch com sessão aberta não ganha outra"
    );
}

#[tokio::test(start_paused = true)]
async fn reconciliar_durante_uma_partida_nao_mata_o_que_parece_orfao() {
    let (c, _memoria, projeto) = cena_de_worktree().await;
    // Uma sessão encerrada cujo rótulo ainda está vivo no hospedeiro: é o que a partida de uma
    // sessão sendo retomada parece, enquanto espera a memória ser solta.
    c.app
        .store
        .upsert(&Session {
            session_id: "orfa".into(),
            project: "outro".into(),
            cwd: "/tmp/outro".into(),
            transcript_path: None,
            hospedagem: Some("ld-orfa".into()),
            canal_id: None,
            status: "ocioso".into(),
            status_msg_id: None,
            model: None,
            effort: None,
            permission_mode: None,
            created_at: 0,
            ended_at: None,
        })
        .unwrap();
    c.app.store.end("orfa").unwrap();
    c.hospedeiro.vivas.lock().unwrap().insert("ld-orfa".into());
    *c.hospedeiro.presa.lock().unwrap() = 2;

    let app = c.app.clone();
    let abrindo = tokio::spawn(async move { app.create_session(&projeto, None, None, None).await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    c.app.reconcile().await.unwrap();
    assert!(
        !c.hospedeiro
            .eventos
            .lock()
            .unwrap()
            .contains(&"mata:ld-orfa".to_string()),
        "a varredura de órfãs rodou no meio de uma partida"
    );

    abrindo.await.unwrap().unwrap();
    c.app.reconcile().await.unwrap();
    assert!(
        c.hospedeiro
            .eventos
            .lock()
            .unwrap()
            .contains(&"mata:ld-orfa".to_string()),
        "sem partida em curso, a órfã vai embora"
    );
}

fn ferramenta(inicio: bool, at_ms: Option<u64>) -> SessionEvent {
    SessionEvent {
        session_id: SESSAO.into(),
        event: if inicio {
            EventKind::ToolStart {
                tool: "Monitor".into(),
                label: "Monitor".into(),
                effort: None,
            }
        } else {
            EventKind::ToolEnd {
                tool: "Monitor".into(),
                ok: true,
            }
        },
        at_ms,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn evento_de_ferramenta_atrasado_nao_prende_a_sessao_trabalhando() {
    let c = cena().await;
    let status = || c.app.store.get(SESSAO).unwrap().unwrap().status;
    c.app.on_event(&ferramenta(true, Some(100))).unwrap();
    assert_eq!(status(), "ferramenta");
    c.app
        .on_stop(&StopReport {
            at_ms: Some(300),
            ..stop("pronto")
        })
        .await
        .unwrap();
    assert_eq!(status(), "ocioso");

    // Os hooks são assíncronos: o fim e o começo de ferramenta do turno que acabou chegam
    // depois do Stop. O `/model` recusava a sessão parada achando que ela trabalhava.
    c.app.on_event(&ferramenta(false, Some(200))).unwrap();
    c.app.on_event(&ferramenta(true, Some(250))).unwrap();
    c.app.on_event(&ferramenta(false, None)).unwrap();
    assert_eq!(status(), "ocioso");
    c.app.relaunch(SESSAO, Some("opus"), None).await.unwrap();

    // Um turno novo, depois do Stop, volta a contar.
    c.app.on_event(&ferramenta(true, Some(u64::MAX))).unwrap();
    assert_eq!(status(), "ferramenta");
}
