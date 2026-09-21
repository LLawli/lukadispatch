//! Camada fina sobre a API do Telegram: tópicos, envio, edição e o formato das mensagens.
//!
//! Duas regras de formatação, e elas existem por motivo prático, não estético:
//!
//! - **Texto do agente vai sem `parse_mode`.** A resposta do Claude tem crase, underscore,
//!   asterisco e colchete o tempo todo; em MarkdownV2 isso vira erro 400 da API e a mensagem
//!   simplesmente não chega. Texto puro sempre chega.
//! - **O que o daemon escreve (status, painel, cards) vai em HTML**, com escape nosso, porque aí
//!   o conteúdo é controlado e negrito ajuda a ler no celular.

use std::time::Duration;

use anyhow::{Context, Result};
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, InputFile, LinkPreviewOptions, MessageId,
    ParseMode, ThreadId,
};

/// Quantos segundos o Telegram segura o `getUpdates` sem novidade. Quanto maior, menos
/// requisições à toa; o teto prático é o timeout do cliente HTTP, que é ajustado a partir daqui.
pub const PRAZO_POLL: u32 = 25;

/// Quanto tempo uma resposta de comando fica no General antes de sumir.
pub const TTL_RESPOSTA: u64 = 25;
/// O teclado do /new espera a sua escolha, então vive mais que uma resposta comum.
pub const TTL_TECLADO: u64 = 300;

/// Teto de uma mensagem do Telegram. O nosso corte é um pouco abaixo para caber o rodapé de
/// continuação sem estourar na virada.
const LIMITE_MSG: usize = 3900;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolvido {
    Apagado,
    JaNaoExiste,
    TenteDepois,
}

#[derive(Clone)]
pub struct Tg {
    bot: Bot,
    chat: ChatId,
}

impl Tg {
    pub fn new(token: String, chat_id: i64) -> Self {
        // O cliente padrão do teloxide tem timeout de 17s, e o `getUpdates` deste projeto pede
        // ao Telegram para segurar a conexão por PRAZO_POLL. Com o padrão, toda janela ociosa
        // morria em erro de rede e reabria: o update não se perdia (o Telegram reenvia), mas o
        // log virava um aviso a cada 20s e cada mensagem podia atrasar alguns segundos.
        let cliente = teloxide::net::default_reqwest_settings()
            .timeout(Duration::from_secs(PRAZO_POLL as u64 + 30))
            .build()
            .expect("cliente http do teloxide");
        Self {
            bot: Bot::with_client(token, cliente),
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

    /// Resultado de uma tentativa de apagar tópico na varredura de limpeza.
    ///
    /// A distinção importa: o Telegram recusa com erro de API quando o tópico já não existe (e aí
    /// o trabalho está feito), mas uma queda de rede é temporária e merece nova tentativa. Sem
    /// separar os dois, ou o daemon insiste para sempre num tópico que já sumiu, ou desiste de um
    /// que ainda está lá.
    pub async fn delete_topic_sweep(&self, topic: i32) -> Resolvido {
        match self
            .bot
            .delete_forum_topic(self.chat, ThreadId(MessageId(topic)))
            .await
        {
            Ok(_) => Resolvido::Apagado,
            Err(teloxide::RequestError::Api(_)) => Resolvido::JaNaoExiste,
            Err(_) => Resolvido::TenteDepois,
        }
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

    /// Manda um arquivo do disco para o tópico, como documento (byte a byte, sem recompressão).
    ///
    /// A legenda vai sem `parse_mode`, pela mesma razão do texto do agente: ela costuma carregar
    /// caminho, crase e underscore, e em MarkdownV2 isso vira erro 400 em vez de mensagem.
    pub async fn send_document(
        &self,
        topic: Option<i32>,
        caminho: &std::path::Path,
        legenda: Option<&str>,
    ) -> Result<MessageId> {
        let mut req = self
            .bot
            .send_document(self.chat, InputFile::file(caminho.to_path_buf()));
        req.caption = legenda.map(str::to_string);
        if let Some(t) = topic {
            req.message_thread_id = Some(ThreadId(MessageId(t)));
        }
        Ok(req.await.context("enviando documento")?.id)
    }

    /// Manda uma imagem como foto: o celular mostra na conversa em vez de pedir download.
    ///
    /// O Telegram recomprime e tem limite próprio de dimensão, então quem chama precisa estar
    /// pronto para cair no documento se isto falhar.
    pub async fn send_photo(
        &self,
        topic: Option<i32>,
        caminho: &std::path::Path,
        legenda: Option<&str>,
    ) -> Result<MessageId> {
        let mut req = self
            .bot
            .send_photo(self.chat, InputFile::file(caminho.to_path_buf()));
        req.caption = legenda.map(str::to_string);
        if let Some(t) = topic {
            req.message_thread_id = Some(ThreadId(MessageId(t)));
        }
        Ok(req.await.context("enviando foto")?.id)
    }

    /// Manda um vídeo como vídeo: o Telegram monta o player e dá para assistir sem baixar.
    ///
    /// Vale a pena tentar antes do documento porque é isso que faz um trecho de vídeo ser útil no
    /// celular; container que o Telegram não digere volta como erro, e aí o documento resolve.
    pub async fn send_video(
        &self,
        topic: Option<i32>,
        caminho: &std::path::Path,
        legenda: Option<&str>,
    ) -> Result<MessageId> {
        let mut req = self
            .bot
            .send_video(self.chat, InputFile::file(caminho.to_path_buf()));
        req.caption = legenda.map(str::to_string);
        // Sem isto o celular baixa o arquivo inteiro antes de mostrar o primeiro quadro.
        req.supports_streaming = Some(true);
        if let Some(t) = topic {
            req.message_thread_id = Some(ThreadId(MessageId(t)));
        }
        Ok(req.await.context("enviando vídeo")?.id)
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

    /// Agenda o apagamento de uma mensagem.
    ///
    /// É o que mantém o General sendo só o painel: comando, resposta e teclado são conversa de
    /// um instante, e o único conteúdo permanente ali é a mensagem de estado que vai sendo
    /// editada. Falhar em apagar não é erro: no pior caso sobra uma linha a mais.
    pub fn efemera(&self, id: MessageId, segundos: u64) {
        let tg = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(segundos)).await;
            tg.delete(id).await;
        });
    }

    /// Envia uma resposta que se apaga sozinha.
    pub async fn responde_efemero(&self, topic: Option<i32>, html: &str, segundos: u64) {
        if let Ok(id) = self.send_html(topic, html).await {
            self.efemera(id, segundos);
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
