//! A mensagem de status: uma só por sessão, editada enquanto o turno anda.
//!
//! O Telegram limita edições por mensagem, e um turno do Claude produz evento a cada ferramenta.
//! Sem controle, isso vira erro 429 e a mensagem congela justo quando ela é útil. A solução aqui
//! é uma tarefa por sessão que junta os eventos: ela só edita a cada `DEBOUNCE`, e quando vários
//! eventos chegam durante a espera, o que vale é o último. Estado intermediário que ninguém
//! chegou a ver não é perda.
//!
//! O id da mensagem é gravado no banco a cada mudança: se o daemon reiniciar no meio de um
//! turno, a mensagem de status é adotada em vez de virar órfã no tópico.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ld_core::state::Store;
use teloxide::types::MessageId;
use tokio::sync::mpsc;

use crate::telegram::{Tg, escape_html};

const DEBOUNCE: Duration = Duration::from_millis(1200);

enum Cmd {
    Set(String),
    Clear,
}

#[derive(Default)]
pub struct StatusBoard {
    canais: Mutex<HashMap<String, mpsc::UnboundedSender<Cmd>>>,
}

impl StatusBoard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mostra (ou atualiza) o status da sessão. Nunca bloqueia: entrega para a tarefa da sessão
    /// e volta na hora, porque quem chama é um hook que precisa sair rápido.
    pub fn set(&self, ctx: &Ctx, session_id: &str, topic: i32, label: String) {
        let _ = self.canal(ctx, session_id, topic).send(Cmd::Set(label));
    }

    /// Apaga o status. Chamado quando a resposta final chega: a partir daí a mensagem de status
    /// seria mentira parada na tela.
    pub fn clear(&self, ctx: &Ctx, session_id: &str, topic: i32) {
        let _ = self.canal(ctx, session_id, topic).send(Cmd::Clear);
    }

    pub fn forget(&self, session_id: &str) {
        self.canais
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    fn canal(&self, ctx: &Ctx, session_id: &str, topic: i32) -> mpsc::UnboundedSender<Cmd> {
        let mut canais = self.canais.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = canais.get(session_id)
            && !tx.is_closed()
        {
            return tx.clone();
        }
        let (tx, rx) = mpsc::unbounded_channel();
        canais.insert(session_id.to_string(), tx.clone());
        // Adota a mensagem que já estava no banco, se houver.
        let inicial = ctx
            .store
            .get(session_id)
            .ok()
            .flatten()
            .and_then(|s| s.status_message_id)
            .map(MessageId);
        tokio::spawn(tarefa(
            ctx.clone(),
            session_id.to_string(),
            topic,
            inicial,
            rx,
        ));
        tx
    }
}

/// O que a tarefa precisa para trabalhar sozinha.
#[derive(Clone)]
pub struct Ctx {
    pub tg: Tg,
    pub store: Arc<Store>,
}

/// Uma tarefa por sessão. Ela é dona do id da mensagem de status, então não há duas partes do
/// daemon criando mensagem ao mesmo tempo.
async fn tarefa(
    ctx: Ctx,
    session_id: String,
    topic: i32,
    inicial: Option<MessageId>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
) {
    let mut atual = inicial;
    let mut ultimo_texto = String::new();
    // Começa no passado para o primeiro status sair na hora: a espera só existe entre edições.
    let mut ultima_edicao = Instant::now() - DEBOUNCE;

    while let Some(mut cmd) = rx.recv().await {
        let desde = ultima_edicao.elapsed();
        if desde < DEBOUNCE {
            tokio::time::sleep(DEBOUNCE - desde).await;
        }
        // Junta o que entrou na fila durante a espera: cinco ferramentas podem ter passado, e só
        // a última descreve o agora.
        while let Ok(proximo) = rx.try_recv() {
            cmd = proximo;
        }

        match cmd {
            Cmd::Set(label) => {
                let html = format!("⚙️ <i>{}</i>", escape_html(&label));
                if html == ultimo_texto {
                    continue;
                }
                let novo = match atual {
                    Some(id) => match ctx.tg.edit_html(id, &html).await {
                        Ok(()) => Some(id),
                        // Mensagem sumiu (apagada na mão, tópico recriado): manda outra.
                        Err(_) => ctx.tg.send_html(Some(topic), &html).await.ok(),
                    },
                    None => ctx.tg.send_html(Some(topic), &html).await.ok(),
                };
                if novo != atual {
                    let _ = ctx.store.set_status_message(&session_id, novo.map(|m| m.0));
                    atual = novo;
                }
                ultimo_texto = html;
                ultima_edicao = Instant::now();
            }
            Cmd::Clear => {
                if let Some(id) = atual.take() {
                    ctx.tg.delete(id).await;
                    let _ = ctx.store.set_status_message(&session_id, None);
                }
                ultimo_texto.clear();
                ultima_edicao = Instant::now();
            }
        }
    }
}
