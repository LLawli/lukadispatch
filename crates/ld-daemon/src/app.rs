//! O miolo: tudo que o Telegram e o socket mandam fazer passa por aqui.
//!
//! Regra de convivência entre os dois lados: o Telegram nunca fala com o tmux direto e o hook
//! nunca fala com o Telegram direto. Os dois chamam método deste tipo, que é quem conhece o
//! estado. Assim existe um só lugar onde "sessão morreu" quer dizer as quatro coisas que ela
//! precisa querer dizer (matar o tmux, apagar o tópico, fechar as perguntas, marcar no banco).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use ld_core::ask::{Answer, Ask};
use ld_core::config::{Config, Project};
use ld_core::context;
use ld_core::paths;
use ld_core::proto::{
    EventKind, RegisterSession, Response, SessionEvent, SessionSummary, StopReport,
};
use ld_core::state::{Session, Store};
use std::path::Path;
use tracing::{info, warn};

use crate::cards::{Acao, Card, Cards, Efeito};
use crate::hub::{Hub, Incoming};
use crate::panel::Panel;
use crate::sessions;
use crate::status::{Ctx, StatusBoard};
use crate::telegram::{Tg, escape_html};

/// Depois de tantas cobranças seguidas de re-arme sem sucesso, o daemon para de insistir e
/// avisa. Insistir para sempre prenderia a sessão num laço de acordar-e-não-resolver.
const TETO_REARME: u32 = 3;

pub struct App {
    pub cfg: Config,
    /// Sem Telegram: nenhum tópico é criado e nada é enviado. Serve para desenvolver e testar o
    /// canal de entrada numa máquina sem bot; a sessão continua real, no tmux, com os hooks.
    pub offline: bool,
    pub store: Arc<Store>,
    pub tg: Tg,
    pub hub: Hub,
    pub status: StatusBoard,
    pub cards: Cards,
    pub panel: Panel,
    /// Último aviso de ociosidade ("Claude is waiting for your input") por sessão.
    ///
    /// Ele é útil quando chega, e vira lixo assim que você responde: some na próxima mensagem
    /// entregue à sessão, para o tópico não acumular uma fila deles.
    avisos: Arc<Mutex<HashMap<String, teloxide::types::MessageId>>>,
    /// Catálogo de modelos lido do binário do Claude Code, com a data dele.
    ///
    /// A leitura varre 200 MB e leva uns 300 ms: rápida para fazer uma vez, cara para repetir a
    /// cada toque de botão. A data de modificação do binário é a chave: atualizar o Claude Code
    /// derruba o cache sozinho, e modelos novos aparecem sem reiniciar o daemon.
    catalogo: Mutex<Option<(std::time::SystemTime, Vec<ld_core::models::Modelo>)>>,
    /// Sessões que estão trocando de modelo agora, com a hora em que a troca começou.
    ///
    /// Relançar exige matar o processo, e matar dispara o hook `SessionEnd`. Sem esta marca o
    /// daemon trataria a troca como fim de sessão: apagaria o tópico e encerraria tudo no meio
    /// do caminho. A janela é por tempo, e não por evento, porque o hook é `async` e pode chegar
    /// depois de a sessão nova já estar de pé.
    relancando: Mutex<HashMap<String, std::time::Instant>>,
    /// Última mensagem entregue a cada sessão pelo Telegram, com a hora.
    ///
    /// O hook `UserPromptSubmit` não distingue o que você digitou no PC do que chegou pelo
    /// celular, e republicar o segundo no tópico seria eco. A comparação é por texto e por
    /// tempo: só o que acabou de sair daqui é descartado.
    entregues: Mutex<HashMap<String, (String, std::time::Instant)>>,
}

impl App {
    /// Precisa rodar dentro de um runtime tokio: o painel sobe a tarefa dele aqui.
    pub fn new(cfg: Config, store: Store, tg: Tg) -> Self {
        let store = Arc::new(store);
        let panel = Panel::start(tg.clone(), store.clone());
        Self {
            cfg,
            offline: std::env::var_os("LUKADISPATCH_OFFLINE").is_some(),
            store,
            tg,
            hub: Hub::new(),
            status: StatusBoard::new(),
            cards: Cards::new(),
            panel,
            avisos: Arc::new(Mutex::new(HashMap::new())),
            catalogo: Mutex::new(None),
            relancando: Mutex::new(HashMap::new()),
            entregues: Mutex::new(HashMap::new()),
        }
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            tg: self.tg.clone(),
            store: self.store.clone(),
        }
    }

    // ---------------------------------------------------------------- ciclo de vida

    /// Cria o tópico, sobe a sessão e devolve o id dela.
    ///
    /// Ordem importa: o tópico vem antes do tmux para a sessão já nascer com para onde falar. Se
    /// o tmux falhar, o tópico recém-criado é apagado, senão sobra tópico órfão a cada tentativa.
    pub async fn create_session(
        &self,
        projeto: &Project,
        model: Option<&str>,
        effort: Option<&str>,
        retomar: Option<&str>,
    ) -> Result<String> {
        let topic = match self.offline {
            true => None,
            false => Some(self.tg.create_topic(&projeto.name).await?),
        };

        // Antes de subir: a pasta precisa estar confiada, senão o Claude Code para num diálogo
        // que só dá para responder no teclado do PC, e do celular a sessão parece muda.
        if self.cfg.trust_projects
            && let Ok(true) =
                ld_core::trust::ensure_trusted(&paths::claude_json(), Path::new(&projeto.path))
        {
            info!(projeto = %projeto.path, "pasta marcada como confiada");
        }

        let modo = self.cfg.permission_mode_for(&projeto.path);
        let spec = sessions::Spec {
            projeto,
            permission_mode: &modo,
            model,
            effort,
            resume: retomar,
            retomada: true,
        };
        let lancada = match sessions::launch(&spec).await {
            Ok(l) => l,
            Err(e) => {
                if let Some(t) = topic {
                    let _ = self.tg.delete_topic(t).await;
                }
                return Err(e);
            }
        };

        self.store.upsert(&Session {
            session_id: lancada.session_id.clone(),
            project: projeto.name.clone(),
            cwd: projeto.path.clone(),
            transcript_path: None,
            tmux: Some(lancada.tmux.clone()),
            topic_id: topic,
            status: "iniciando".into(),
            status_message_id: None,
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
            created_at: 0,
            ended_at: None,
        })?;

        if let Some(topic) = topic {
            let _ = self
            .tg
            .send_html(
                Some(topic),
                &format!(
                    "🟢 <b>{}</b>\n<code>{}</code>\n{}\n\nPode falar. Para fechar, mande /kill.",
                    escape_html(&projeto.name),
                    escape_html(&projeto.path),
                    escape_html(&ficha(model, effort, &modo, &lancada.tmux)),
                ),
            )
            .await;
        }

        self.panel.refresh();
        // Retomando: o tópico nasce com o que já foi conversado, senão você continua às cegas.
        if let (Some(topic), Some(_)) = (topic, retomar) {
            self.publica_historico(topic, &lancada.session_id).await;
        }

        info!(sessao = %lancada.session_id, topico = ?topic, projeto = %projeto.name, retomada = retomar.is_some(), "sessão criada");
        Ok(lancada.session_id)
    }

    /// Encerra a sessão: mata o tmux, fecha as perguntas abertas, apaga o tópico e marca no
    /// banco. Idempotente de propósito, porque dois caminhos chegam aqui (o /kill do Telegram e
    /// o hook SessionEnd de quando você fecha o Claude no PC).
    pub async fn end_session(&self, session_id: &str, apagar_topico: bool) -> Result<()> {
        let Some(s) = self.store.get(session_id)? else {
            return Ok(());
        };
        if s.ended_at.is_some() {
            return Ok(());
        }
        // Troca de modelo em andamento: este fim é do processo velho, não da sessão.
        if self.em_relancamento(session_id) {
            info!(sessao = %session_id, "fim ignorado: a sessão está sendo relançada");
            return Ok(());
        }

        for ask in self.cards.da_sessao(session_id) {
            self.cleanup_ask(&ask).await;
        }
        for ask in self.hub.asks_of(session_id) {
            self.hub.close_ask(&ask);
        }
        self.hub.unlisten(session_id);
        self.status.forget(session_id);

        if let Some(tmux) = &s.tmux
            && sessions::has_session(tmux).await
            && let Err(e) = sessions::kill(tmux).await
        {
            warn!(sessao = %session_id, erro = %e, "não consegui matar o tmux");
        }

        if apagar_topico && let Some(topic) = s.topic_id {
            match self.tg.delete_topic(topic).await {
                // Só esquece o tópico depois de apagá-lo: enquanto ele estiver no banco, a
                // varredura de tópico vazado sabe que ainda há o que limpar.
                Ok(()) => self.store.clear_topic(session_id)?,
                Err(e) => warn!(topico = topic, erro = %e, "não consegui apagar o tópico"),
            }
        }

        self.store.end(session_id)?;
        self.panel.refresh();
        info!(sessao = %session_id, "sessão encerrada");
        Ok(())
    }

    /// Uma troca de modelo em andamento silencia o fim da sessão antiga por esta janela.
    const JANELA_RELANCAMENTO: std::time::Duration = std::time::Duration::from_secs(90);

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

    /// Catálogo de modelos, relido só quando o binário do Claude Code muda.
    pub fn modelos(&self) -> Vec<ld_core::models::Modelo> {
        let preferido = self.cfg.claude_binary.as_deref().map(std::path::Path::new);
        // A data do binário é a chave do cache, então atualizar o Claude Code derruba o cache
        // sozinho. Sem binário conhecido ainda, tenta de novo a cada chamada.
        let data_atual = preferido
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());

        {
            let cache = self.catalogo.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((quando, modelos)) = cache.as_ref()
                && !modelos.is_empty()
                && data_atual.is_none_or(|d| d == *quando)
            {
                return modelos.clone();
            }
        }

        let (achado, modelos) = ld_core::models::catalog_auto(preferido);
        match &achado {
            Some(bin) => {
                info!(quantos = modelos.len(), binario = %bin.display(), "catálogo de modelos lido")
            }
            None => warn!(
                "não achei um binário do Claude Code com modelos dentro;                  aponte `claude_binary` no config.toml"
            ),
        }
        let data = achado
            .and_then(|b| std::fs::metadata(b).ok())
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        *self.catalogo.lock().unwrap_or_else(|e| e.into_inner()) = Some((data, modelos.clone()));
        modelos
    }

    /// Troca modelo ou esforço de uma sessão viva, sem perder a conversa.
    ///
    /// `/model` e `/effort` são comandos do frontend do Claude Code: nenhum evento consegue
    /// dispará-los, e digitar no terminal está fora de questão neste projeto. O que dá para
    /// fazer sem trapaça é reiniciar o processo com `--resume <id>`, que volta com o mesmo
    /// transcript e o mesmo id, só que com a flag nova. A conversa continua; o que se perde é o
    /// Monitor, e o prompt de re-arme cuida disso.
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

        let projeto = Project {
            name: s.project.clone(),
            path: s.cwd.clone(),
            permission_mode: None,
            model: None,
            effort: None,
        };
        let modo = self.cfg.permission_mode_for(&s.cwd);
        // O que não foi pedido agora continua valendo: trocar só o esforço não derruba o modelo.
        let model_final = model.map(str::to_string).or_else(|| s.model.clone());
        let effort_final = effort.map(str::to_string).or_else(|| s.effort.clone());

        self.marca_relancamento(session_id);
        if let Some(tmux) = &s.tmux {
            sessions::kill(tmux).await?;
            // Sem esta pausa o `--resume` pode esbarrar no processo anterior ainda saindo.
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        self.hub.unlisten(session_id);
        self.status.forget(session_id);

        let spec = sessions::Spec {
            projeto: &projeto,
            permission_mode: &modo,
            model: model_final.as_deref(),
            effort: effort_final.as_deref(),
            resume: Some(session_id),
            retomada: false,
        };
        let lancada = sessions::launch(&spec).await?;

        self.store
            .set_model(session_id, model_final.as_deref(), effort_final.as_deref())?;
        self.store.set_status(session_id, "iniciando")?;
        if let Some(topic) = s.topic_id {
            let _ = self
                .tg
                .send_html(
                    Some(topic),
                    &format!(
                        "♻️ Sessão reiniciada com o contexto inteiro.\n{}",
                        escape_html(&ficha(
                            model_final.as_deref(),
                            effort_final.as_deref(),
                            &modo,
                            &lancada.tmux
                        ))
                    ),
                )
                .await;
        }
        self.panel.refresh();
        info!(sessao = %session_id, modelo = ?model_final, esforco = ?effort_final, "sessão relançada");
        Ok(())
    }

    /// Despeja no tópico as últimas falas da conversa retomada.
    ///
    /// Vai em mensagens separadas por papel, e não num bloco só, porque no celular um muro de
    /// texto misturando pergunta e resposta não se lê. O que não é diálogo (ferramenta,
    /// raciocínio) fica de fora: aqui interessa o fio da conversa.
    async fn publica_historico(&self, topic: i32, session_id: &str) {
        let Some(s) = self.store.get(session_id).ok().flatten() else {
            return;
        };
        let caminho = match s.transcript_path.as_deref() {
            Some(p) => std::path::PathBuf::from(p),
            None => ld_core::transcript::dir_do_projeto(&paths::claude_dir(), &s.cwd)
                .join(format!("{session_id}.jsonl")),
        };
        let falas = ld_core::transcript::historico(&caminho, self.cfg.history_lines);
        info!(
            sessao = %session_id,
            falas = falas.len(),
            transcript = %caminho.display(),
            "histórico publicado no tópico"
        );
        if falas.is_empty() {
            return;
        }

        let _ = self
            .tg
            .send_html(
                Some(topic),
                &format!(
                    "📜 <b>Retomando a conversa</b> <i>(últimas {} falas)</i>",
                    falas.len()
                ),
            )
            .await;
        for f in falas {
            let (marca, texto) = match f.papel {
                ld_core::transcript::Papel::Usuario => ("👤", f.texto),
                ld_core::transcript::Papel::Assistente => ("🤖", f.texto),
            };
            let corpo = corta(&texto, 1200);
            let _ = self
                .tg
                .send_html(Some(topic), &format!("{marca} {}", escape_html(&corpo)))
                .await;
        }
        let _ = self
            .tg
            .send_html(
                Some(topic),
                "— <i>fim do histórico; pode continuar daqui</i>",
            )
            .await;
    }

    /// Encerra as sessões cujo tmux não existe mais.
    ///
    /// Duas coisas deixam esse lixo para trás: a sessão morre sozinha (crash, `exit` digitado no
    /// PC) com o daemon fora do ar, e o lançamento falha depois que o registro já foi gravado.
    /// Sem isto elas ficam no painel para sempre, e o tópico delas vira um canal que não responde.
    pub async fn reconcile(&self) -> Result<usize> {
        let mut mortas = 0;
        for s in self.store.live()? {
            let Some(tmux) = &s.tmux else {
                continue; // sessão do terminal: quem cuida dela é o hook SessionEnd.
            };
            if sessions::has_session(tmux).await || self.em_relancamento(&s.session_id) {
                continue;
            }
            warn!(sessao = %s.session_id, tmux = %tmux, "tmux sumiu; encerrando a sessão");
            self.end_session(&s.session_id, true).await?;
            mortas += 1;
        }

        // Tópico de sessão encerrada que sobrou no grupo (daemon caiu no meio do fechamento,
        // API fora do ar na hora): vira um canal que não responde a ninguém.
        for (sessao, topico) in self.store.topicos_vazados().unwrap_or_default() {
            if self.em_relancamento(&sessao) {
                continue;
            }
            match self.tg.delete_topic_sweep(topico).await {
                crate::telegram::Resolvido::Apagado => {
                    warn!(topico, sessao = %sessao, "tópico vazado; apagado");
                    let _ = self.store.clear_topic(&sessao);
                }
                // Já não existe: o objetivo era não ter esse tópico, e ele não está lá.
                crate::telegram::Resolvido::JaNaoExiste => {
                    let _ = self.store.clear_topic(&sessao);
                }
                crate::telegram::Resolvido::TenteDepois => {}
            }
        }

        // O contrário também acontece: o tmux ficou vivo com a sessão já encerrada no banco (um
        // relançamento interrompido no meio, por exemplo). Ninguém mais fala com ele, e o tópico
        // dele já foi apagado, então é lixo que só consome memória.
        for tmux in sessions::nossas_sessoes().await {
            if self.store.tmux_de_sessao_morta(&tmux).unwrap_or(false) {
                warn!(tmux = %tmux, "tmux órfão de sessão encerrada; matando");
                let _ = sessions::kill(&tmux).await;
            }
        }
        Ok(mortas)
    }

    // ---------------------------------------------------------------- vindo do Telegram

    /// Mensagem sua num tópico de sessão.
    pub async fn on_incoming(&self, topic: i32, texto: &str, de: &str) -> Result<()> {
        let Some(s) = self.store.by_topic(topic)? else {
            bail!("tópico {topic} não tem sessão viva");
        };

        let msg = Incoming {
            text: texto.to_string(),
            from: de.to_string(),
            at: agora(),
        };
        if !self.hub.deliver(&s.session_id, msg) {
            // Sem monitor armado: guarda para entregar assim que ele voltar, e diz isso, senão
            // parece que a mensagem sumiu.
            self.store.enqueue(&s.session_id, texto, de)?;
            let _ = self
                .tg
                .send_html(
                    Some(topic),
                    "⏳ <i>a sessão está sem monitor armado; guardei a mensagem e ela entra assim que ele voltar</i>",
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
            self.tg.delete(id).await;
        }

        self.store.set_status(&s.session_id, "pensando")?;
        self.status
            .set(&self.ctx(), &s.session_id, topic, "Pensando...".into());
        Ok(())
    }

    /// `true` quando este prompt é o que o daemon acabou de entregar pelo Telegram.
    fn e_eco(&self, session_id: &str, texto: &str) -> bool {
        const JANELA: std::time::Duration = std::time::Duration::from_secs(120);
        let mut mapa = self.entregues.lock().unwrap_or_else(|e| e.into_inner());
        mapa.retain(|_, (_, quando)| quando.elapsed() < JANELA);
        match mapa.get(session_id) {
            Some((entregue, _)) => entregue.trim() == texto.trim(),
            None => false,
        }
    }

    fn tira_aviso(&self, session_id: &str) -> Option<teloxide::types::MessageId> {
        self.avisos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id)
    }

    fn avisos_handle(&self) -> Arc<Mutex<HashMap<String, teloxide::types::MessageId>>> {
        // O mapa é consultado de dentro de uma tarefa que sobrevive a esta chamada, então ele
        // precisa ser compartilhado por Arc, e não emprestado.
        self.avisos.clone()
    }

    /// Entrega uma mensagem direto a uma sessão, sem Telegram no caminho.
    pub fn inject(&self, session_id: &str, texto: &str) -> Result<bool> {
        let entregue = self.hub.deliver(
            session_id,
            Incoming {
                text: texto.to_string(),
                from: "pc".into(),
                at: agora(),
            },
        );
        if !entregue {
            self.store.enqueue(session_id, texto, "pc")?;
        }
        Ok(entregue)
    }

    // ---------------------------------------------------------------- vindo dos hooks

    pub fn on_register(&self, r: &RegisterSession) -> Result<()> {
        // Sessão que o bot criou: só falta o caminho do transcript.
        if let Some(existente) = self.store.get(&r.session_id)? {
            if existente.transcript_path.as_deref() != Some(r.transcript_path.as_str()) {
                self.store
                    .set_transcript(&r.session_id, &r.transcript_path)?;
            }
            return Ok(());
        }

        // `/clear` troca o id da sessão sem trocar o terminal. Sem isto o tópico ficaria falando
        // com um id morto, que é exatamente o bug que derrubava a ponte antiga.
        if r.reason == "clear"
            && let Some(anterior) = self.store.live_by_cwd(&r.cwd, &r.session_id)?
        {
            self.store.upsert(&nova_sessao(r))?;
            self.store.rekey(&anterior.session_id, &r.session_id)?;
            self.hub.unlisten(&anterior.session_id);
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
        // Troca de modelo conta para o painel mesmo em sessão sem tópico (as do seu terminal).
        if let EventKind::ModelSwitch { model } = &ev.event {
            self.store.set_model(&ev.session_id, Some(model), None)?;
            self.panel.refresh();
            return Ok(());
        }
        let Some(topic) = s.topic_id else {
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
                    .set(&self.ctx(), &ev.session_id, topic, label.clone());
            }
            EventKind::ToolEnd { .. } => {
                self.store.set_status(&ev.session_id, "pensando")?;
                self.status
                    .set(&self.ctx(), &ev.session_id, topic, "Pensando...".into());
            }
            EventKind::Streaming => {
                self.status
                    .set(&self.ctx(), &ev.session_id, topic, "Escrevendo...".into());
            }
            EventKind::UserPrompt { text } => {
                if self.e_eco(&ev.session_id, text) {
                    return Ok(());
                }
                info!(sessao = %ev.session_id, "prompt digitado no PC, espelhado no tópico");
                self.store.set_status(&ev.session_id, "pensando")?;
                let corpo = format!("👤 <i>do PC</i>\n{}", escape_html(&corta(text, 1200)));
                let tg = self.tg.clone();
                tokio::spawn(async move {
                    let _ = tg.send_html(Some(topic), &corpo).await;
                });
            }
            EventKind::ModelSwitch { model } => {
                self.store.set_model(&ev.session_id, Some(model), None)?;
                self.panel.refresh();
            }
            EventKind::Notification { text } | EventKind::Failure { text } => {
                let texto = text.clone();
                let tg = self.tg.clone();
                let anterior = self.tira_aviso(&ev.session_id);
                let sessao = ev.session_id.clone();
                let app_avisos = self.avisos_handle();
                tokio::spawn(async move {
                    // Dois avisos seguidos não se acumulam: o novo substitui o velho.
                    if let Some(id) = anterior {
                        tg.delete(id).await;
                    }
                    if let Ok(id) = tg
                        .send_html(Some(topic), &format!("⚠️ {}", escape_html(&texto)))
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

        if let Some(topic) = s.topic_id {
            self.status.clear(&self.ctx(), &r.session_id, topic);
            if let Some(texto) = r.last_assistant_message.as_deref()
                && !texto.trim().is_empty()
            {
                self.tg.send(Some(topic), texto).await?;
            }
        }

        // Só sessão do bot com tópico precisa de monitor: a do terminal fala pelo teclado.
        let precisa_monitor = s.owned_by_bot() && s.topic_id.is_some();
        if !precisa_monitor || self.hub.has_listener(&r.session_id) {
            return Ok(Response::Listener {
                alive: true,
                rearm_attempts: 0,
                rearm_command: None,
            });
        }

        let tentativas = self.hub.bump_rearm(&r.session_id);
        if tentativas > TETO_REARME {
            if let Some(topic) = s.topic_id {
                let _ = self.tg.send_html(
                    Some(topic),
                    "🔇 <b>Sessão surda.</b> O monitor não voltou depois de três lembretes, então parei de insistir. Mande /kill e abra outra, ou reative pelo tmux.",
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

    // ---------------------------------------------------------------- perguntas

    /// Abre o card de pergunta no tópico da sessão e devolve por onde a resposta chega.
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
        let Some(topic) = s.topic_id else {
            // Sessão de terminal: o menu nativo do Claude Code é melhor do que um card no
            // celular para quem já está na frente do teclado.
            bail!("sessão sem tópico");
        };
        if ask.is_empty() {
            bail!("pergunta vazia");
        }

        let (ask_id, rx) = self.hub.open_ask(session_id);
        let mut card = Card::nova_pergunta(
            ask_id.clone(),
            session_id.to_string(),
            topic,
            teloxide::types::MessageId(0),
            ask,
        );
        let Efeito::Redesenhar(texto, teclado) = card.desenhar() else {
            self.hub.close_ask(&ask_id);
            bail!("não consegui desenhar o card");
        };
        let msg = self.tg.send_keyboard(Some(topic), &texto, teclado).await?;
        card.msg = msg;
        self.cards.abrir(&ask_id, card);

        self.store.set_status(session_id, "perguntando")?;
        self.status
            .set(&self.ctx(), session_id, topic, "Perguntando...".into());
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
        let Some(topic) = s.topic_id else {
            bail!("sessão sem tópico");
        };

        let (ask_id, rx) = self.hub.open_ask(session_id);
        let detalhe = ld_core::labels::label_for_tool(ferramenta, entrada);
        let texto = format!(
            "🔐 <b>Permissão</b>\n{}\n<code>{}</code>",
            escape_html(ferramenta),
            escape_html(&detalhe)
        );
        let teclado = crate::telegram::coluna(vec![
            ("✅ Permitir".into(), format!("p:{ask_id}:a")),
            ("⛔ Negar".into(), format!("p:{ask_id}:d")),
        ]);
        let msg = self.tg.send_keyboard(Some(topic), &texto, teclado).await?;
        self.cards.abrir(
            &ask_id,
            Card::nova_permissao(ask_id.clone(), session_id.to_string(), topic, msg),
        );
        self.store.set_status(session_id, "permissão")?;
        self.status.set(
            &self.ctx(),
            session_id,
            topic,
            "Esperando você liberar...".into(),
        );
        Ok((ask_id, rx))
    }

    /// Fecha o card, respondido ou não. É o único ponto de limpeza: quem espera a resposta chama
    /// isto ao terminar, tanto no caminho feliz quanto no timeout.
    pub async fn cleanup_ask(&self, ask_id: &str) {
        self.hub.close_ask(ask_id);
        if let Some(card) = self.cards.fechar(ask_id) {
            self.tg.delete(card.msg).await;
            // A sessão volta a trabalhar: deixar "Perguntando..." parado seria mentira na tela.
            let _ = self.store.set_status(&card.session_id, "pensando");
            self.status.set(
                &self.ctx(),
                &card.session_id,
                card.topic,
                "Pensando...".into(),
            );
        }
    }

    /// Toque em botão de card, vindo do Telegram.
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
                    Efeito::Redesenhar(texto, teclado) => {
                        if let Some(msg) = self.cards.msg(&ask_id) {
                            self.tg.edit_keyboard(msg, &texto, teclado).await?;
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

    // ---------------------------------------------------------------- painel

    pub fn summaries(&self) -> Result<Vec<SessionSummary>> {
        Ok(self
            .store
            .live()?
            .into_iter()
            .map(|s| {
                let ctx = s
                    .transcript_path
                    .as_deref()
                    .map(std::path::Path::new)
                    .and_then(context::read);
                s.summary(ctx)
            })
            .collect())
    }

    pub async fn session_for_topic(&self, topic: i32) -> Result<Option<Session>> {
        self.store.by_topic(topic).context("consultando tópico")
    }
}

/// Linha de identificação da sessão: modelo, esforço, modo de permissão e tmux.
fn ficha(model: Option<&str>, effort: Option<&str>, modo: &str, tmux: &str) -> String {
    format!(
        "modelo: {} · esforço: {} · permissão: {modo} · tmux: {tmux}",
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
        tmux: None,
        topic_id: None,
        status: "ocioso".into(),
        status_message_id: None,
        model: r.model.clone(),
        effort: None,
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
