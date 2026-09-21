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
            // O tópico da mensagem do teclado diz de qual sessão se trata, então o dado do botão
            // não precisa carregar o id (e não caberia: o limite é 64 bytes).
            let topico = q
                .message
                .as_ref()
                .and_then(|m| m.regular_message())
                .and_then(|m| m.thread_id)
                .map(|t| t.0.0);
            // Teclado do General já cumpriu o papel ao ser tocado: some na hora. O card de
            // pergunta e o de permissão NÃO entram aqui: quem os apaga é o `cleanup_ask`, e só
            // depois que a resposta chega ao Claude.
            if let Some(msg) = q.message.as_ref()
                && dado.as_deref().is_some_and(|d| d.starts_with("n:"))
            {
                app.tg.delete(msg.id()).await;
            }
            if let Some(dado) = dado.as_deref() {
                botao(&app, dado, topico, q.message.as_ref().map(|m| m.id())).await?;
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
        // `/model` e `/effort` são comandos do frontend do Claude Code, e nenhum evento consegue
        // dispará-los. O que dá para fazer sem digitar no terminal é reiniciar a sessão com
        // `--resume`, que volta com o mesmo contexto e a flag nova.
        "/model" | "/modelo" | "/effort" | "/esforco" | "/esforço" => {
            let Some(s) = app.session_for_topic(topic).await? else {
                let _ = app
                    .tg
                    .send_html(Some(topic), "Este tópico não tem sessão viva.")
                    .await;
                return Ok(());
            };
            let e_modelo = comando.starts_with("/mod");
            let valor = texto.split_whitespace().nth(1);
            let Some(valor) = valor else {
                // Sem argumento, a escolha vira teclado: família e depois versão.
                if e_modelo {
                    // Teclado vazio no Telegram simplesmente não aparece, e o comando pareceria
                    // ignorado. Se o catálogo falhou, o certo é dizer isso.
                    let teclado = teclado_familias(app);
                    if app.modelos().is_empty() {
                        let _ = app
                            .tg
                            .send_html(
                                Some(topic),
                                "Não consegui ler o catálogo de modelos do binário do Claude Code. \
                                 Aponte <code>claude_binary</code> no config.toml, ou mande o nome \
                                 inteiro: <code>/model claude-opus-4-8[1m]</code>",
                            )
                            .await;
                        return Ok(());
                    }
                    let _ = app
                        .tg
                        .send_keyboard(Some(topic), ESCOLHA_FAMILIA, teclado)
                        .await;
                } else {
                    let botoes = ESFORCOS
                        .iter()
                        .map(|n| (n.to_string(), format!("ef:{n}")))
                        .collect();
                    let _ = app
                        .tg
                        .send_keyboard(Some(topic), "⚡ Qual nível de esforço?", coluna(botoes))
                        .await;
                }
                return Ok(());
            };
            let (model, effort) = if e_modelo {
                (Some(valor), None)
            } else {
                (None, Some(valor))
            };
            if let Err(e) = app.relaunch(&s.session_id, model, effort).await {
                let _ = app
                    .tg
                    .send_html(Some(topic), &format!("⚠️ {}", escape_html(&e.to_string())))
                    .await;
            }
            Ok(())
        }
        _ => {
            // Card aberto? Então o que você escreveu é a resposta dele. A sessão está parada
            // dentro da pergunta e não leria esta mensagem agora de qualquer jeito.
            if let Some(s) = app.session_for_topic(topic).await?
                && app
                    .on_card_text(&s.session_id, texto)
                    .await
                    .unwrap_or(false)
            {
                return Ok(());
            }
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
            // Com argumento, abre direto; sem, mostra o seletor. Palavra que for nome de
            // modelo ou nível de esforço sai do nome do projeto e vira flag.
            if !resto.is_empty() {
                let (alvo, model, effort) = separa_flags(&resto);
                let (model, effort) = (model.as_deref(), effort.as_deref());
                match achar(&projetos, &alvo) {
                    Some(p) => return escolhe_retomada(app, &p, model, effort).await,
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
                     /new &lt;projeto&gt; [opus|sonnet|fable] [high|max]: abre direto\n\
                     /ls: lista as sessões vivas\n\
                     /kill &lt;id&gt;: fecha uma sessão\n\n\
                     Cada sessão vira um tópico. Fale com ela lá dentro; /kill no tópico fecha e apaga.\n\
                     Dentro do tópico: /model e /effort reiniciam a sessão com o contexto inteiro.",
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

async fn botao(
    app: &Arc<App>,
    dado: &str,
    topico: Option<i32>,
    msg: Option<teloxide::types::MessageId>,
) -> anyhow::Result<()> {
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
        if let Some(msg) = msg {
            app.tg.delete(msg).await;
        }
        return escolhe_retomada(app, p, None, None).await;
    }

    // Segunda etapa do /new: continuar a conversa anterior, ou começar do zero.
    if let Some(resto) = dado.strip_prefix("c:") {
        let (idx, sessao) = resto.split_once(':').unwrap_or((resto, ""));
        let projetos = app.cfg.projects_available();
        let Some(p) = idx.parse::<usize>().ok().and_then(|i| projetos.get(i)) else {
            return Ok(());
        };
        if let Some(msg) = msg {
            app.tg.delete(msg).await;
        }
        return abrir(app, p, None, None, Some(sessao)).await;
    }
    if let Some(idx) = dado.strip_prefix("z:") {
        let projetos = app.cfg.projects_available();
        let Some(p) = idx.parse::<usize>().ok().and_then(|i| projetos.get(i)) else {
            return Ok(());
        };
        if let Some(msg) = msg {
            app.tg.delete(msg).await;
        }
        return abrir(app, p, None, None, None).await;
    }
    // Card de pergunta ou de permissão.
    if dado.starts_with("a:") || dado.starts_with("p:") {
        return app.on_card_touch(dado).await;
    }

    // Escolha de modelo, primeira etapa: a família vira a lista de versões, na mesma mensagem.
    if let Some(familia) = dado.strip_prefix("mf:") {
        let (Some(topico), Some(msg)) = (topico, msg) else {
            return Ok(());
        };
        if familia == "*" {
            let _ = app
                .tg
                .edit_keyboard(msg, ESCOLHA_FAMILIA, teclado_familias(app))
                .await;
            return Ok(());
        }
        let modelos = app.modelos();
        let botoes: Vec<(String, String)> = modelos
            .iter()
            .filter(|m| m.familia == familia)
            .map(|m| (m.rotulo(), format!("mv:{}", m.id)))
            .chain(std::iter::once(("« famílias".into(), "mf:*".into())))
            .collect();
        let _ = app
            .tg
            .edit_keyboard(
                msg,
                &format!(
                    "🧠 <b>{}</b>: qual versão?\n<i>o tópico continua o mesmo, o contexto também</i>",
                    escape_html(familia)
                ),
                coluna(botoes),
            )
            .await;
        let _ = topico;
        return Ok(());
    }

    // Segunda etapa: a versão escolhida reinicia a sessão.
    if let Some(id) = dado.strip_prefix("mv:") {
        return troca(app, topico, msg, Some(id), None).await;
    }
    if let Some(nivel) = dado.strip_prefix("ef:") {
        return troca(app, topico, msg, None, Some(nivel)).await;
    }
    Ok(())
}

const ESCOLHA_FAMILIA: &str = "🧠 Qual família?\n<i>ou mande o nome inteiro, por exemplo</i> <code>/model claude-opus-4-8[1m]</code>";

fn teclado_familias(app: &Arc<App>) -> teloxide::types::InlineKeyboardMarkup {
    let modelos = app.modelos();
    let botoes: Vec<(String, String)> = ld_core::models::por_familia(&modelos)
        .into_iter()
        .map(|(familia, _)| {
            let rotulo = familia
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string() + &familia[1..])
                .unwrap_or_else(|| familia.clone());
            (rotulo, format!("mf:{familia}"))
        })
        .collect();
    coluna(botoes)
}

/// Aplica a troca e limpa o teclado.
async fn troca(
    app: &Arc<App>,
    topico: Option<i32>,
    msg: Option<teloxide::types::MessageId>,
    model: Option<&str>,
    effort: Option<&str>,
) -> anyhow::Result<()> {
    let Some(topico) = topico else { return Ok(()) };
    if let Some(msg) = msg {
        app.tg.delete(msg).await;
    }
    let Some(s) = app.session_for_topic(topico).await? else {
        return Ok(());
    };
    if let Err(e) = app.relaunch(&s.session_id, model, effort).await {
        let _ = app
            .tg
            .send_html(Some(topico), &format!("⚠️ {}", escape_html(&e.to_string())))
            .await;
    }
    Ok(())
}

/// Pergunta se a sessão continua a conversa anterior daquele projeto ou começa do zero.
///
/// Só pergunta quando há o que continuar, e quando a conversa anterior não está aberta em outro
/// lugar: retomar uma sessão que já está rodando geraria duas cópias da mesma conversa.
async fn escolhe_retomada(
    app: &Arc<App>,
    p: &Project,
    model: Option<&str>,
    effort: Option<&str>,
) -> anyhow::Result<()> {
    let anterior = ld_core::transcript::ultima_sessao(&ld_core::paths::claude_dir(), &p.path);
    let idx = app
        .cfg
        .projects_available()
        .iter()
        .position(|x| x.path == p.path);

    match (anterior, idx) {
        (Some(a), Some(idx))
            if app
                .store
                .get(&a.session_id)
                .ok()
                .flatten()
                .is_none_or(|s| s.ended_at.is_some()) =>
        {
            let botoes = vec![
                (
                    format!("▶️ Continuar ({})", ha_quanto(a.quando)),
                    format!("c:{idx}:{}", a.session_id),
                ),
                ("🆕 Começar do zero".to_string(), format!("z:{idx}")),
            ];
            let _ = app
                .tg
                .send_keyboard(
                    None,
                    &format!(
                        "<b>{}</b> tem conversa anterior:\n<i>{}</i>",
                        escape_html(&p.name),
                        escape_html(&a.resumo)
                    ),
                    coluna(botoes),
                )
                .await;
            Ok(())
        }
        // Sem histórico (ou com a conversa anterior já aberta): não há escolha a fazer.
        _ => abrir(app, p, model, effort, None).await,
    }
}

fn ha_quanto(epoch: i64) -> String {
    let agora = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let s = (agora - epoch).max(0);
    if s < 3600 {
        return format!("há {}min", s / 60);
    }
    if s < 86400 {
        return format!("há {}h", s / 3600);
    }
    format!("há {}d", s / 86400)
}

async fn abrir(
    app: &Arc<App>,
    p: &Project,
    model: Option<&str>,
    effort: Option<&str>,
    retomar: Option<&str>,
) -> anyhow::Result<()> {
    // O que veio no comando ganha do padrão do projeto, que ganha do padrão do Claude Code.
    let model = model.or(p.model.as_deref());
    let effort = effort.or(p.effort.as_deref());
    info!(projeto = %p.name, ?model, ?effort, "abrindo sessão a pedido do Telegram");
    if let Err(e) = app.create_session(p, model, effort, retomar).await {
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

/// Modelos e níveis de esforço que o Claude Code aceita como apelido.
const MODELOS: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];
const ESFORCOS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Separa `<projeto> [modelo] [esforço]`, em qualquer ordem depois do nome.
///
/// Sem isto, `/new tintim opus` procuraria um projeto chamado "tintim opus". Um projeto que se
/// chame literalmente "opus" ainda funciona pelo seletor de botões.
fn separa_flags(palavras: &[&str]) -> (String, Option<String>, Option<String>) {
    let mut nome = Vec::new();
    let (mut model, mut effort) = (None, None);
    for p in palavras {
        let baixo = p.to_lowercase();
        // Apelido ("opus") ou nome inteiro ("claude-opus-4-8[1m]"): os dois valem em --model.
        if model.is_none() && (MODELOS.contains(&baixo.as_str()) || baixo.starts_with("claude-")) {
            model = Some(baixo);
        } else if effort.is_none() && ESFORCOS.contains(&baixo.as_str()) {
            effort = Some(baixo);
        } else {
            nome.push(*p);
        }
    }
    (nome.join(" "), model, effort)
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
        let modelo = x
            .model
            .as_deref()
            .map(|m| format!(" · {}", escape_html(m)))
            .unwrap_or_default();
        s.push_str(&format!(
            "\n{dono} <b>{}</b> · {}{}{}\n<code>{}</code>",
            escape_html(&x.project),
            escape_html(&x.status),
            modelo,
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
            model: None,
            effort: None,
        }
    }

    #[test]
    fn separa_modelo_e_esforco_do_nome() {
        let (nome, m, e) = separa_flags(&["tintim", "opus", "high"]);
        assert_eq!(nome, "tintim");
        assert_eq!(m.as_deref(), Some("opus"));
        assert_eq!(e.as_deref(), Some("high"));
    }

    #[test]
    fn ordem_das_flags_nao_importa() {
        let (nome, m, e) = separa_flags(&["max", "meu", "projeto", "Sonnet"]);
        assert_eq!(nome, "meu projeto");
        assert_eq!(m.as_deref(), Some("sonnet"));
        assert_eq!(e.as_deref(), Some("max"));
    }

    #[test]
    fn aceita_nome_inteiro_de_modelo() {
        let (nome, m, _) = separa_flags(&["tintim", "claude-opus-4-8[1m]"]);
        assert_eq!(nome, "tintim");
        assert_eq!(m.as_deref(), Some("claude-opus-4-8[1m]"));
    }

    #[test]
    fn sem_flag_o_nome_fica_inteiro() {
        let (nome, m, e) = separa_flags(&["site", "energia", "vital"]);
        assert_eq!(nome, "site energia vital");
        assert!(m.is_none() && e.is_none());
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
