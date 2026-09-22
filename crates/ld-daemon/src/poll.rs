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
use crate::arquivos::{self, Achado, Anexo};
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
            let nome = quem.first_name.clone();
            // Anexo vem com legenda, e não com texto: para o resto do fluxo os dois são a mesma
            // coisa, o que você escreveu junto.
            let texto = msg.text().or_else(|| msg.caption()).unwrap_or("");
            let achado = arquivos::anexos(&msg);

            match msg.thread_id {
                Some(t) => match achado {
                    Achado::Arquivos(lista) => {
                        com_arquivos(&app, t.0.0, texto, &nome, lista, Some(msg.id)).await
                    }
                    Achado::Nada if texto.is_empty() => Ok(()),
                    Achado::Nada => {
                        let alvo = msg.reply_to_message().map(|r| r.id);
                        em_topico(&app, t.0.0, texto, &nome, msg.id, alvo).await
                    }
                },
                None if texto.is_empty() => Ok(()),
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

async fn em_topico(
    app: &Arc<App>,
    topic: i32,
    texto: &str,
    de: &str,
    msg: teloxide::types::MessageId,
    // A qual mensagem você respondeu, quando respondeu. É o que distingue corrigir uma
    // transcrição de simplesmente escrever outra coisa com um card aberto.
    responde_a: Option<teloxide::types::MessageId>,
) -> anyhow::Result<()> {
    let comando = texto.split_whitespace().next().unwrap_or("");

    // Escrever com uma transcrição à espera é o terceiro caminho: os dois vão juntos, com a
    // correção mandando. É o caso de "está quase certo, só essa palavra" — reescrever a frase
    // inteira à mão anularia o ganho de ter falado.
    //
    // Comando continua sendo comando: quem manda /kill com um card aberto quer fechar a sessão,
    // não corrigir a transcrição.
    // Correção de transcrição exige responder ao card. Sem essa exigência, QUALQUER texto
    // digitado com um card aberto virava correção, e não havia como mandar uma mensagem nova e
    // independente enquanto uma transcrição esperava confirmação.
    if !comando.starts_with('/')
        && let Some(alvo) = responde_a
        && let Some(p) = app.confirmacoes.tira_por_msg(alvo)
    {
        // A correção que você digitou some, e o card vira o registro dos dois textos juntos.
        // Deixar a sua mensagem solta no tópico espalharia em três lugares (áudio, card,
        // mensagem) uma coisa só, e nenhum deles mostraria o que a sessão de fato recebeu.
        app.tg.delete(msg).await;
        if let Some(m) = p.msg {
            let _ = app
                .tg
                .edit_keyboard(m, &registro(&p.texto, Some(texto)), sem_botoes())
                .await;
        }
        info!(sessao = %p.session_id, "transcrição enviada com correção escrita");
        let junto = p.para_sessao(Some(texto));
        let r = app
            .on_incoming_com_arquivos(topic, &junto, de, p.arquivos)
            .await;
        mostra_proximo(app, topic).await;
        return r;
    }

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
        // Modo de permissão: mesma mecânica de /model, porque também é flag de partida.
        "/mode" | "/modo" | "/permissao" | "/permissão" => {
            let Some(s) = app.session_for_topic(topic).await? else {
                let _ = app
                    .tg
                    .send_html(Some(topic), "Este tópico não tem sessão viva.")
                    .await;
                return Ok(());
            };
            match texto.split_whitespace().nth(1) {
                Some(modo) => {
                    if let Err(e) = app.relaunch_modo(&s.session_id, modo).await {
                        let _ = app
                            .tg
                            .send_html(
                                Some(topic),
                                &format!("⚠️ {}", escape_html(&format!("{e:#}"))),
                            )
                            .await;
                    }
                }
                None => {
                    let atual = s.permission_mode.as_deref().unwrap_or("auto");
                    let botoes = MODOS
                        .iter()
                        .map(|(id, rotulo)| {
                            let marca = if *id == atual { "● " } else { "" };
                            (format!("{marca}{rotulo}"), format!("pm:{id}"))
                        })
                        .collect();
                    let _ = app
                        .tg
                        .send_keyboard(
                            Some(topic),
                            &format!("🔐 Modo de permissão\n<i>agora: {}</i>", escape_html(atual)),
                            coluna(botoes),
                        )
                        .await;
                }
            }
            Ok(())
        }
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
                    .send_html(
                        Some(topic),
                        &format!("⚠️ {}", escape_html(&format!("{e:#}"))),
                    )
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
                // O card respondido já mostra o que você escreveu; manter a sua mensagem ao lado
                // deixaria a mesma resposta duas vezes seguidas no tópico.
                app.tg.delete(msg).await;
                return Ok(());
            }
            if let Err(e) = app.on_incoming(topic, texto, de).await {
                let _ = app
                    .tg
                    .send_html(
                        Some(topic),
                        &format!("⚠️ {}", escape_html(&format!("{e:#}"))),
                    )
                    .await;
            }
            Ok(())
        }
    }
}

/// Mensagem com anexo: baixa, grava em disco e entrega o caminho à sessão.
///
/// O arquivo é baixado antes de a mensagem seguir, e de propósito: o `file_id` do Telegram não é
/// eterno, e uma sessão que só recebesse o id teria de falar com a API por conta própria. O que
/// ela recebe é um caminho que já existe.
async fn com_arquivos(
    app: &Arc<App>,
    topic: i32,
    legenda: &str,
    de: &str,
    lista: Vec<Anexo>,
    // A mensagem do áudio, para o card de transcrição responder a ela.
    origem: Option<teloxide::types::MessageId>,
) -> anyhow::Result<()> {
    let Some(s) = app.session_for_topic(topic).await? else {
        let _ = app
            .tg
            .send_html(Some(topic), "Este tópico não tem sessão viva.")
            .await;
        return Ok(());
    };

    // Áudio que vai virar card de transcrição não ganha card de anexo: o caminho do .oga no meio
    // do tópico é ruído, e a transcrição já mostra o que interessa.
    let calado = app.cfg.transcricao.ativa && lista.iter().all(|a| ehaudio(a.tipo));

    let mut caminhos = Vec::new();
    for anexo in &lista {
        match arquivos::baixa(app.tg.bot(), &s.session_id, anexo).await {
            Ok(caminho) => {
                info!(sessao = %s.session_id, arquivo = %caminho.display(), "anexo recebido");
                if !calado {
                    let _ = app
                        .tg
                        .send_html(
                            Some(topic),
                            &format!(
                                "📎 <b>{}</b> · {}\n<code>{}</code>",
                                escape_html(
                                    caminho
                                        .file_name()
                                        .map(|n| n.to_string_lossy())
                                        .unwrap_or_default()
                                        .as_ref()
                                ),
                                arquivos::humano(anexo.tamanho),
                                escape_html(&caminho.to_string_lossy())
                            ),
                        )
                        .await;
                }
                caminhos.push(caminho.to_string_lossy().into_owned());
            }
            Err(e) => {
                warn!(sessao = %s.session_id, erro = %e, "não consegui baixar o anexo");
                let _ = app
                    .tg
                    .send_html(
                        Some(topic),
                        &format!(
                            "⚠️ não consegui trazer {} {}: {}",
                            artigo(anexo.tipo),
                            escape_html(anexo.tipo),
                            escape_html(&format!("{e:#}"))
                        ),
                    )
                    .await;
            }
        }
    }

    // Nada chegou em disco: a sessão não tem o que ler, e o aviso do erro já foi para o tópico.
    if caminhos.is_empty() {
        return Ok(());
    }

    // Voz é o único anexo que não se entrega sozinho: um `.oga` não diz nada à sessão, e
    // transcrevê-lo leva mais que o turno inteiro. Sai do caminho da resposta e volta como
    // mensagem própria quando ficar pronto.
    if lista.iter().all(|a| ehaudio(a.tipo)) && app.cfg.transcricao.ativa {
        transcreve_depois(app, topic, legenda, de, caminhos, origem);
        return Ok(());
    }

    app.on_incoming_com_arquivos(topic, &texto_com_anexo(legenda, &caminhos), de, caminhos)
        .await
}

#[cfg(test)]
mod tests_registro {
    use super::*;

    #[test]
    fn registro_sem_ratificacao_mostra_so_a_transcricao() {
        let t = registro("roda os testes", None);
        assert!(t.contains("Transcrição"), "{t}");
        assert!(t.contains("roda os testes"), "{t}");
        assert!(!t.contains("Ratificação"), "{t}");
    }

    #[test]
    fn registro_com_ratificacao_mostra_os_dois() {
        let t = registro("roda os testes do arquivos", Some("é do transcricao"));
        assert!(t.contains("🎤 <b>Transcrição</b>"), "{t}");
        assert!(t.contains("✍️ <b>Ratificação</b>"), "{t}");
        assert!(t.contains("é do transcricao"), "{t}");
    }

    #[test]
    fn ratificacao_em_branco_nao_cria_secao_vazia() {
        assert!(!registro("texto", Some("   ")).contains("Ratificação"));
    }

    #[test]
    fn html_do_seu_texto_nao_escapa_para_a_marcacao() {
        // Falar de "<b>" num áudio não pode quebrar o card nem injetar marcação.
        let t = registro(
            "use <b>negrito</b> & cia",
            Some("na verdade <i>itálico</i>"),
        );
        assert!(t.contains("&lt;b&gt;negrito&lt;/b&gt; &amp; cia"), "{t}");
        assert!(t.contains("&lt;i&gt;itálico&lt;/i&gt;"), "{t}");
    }

    #[test]
    fn teclado_vazio_e_mesmo_vazio() {
        assert!(sem_botoes().inline_keyboard.is_empty());
    }
}

/// Sobe o card da próxima transcrição da fila, se não houver nenhum na tela.
///
/// Chamada depois de cada resolução, e é o que faz a fila andar: você decide uma, a seguinte
/// aparece. Mostrar todas de uma vez tornaria ambíguo a qual delas uma correção escrita se
/// refere, e numerar cards para desfazer essa ambiguidade seria pior que esperar a vez.
async fn mostra_proximo(app: &Arc<App>, topic: i32) {
    let Some(p) = app.confirmacoes.proximo_sem_card(topic) else {
        return;
    };
    let atras = app.confirmacoes.na_fila(topic).saturating_sub(1);
    let rodape = if atras > 0 {
        format!(
            "<i>Confirme, descarte, ou <b>responda a esta mensagem</b> com a correção. Mais {atras} na fila.</i>"
        )
    } else {
        "<i>Confirme, descarte, ou <b>responda a esta mensagem</b> com a correção.</i>".to_string()
    };
    match app
        .tg
        .send_keyboard_reply(
            Some(topic),
            &format!(
                "🎤 <b>Transcrição</b>\n\n{}\n\n{rodape}",
                escape_html(&p.texto)
            ),
            coluna(vec![
                ("✅ Enviar".into(), format!("t:ok:{}", p.id)),
                ("🗑 Descartar".into(), format!("t:no:{}", p.id)),
            ]),
            // A seta do Telegram é o que amarra o card ao áudio que o gerou.
            p.origem,
        )
        .await
    {
        Ok(m) => app.confirmacoes.marca_na_tela(&p.id, m),
        Err(e) => {
            // Sem card, o pendente ficaria preso na fila para sempre e seguraria os próximos.
            warn!(erro = %e, "não consegui mostrar a transcrição; tiro da fila");
            app.confirmacoes.tira_por_id(&p.id);
        }
    }
}

/// O card depois de resolvido: o que a sessão recebeu, sem botão para tocar de novo.
fn registro(transcricao: &str, ratificacao: Option<&str>) -> String {
    let mut t = format!("🎤 <b>Transcrição</b>\n{}", escape_html(transcricao.trim()));
    if let Some(r) = ratificacao.map(str::trim).filter(|r| !r.is_empty()) {
        t.push_str(&format!("\n\n✍️ <b>Ratificação</b>\n{}", escape_html(r)));
    }
    t
}

/// Teclado vazio: o Telegram não tem "remover botões", então se edita com nenhum.
fn sem_botoes() -> teloxide::types::InlineKeyboardMarkup {
    teloxide::types::InlineKeyboardMarkup::new(
        Vec::<Vec<teloxide::types::InlineKeyboardButton>>::new(),
    )
}

fn ehaudio(tipo: &str) -> bool {
    matches!(tipo, "mensagem de voz" | "áudio")
}

/// Transcreve fora do turno e entrega o texto como se você o tivesse escrito.
///
/// Em segundo plano porque o número manda: a configuração escolhida leva 44 s por minuto de
/// fala, e o hook `Stop` desiste em 60 s. Transcrever antes de responder faria um áudio de dois
/// minutos derrubar a resposta inteira.
fn transcreve_depois(
    app: &Arc<App>,
    topic: i32,
    legenda: &str,
    de: &str,
    caminhos: Vec<String>,
    origem: Option<teloxide::types::MessageId>,
) {
    let app = Arc::clone(app);
    let legenda = legenda.to_string();
    let de = de.to_string();
    tokio::spawn(async move {
        let aviso = app
            .tg
            .send_html(Some(topic), "🎤 <i>transcrevendo…</i>")
            .await;

        let mut partes = Vec::new();
        for caminho in &caminhos {
            match crate::transcricao::transcreve(
                &app.cfg.transcricao,
                std::path::Path::new(caminho),
            )
            .await
            {
                Ok(Some(t)) => {
                    info!(sessao = %topic, segundos = t.duracao.as_secs_f32(), "voz transcrita");
                    partes.push(t.texto);
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(arquivo = %caminho, erro = %e, "não consegui transcrever");
                    let _ = app
                        .tg
                        .send_html(
                            Some(topic),
                            &format!(
                                "⚠️ não consegui transcrever: {}\nO áudio está em <code>{}</code>.",
                                escape_html(&format!("{e:#}")),
                                escape_html(caminho)
                            ),
                        )
                        .await;
                }
            }
        }
        if let Ok(id) = aviso {
            app.tg.delete(id).await;
        }
        if partes.is_empty() {
            return;
        }

        // A transcrição NÃO vai direto para a sessão: ela erra, e a sessão agindo sobre algo
        // que o Luka não disse custa mais que o tempo que a voz economizou. Vira um card, e ele
        // decide.
        let transcrito = partes.join("\n");
        let Some(sessao) = app.session_for_topic(topic).await.ok().flatten() else {
            return;
        };
        // Entra na fila; o card só sobe quando for a vez dele.
        app.confirmacoes.guarda(crate::confirmacao::Pendente {
            id: app.confirmacoes.novo_id(),
            session_id: sessao.session_id.clone(),
            topic,
            origem,
            msg: None,
            texto: transcrito,
            legenda: legenda.clone(),
            de: de.clone(),
            arquivos: caminhos,
        });
        mostra_proximo(&app, topic).await;
    });
}

/// O que a sessão lê quando chega um arquivo.
///
/// O caminho vai no texto, e não só no campo `files`, porque o texto é o que o agente lê primeiro
/// (e é o que sobrevive a um `lukadispatch listen` de versão anterior, que repassa a linha sem
/// entender o campo novo). Sem legenda, o texto não pode ficar vazio: uma mensagem em branco
/// chegando do nada parece bug.
fn texto_com_anexo(legenda: &str, caminhos: &[String]) -> String {
    let lista = caminhos
        .iter()
        .map(|c| format!("[arquivo recebido: {c}]"))
        .collect::<Vec<_>>()
        .join("\n");
    if legenda.trim().is_empty() {
        lista
    } else {
        format!("{legenda}\n\n{lista}")
    }
}

/// "a foto", "o documento": o tipo do anexo já vem em português, só falta concordar.
fn artigo(tipo: &str) -> &'static str {
    match tipo {
        "foto" | "animação" | "figurinha" | "nota de vídeo" | "mensagem de voz" => "a",
        _ => "o",
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
                     Dentro do tópico: /model, /effort e /mode reiniciam a sessão com o contexto inteiro.",
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
    // Transcrição esperando aval: confirmar manda para a sessão, descartar apaga sem deixar
    // rastro. Nos dois casos o card some, porque um teclado que já foi tocado só confunde.
    if let Some(resto) = dado.strip_prefix("t:")
        && let Some((acao, id)) = resto.split_once(':')
        && matches!(acao, "ok" | "no")
        && let Some(topic) = topico
    {
        let Some(p) = app.confirmacoes.tira_por_id(id) else {
            // Card de um daemon anterior, ou tocado duas vezes. Some em silêncio: dizer
            // "expirou" seria barulho sobre algo que não tem conserto.
            if let Some(m) = msg {
                app.tg.delete(m).await;
            }
            return Ok(());
        };
        if acao == "no" {
            // "Finge que não existiu": some tudo, e a sessão nunca soube que houve áudio.
            if let Some(m) = p.msg {
                app.tg.delete(m).await;
            }
            info!(sessao = %p.session_id, "transcrição descartada");
            mostra_proximo(app, topic).await;
            return Ok(());
        }
        // O card não some: vira o registro do que foi enviado. Sem isso o tópico fica com um
        // áudio seu e nenhuma pista do texto que a sessão recebeu.
        if let Some(m) = p.msg {
            let _ = app
                .tg
                .edit_keyboard(m, &registro(&p.texto, None), sem_botoes())
                .await;
        }
        let texto = p.para_sessao(None);
        let r = app
            .on_incoming_com_arquivos(topic, &texto, &p.de, p.arquivos)
            .await;
        mostra_proximo(app, topic).await;
        return r;
    }

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
    if let Some(modo) = dado.strip_prefix("pm:") {
        let (Some(topico), Some(msg)) = (topico, msg) else {
            return Ok(());
        };
        app.tg.delete(msg).await;
        let Some(s) = app.session_for_topic(topico).await? else {
            return Ok(());
        };
        if let Err(e) = app.relaunch_modo(&s.session_id, modo).await {
            let _ = app
                .tg
                .send_html(
                    Some(topico),
                    &format!("⚠️ {}", escape_html(&format!("{e:#}"))),
                )
                .await;
        }
        return Ok(());
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
            .send_html(
                Some(topico),
                &format!("⚠️ {}", escape_html(&format!("{e:#}"))),
            )
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
                    escape_html(&format!("{e:#}"))
                ),
                TTL_TECLADO,
            )
            .await;
    }
    Ok(())
}

/// Modos de permissão oferecidos no Telegram, com nome legível.
///
/// `perguntar` é modo do lukadispatch, não do Claude Code: por baixo ele é `dontAsk` (o terminal
/// nunca abre prompt) mais o nosso portão no `PreToolUse`, que é quem pergunta no celular. Os
/// modos nativos que parecem servir para isso não servem: `manual` mostra o prompt e ignora a
/// decisão do hook, e `dontAsk` sozinho nega tudo sem perguntar a ninguém. Ver `docs/permissoes.md`.
const MODOS: [(&str, &str); 4] = [
    ("auto", "🤖 auto (classificador decide)"),
    ("perguntar", "🙋 perguntar no celular"),
    ("plan", "📋 plano (só propõe)"),
    ("bypassPermissions", "⚠️ liberar tudo"),
];

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
