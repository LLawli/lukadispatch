//! O Bot API do Telegram de mentira: um servidor HTTP local com o qual o adaptador de verdade
//! (teloxide, `frontend::telegram`) fala sem saber que não é o Telegram.
//!
//! Guarda o chat como o Telegram guardaria: as mensagens do bot com o texto que aparece na tela
//! (o HTML do `parse_mode` já sem as tags) e os botões, os tópicos criados e apagados, e a fila de
//! updates que o `getUpdates` entrega. Os updates que o teste injeta têm a forma do Telegram de
//! verdade: mensagem de tópico respondendo à raiz dele, toque com a mensagem do teclado junto,
//! comando com a entidade `bot_command`.
//!
//! Só os métodos que o daemon chama estão aqui. Um método novo responde `true` e fica em
//! [`Chat::chamadas`], e o teloxide reclama no log do daemon se esperava outra coisa.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;

/// O supergrupo com tópicos.
pub const CHAT: i64 = -1001234567890;
/// Quem está na allowlist.
pub const LUKA: i64 = 42;
const BOT: i64 = 7000000001;
pub const TOKEN: &str = "7000000001:e2e-token-falso";

/// Uma mensagem que o bot mandou, como está na tela agora.
#[derive(Debug, Clone)]
pub struct Mensagem {
    pub id: i32,
    /// `None` é o General.
    pub topico: Option<i32>,
    pub texto: String,
    /// (rótulo, callback_data), na ordem do teclado.
    pub botoes: Vec<(String, String)>,
    pub apagada: bool,
}

#[derive(Debug, Clone)]
pub struct Topico {
    pub id: i32,
    pub nome: String,
    pub apagado: bool,
}

/// O que o teste pode ler do chat.
#[derive(Default)]
pub struct Chat {
    pub mensagens: Vec<Mensagem>,
    pub topicos: Vec<Topico>,
    /// Os métodos chamados, na ordem.
    pub chamadas: Vec<String>,
    updates: Vec<Value>,
    /// O maior `offset` já pedido: o que está antes dele o Telegram não entrega mais.
    confirmado: i64,
    proxima_msg: i32,
    proximo_toque: u32,
}

impl Chat {
    fn nova_msg(&mut self) -> i32 {
        self.proxima_msg += 1;
        self.proxima_msg
    }

    fn enfileira(&mut self, mut corpo: Value) {
        corpo["update_id"] = json!(self.updates.len() as i64 + 1);
        self.updates.push(corpo);
    }

    pub fn topico(&self, nome: &str) -> Option<&Topico> {
        self.topicos.iter().rev().find(|t| t.nome == nome)
    }

    /// As mensagens visíveis de um lugar (o General com `None`).
    pub fn em(&self, topico: Option<i32>) -> impl Iterator<Item = &Mensagem> {
        self.mensagens
            .iter()
            .filter(move |m| !m.apagada && m.topico == topico)
    }

    /// O chat inteiro, legível, para a mensagem de falha de um teste.
    pub fn conversa(&self) -> String {
        let mut s = String::new();
        for t in &self.topicos {
            s.push_str(&format!(
                "tópico {} {:?}{}\n",
                t.id,
                t.nome,
                if t.apagado { " (apagado)" } else { "" }
            ));
        }
        for m in &self.mensagens {
            let onde = m.topico.map(|t| t.to_string()).unwrap_or("general".into());
            let botoes: Vec<&str> = m.botoes.iter().map(|(r, _)| r.as_str()).collect();
            s.push_str(&format!(
                "[{onde}] #{}{} {:?} {botoes:?}\n",
                m.id,
                if m.apagada { " (apagada)" } else { "" },
                m.texto
            ));
        }
        s
    }
}

pub struct BotApi {
    chat: Arc<Mutex<Chat>>,
    /// O endereço base, como o `LUKADISPATCH_TELEGRAM_API` espera.
    pub url: String,
}

fn agora() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn chat_json() -> Value {
    json!({"id": CHAT, "type": "supergroup", "title": "lukadispatch e2e", "is_forum": true})
}

fn luka() -> Value {
    json!({"id": LUKA, "is_bot": false, "first_name": "Luka", "language_code": "pt-br"})
}

fn bot() -> Value {
    json!({"id": BOT, "is_bot": true, "first_name": "ld e2e", "username": "ld_e2e_bot"})
}

/// A mensagem de serviço que abriu o tópico. O id dela é o `message_thread_id` de todas as
/// outras, e é a ela que toda mensagem do tópico responde.
fn raiz(id: i32, nome: &str) -> Value {
    json!({
        "message_id": id, "date": agora(), "chat": chat_json(), "from": bot(),
        "message_thread_id": id, "is_topic_message": true,
        "forum_topic_created": {"name": nome, "icon_color": 7322096}
    })
}

/// O texto que aparece na tela para um `parse_mode = HTML`: sem as tags, com as entidades
/// desfeitas.
fn visivel(html: &str) -> String {
    let mut s = String::with_capacity(html.len());
    let mut dentro = false;
    for c in html.chars() {
        match c {
            '<' => dentro = true,
            '>' if dentro => dentro = false,
            _ if !dentro => s.push(c),
            _ => {}
        }
    }
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn botoes_de(markup: &Value) -> Vec<(String, String)> {
    markup["inline_keyboard"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|linha| linha.as_array().cloned().unwrap_or_default())
        .filter_map(|b| {
            Some((
                b["text"].as_str()?.to_string(),
                b["callback_data"].as_str()?.to_string(),
            ))
        })
        .collect()
}

impl Chat {
    fn nome_do_topico(&self, id: i32) -> String {
        self.topicos
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.nome.clone())
            .unwrap_or_default()
    }

    /// A mensagem do bot no formato do Bot API.
    fn json_da(&self, m: &Mensagem) -> Value {
        let mut v = json!({
            "message_id": m.id, "date": agora(), "chat": chat_json(), "from": bot(),
            "text": m.texto,
        });
        if let Some(t) = m.topico {
            v["message_thread_id"] = json!(t);
            v["is_topic_message"] = json!(true);
            v["reply_to_message"] = raiz(t, &self.nome_do_topico(t));
        }
        if !m.botoes.is_empty() {
            let teclado: Vec<Value> = m
                .botoes
                .iter()
                .map(|(r, d)| json!([{"text": r, "callback_data": d}]))
                .collect();
            v["reply_markup"] = json!({ "inline_keyboard": teclado });
        }
        v
    }

    fn texto_do_pedido(p: &Value) -> String {
        let bruto = p["text"].as_str().unwrap_or_default();
        if p["parse_mode"].as_str() == Some("HTML") {
            visivel(bruto)
        } else {
            bruto.to_string()
        }
    }

    /// Responde a um método. `Err` é um erro do Bot API (`ok: false`).
    ///
    /// O nome do método vem como o cliente escreveu (o teloxide manda `SendMessage`), e o
    /// Telegram não diferencia maiúsculas nele.
    fn trata(&mut self, metodo: &str, p: &Value) -> Result<Value, String> {
        self.chamadas.push(metodo.to_string());
        match metodo.to_ascii_lowercase().as_str() {
            "getme" => {
                let mut eu = bot();
                for campo in [
                    "can_join_groups",
                    "can_read_all_group_messages",
                    "supports_inline_queries",
                    "can_connect_to_business",
                    "has_main_web_app",
                ] {
                    eu[campo] = json!(campo == "can_join_groups");
                }
                Ok(eu)
            }
            "getchat" => {
                if p["chat_id"].as_i64() != Some(CHAT) {
                    return Err("Bad Request: chat not found".into());
                }
                let mut c = chat_json();
                c["accent_color_id"] = json!(0);
                c["max_reaction_count"] = json!(11);
                c["accepted_gift_types"] = json!({
                    "unlimited_gifts": false, "limited_gifts": false,
                    "unique_gifts": false, "premium_subscription": false
                });
                Ok(c)
            }
            "sendmessage" => {
                let topico = p["message_thread_id"].as_i64().map(|t| t as i32);
                if let Some(t) = topico
                    && self.topicos.iter().any(|x| x.id == t && x.apagado)
                {
                    return Err("Bad Request: message thread not found".into());
                }
                let id = self.nova_msg();
                let m = Mensagem {
                    id,
                    topico,
                    texto: Self::texto_do_pedido(p),
                    botoes: botoes_de(&p["reply_markup"]),
                    apagada: false,
                };
                let v = self.json_da(&m);
                self.mensagens.push(m);
                Ok(v)
            }
            "editmessagetext" => {
                let id = p["message_id"].as_i64().unwrap_or_default() as i32;
                let texto = Self::texto_do_pedido(p);
                let botoes = botoes_de(&p["reply_markup"]);
                let Some(m) = self.mensagens.iter_mut().find(|m| m.id == id && !m.apagada) else {
                    return Err("Bad Request: message to edit not found".into());
                };
                if m.texto == texto && m.botoes == botoes {
                    return Err("Bad Request: message is not modified".into());
                }
                m.texto = texto;
                m.botoes = botoes;
                let m = m.clone();
                Ok(self.json_da(&m))
            }
            "deletemessage" => {
                let id = p["message_id"].as_i64().unwrap_or_default() as i32;
                match self.mensagens.iter_mut().find(|m| m.id == id && !m.apagada) {
                    Some(m) => {
                        m.apagada = true;
                        Ok(json!(true))
                    }
                    None => Err("Bad Request: message to delete not found".into()),
                }
            }
            "createforumtopic" => {
                let nome = p["name"].as_str().unwrap_or_default().to_string();
                let id = self.nova_msg();
                self.topicos.push(Topico {
                    id,
                    nome: nome.clone(),
                    apagado: false,
                });
                Ok(json!({"message_thread_id": id, "name": nome, "icon_color": 7322096}))
            }
            "deleteforumtopic" => {
                let id = p["message_thread_id"].as_i64().unwrap_or_default() as i32;
                let Some(t) = self.topicos.iter_mut().find(|t| t.id == id && !t.apagado) else {
                    return Err("Bad Request: TOPIC_ID_INVALID".into());
                };
                t.apagado = true;
                for m in self.mensagens.iter_mut().filter(|m| m.topico == Some(id)) {
                    m.apagada = true;
                }
                Ok(json!(true))
            }
            // pinChatMessage, answerCallbackQuery e o que mais aparecer.
            _ => Ok(json!(true)),
        }
    }
}

impl BotApi {
    pub async fn sobe() -> Self {
        let ouvinte = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", ouvinte.local_addr().unwrap());
        let chat = Arc::new(Mutex::new(Chat::default()));
        let estado = chat.clone();
        tokio::spawn(async move {
            loop {
                let Ok((conexao, _)) = ouvinte.accept().await else {
                    return;
                };
                let estado = estado.clone();
                tokio::spawn(async move {
                    let servico = hyper::service::service_fn(move |req| {
                        let estado = estado.clone();
                        async move { Ok::<_, Infallible>(atende(estado, req).await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(conexao), servico)
                        .await;
                });
            }
        });
        Self { chat, url }
    }

    pub fn le<T>(&self, f: impl FnOnce(&Chat) -> T) -> T {
        f(&self.chat.lock().unwrap())
    }

    /// Luka escreve no General.
    pub fn no_principal(&self, texto: &str) {
        let mut c = self.chat.lock().unwrap();
        let id = c.nova_msg();
        let msg = mensagem_do_luka(id, texto);
        c.enfileira(json!({ "message": msg }));
    }

    /// Luka escreve num tópico. Como no Telegram, a mensagem responde à raiz do tópico.
    pub fn no_topico(&self, topico: i32, texto: &str) {
        let mut c = self.chat.lock().unwrap();
        let id = c.nova_msg();
        let mut msg = mensagem_do_luka(id, texto);
        msg["message_thread_id"] = json!(topico);
        msg["is_topic_message"] = json!(true);
        msg["reply_to_message"] = raiz(topico, &c.nome_do_topico(topico));
        c.enfileira(json!({ "message": msg }));
    }

    /// Luka toca o botão com `rotulo` na mensagem mais nova que o tem. Devolve `false` se não há
    /// botão assim na tela.
    pub fn toca(&self, rotulo: &str) -> bool {
        let mut c = self.chat.lock().unwrap();
        let Some((m, dado)) = c
            .mensagens
            .iter()
            .rev()
            .filter(|m| !m.apagada)
            .find_map(|m| {
                m.botoes
                    .iter()
                    .find(|(r, _)| r.contains(rotulo))
                    .map(|(_, d)| (m.clone(), d.clone()))
            })
        else {
            return false;
        };
        c.proximo_toque += 1;
        let toque = json!({
            "id": format!("{}", 900000 + c.proximo_toque),
            "from": luka(),
            "chat_instance": "-5000000000000000001",
            "message": c.json_da(&m),
            "data": dado,
        });
        c.enfileira(json!({ "callback_query": toque }));
        true
    }
}

fn mensagem_do_luka(id: i32, texto: &str) -> Value {
    let mut msg = json!({
        "message_id": id, "date": agora(), "chat": chat_json(), "from": luka(), "text": texto
    });
    if texto.starts_with('/') {
        let comando = texto.split_whitespace().next().unwrap_or(texto);
        msg["entities"] = json!([{
            "type": "bot_command", "offset": 0, "length": comando.encode_utf16().count()
        }]);
    }
    msg
}

async fn atende(chat: Arc<Mutex<Chat>>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let caminho = req.uri().path().to_string();
    let corpo = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    let params: Value = serde_json::from_slice(&corpo).unwrap_or(json!({}));

    // `/bot<token>/<método>`. O download de arquivo (`/file/bot<token>/...`) não é usado aqui.
    let mut partes = caminho.trim_start_matches('/').splitn(2, '/');
    let dono = partes.next().unwrap_or_default();
    let metodo = partes.next().unwrap_or_default().to_string();

    let resposta = if dono != format!("bot{TOKEN}") {
        Err("Unauthorized".to_string())
    } else if metodo.eq_ignore_ascii_case("getUpdates") {
        Ok(get_updates(&chat, &params).await)
    } else {
        chat.lock().unwrap().trata(&metodo, &params)
    };

    let (status, corpo) = match resposta {
        Ok(result) => (200, json!({ "ok": true, "result": result })),
        Err(d) => {
            let codigo = if d == "Unauthorized" { 401 } else { 400 };
            (
                codigo,
                json!({ "ok": false, "error_code": codigo, "description": d }),
            )
        }
    };
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(corpo.to_string())))
        .unwrap()
}

/// O long polling: devolve o que está depois do `offset`, ou espera novidade até o `timeout`
/// pedido (com teto de 2 s, para o teste não ficar preso a um poll de 25 s ao terminar).
///
/// Como no Telegram, pedir um `offset` confirma tudo o que veio antes dele, para qualquer um que
/// pergunte depois: um daemon que reinicia e começa do zero não recebe de novo o que o anterior
/// já tratou.
async fn get_updates(chat: &Mutex<Chat>, p: &Value) -> Value {
    let offset = {
        let mut c = chat.lock().unwrap();
        c.confirmado = c.confirmado.max(p["offset"].as_i64().unwrap_or(0));
        c.confirmado
    };
    let prazo = Duration::from_secs(p["timeout"].as_u64().unwrap_or(0).min(2));
    let fim = Instant::now() + prazo;
    loop {
        let pendentes: Vec<Value> = chat
            .lock()
            .unwrap()
            .updates
            .iter()
            .filter(|u| u["update_id"].as_i64().unwrap_or_default() >= offset)
            .cloned()
            .collect();
        if !pendentes.is_empty() || Instant::now() >= fim {
            return json!(pendentes);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
