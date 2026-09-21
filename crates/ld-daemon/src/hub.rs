//! Estado vivo do daemon: quem está ouvindo, o que está esperando resposta.
//!
//! O que mora aqui não sobrevive a um restart de propósito. Uma conexão `Listen` é uma conexão
//! aberta, e uma pergunta pendente é um hook bloqueado do outro lado: se o daemon reiniciou, as
//! duas coisas já morreram junto. O que precisa sobreviver (vínculo sessão <-> tópico, fila de
//! mensagens) está no SQLite, não aqui.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::{mpsc, oneshot};

/// Uma mensagem do Telegram a caminho da sessão.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub text: String,
    pub from: String,
    pub at: i64,
}

/// Uma pergunta (ou pedido de permissão) esperando resposta humana.
struct Pending {
    session_id: String,
    responder: oneshot::Sender<String>,
}

#[derive(Default)]
pub struct Hub {
    listeners: Mutex<HashMap<String, mpsc::UnboundedSender<Incoming>>>,
    pending: Mutex<HashMap<String, Pending>>,
    /// Quantas vezes seguidas pedimos que a sessão re-armasse o Monitor. Zera quando um
    /// `Listen` novo aparece. Serve para desistir em vez de insistir para sempre.
    rearm: Mutex<HashMap<String, u32>>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registra o canal de entrada de uma sessão. Substituir um registro anterior é o certo: se
    /// o Monitor foi re-armado, o `listen` velho já está morto do outro lado.
    pub fn listen(&self, session_id: &str) -> mpsc::UnboundedReceiver<Incoming> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), tx);
        self.rearm
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        rx
    }

    pub fn unlisten(&self, session_id: &str) {
        self.listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    pub fn has_listener(&self, session_id: &str) -> bool {
        self.listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .is_some_and(|tx| !tx.is_closed())
    }

    /// Entrega imediata: `true` quando a sessão estava ouvindo. `false` quer dizer que o
    /// chamador precisa guardar a mensagem na fila do banco.
    pub fn deliver(&self, session_id: &str, msg: Incoming) -> bool {
        let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        match listeners.get(session_id) {
            Some(tx) => match tx.send(msg) {
                Ok(()) => true,
                Err(_) => {
                    // Receptor morreu sem passar pelo `unlisten` (processo do Monitor morto).
                    listeners.remove(session_id);
                    false
                }
            },
            None => false,
        }
    }

    /// Conta mais uma cobrança de re-arme e devolve o total acumulado.
    pub fn bump_rearm(&self, session_id: &str) -> u32 {
        let mut r = self.rearm.lock().unwrap_or_else(|e| e.into_inner());
        let n = r.entry(session_id.to_string()).or_insert(0);
        *n += 1;
        *n
    }

    /// Abre uma pendência de resposta. Devolve o id do card e o receptor da resposta.
    pub fn open_ask(&self, session_id: &str) -> (String, oneshot::Receiver<String>) {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id.clone(),
                Pending {
                    session_id: session_id.to_string(),
                    responder: tx,
                },
            );
        (id, rx)
    }

    /// Responde uma pendência. `false` quando ela já tinha sido respondida ou expirado, que é o
    /// caso normal da corrida entre Telegram e janela nativa: o segundo a chegar perde.
    pub fn answer(&self, ask_id: &str, texto: String) -> bool {
        let pendente = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(ask_id);
        match pendente {
            Some(p) => p.responder.send(texto).is_ok(),
            None => false,
        }
    }

    pub fn close_ask(&self, ask_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(ask_id);
    }

    /// Perguntas abertas de uma sessão, usado para limpar cards quando a sessão morre.
    pub fn asks_of(&self, session_id: &str) -> Vec<String> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, p)| p.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sem_listener_a_entrega_falha() {
        let hub = Hub::new();
        assert!(!hub.has_listener("s1"));
        assert!(!hub.deliver(
            "s1",
            Incoming {
                text: "oi".into(),
                from: "luka".into(),
                at: 0
            }
        ));
    }

    #[tokio::test]
    async fn entrega_chega_no_listener() {
        let hub = Hub::new();
        let mut rx = hub.listen("s1");
        assert!(hub.has_listener("s1"));
        assert!(hub.deliver(
            "s1",
            Incoming {
                text: "oi".into(),
                from: "luka".into(),
                at: 0
            }
        ));
        assert_eq!(rx.recv().await.unwrap().text, "oi");
    }

    #[test]
    fn listener_morto_e_removido_na_entrega() {
        let hub = Hub::new();
        let rx = hub.listen("s1");
        drop(rx);
        assert!(!hub.deliver(
            "s1",
            Incoming {
                text: "oi".into(),
                from: "luka".into(),
                at: 0
            }
        ));
        assert!(!hub.has_listener("s1"));
    }

    #[test]
    fn rearm_conta_e_zera_ao_reouvir() {
        let hub = Hub::new();
        assert_eq!(hub.bump_rearm("s1"), 1);
        assert_eq!(hub.bump_rearm("s1"), 2);
        let _rx = hub.listen("s1");
        assert_eq!(hub.bump_rearm("s1"), 1, "listen novo zera a contagem");
    }

    #[tokio::test]
    async fn so_a_primeira_resposta_vale() {
        let hub = Hub::new();
        let (id, rx) = hub.open_ask("s1");
        assert!(hub.answer(&id, "telegram".into()));
        assert!(
            !hub.answer(&id, "janela".into()),
            "o segundo a responder perde a corrida"
        );
        assert_eq!(rx.await.unwrap(), "telegram");
    }

    #[test]
    fn asks_da_sessao() {
        let hub = Hub::new();
        let (a, _rx1) = hub.open_ask("s1");
        let (_b, _rx2) = hub.open_ask("s2");
        assert_eq!(hub.asks_of("s1"), vec![a]);
    }
}
