//! O setup do Telegram: o bot, o grupo com tópicos e os direitos do bot nele.
//!
//! Cada verificação é feita na API e não na palavra de quem responde: o `getMe` diz se o Group
//! Privacy está desligado, o `getChat` se o grupo é fórum, o `getChatMember` se o bot é admin
//! com os dois direitos que o daemon usa. Enquanto faltar algo, o passo explica o que fazer no
//! celular e confere de novo. O `chat_id` e o seu `user_id` saem de uma mensagem que você manda
//! no grupo, e não de você ter de achá-los num JSON.
//!
//! A API fica atrás de [`ApiDoBot`] para os laços serem testados com um bot de mentira.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use super::arquivos::{CHAVE_TOKEN, Rascunho};
use super::tela::Tela;

/// O remetente que o Telegram põe no lugar de um admin que posta anonimamente.
const ADMIN_ANONIMO: u64 = 1087968824;

pub struct InfoBot {
    pub id: u64,
    pub usuario: String,
    /// `false` com o Group Privacy ligado: o bot só veria comandos, e não a conversa.
    pub le_grupo_todo: bool,
}

/// Uma mensagem que chegou ao bot, reduzida ao que o setup usa.
#[derive(Debug, Clone, PartialEq)]
pub struct Visto {
    pub chat_id: i64,
    pub grupo: bool,
    pub titulo: String,
    /// O grupo virou supergrupo (acontece ao ligar os tópicos) e o id passou a ser este.
    pub migrou_para: Option<i64>,
    pub de: Option<Remetente>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Remetente {
    pub id: u64,
    pub nome: String,
    pub bot: bool,
}

pub struct InfoChat {
    /// O id atual, que muda se o grupo virou supergrupo depois da mensagem.
    pub id: i64,
    pub titulo: String,
    pub forum: bool,
}

pub struct Direitos {
    pub admin: bool,
    pub topicos: bool,
    pub apagar: bool,
}

#[async_trait]
pub trait ApiDoBot: Send + Sync {
    async fn quem_sou(&self) -> Result<InfoBot>;
    /// Esquece o que chegou antes e devolve o offset para esperar só o que vier depois.
    async fn marca_inicio(&self) -> Result<i32>;
    /// Espera mensagens novas (long polling) e devolve o offset seguinte.
    async fn espera(&self, desde: i32) -> Result<(Vec<Visto>, i32)>;
    async fn chat(&self, id: i64) -> Result<InfoChat>;
    async fn direitos(&self, chat: i64, bot: u64) -> Result<Direitos>;
}

/// Monta a API para um token. No uso real é o teloxide; nos testes, um bot roteirizado.
pub type Conecta<'a> = &'a dyn Fn(&str) -> Arc<dyn ApiDoBot>;

pub async fn configura(tela: &mut Tela<'_>, r: &mut Rascunho, conecta: Conecta<'_>) -> Result<()> {
    let (token, api, bot) = escolhe_bot(tela, r, conecta).await?;
    privacidade(tela, &*api, &bot).await?;
    let (chat, usuario) = escolhe_grupo(tela, r, &*api, &bot).await?;
    let chat = forum(tela, &*api, chat).await?;
    direitos(tela, &*api, &chat, &bot).await?;

    r.poe_env(CHAVE_TOKEN, &token);
    r.poe(None, "frontend", "telegram");
    r.poe(Some("telegram"), "chat_id", chat.id);
    if let Some(de) = usuario {
        r.inclui_id("telegram", "allowed_user_ids", de.id as i64);
        r.nome_sugerido = de.nome.split_whitespace().next().map(str::to_string);
    }
    tela.diz(&format!(
        "Grupo \"{}\" pronto para o @{}.",
        chat.titulo, bot.usuario
    ));
    Ok(())
}

async fn escolhe_bot(
    tela: &mut Tela<'_>,
    r: &Rascunho,
    conecta: Conecta<'_>,
) -> Result<(String, Arc<dyn ApiDoBot>, InfoBot)> {
    tela.passo("O bot do Telegram");
    if let Some(token) = r.valor_env(CHAVE_TOKEN) {
        let api = conecta(&token);
        match api.quem_sou().await {
            Ok(bot) => {
                if tela.sim(&format!(
                    "Já existe um bot configurado, @{}. Usar ele?",
                    bot.usuario
                ))? {
                    return Ok((token, api, bot));
                }
            }
            Err(e) => tela.diz(&format!(
                "O token que está no .env não funciona mais ({e:#})."
            )),
        }
    }
    tela.diz(
        "Crie um bot no Telegram: abra uma conversa com o @BotFather, mande /newbot e siga \
         as perguntas (nome e usuário do bot). No fim ele manda um token parecido com \
         123456789:AAH...",
    );
    loop {
        let token = tela.segredo("Cole o token aqui (não aparece na tela)")?;
        if token.is_empty() {
            continue;
        }
        let api = conecta(&token);
        match api.quem_sou().await {
            Ok(bot) => {
                tela.diz(&format!("Bot @{} encontrado.", bot.usuario));
                return Ok((token, api, bot));
            }
            Err(e) => tela.diz(&format!(
                "O Telegram recusou esse token ({e:#}). Tente de novo."
            )),
        }
    }
}

async fn privacidade(tela: &mut Tela<'_>, api: &dyn ApiDoBot, bot: &InfoBot) -> Result<()> {
    let mut atual = bot.le_grupo_todo;
    while !atual {
        tela.diz(&format!(
            "O @{} está com o Group Privacy ligado, e assim só veria as mensagens que começam \
             com /. No @BotFather: /mybots > @{} > Bot Settings > Group Privacy > Turn off.",
            bot.usuario, bot.usuario
        ));
        tela.espera("Desligue.")?;
        atual = api.quem_sou().await?.le_grupo_todo;
    }
    Ok(())
}

async fn escolhe_grupo(
    tela: &mut Tela<'_>,
    r: &Rascunho,
    api: &dyn ApiDoBot,
    bot: &InfoBot,
) -> Result<(i64, Option<Remetente>)> {
    tela.passo("O grupo");
    let chat_atual = r.atual.telegram.chat_id;
    if chat_atual != 0 && !r.atual.telegram.allowed_user_ids.is_empty() {
        match api.chat(chat_atual).await {
            Ok(c) => {
                if tela.sim(&format!(
                    "O grupo \"{}\" já está configurado. Usar ele?",
                    c.titulo
                ))? {
                    return Ok((c.id, None));
                }
            }
            Err(e) => tela.diz(&format!(
                "O grupo que está no config não responde para este bot ({e:#})."
            )),
        }
    }
    tela.diz(&format!(
        "No Telegram:\n\
         1. Crie um grupo novo (Nova mensagem > Novo grupo). O nome é livre; pode ser só você.\n\
         2. Nas configurações do grupo (Editar), ligue os Tópicos.\n\
         3. Adicione o @{} ao grupo como membro.\n\
         4. Mande qualquer mensagem no grupo.\n\
         Se o bot já estava no grupo antes de você desligar o Group Privacy, tire e ponha ele \
         de novo: a mudança só vale para grupos em que ele entrar depois.",
        bot.usuario
    ));
    tela.diz("Esperando a mensagem no grupo (Ctrl+C cancela)...");
    let mut desde = api.marca_inicio().await?;
    loop {
        let (vistos, proximo) = api.espera(desde).await?;
        desde = proximo;
        for v in vistos {
            if !v.grupo {
                tela.diz("Essa mensagem chegou no privado do bot. Mande no grupo.");
                continue;
            }
            let Some(de) = v.de else {
                continue;
            };
            // Antes do filtro de bots: o admin anônimo chega com um remetente que é bot.
            if de.id == ADMIN_ANONIMO {
                tela.diz(
                    "A mensagem chegou como admin anônimo, e assim não dá para saber quem você é. \
                     Desligue \"Permanecer anônimo\" nos seus direitos de admin do grupo e mande \
                     outra.",
                );
                continue;
            }
            if de.bot {
                continue;
            }
            let chat = v.migrou_para.unwrap_or(v.chat_id);
            tela.diz(&format!(
                "Mensagem de {} no grupo \"{}\". É você quem o bot vai obedecer.",
                de.nome, v.titulo
            ));
            return Ok((chat, Some(de)));
        }
    }
}

async fn forum(tela: &mut Tela<'_>, api: &dyn ApiDoBot, chat: i64) -> Result<InfoChat> {
    loop {
        let info = api.chat(chat).await?;
        if info.forum {
            return Ok(info);
        }
        tela.diz(&format!(
            "O grupo \"{}\" está sem tópicos, e cada sessão precisa de um. Nas configurações do \
             grupo (Editar), ligue os Tópicos.",
            info.titulo
        ));
        tela.espera("Ligue.")?;
    }
}

async fn direitos(
    tela: &mut Tela<'_>,
    api: &dyn ApiDoBot,
    chat: &InfoChat,
    bot: &InfoBot,
) -> Result<()> {
    loop {
        let d = api.direitos(chat.id, bot.id).await?;
        let mut falta = Vec::new();
        if !d.admin {
            falta.push("ser administrador");
        }
        if !d.topicos {
            falta.push("Gerenciar tópicos (para criar e apagar o tópico de cada sessão)");
        }
        if !d.apagar {
            falta.push("Apagar mensagens (para manter o tópico General limpo)");
        }
        if falta.is_empty() {
            return Ok(());
        }
        tela.diz(&format!(
            "No grupo \"{}\", promova o @{} a administrador (Editar > Administradores > \
             Adicionar). Falta: {}.",
            chat.titulo,
            bot.usuario,
            falta.join("; ")
        ));
        tela.espera("Ajuste.")?;
    }
}

/// A API de verdade, pelo teloxide.
#[cfg(feature = "telegram")]
pub mod teloxide_api {
    use super::*;
    use anyhow::{Context, bail};
    use teloxide::prelude::*;
    use teloxide::types::{
        ChatFullInfo, ChatFullInfoKind, ChatFullInfoPublicKind, ChatMemberKind, UpdateKind,
    };
    use teloxide::{ApiError, RequestError};

    use crate::frontend::telegram::{PRAZO_POLL, bot};

    pub struct Teloxide(pub Bot);

    pub fn conecta(token: &str) -> Arc<dyn ApiDoBot> {
        Arc::new(Teloxide(bot(token.to_string())))
    }

    /// Reduz um update ao que o setup usa. Função pura para testar com payload real.
    pub fn visto(u: &Update) -> Option<Visto> {
        let UpdateKind::Message(m) = &u.kind else {
            return None;
        };
        Some(Visto {
            chat_id: m.chat.id.0,
            grupo: m.chat.is_group() || m.chat.is_supergroup(),
            titulo: m.chat.title().unwrap_or_default().to_string(),
            migrou_para: m.migrate_to_chat_id().map(|c| c.0),
            de: m.from.as_ref().map(|f| Remetente {
                id: f.id.0,
                nome: f.full_name(),
                bot: f.is_bot,
            }),
        })
    }

    fn e_forum(info: &ChatFullInfo) -> bool {
        match &info.kind {
            ChatFullInfoKind::Public(p) => match &p.kind {
                ChatFullInfoPublicKind::Supergroup(s) => s.is_forum,
                _ => false,
            },
            ChatFullInfoKind::Private(_) => false,
        }
    }

    #[async_trait]
    impl ApiDoBot for Teloxide {
        async fn quem_sou(&self) -> Result<InfoBot> {
            let eu = self.0.get_me().await?;
            Ok(InfoBot {
                id: eu.user.id.0,
                usuario: eu.user.username.clone().unwrap_or_default(),
                le_grupo_todo: eu.can_read_all_group_messages,
            })
        }

        async fn marca_inicio(&self) -> Result<i32> {
            // Offset negativo devolve só o último update e faz o Telegram esquecer os anteriores:
            // uma mensagem velha, de outro grupo, não passa por resposta de agora.
            let ultimos = self.0.get_updates().offset(-1).timeout(0).await?;
            Ok(ultimos.last().map(|u| u.id.0 as i32 + 1).unwrap_or(0))
        }

        async fn espera(&self, desde: i32) -> Result<(Vec<Visto>, i32)> {
            let updates = self
                .0
                .get_updates()
                .offset(desde)
                .timeout(PRAZO_POLL)
                .await
                .context("esperando mensagens do Telegram")?;
            let proximo = updates.last().map(|u| u.id.0 as i32 + 1).unwrap_or(desde);
            Ok((updates.iter().filter_map(visto).collect(), proximo))
        }

        async fn chat(&self, id: i64) -> Result<InfoChat> {
            let mut id = ChatId(id);
            // Ligar os tópicos converte o grupo em supergrupo, e o id antigo passa a responder
            // com o id novo em vez do chat.
            let info = loop {
                match self.0.get_chat(id).await {
                    Ok(i) => break i,
                    Err(RequestError::MigrateToChatId(novo)) => id = novo,
                    Err(e) => return Err(e.into()),
                }
            };
            Ok(InfoChat {
                id: info.id.0,
                titulo: info.title().unwrap_or_default().to_string(),
                forum: e_forum(&info),
            })
        }

        async fn direitos(&self, chat: i64, bot: u64) -> Result<Direitos> {
            let membro = match self.0.get_chat_member(ChatId(chat), UserId(bot)).await {
                Ok(m) => m,
                Err(RequestError::Api(ApiError::UserNotFound)) => {
                    bail!("o bot não está no grupo")
                }
                Err(e) => return Err(e.into()),
            };
            Ok(match membro.kind {
                ChatMemberKind::Owner(_) => Direitos {
                    admin: true,
                    topicos: true,
                    apagar: true,
                },
                ChatMemberKind::Administrator(a) => Direitos {
                    admin: true,
                    topicos: a.can_manage_topics,
                    apagar: a.can_delete_messages,
                },
                _ => Direitos {
                    admin: false,
                    topicos: false,
                    apagar: false,
                },
            })
        }
    }
}
