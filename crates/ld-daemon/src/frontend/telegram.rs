//! O Telegram como [`Frontend`]: supergrupo com tópicos, um tópico por sessão, o General como
//! painel.
//!
//! Tudo que é Telegram mora aqui e só aqui: o teloxide, o long polling, a allowlist por id
//! numérico, o `answer_callback_query`, o reply automático do fórum, os tetos do Bot API e a
//! tradução dos ids (tópico é `ThreadId`, mensagem é `MessageId`, os dois viram texto opaco).
//!
//! Polling e não webhook: webhook exigiria porta aberta, TLS e um domínio, e o daemon roda numa
//! máquina doméstica atrás de NAT. Polling só precisa de saída para a internet.
//!
//! A marcação do domínio ([`super::formato`]) é o HTML que o Telegram aceita, então o texto rico
//! passa direto com `parse_mode = Html`. Texto puro (a resposta do agente) vai sem `parse_mode`:
//! em MarkdownV2 a crase e o sublinhado da resposta do Claude virariam erro 400 e a mensagem não
//! chegaria.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, InputFile, LinkPreviewOptions, MessageId,
    ParseMode, ThreadId, Update, UpdateKind,
};
use tokio::sync::mpsc;
use tracing::warn;

use super::{
    Anexo, Autor, Botao, Canal, Evento, Frontend, Limites, Midia, MsgId, Resolvido, TipoAnexo,
};

/// Quantos segundos o Telegram segura o `getUpdates` sem novidade.
pub const PRAZO_POLL: u32 = 25;

pub struct Telegram {
    bot: Bot,
    chat: ChatId,
    /// Só estes ids viram [`Evento`]. Vazia nega todo mundo.
    permitidos: Vec<i64>,
}

impl Telegram {
    /// `permitidos` é a allowlist: só estes ids viram [`Evento`]. Vazia nega todo mundo.
    pub fn new(token: String, chat_id: i64, permitidos: Vec<i64>) -> Self {
        Self {
            bot: bot(token),
            chat: ChatId(chat_id),
            permitidos,
        }
    }
}

/// O `Bot` com um cliente HTTP que aguenta o long polling. O setup usa o mesmo.
///
/// O cliente padrão do teloxide tem timeout de 17s, e o `getUpdates` deste projeto pede ao
/// Telegram para segurar a conexão por PRAZO_POLL. Com o padrão, toda janela ociosa morria em
/// erro de rede e reabria: o update não se perdia (o Telegram reenvia), mas o log virava um
/// aviso a cada 20s e cada mensagem podia atrasar alguns segundos.
pub fn bot(token: String) -> Bot {
    let cliente = teloxide::net::default_reqwest_settings()
        .timeout(Duration::from_secs(PRAZO_POLL as u64 + 30))
        .build()
        .expect("cliente http do teloxide");
    Bot::with_client(token, cliente)
}

/// Traduz um update do Telegram num [`Evento`], ou descarta.
///
/// Descarta, sem ler o conteúdo: remetente fora de `permitidos`, bot (inclusive a mensagem de
/// serviço que o próprio bot gera ao criar tópico), chat diferente de `chat`, e mensagem sem
/// texto nem anexo. Tira o reply automático do fórum de `responde_a`.
///
/// Função pura de propósito: é a fronteira de segurança do daemon, e precisa ser testável sem
/// rede.
pub fn traduz(u: &Update, chat: i64, permitidos: &[i64]) -> Option<Evento> {
    match &u.kind {
        UpdateKind::Message(msg) => {
            let quem = msg.from.as_ref()?;
            if quem.is_bot {
                return None;
            }
            let autor = quem.id.0 as i64;
            if !permitidos.contains(&autor) {
                return None;
            }
            if msg.chat.id != ChatId(chat) {
                return None;
            }

            let texto = msg.text().or_else(|| msg.caption()).unwrap_or("");
            let anexos = anexos_da_mensagem(msg);
            if texto.is_empty() && anexos.is_empty() {
                // Mensagem de serviço (criar tópico, por exemplo): não é conversa.
                return None;
            }

            let canal = msg.thread_id.map(|t| Canal::new(t.0.0.to_string()));
            let topico = msg.thread_id.map(|t| t.0.0);
            let responde_a = topico.and_then(|t| {
                filtra_reply(msg.reply_to_message().map(|r| r.id), t)
                    .map(|id| MsgId::new(id.0.to_string()))
            });

            Some(Evento::Mensagem {
                autor: Autor {
                    id: quem.id.0.to_string(),
                    nome: quem.first_name.clone(),
                },
                canal,
                msg: MsgId::new(msg.id.0.to_string()),
                texto: texto.to_string(),
                responde_a,
                anexos,
            })
        }
        UpdateKind::CallbackQuery(q) => {
            let autor = q.from.id.0 as i64;
            if !permitidos.contains(&autor) {
                return None;
            }
            // O teclado precisa ser deste chat. A allowlist já barra estranhos, mas o bot pode
            // estar em outro grupo onde a mesma pessoa toca num botão velho, e esse toque não
            // pode resolver card de sessão nenhuma daqui.
            let regular = q.message.as_ref().and_then(|m| m.regular_message());
            if regular.is_some_and(|m| m.chat.id != ChatId(chat)) {
                return None;
            }
            let dado = q.data.clone()?;
            let canal = q
                .message
                .as_ref()
                .and_then(|m| m.regular_message())
                .and_then(|m| m.thread_id)
                .map(|t| Canal::new(t.0.0.to_string()));
            let msg = q.message.as_ref().map(|m| MsgId::new(m.id().0.to_string()));
            Some(Evento::Toque {
                autor: Autor {
                    id: q.from.id.0.to_string(),
                    nome: q.from.first_name.clone(),
                },
                canal,
                msg,
                dado,
            })
        }
        _ => None,
    }
}

/// A qual mensagem a pessoa respondeu de verdade, se respondeu a alguma.
///
/// Num fórum do Telegram, TODA mensagem dentro de um tópico vem com `reply_to_message`
/// preenchido, apontando para a mensagem que abriu o tópico, e o id dessa mensagem é o próprio
/// `thread_id`. Ou seja, "respondeu a alguma coisa" é verdadeiro sempre, e usar isso direto faz
/// texto solto se passar por resposta.
///
/// Custou um bug silencioso: a guarda de pendência nunca disparava e a correção por reply nunca
/// casava, porque as duas liam esse campo sem descontar o reply automático do tópico.
fn filtra_reply(alvo: Option<MessageId>, topic: i32) -> Option<MessageId> {
    alvo.filter(|id| id.0 != topic)
}

/// Os anexos de uma mensagem, na ordem de prioridade do Telegram: voz antes de áudio (quem grava
/// segurando o microfone manda `voice`, e é esse o caso comum), depois documento, foto (a maior,
/// já que a ordem das resoluções não é garantida pela API), vídeo, animação, nota de vídeo e
/// figurinha.
fn anexos_da_mensagem(msg: &teloxide::types::Message) -> Vec<Anexo> {
    if let Some(v) = msg.voice() {
        return vec![Anexo {
            id: v.file.id.to_string(),
            tamanho: v.file.size as u64,
            nome: None,
            tipo: TipoAnexo::Voz,
        }];
    }
    if let Some(a) = msg.audio() {
        return vec![Anexo {
            id: a.file.id.to_string(),
            tamanho: a.file.size as u64,
            nome: a.file_name.clone(),
            tipo: TipoAnexo::Audio,
        }];
    }
    if let Some(d) = msg.document() {
        return vec![Anexo {
            id: d.file.id.to_string(),
            tamanho: d.file.size as u64,
            nome: d.file_name.clone(),
            tipo: TipoAnexo::Documento,
        }];
    }
    if let Some(tamanhos) = msg.photo() {
        if let Some(maior) = tamanhos.iter().max_by_key(|p| p.file.size) {
            return vec![Anexo {
                id: maior.file.id.to_string(),
                tamanho: maior.file.size as u64,
                nome: None,
                tipo: TipoAnexo::Foto,
            }];
        }
        return vec![];
    }
    if let Some(v) = msg.video() {
        return vec![Anexo {
            id: v.file.id.to_string(),
            tamanho: v.file.size as u64,
            nome: v.file_name.clone(),
            tipo: TipoAnexo::Video,
        }];
    }
    if let Some(a) = msg.animation() {
        return vec![Anexo {
            id: a.file.id.to_string(),
            tamanho: a.file.size as u64,
            nome: a.file_name.clone(),
            tipo: TipoAnexo::Animacao,
        }];
    }
    if let Some(v) = msg.video_note() {
        return vec![Anexo {
            id: v.file.id.to_string(),
            tamanho: v.file.size as u64,
            nome: None,
            tipo: TipoAnexo::NotaDeVideo,
        }];
    }
    if let Some(s) = msg.sticker() {
        return vec![Anexo {
            id: s.file.id.to_string(),
            tamanho: s.file.size as u64,
            nome: None,
            tipo: TipoAnexo::Figurinha,
        }];
    }
    vec![]
}

/// Botões em uma coluna, cada um com o `dado` como `callback_data`. No celular, botão largo é
/// mais fácil de acertar que grade.
pub fn teclado(botoes: &[Botao]) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(botoes.iter().map(|b| {
        vec![InlineKeyboardButton::callback(
            b.rotulo.clone(),
            b.dado.clone(),
        )]
    }))
}

/// O tópico que um [`Canal`] representa. Canal que não é número (de outro frontend, ou do nulo)
/// é erro, e não pânico: pode ter sobrado no banco de uma execução anterior.
pub(crate) fn thread(canal: &Canal) -> Result<ThreadId> {
    let n: i32 = canal
        .as_str()
        .parse()
        .with_context(|| format!("canal '{}' não é um tópico do Telegram", canal.as_str()))?;
    Ok(ThreadId(MessageId(n)))
}

/// A mensagem que um [`MsgId`] representa. Mesma regra de [`thread`].
pub(crate) fn mensagem(msg: &MsgId) -> Result<MessageId> {
    let n: i32 = msg
        .as_str()
        .parse()
        .with_context(|| format!("mensagem '{}' não é do Telegram", msg.as_str()))?;
    Ok(MessageId(n))
}

fn sem_preview() -> LinkPreviewOptions {
    // Sem isto, qualquer URL no meio de uma resposta vira um cartão gigante no celular.
    LinkPreviewOptions {
        is_disabled: true,
        url: None,
        prefer_small_media: false,
        prefer_large_media: false,
        show_above_text: false,
    }
}

fn mensagem_nao_mudou(e: &teloxide::RequestError) -> bool {
    matches!(e, teloxide::RequestError::Api(api) if format!("{api:?}").contains("MessageNotModified"))
        || format!("{e}").contains("message is not modified")
}

#[async_trait]
impl Frontend for Telegram {
    fn nome(&self) -> &'static str {
        "telegram"
    }

    fn limites(&self) -> Limites {
        Limites::default()
    }

    fn plataforma(&self) -> &'static str {
        "Telegram"
    }

    fn onde(&self, nome_do_canal: &str) -> String {
        format!("o tópico \"{nome_do_canal}\" de um grupo do Telegram")
    }

    fn renderiza_markdown(&self) -> bool {
        // O texto do agente sai sem `parse_mode`, então Markdown aparece cru na tela: asterisco
        // e cerquilha ficam do jeito que foram escritos, em vez de virar negrito e cabeçalho.
        false
    }

    async fn confere(&self) -> Result<String> {
        let eu = self.bot.get_me().await.context("token do bot recusado")?;
        self.bot
            .get_chat(self.chat)
            .await
            .with_context(|| format!("não alcancei o chat {}", self.chat))?;
        Ok(eu.username().to_string())
    }

    async fn escuta(&self, saida: mpsc::UnboundedSender<Evento>) {
        let mut offset: i32 = 0;
        loop {
            let pedido = self
                .bot
                .get_updates()
                .offset(offset)
                .timeout(PRAZO_POLL)
                .await;

            let updates = match pedido {
                Ok(updates) => updates,
                Err(e) => {
                    // Internet caiu, Telegram fora do ar, 429: espera e volta. Derrubar o daemon
                    // por isso perderia as sessões vivas, que continuam rodando.
                    warn!(erro = %e, "getUpdates falhou; tento de novo em 3s");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };

            for u in updates {
                offset = (u.id.0 as i32).saturating_add(1);

                // Responder o toque ANTES de traduzir: senão o botão fica "rodando" no celular
                // enquanto o resto do daemon processa o evento.
                if let UpdateKind::CallbackQuery(q) = &u.kind
                    && self.permitidos.contains(&(q.from.id.0 as i64))
                {
                    let _ = self.bot.answer_callback_query(q.id.clone()).await;
                }

                let Some(evento) = traduz(&u, self.chat.0, &self.permitidos) else {
                    continue;
                };
                if saida.send(evento).is_err() {
                    // Quem lia fechou: não há mais para onde entregar.
                    return;
                }
            }
        }
    }

    async fn cria_canal(&self, nome: &str) -> Result<Canal> {
        let t = self
            .bot
            .create_forum_topic(self.chat, nome)
            .await
            .context("criando tópico (o bot é admin com 'Gerenciar tópicos'?)")?;
        Ok(Canal::new(t.thread_id.0.0.to_string()))
    }

    async fn apaga_canal(&self, canal: &Canal) -> Resolvido {
        let Ok(t) = thread(canal) else {
            // Canal de outro frontend (ou sobra do nulo): não é nosso para apagar, e não é erro.
            return Resolvido::JaNaoExiste;
        };
        match self.bot.delete_forum_topic(self.chat, t).await {
            Ok(_) => Resolvido::Apagado,
            Err(teloxide::RequestError::Api(_)) => Resolvido::JaNaoExiste,
            Err(_) => Resolvido::TenteDepois,
        }
    }

    async fn envia_texto(&self, canal: Option<&Canal>, texto: &str) -> Result<MsgId> {
        let mut ultimo = None;
        for pedaco in super::formato::quebra(texto, self.limites().mensagem) {
            let mut req = self.bot.send_message(self.chat, pedaco);
            req.link_preview_options = Some(sem_preview());
            if let Some(c) = canal {
                req.message_thread_id = Some(thread(c)?);
            }
            ultimo = Some(req.await.context("enviando mensagem")?.id);
        }
        ultimo
            .map(|id| MsgId::new(id.0.to_string()))
            .context("mensagem vazia não é enviável")
    }

    async fn envia(
        &self,
        canal: Option<&Canal>,
        rico: &str,
        botoes: &[Botao],
        responde_a: Option<&MsgId>,
    ) -> Result<MsgId> {
        let mut req = self.bot.send_message(self.chat, rico);
        req.parse_mode = Some(ParseMode::Html);
        req.link_preview_options = Some(sem_preview());
        if !botoes.is_empty() {
            req.reply_markup = Some(teclado(botoes).into());
        }
        if let Some(c) = canal {
            req.message_thread_id = Some(thread(c)?);
        }
        if let Some(alvo) = responde_a {
            // `allow_sending_without_reply`: se a mensagem respondida já sumiu (foi apagada, por
            // exemplo), a resposta ainda precisa sair, senão ela some sem explicação nenhuma.
            req.reply_parameters = Some(teloxide::types::ReplyParameters {
                message_id: mensagem(alvo)?,
                chat_id: None,
                allow_sending_without_reply: Some(true),
                quote: None,
                quote_parse_mode: None,
                quote_entities: None,
                quote_position: None,
            });
        }
        Ok(MsgId::new(
            req.await.context("enviando mensagem")?.id.0.to_string(),
        ))
    }

    async fn edita(&self, msg: &MsgId, rico: &str, botoes: &[Botao]) -> Result<()> {
        let id = mensagem(msg)?;
        let mut req = self.bot.edit_message_text(self.chat, id, rico);
        req.parse_mode = Some(ParseMode::Html);
        req.link_preview_options = Some(sem_preview());
        // Vazio tira os botões: o Telegram não distingue "sem markup" de "markup vazio" aqui, e
        // mandar o teclado vazio é o jeito de limpar o que já estava lá.
        req.reply_markup = Some(teclado(botoes));
        match req.await {
            Ok(_) => Ok(()),
            Err(e) if mensagem_nao_mudou(&e) => Ok(()),
            Err(e) => Err(e).context("editando mensagem"),
        }
    }

    async fn apaga(&self, msg: &MsgId) {
        // Best-effort: id inválido ou mensagem que já sumiu não é problema de ninguém.
        if let Ok(id) = mensagem(msg) {
            let _ = self.bot.delete_message(self.chat, id).await;
        }
    }

    async fn fixa(&self, msg: &MsgId) {
        if let Ok(id) = mensagem(msg) {
            let _ = self.bot.pin_chat_message(self.chat, id).await;
        }
    }

    async fn envia_arquivo(
        &self,
        canal: Option<&Canal>,
        caminho: &Path,
        legenda: Option<&str>,
        como: Midia,
    ) -> Result<MsgId> {
        let arquivo = InputFile::file(caminho.to_path_buf());
        let id = match como {
            Midia::Documento => {
                let mut req = self.bot.send_document(self.chat, arquivo);
                req.caption = legenda.map(str::to_string);
                if let Some(c) = canal {
                    req.message_thread_id = Some(thread(c)?);
                }
                req.await.context("enviando documento")?.id
            }
            Midia::Foto => {
                let mut req = self.bot.send_photo(self.chat, arquivo);
                req.caption = legenda.map(str::to_string);
                if let Some(c) = canal {
                    req.message_thread_id = Some(thread(c)?);
                }
                req.await.context("enviando foto")?.id
            }
            Midia::Video => {
                let mut req = self.bot.send_video(self.chat, arquivo);
                req.caption = legenda.map(str::to_string);
                // Sem isto o celular baixa o arquivo inteiro antes de mostrar o primeiro quadro.
                req.supports_streaming = Some(true);
                if let Some(c) = canal {
                    req.message_thread_id = Some(thread(c)?);
                }
                req.await.context("enviando vídeo")?.id
            }
        };
        Ok(MsgId::new(id.0.to_string()))
    }

    async fn baixa(&self, anexo: &Anexo, dir: &Path) -> Result<PathBuf> {
        if anexo.tamanho > self.limites().baixar {
            bail!(
                "{} tem {}, e o Bot API só entrega até 20 MB",
                anexo.tipo.nome(),
                humano(anexo.tamanho)
            );
        }

        let arquivo = com_retentativa("pedindo o arquivo ao Telegram", || {
            let bot = self.bot.clone();
            let id = teloxide::types::FileId(anexo.id.clone());
            async move { bot.get_file(id).await }
        })
        .await?;

        // Sem nome vindo da plataforma (foto, figurinha), o caminho do lado do Telegram é a
        // única fonte de extensão: ele vem como "photos/file_42.jpg".
        let bruto = anexo.nome.clone().unwrap_or_else(|| arquivo.path.clone());
        let destino = super::caminho_livre(dir, &bruto);

        let mut dst = tokio::fs::File::create(&destino)
            .await
            .with_context(|| format!("criando {}", destino.display()))?;
        if let Err(e) = self.bot.download_file(&arquivo.path, &mut dst).await {
            // Arquivo pela metade é pior que arquivo nenhum: quem lê abriria algo truncado sem
            // saber disso.
            let _ = tokio::fs::remove_file(&destino).await;
            return Err(e).context("baixando o arquivo");
        }

        // Código de saída zero não é prova de artefato: confere que chegou byte de verdade.
        let gravado = tokio::fs::metadata(&destino)
            .await
            .with_context(|| format!("conferindo {}", destino.display()))?
            .len();
        if gravado == 0 {
            let _ = tokio::fs::remove_file(&destino).await;
            bail!("o download veio vazio");
        }

        Ok(destino)
    }
}

fn humano(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{} KB", bytes / 1024)
    }
}

/// Repete o pedido quando a rede falha, e só quando a rede falha.
///
/// Um timeout ao pedir o arquivo perdia o download inteiro: o arquivo ficava no Telegram, o erro
/// aparecia e não havia segunda chance. Só que repetir tudo também é errado: um "arquivo grande
/// demais" ou um file_id vencido são respostas definitivas da API, e insistir neles só atrasa o
/// aviso de que não vai dar.
async fn com_retentativa<T, F, Fut>(o_que: &str, mut tentar: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, teloxide::RequestError>>,
{
    const ESPERAS: [u64; 3] = [1, 3, 8];
    for (n, espera) in ESPERAS.iter().enumerate() {
        match tentar().await {
            Ok(v) => return Ok(v),
            Err(e) if transitorio(&e) => {
                warn!(tentativa = n + 1, erro = %e, "{o_que}: a rede falhou, tento de novo em {espera}s");
                tokio::time::sleep(Duration::from_secs(*espera)).await;
            }
            Err(e) => return Err(e).context(o_que.to_string()),
        }
    }
    tentar()
        .await
        .with_context(|| format!("{o_que} (mesmo depois de {} tentativas)", ESPERAS.len() + 1))
}

/// Vale a pena tentar de novo? Rede e I/O sim; resposta da API não.
fn transitorio(e: &teloxide::RequestError) -> bool {
    matches!(
        e,
        teloxide::RequestError::Network(_)
            | teloxide::RequestError::Io(_)
            | teloxide::RequestError::RetryAfter(_)
    )
}

#[cfg(test)]
mod testes {
    //! A tradução de entrada, com payloads na forma que o Telegram manda de verdade: a raiz do
    //! tópico como `reply_to_message`, card com data real (`date: 0` o teloxide lê como mensagem
    //! inacessível) e o `Update` desserializado de texto (de `serde_json::Value` ele perde o
    //! tipo e cai em `UpdateKind::Error`).

    use super::*;
    use crate::frontend::{Autor, TipoAnexo};
    use serde_json::{Value, json};

    const CHAT: i64 = -100123;
    const LUKA: i64 = 42;
    const TOPICO: i32 = 630;

    fn chat(id: i64) -> Value {
        json!({"id": id, "type": "supergroup", "title": "ld", "is_forum": true})
    }

    fn pessoa(id: i64, bot: bool) -> Value {
        json!({"id": id, "is_bot": bot, "first_name": if bot { "ld_bot" } else { "Luka" }})
    }

    /// A mensagem que abriu o tópico. O id dela É o `message_thread_id` de todas as outras.
    fn raiz_do_topico() -> Value {
        json!({
            "message_id": TOPICO, "date": 0, "chat": chat(CHAT), "from": pessoa(1, true),
            "message_thread_id": TOPICO, "is_topic_message": true,
            "forum_topic_created": {"name": "proj", "icon_color": 7322096}
        })
    }

    fn card(id: i32) -> Value {
        // `date: 0` é o jeito que o Bot API marca uma mensagem inacessível (vira
        // `MaybeInaccessibleMessage::Inaccessible`, sem tópico nem texto). Um card de verdade
        // tem data real; por isso 1 aqui, e não 0.
        json!({
            "message_id": id, "date": 1, "chat": chat(CHAT), "from": pessoa(1, true),
            "message_thread_id": TOPICO, "is_topic_message": true, "text": "card"
        })
    }

    /// Mensagem do Luka no chat certo, com `corpo` por cima dos campos base.
    fn update(corpo: Value) -> Update {
        let mut m = json!({
            "message_id": 900, "date": 0, "chat": chat(CHAT), "from": pessoa(LUKA, false)
        });
        for (k, v) in corpo.as_object().unwrap() {
            m[k] = v.clone();
        }
        // `Update` tem `#[serde(flatten)]` em cima de um `Deserialize` escrito à mão para
        // `UpdateKind`, que lê a chave via `deserialize_any`. Passando por `serde_json::Value`
        // (em vez de texto) esse combo perde a chave e cai no variante de erro vazio; textar e
        // reler como string é o mesmo update, só por um caminho que o teloxide 0.17 sabe ler.
        let texto = serde_json::to_string(&json!({"update_id": 1, "message": m})).unwrap();
        serde_json::from_str(&texto).expect("Update válido")
    }

    fn no_topico(mut corpo: Value) -> Update {
        corpo["message_thread_id"] = json!(TOPICO);
        corpo["is_topic_message"] = json!(true);
        if corpo.get("reply_to_message").is_none() {
            // É assim que o Telegram manda TODA mensagem de tópico: respondendo à raiz.
            corpo["reply_to_message"] = raiz_do_topico();
        }
        update(corpo)
    }

    fn luka() -> Autor {
        Autor {
            id: LUKA.to_string(),
            nome: "Luka".into(),
        }
    }

    fn traduz_luka(u: &Update) -> Option<Evento> {
        traduz(u, CHAT, &[LUKA])
    }

    #[test]
    fn comando_no_general_vira_mensagem_sem_canal() {
        assert_eq!(
            traduz_luka(&update(json!({"text": "/ls"}))),
            Some(Evento::Mensagem {
                autor: luka(),
                canal: None,
                msg: MsgId::new("900"),
                texto: "/ls".into(),
                responde_a: None,
                anexos: vec![],
            })
        );
    }

    #[test]
    fn reply_automatico_do_topico_nao_e_resposta() {
        // O furo que desligou a guarda de pendência e a correção por reply com o CI verde.
        let ev = traduz_luka(&no_topico(json!({"text": "oi"}))).expect("mensagem válida");
        let Evento::Mensagem {
            canal, responde_a, ..
        } = ev
        else {
            panic!("esperava mensagem");
        };
        assert_eq!(canal, Some(Canal::new(TOPICO.to_string())));
        assert_eq!(responde_a, None, "o reply à raiz do tópico vazou");
    }

    #[test]
    fn responder_a_um_card_e_resposta() {
        let ev = traduz_luka(&no_topico(
            json!({"text": "correção", "reply_to_message": card(700)}),
        ))
        .unwrap();
        let Evento::Mensagem { responde_a, .. } = ev else {
            panic!("esperava mensagem");
        };
        assert_eq!(responde_a, Some(MsgId::new("700")));
    }

    #[test]
    fn quem_nao_esta_na_allowlist_some_sem_ser_lido() {
        let u = update(json!({"text": "rm -rf", "from": pessoa(43, false)}));
        assert_eq!(traduz_luka(&u), None);
        assert_eq!(
            traduz(&update(json!({"text": "oi"})), CHAT, &[]),
            None,
            "allowlist vazia nega todo mundo"
        );
    }

    #[test]
    fn bot_e_outro_chat_somem() {
        assert_eq!(
            traduz_luka(&update(json!({"text": "x", "from": pessoa(LUKA, true)}))),
            None,
            "mensagem de bot"
        );
        assert_eq!(
            traduz_luka(&update(json!({"text": "x", "chat": chat(-100999)}))),
            None,
            "outro chat"
        );
    }

    #[test]
    fn mensagem_de_servico_do_topico_nao_vira_evento() {
        // Criar tópico gera uma mensagem de serviço sem texto; ela não é conversa.
        let u = update(json!({
            "from": pessoa(LUKA, false),
            "message_thread_id": TOPICO, "is_topic_message": true,
            "forum_topic_created": {"name": "proj", "icon_color": 7322096}
        }));
        assert_eq!(traduz_luka(&u), None);
    }

    #[test]
    fn voz_vira_anexo_de_voz_sem_nome() {
        let ev = traduz_luka(&no_topico(json!({"voice": {
            "file_id": "v1", "file_unique_id": "u1", "file_size": 12345,
            "duration": 7, "mime_type": "audio/ogg"
        }})))
        .unwrap();
        let Evento::Mensagem { texto, anexos, .. } = ev else {
            panic!("esperava mensagem");
        };
        assert_eq!(texto, "");
        assert_eq!(
            anexos,
            vec![Anexo {
                id: "v1".into(),
                tamanho: 12345,
                nome: None,
                tipo: TipoAnexo::Voz,
            }]
        );
    }

    #[test]
    fn foto_escolhe_a_maior_e_a_legenda_vira_texto() {
        let ev = traduz_luka(&no_topico(json!({
            "caption": "olha isso",
            "photo": [
                {"file_id": "grande", "file_unique_id": "g", "file_size": 90000, "width": 1280, "height": 960},
                {"file_id": "miniatura", "file_unique_id": "p", "file_size": 900, "width": 90, "height": 67}
            ]
        })))
        .unwrap();
        let Evento::Mensagem { texto, anexos, .. } = ev else {
            panic!("esperava mensagem");
        };
        assert_eq!(texto, "olha isso");
        assert_eq!(anexos.len(), 1);
        assert_eq!(anexos[0].id, "grande", "a ordem não é garantida pela API");
        assert_eq!(anexos[0].tipo, TipoAnexo::Foto);
    }

    #[test]
    fn documento_mantem_o_nome_que_veio() {
        let ev = traduz_luka(&no_topico(json!({"document": {
            "file_id": "d1", "file_unique_id": "u", "file_size": 10,
            "file_name": "nota.pdf", "mime_type": "application/pdf"
        }})))
        .unwrap();
        let Evento::Mensagem { anexos, .. } = ev else {
            panic!("esperava mensagem");
        };
        assert_eq!(anexos[0].nome.as_deref(), Some("nota.pdf"));
        assert_eq!(anexos[0].tipo, TipoAnexo::Documento);
    }

    fn toque(de: i64) -> Update {
        // Mesmo motivo do round-trip em `update`: `Update` só desserializa este formato direto
        // de texto.
        let texto = serde_json::to_string(&json!({"update_id": 2, "callback_query": {
            "id": "q1", "from": pessoa(de, false), "chat_instance": "ci",
            "data": "t:ok:1", "message": card(800)
        }}))
        .unwrap();
        serde_json::from_str(&texto).expect("Update de callback válido")
    }

    #[test]
    fn toque_em_botao_traz_canal_mensagem_e_dado() {
        assert_eq!(
            traduz_luka(&toque(LUKA)),
            Some(Evento::Toque {
                autor: luka(),
                canal: Some(Canal::new(TOPICO.to_string())),
                msg: Some(MsgId::new("800")),
                dado: "t:ok:1".into(),
            })
        );
    }

    #[test]
    fn toque_em_teclado_de_outro_chat_some() {
        let mut outro = card(800);
        outro["chat"] = chat(-100999);
        let u: Update = serde_json::from_str(
            &json!({"update_id": 3, "callback_query": {
                "id": "q2", "from": pessoa(LUKA, false), "chat_instance": "ci",
                "data": "t:ok:1", "message": outro
            }})
            .to_string(),
        )
        .expect("Update de callback válido");
        assert_eq!(traduz_luka(&u), None);
    }

    #[test]
    fn toque_de_estranho_some() {
        assert_eq!(traduz_luka(&toque(43)), None);
    }

    #[test]
    fn teclado_e_uma_coluna_com_o_dado_de_cada_botao() {
        let t = teclado(&[Botao::new("✅ Enviar", "t:ok:1"), Botao::new("🗑", "t:no:1")]);
        assert_eq!(t.inline_keyboard.len(), 2, "uma linha por botão");
        assert!(t.inline_keyboard.iter().all(|linha| linha.len() == 1));
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains("t:ok:1") && json.contains("t:no:1"), "{json}");
        assert!(
            teclado(&[]).inline_keyboard.is_empty(),
            "vazio tira os botões"
        );
    }

    #[test]
    fn ids_opacos_voltam_a_ser_ids_do_telegram() {
        assert_eq!(
            thread(&Canal::new("630")).unwrap(),
            ThreadId(MessageId(630))
        );
        assert_eq!(mensagem(&MsgId::new("900")).unwrap(), MessageId(900));
        assert!(thread(&Canal::new("nulo-abc")).is_err());
        assert!(mensagem(&MsgId::new("120363@g.us")).is_err());
    }

    #[test]
    fn se_apresenta_como_topico_de_grupo_sem_markdown() {
        let t = Telegram::new("0:falso".into(), CHAT, vec![LUKA]);
        assert_eq!(t.plataforma(), "Telegram");
        assert_eq!(t.onde("proj"), "o tópico \"proj\" de um grupo do Telegram");
        assert!(
            !t.renderiza_markdown(),
            "o texto do agente vai sem parse_mode: Markdown aparece cru"
        );
    }

    #[test]
    fn limites_sao_os_do_bot_api() {
        let t = Telegram::new("0:falso".into(), CHAT, vec![LUKA]);
        assert_eq!(t.limites(), Limites::default());
    }
}
