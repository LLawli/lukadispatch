//! Camada fina sobre a API do Telegram: tópicos, envio, edição e o formato das mensagens.
//!
//! Duas regras de formatação, e elas existem por motivo prático, não estético:
//!
//! - **Texto do agente vai sem `parse_mode`.** A resposta do Claude tem crase, underscore,
//!   asterisco e colchete o tempo todo; em MarkdownV2 isso vira erro 400 da API e a mensagem
//!   simplesmente não chega. Texto puro sempre chega.
//! - **O que o daemon escreve (status, painel, cards) vai em HTML**, com escape nosso, porque aí
//!   o conteúdo é controlado e negrito ajuda a ler no celular.

use anyhow::{Context, Result};
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, LinkPreviewOptions, MessageId, ParseMode, ThreadId,
};

/// Teto de uma mensagem do Telegram. O nosso corte é um pouco abaixo para caber o rodapé de
/// continuação sem estourar na virada.
const LIMITE_MSG: usize = 3900;

#[derive(Clone)]
pub struct Tg {
    bot: Bot,
    chat: ChatId,
}

impl Tg {
    pub fn new(token: String, chat_id: i64) -> Self {
        Self {
            bot: Bot::new(token),
            chat: ChatId(chat_id),
        }
    }

    pub fn bot(&self) -> &Bot {
        &self.bot
    }

    pub fn chat(&self) -> ChatId {
        self.chat
    }

    /// Confere que dá para trabalhar: token válido e o chat alcançável. Chamado na partida para
    /// o erro aparecer no log do serviço, e não seis horas depois na primeira mensagem.
    pub async fn preflight(&self) -> Result<String> {
        let eu = self.bot.get_me().await.context("token do bot recusado")?;
        self.bot
            .get_chat(self.chat)
            .await
            .with_context(|| format!("não alcancei o chat {}", self.chat))?;
        Ok(eu.username().to_string())
    }

    pub async fn create_topic(&self, nome: &str) -> Result<i32> {
        let t = self
            .bot
            .create_forum_topic(self.chat, nome)
            .await
            .context("criando tópico (o bot é admin com 'Gerenciar tópicos'?)")?;
        Ok(t.thread_id.0.0)
    }

    pub async fn delete_topic(&self, topic: i32) -> Result<()> {
        self.bot
            .delete_forum_topic(self.chat, ThreadId(MessageId(topic)))
            .await
            .context("apagando tópico")?;
        Ok(())
    }

    /// Envia texto puro num tópico, quebrando no limite do Telegram. Devolve o id da última
    /// mensagem (é a que interessa para editar depois).
    pub async fn send(&self, topic: Option<i32>, texto: &str) -> Result<MessageId> {
        let mut ultimo = None;
        for pedaco in split(texto, LIMITE_MSG) {
            let mut req = self.bot.send_message(self.chat, pedaco);
            req.link_preview_options = Some(sem_preview());
            if let Some(t) = topic {
                req.message_thread_id = Some(ThreadId(MessageId(t)));
            }
            ultimo = Some(req.await.context("enviando mensagem")?.id);
        }
        ultimo.context("mensagem vazia não é enviável")
    }

    pub async fn send_html(&self, topic: Option<i32>, html: &str) -> Result<MessageId> {
        let mut req = self.bot.send_message(self.chat, html);
        req.parse_mode = Some(ParseMode::Html);
        req.link_preview_options = Some(sem_preview());
        if let Some(t) = topic {
            req.message_thread_id = Some(ThreadId(MessageId(t)));
        }
        Ok(req.await.context("enviando html")?.id)
    }

    pub async fn send_keyboard(
        &self,
        topic: Option<i32>,
        html: &str,
        teclado: InlineKeyboardMarkup,
    ) -> Result<MessageId> {
        let mut req = self.bot.send_message(self.chat, html);
        req.parse_mode = Some(ParseMode::Html);
        req.reply_markup = Some(teclado.into());
        req.link_preview_options = Some(sem_preview());
        if let Some(t) = topic {
            req.message_thread_id = Some(ThreadId(MessageId(t)));
        }
        Ok(req.await.context("enviando teclado")?.id)
    }

    /// Edita uma mensagem em HTML. Um "não mudou nada" do Telegram não é erro para nós: é o caso
    /// comum do status que recalculou igual, e tratá-lo como falha encheria o log de ruído.
    pub async fn edit_html(&self, id: MessageId, html: &str) -> Result<()> {
        let mut req = self.bot.edit_message_text(self.chat, id, html);
        req.parse_mode = Some(ParseMode::Html);
        req.link_preview_options = Some(sem_preview());
        match req.await {
            Ok(_) => Ok(()),
            Err(e) if mensagem_nao_mudou(&e) => Ok(()),
            Err(e) => Err(e).context("editando mensagem"),
        }
    }

    pub async fn edit_keyboard(
        &self,
        id: MessageId,
        html: &str,
        teclado: InlineKeyboardMarkup,
    ) -> Result<()> {
        let mut req = self.bot.edit_message_text(self.chat, id, html);
        req.parse_mode = Some(ParseMode::Html);
        req.reply_markup = Some(teclado);
        req.link_preview_options = Some(sem_preview());
        match req.await {
            Ok(_) => Ok(()),
            Err(e) if mensagem_nao_mudou(&e) => Ok(()),
            Err(e) => Err(e).context("editando teclado"),
        }
    }

    /// Apaga best-effort: mensagem já apagada, velha demais ou de tópico que sumiu não é
    /// problema nosso.
    pub async fn delete(&self, id: MessageId) {
        let _ = self.bot.delete_message(self.chat, id).await;
    }

    pub async fn pin(&self, id: MessageId) {
        let _ = self.bot.pin_chat_message(self.chat, id).await;
    }
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

/// Escapa o que o Telegram trata como marcação em HTML.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Quebra o texto em pedaços que cabem numa mensagem, preferindo cortar em quebra de linha para
/// não picar código no meio.
pub fn split(texto: &str, limite: usize) -> Vec<String> {
    if texto.chars().count() <= limite {
        return vec![texto.to_string()];
    }
    let mut pedacos = Vec::new();
    let mut atual = String::new();
    for linha in texto.split_inclusive('\n') {
        if atual.chars().count() + linha.chars().count() > limite {
            if !atual.is_empty() {
                pedacos.push(std::mem::take(&mut atual));
            }
            // Linha única maior que o limite (log gigante, base64): corta no braço.
            let mut resto: Vec<char> = linha.chars().collect();
            while resto.len() > limite {
                let cabeca: String = resto.drain(..limite).collect();
                pedacos.push(cabeca);
            }
            atual = resto.into_iter().collect();
        } else {
            atual.push_str(linha);
        }
    }
    if !atual.is_empty() {
        pedacos.push(atual);
    }
    pedacos
}

/// Teclado de uma coluna: no celular, botão largo é mais fácil de acertar que grade.
pub fn coluna(botoes: Vec<(String, String)>) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(
        botoes
            .into_iter()
            .map(|(texto, dado)| vec![InlineKeyboardButton::callback(texto, dado)]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texto_curto_nao_e_quebrado() {
        assert_eq!(split("oi", 10), vec!["oi"]);
    }

    #[test]
    fn quebra_em_linha_quando_da() {
        let t = "aaaa\nbbbb\ncccc\n";
        let p = split(t, 10);
        assert!(p.len() > 1);
        assert!(p.iter().all(|x| x.chars().count() <= 10));
        assert_eq!(
            p.concat(),
            t,
            "quebrar não pode perder nem inventar caractere"
        );
    }

    #[test]
    fn linha_gigante_e_cortada_no_braco() {
        let t = "x".repeat(25);
        let p = split(&t, 10);
        assert_eq!(p.len(), 3);
        assert_eq!(p.concat(), t);
    }

    #[test]
    fn escape_cobre_os_tres_de_html() {
        assert_eq!(escape_html("a<b>&c"), "a&lt;b&gt;&amp;c");
    }

    #[test]
    fn escape_do_e_comercial_vem_primeiro() {
        // Se o & fosse trocado depois, "&lt;" viraria "&amp;lt;" e o usuário veria o escape cru.
        assert_eq!(escape_html("<"), "&lt;");
    }
}
