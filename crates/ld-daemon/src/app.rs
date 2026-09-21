//! O miolo: tudo que o Telegram e o socket mandam fazer passa por aqui.
//!
//! Regra de convivência entre os dois lados: o Telegram nunca fala com o tmux direto e o hook
//! nunca fala com o Telegram direto. Os dois chamam método deste tipo, que é quem conhece o
//! estado. Assim existe um só lugar onde "sessão morreu" quer dizer as quatro coisas que ela
//! precisa querer dizer (matar o tmux, apagar o tópico, fechar as perguntas, marcar no banco).

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use ld_core::ask::{Answer, Ask};
use ld_core::config::{Config, Project};
use ld_core::context;
use ld_core::proto::{
    EventKind, RegisterSession, Response, SessionEvent, SessionSummary, StopReport,
};
use ld_core::state::{Session, Store};
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
    pub store: Arc<Store>,
    pub tg: Tg,
    pub hub: Hub,
    pub status: StatusBoard,
    pub cards: Cards,
    pub panel: Panel,
}

impl App {
    /// Precisa rodar dentro de um runtime tokio: o painel sobe a tarefa dele aqui.
    pub fn new(cfg: Config, store: Store, tg: Tg) -> Self {
        let store = Arc::new(store);
        let panel = Panel::start(tg.clone(), store.clone());
        Self {
            cfg,
            store,
            tg,
            hub: Hub::new(),
            status: StatusBoard::new(),
            cards: Cards::new(),
            panel,
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
    pub async fn create_session(&self, projeto: &Project) -> Result<String> {
        let topic = self.tg.create_topic(&projeto.name).await?;

        let modo = self.cfg.permission_mode_for(&projeto.path);
        let lancada = match sessions::launch(projeto, &modo).await {
            Ok(l) => l,
            Err(e) => {
                let _ = self.tg.delete_topic(topic).await;
                return Err(e);
            }
        };

        self.store.upsert(&Session {
            session_id: lancada.session_id.clone(),
            project: projeto.name.clone(),
            cwd: projeto.path.clone(),
            transcript_path: None,
            tmux: Some(lancada.tmux.clone()),
            topic_id: Some(topic),
            status: "iniciando".into(),
            status_message_id: None,
            created_at: 0,
            ended_at: None,
        })?;

        let _ = self
            .tg
            .send_html(
                Some(topic),
                &format!(
                    "🟢 <b>{}</b>\n<code>{}</code>\nmodo: {modo} · tmux: <code>{}</code>\n\nPode falar. Para fechar, mande /kill.",
                    escape_html(&projeto.name),
                    escape_html(&projeto.path),
                    escape_html(&lancada.tmux),
                ),
            )
            .await;

        self.panel.refresh();
        info!(sessao = %lancada.session_id, topico = topic, projeto = %projeto.name, "sessão criada");
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

        if apagar_topico
            && let Some(topic) = s.topic_id
            && let Err(e) = self.tg.delete_topic(topic).await
        {
            warn!(topico = topic, erro = %e, "não consegui apagar o tópico");
        }

        self.store.end(session_id)?;
        self.panel.refresh();
        info!(sessao = %session_id, "sessão encerrada");
        Ok(())
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

        self.store.set_status(&s.session_id, "pensando")?;
        self.status
            .set(&self.ctx(), &s.session_id, topic, "Pensando...".into());
        Ok(())
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
        let Some(topic) = s.topic_id else {
            // Sessão de terminal: conta para o painel, não tem onde escrever.
            return Ok(());
        };

        match &ev.event {
            EventKind::ToolStart { label, .. } => {
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
            EventKind::Notification { text } | EventKind::Failure { text } => {
                let texto = text.clone();
                let tg = self.tg.clone();
                tokio::spawn(async move {
                    let _ = tg
                        .send_html(Some(topic), &format!("⚠️ {}", escape_html(&texto)))
                        .await;
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
        created_at: 0,
        ended_at: None,
    }
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
