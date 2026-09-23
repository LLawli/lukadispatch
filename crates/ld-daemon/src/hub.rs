//! Estado vivo do daemon: quem está ouvindo, o que está esperando resposta.
//!
//! O que mora aqui não sobrevive a um restart de propósito. Uma conexão `Listen` é uma conexão
//! aberta, e uma pergunta pendente é um hook bloqueado do outro lado: se o daemon reiniciou, as
//! duas coisas já morreram junto. O que precisa sobreviver (vínculo sessão <-> tópico, fila de
//! mensagens) está no SQLite, não aqui.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::{mpsc, oneshot};

/// O que trafega no canal de um `Listen`.
#[derive(Debug, Clone)]
pub enum Aviso {
    /// Mensagem para a sessão.
    Mensagem(Incoming),
    /// Outro monitor assumiu: este deve sair sem reconectar.
    Substituido,
}

/// Uma mensagem do chat a caminho da sessão.
#[derive(Debug, Clone)]
pub struct Incoming {
    pub text: String,
    pub from: String,
    pub at: i64,
    /// Caminhos absolutos dos anexos já gravados em disco. Vazio na mensagem de texto.
    pub files: Vec<String>,
}

impl Incoming {
    /// Mensagem sem anexo, que é a esmagadora maioria.
    pub fn texto(text: impl Into<String>, from: impl Into<String>, at: i64) -> Self {
        Self {
            text: text.into(),
            from: from.into(),
            at,
            files: Vec::new(),
        }
    }
}

/// Uma pergunta (ou pedido de permissão) esperando resposta humana.
struct Pending {
    session_id: String,
    responder: oneshot::Sender<String>,
}

/// Registro de um `Listen` aberto.
///
/// O `token` é o que impede o bug do monitor que se auto-derruba: quando o agente re-arma antes
/// de o monitor antigo morrer, o antigo chamava `unlisten` e apagava o registro do NOVO. O
/// daemon então dizia "sem monitor armado", o hook Stop mandava re-armar, e a sessão entrava num
/// laço de re-armar para sempre. Com token, cada conexão só remove a si mesma.
struct Registro {
    token: u64,
    tx: mpsc::UnboundedSender<Aviso>,
}

#[derive(Default)]
pub struct Hub {
    listeners: Mutex<HashMap<String, Registro>>,
    proximo_token: Mutex<u64>,
    pending: Mutex<HashMap<String, Pending>>,
    /// Quantas vezes seguidas pedimos que a sessão re-armasse o Monitor. Zera quando um
    /// `Listen` novo aparece. Serve para desistir em vez de insistir para sempre.
    rearm: Mutex<HashMap<String, u32>>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registra o canal de entrada de uma sessão e devolve o token desse registro.
    ///
    /// Um registro novo substitui o anterior, e o anterior é avisado para sair sem reconectar.
    pub fn listen(&self, session_id: &str) -> (u64, mpsc::UnboundedReceiver<Aviso>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let token = {
            let mut n = self.proximo_token.lock().unwrap_or_else(|e| e.into_inner());
            *n += 1;
            *n
        };

        let anterior = self
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), Registro { token, tx });
        if let Some(velho) = anterior {
            let _ = velho.tx.send(Aviso::Substituido);
        }

        self.rearm
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        (token, rx)
    }

    /// Tira o registro, mas só se ele ainda for o desta conexão.
    pub fn unlisten(&self, session_id: &str, token: u64) {
        let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        if listeners.get(session_id).is_some_and(|r| r.token == token) {
            listeners.remove(session_id);
        }
    }

    /// Tira o registro seja qual for o token. Só para quando a sessão acaba de vez (fim,
    /// relançamento, remapeamento de `/clear`): aí não existe mais monitor legítimo para
    /// preservar.
    pub fn unlisten_qualquer(&self, session_id: &str) {
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
            .is_some_and(|r| !r.tx.is_closed())
    }

    /// Entrega imediata: `true` quando a sessão estava ouvindo. `false` quer dizer que o
    /// chamador precisa guardar a mensagem na fila do banco.
    pub fn deliver(&self, session_id: &str, msg: Incoming) -> bool {
        let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        match listeners.get(session_id) {
            Some(r) => match r.tx.send(Aviso::Mensagem(msg)) {
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
    /// caso normal da corrida entre o card do chat e a janela nativa: o segundo a chegar perde.
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
        assert!(!hub.deliver("s1", Incoming::texto("oi", "luka", 0)));
    }

    #[tokio::test]
    async fn entrega_chega_no_listener() {
        let hub = Hub::new();
        let (_t, mut rx) = hub.listen("s1");
        assert!(hub.has_listener("s1"));
        assert!(hub.deliver("s1", Incoming::texto("oi", "luka", 0)));
        match rx.recv().await.unwrap() {
            Aviso::Mensagem(m) => assert_eq!(m.text, "oi"),
            Aviso::Substituido => panic!("não houve substituição"),
        }
    }

    #[tokio::test]
    async fn monitor_novo_avisa_o_velho_e_fica_com_o_registro() {
        // O bug que isto trava: o velho, ao morrer, apagava o registro do novo, o daemon dizia
        // "sem monitor armado" e o hook Stop mandava re-armar para sempre.
        let hub = Hub::new();
        let (token_velho, mut rx_velho) = hub.listen("s1");
        let (_token_novo, _rx_novo) = hub.listen("s1");

        assert!(
            matches!(rx_velho.recv().await, Some(Aviso::Substituido)),
            "o velho precisa saber que saiu"
        );
        hub.unlisten("s1", token_velho);
        assert!(
            hub.has_listener("s1"),
            "o registro do novo tem que sobreviver à saída do velho"
        );
    }

    #[test]
    fn listener_morto_e_removido_na_entrega() {
        let hub = Hub::new();
        let (_t, rx) = hub.listen("s1");
        drop(rx);
        assert!(!hub.deliver("s1", Incoming::texto("oi", "luka", 0)));
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
