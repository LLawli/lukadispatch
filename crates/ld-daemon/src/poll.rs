//! Laço de long polling do Telegram e roteamento dos updates.
//!
//! Polling e não webhook: webhook exigiria porta aberta, TLS e um domínio, e o daemon roda numa
//! máquina doméstica atrás de NAT. Polling só precisa de saída para a internet.
//!
//! Regra de roteamento, curta: mensagem em tópico é conversa com a sessão daquele tópico;
//! mensagem no General é comando.

use std::sync::Arc;
use std::time::Duration;

use ld_core::config::Project;
use teloxide::prelude::*;
use teloxide::types::{Update, UpdateKind};
use tracing::{info, warn};

use crate::app::App;
use crate::telegram::{TTL_RESPOSTA, TTL_TECLADO, coluna, escape_html};

pub async fn run(app: Arc<App>) {
    let mut offset: i32 = 0;
    loop {
        let pedido = app
            .tg
            .bot()
            .get_updates()
            .offset(offset)
            .timeout(crate::telegram::PRAZO_POLL)
            .await;

        match pedido {
            Ok(updates) => {
                for u in updates {
                    offset = (u.id.0 as i32).saturating_add(1);
                    let app = app.clone();
                    tokio::spawn(async move {
                        if let Err(e) = trata(app, u).await {
                            warn!(erro = %e, "falhei ao tratar update");
                        }
                    });
                }
            }
            Err(e) => {
                // Internet caiu, Telegram fora do ar, 429: espera e volta. Derrubar o daemon por
                // isso perderia as sessões vivas, que continuam rodando no tmux.
                warn!(erro = %e, "getUpdates falhou; tento de novo em 3s");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

async fn trata(app: Arc<App>, u: Update) -> anyhow::Result<()> {
    match u.kind {
        UpdateKind::Message(msg) => {
            // Autorização primeiro, sempre. Sem remetente conhecido, nem lemos o texto.
            let Some(quem) = msg.from.as_ref() else {
                return Ok(());
            };
            // Mensagem de serviço do próprio bot (criar tópico gera uma). Sai em silêncio: cair
            // no aviso de allowlist encheria o log de alarme falso a cada sessão nova.
            if quem.is_bot {
                return Ok(());
            }
            let autor = quem.id.0 as i64;
            if !app.cfg.allows(autor) {
                warn!(autor, "update de usuário fora da allowlist, descartado");
                return Ok(());
            }
            if msg.chat.id != app.tg.chat() {
                return Ok(());
            }
            let Some(texto) = msg.text() else {
                return Ok(());
            };
            let nome = quem.first_name.clone();

            match msg.thread_id {
                Some(t) => em_topico(&app, t.0.0, texto, &nome).await,
                None => {
                    // O General é o painel, e só. O comando que você mandou some junto com a
                    // resposta dele; o que fica é a mensagem de estado, que é editada no lugar.
                    let r = no_general(&app, texto).await;
                    app.tg.delete(msg.id).await;
                    r
                }
            }
        }
        UpdateKind::CallbackQuery(q) => {
            if !app.cfg.allows(q.from.id.0 as i64) {
                return Ok(());
            }
            // Responder sempre, mesmo em erro: sem isso o botão fica rodando no celular.
            let _ = app.tg.bot().answer_callback_query(q.id.clone()).await;
            let dado = q.data.clone();
            // Teclado do General já cumpriu o papel ao ser tocado: some na hora. O card de
            // pergunta e o de permissão NÃO entram aqui: quem os apaga é o `cleanup_ask`, e só
            // depois que a resposta chega ao Claude.
            if let Some(msg) = q.message.as_ref()
                && dado.as_deref().is_some_and(|d| d.starts_with("n:"))
            {
                app.tg.delete(msg.id()).await;
            }
            if let Some(dado) = dado.as_deref() {
                botao(&app, dado).await?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

async fn em_topico(app: &Arc<App>, topic: i32, texto: &str, de: &str) -> anyhow::Result<()> {
    let comando = texto.split_whitespace().next().unwrap_or("");
    match comando {
        "/kill" | "/fechar" => {
            let Some(s) = app.session_for_topic(topic).await? else {
                let _ = app
                    .tg
                    .send_html(Some(topic), "Este tópico não tem sessão viva.")
                    .await;
                return Ok(());
            };
            // O tópico some junto, então não adianta avisar aqui dentro.
            app.end_session(&s.session_id, true).await?;
            Ok(())
        }
        "/ls" | "/sessoes" => {
            let _ = app.tg.send_html(Some(topic), &lista(app)?).await;
            Ok(())
        }
        _ => {
            if let Err(e) = app.on_incoming(topic, texto, de).await {
                let _ = app
                    .tg
                    .send_html(Some(topic), &format!("⚠️ {}", escape_html(&e.to_string())))
                    .await;
            }
            Ok(())
        }
    }
}

async fn no_general(app: &Arc<App>, texto: &str) -> anyhow::Result<()> {
    let mut partes = texto.split_whitespace();
    let comando = partes.next().unwrap_or("");
    let resto: Vec<&str> = partes.collect();

    match comando {
        "/new" | "/nova" => {
            let projetos = app.cfg.projects_available();
            if projetos.is_empty() {
                app.tg
                    .responde_efemero(
                        None,
                        "Nenhum projeto encontrado. Verifique <code>[scan] roots</code> ou adicione um <code>[[projects]]</code> no config.toml.",
                        TTL_RESPOSTA,
                    )
                    .await;
                return Ok(());
            }
            // Com argumento, abre direto; sem, mostra o seletor.
            if !resto.is_empty() {
                let alvo = resto.join(" ");
                match achar(&projetos, &alvo) {
                    Some(p) => return abrir(app, &p).await,
                    None => {
                        let _ = app
                            .tg
                            .send_html(
                                None,
                                &format!("Não achei o projeto <b>{}</b>.", escape_html(&alvo)),
                            )
                            .await;
                        return Ok(());
                    }
                }
            }
            let botoes = projetos
                .iter()
                .take(40)
                .enumerate()
                .map(|(i, p)| (p.name.clone(), format!("n:{i}")))
                .collect();
            if let Ok(id) = app
                .tg
                .send_keyboard(None, "Abrir sessão em qual projeto?", coluna(botoes))
                .await
            {
                app.tg.efemera(id, TTL_TECLADO);
            }
            Ok(())
        }
        "/ls" | "/sessoes" => {
            // No General a lista já é o painel: em vez de mandar uma cópia que viraria lixo,
            // força o redesenho da mensagem de estado.
            app.panel.refresh();
            Ok(())
        }
        "/kill" => {
            let Some(alvo) = resto.first() else {
                app.tg
                    .responde_efemero(
                        None,
                        "Uso: <code>/kill &lt;id da sessão&gt;</code> (ou mande /kill dentro do tópico dela).",
                        TTL_RESPOSTA,
                    )
                    .await;
                return Ok(());
            };
            let achada = app
                .summaries()?
                .into_iter()
                .find(|s| s.session_id.starts_with(alvo));
            match achada {
                Some(s) => {
                    app.end_session(&s.session_id, true).await?;
                    app.tg
                        .responde_efemero(
                            None,
                            &format!("Fechei <b>{}</b>.", escape_html(&s.project)),
                            TTL_RESPOSTA,
                        )
                        .await;
                }
                None => {
                    app.tg
                        .responde_efemero(None, "Não achei essa sessão.", TTL_RESPOSTA)
                        .await;
                }
            }
            Ok(())
        }
        "/help" | "/ajuda" | "/start" => {
            app.tg
                .responde_efemero(
                    None,
                    "<b>lukadispatch</b>\n\n\
                     /new: abre uma sessão (mostra os projetos)\n\
                     /new &lt;projeto&gt;: abre direto\n\
                     /ls: lista as sessões vivas\n\
                     /kill &lt;id&gt;: fecha uma sessão\n\n\
                     Cada sessão vira um tópico. Fale com ela lá dentro; /kill no tópico fecha e apaga.",
                    TTL_TECLADO,
                )
                .await;
            Ok(())
        }
        // Texto solto no General: o painel não é lugar de conversa, e some junto com o aviso.
        _ => {
            app.tg
                .responde_efemero(
                    None,
                    "O General é só o painel. Use <b>/new</b> para abrir uma sessão, ou fale dentro do tópico de uma.",
                    TTL_RESPOSTA,
                )
                .await;
            Ok(())
        }
    }
}

async fn botao(app: &Arc<App>, dado: &str) -> anyhow::Result<()> {
    if let Some(idx) = dado.strip_prefix("n:") {
        // O dado do botão é índice, e não caminho, porque callback_data do Telegram só tem 64
        // bytes: caminho de projeto não cabe.
        let projetos = app.cfg.projects_available();
        let Some(p) = idx.parse::<usize>().ok().and_then(|i| projetos.get(i)) else {
            let _ = app
                .tg
                .send_html(None, "Essa lista já mudou. Mande /new de novo.")
                .await;
            return Ok(());
        };
        return abrir(app, p).await;
    }
    // Card de pergunta ou de permissão.
    if dado.starts_with("a:") || dado.starts_with("p:") {
        return app.on_card_touch(dado).await;
    }
    Ok(())
}

async fn abrir(app: &Arc<App>, p: &Project) -> anyhow::Result<()> {
    info!(projeto = %p.name, "abrindo sessão a pedido do Telegram");
    if let Err(e) = app.create_session(p).await {
        // Falha de abertura precisa ser lida com calma (costuma trazer o motivo do ai-memory ou
        // do tmux), então vive mais que uma resposta comum antes de sumir.
        app.tg
            .responde_efemero(
                None,
                &format!(
                    "❌ Não consegui abrir <b>{}</b>: {}",
                    escape_html(&p.name),
                    escape_html(&e.to_string())
                ),
                TTL_TECLADO,
            )
            .await;
    }
    Ok(())
}

fn achar(projetos: &[Project], alvo: &str) -> Option<Project> {
    let alvo_baixo = alvo.to_lowercase();
    projetos
        .iter()
        .find(|p| p.name.to_lowercase() == alvo_baixo || p.path == alvo)
        .or_else(|| {
            projetos
                .iter()
                .find(|p| p.name.to_lowercase().contains(&alvo_baixo))
        })
        .cloned()
}

fn lista(app: &Arc<App>) -> anyhow::Result<String> {
    let sessoes = app.summaries()?;
    if sessoes.is_empty() {
        return Ok("Nenhuma sessão viva.".into());
    }
    let mut s = String::from("<b>Sessões</b>\n");
    for x in sessoes {
        let ctx = match (x.context_tokens, x.context_limit) {
            (Some(t), Some(l)) => format!(" · {}k/{}k", t / 1000, l / 1000),
            _ => String::new(),
        };
        let dono = if x.owned_by_bot { "🤖" } else { "💻" };
        s.push_str(&format!(
            "\n{dono} <b>{}</b> · {}{}\n<code>{}</code>",
            escape_html(&x.project),
            escape_html(&x.status),
            ctx,
            &x.session_id[..8.min(x.session_id.len())],
        ));
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(nome: &str, caminho: &str) -> Project {
        Project {
            name: nome.into(),
            path: caminho.into(),
            permission_mode: None,
        }
    }

    #[test]
    fn acha_por_nome_exato_ignorando_caixa() {
        let ps = vec![p("Alfa", "/a"), p("beta", "/b")];
        assert_eq!(achar(&ps, "alfa").unwrap().path, "/a");
        assert_eq!(achar(&ps, "BETA").unwrap().path, "/b");
    }

    #[test]
    fn acha_por_caminho() {
        let ps = vec![p("Alfa", "/a")];
        assert_eq!(achar(&ps, "/a").unwrap().name, "Alfa");
    }

    #[test]
    fn nome_exato_ganha_do_prefixo() {
        // "beta" existe e "beta-web" contém "beta": o exato tem que vencer.
        let ps = vec![p("beta-web", "/w"), p("beta", "/b")];
        assert_eq!(achar(&ps, "beta").unwrap().path, "/b");
    }

    #[test]
    fn sem_correspondencia_devolve_none() {
        let ps = vec![p("Alfa", "/a")];
        assert!(achar(&ps, "zeta").is_none());
    }
}
