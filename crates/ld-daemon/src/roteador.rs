//! Recebe os eventos do frontend e decide o que fazer com cada um.
//!
//! Regra de roteamento, curta: mensagem num canal de sessão é conversa com a sessão daquele
//! canal; mensagem no canal principal (`None`) é comando.
//!
//! Tudo que é específico de plataforma já ficou para trás em [`crate::frontend`]: allowlist,
//! reply automático do fórum, extração de anexo. O que chega aqui é só [`Evento`], que qualquer
//! adaptador sabe produzir.

use std::sync::Arc;

use ld_core::config::Project;
use tracing::{info, warn};

use crate::app::App;
use crate::frontend::formato::escapa;
use crate::frontend::{Anexo, Botao, Canal, Evento, MsgId};

/// Quanto tempo um aviso efêmero (resposta a comando, guarda de pendência) fica na tela antes de
/// sumir sozinho.
pub const TTL_RESPOSTA: u64 = 25;
/// Idem, para um teclado (seletor de projeto, escolha de esforço).
pub const TTL_TECLADO: u64 = 300;

pub async fn run(app: Arc<App>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Evento>();
    let frontend = app.frontend.clone();
    tokio::spawn(async move { frontend.escuta(tx).await });

    while let Some(ev) = rx.recv().await {
        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = trata(app, ev).await {
                warn!(erro = %e, "falhei ao tratar evento");
            }
        });
    }
}

pub async fn trata(app: Arc<App>, ev: Evento) -> anyhow::Result<()> {
    match ev {
        Evento::Mensagem {
            autor,
            canal,
            msg,
            texto,
            responde_a,
            anexos,
        } => match canal {
            Some(canal) => {
                if !anexos.is_empty() {
                    com_anexos(&app, &canal, &texto, &autor.nome, anexos, Some(msg)).await
                } else if texto.is_empty() {
                    Ok(())
                } else {
                    em_canal(&app, &canal, &texto, &autor.nome, &msg, responde_a).await
                }
            }
            None if texto.is_empty() => Ok(()),
            None => {
                // O canal principal é o painel, e só. O comando que você mandou some junto com a
                // resposta dele; o que fica é a mensagem de estado, que é editada no lugar.
                let r = no_principal(&app, &texto).await;
                app.frontend.apaga(&msg).await;
                r
            }
        },
        Evento::Toque {
            canal, msg, dado, ..
        } => {
            // O teclado do /new some ao ser tocado, e quem o apaga é o `novo`. O card de pergunta
            // e o de permissão também não somem aqui: quem os apaga é o `cleanup_ask`, e só depois
            // que a resposta chega ao Claude.
            botao(&app, &dado, canal.as_ref(), msg.as_ref()).await
        }
    }
}

async fn em_canal(
    app: &Arc<App>,
    canal: &Canal,
    texto: &str,
    de: &str,
    msg: &MsgId,
    // A qual mensagem você respondeu, quando respondeu. É o que distingue corrigir uma
    // transcrição de simplesmente escrever outra coisa com um card aberto.
    responde_a: Option<MsgId>,
) -> anyhow::Result<()> {
    let comando = texto.split_whitespace().next().unwrap_or("");

    // Com pendência aberta, o canal só aceita duas coisas: a resposta a ela, e comando. Comando
    // passa sempre: quem manda /kill com um card aberto quer fechar a sessão, e sem isso um card
    // preso trancaria o canal por dentro.
    //
    // Responder ao card de transcrição com um texto manda os dois juntos, com a correção
    // mandando. É o caso de "está quase certo, só essa palavra": reescrever a frase inteira à mão
    // anularia o ganho de ter falado.
    //
    // A ordem aqui importa. Primeiro tenta-se casar a correção com o card; só o que NÃO for
    // resposta àquele card cai na guarda. Antes a guarda liberava qualquer reply, e responder a
    // uma mensagem antiga qualquer passava por cima de um card aberto.
    if !comando.starts_with('/') {
        // Correção: responder ao card manda a transcrição junto com o que você escreveu.
        if let Some(alvo) = &responde_a
            && let Some(p) = app.confirmacoes.tira_por_msg(alvo)
        {
            // A correção que você digitou some, e o card vira o registro dos dois textos juntos.
            // Deixar a sua mensagem solta no canal espalharia em três lugares (áudio, card,
            // mensagem) uma coisa só, e nenhum deles mostraria o que a sessão de fato recebeu.
            app.frontend.apaga(msg).await;
            if let Some(m) = &p.msg {
                let _ = app
                    .frontend
                    .edita(m, &registro(&p.texto, Some(texto)), &[])
                    .await;
            }
            info!(sessao = %p.session_id, "transcrição enviada com correção escrita");
            let junto = p.para_sessao(Some(texto));
            let r = app
                .on_incoming_com_arquivos(canal, &junto, de, p.arquivos.clone())
                .await;
            mostra_proximo(app, canal).await;
            return r;
        }

        // Qualquer outra coisa espera: texto solto, e também resposta a outra mensagem. O que
        // chegaria ao Claude seria um pedido novo no meio de um que ele ainda não pôde executar,
        // e perguntar para ser atropelado é pior que esperar.
        if let Some(o_que) = pendencia_aberta(app, canal).await {
            app.frontend.apaga(msg).await;
            let rico = format!(
                "⏳ <b>Tem {o_que} esperando você.</b>\nResponda a ela primeiro; depois disso o canal volta ao normal.\n\n<i>Sua mensagem não foi enviada:</i>\n{}",
                escapa(texto)
            );
            crate::frontend::responde_efemero(&app.frontend, Some(canal), &rico, TTL_RESPOSTA)
                .await;
            return Ok(());
        }
    }

    match comando {
        "/kill" | "/fechar" => {
            let Some(s) = app.session_for_canal(canal).await? else {
                let _ = app
                    .frontend
                    .envia(Some(canal), "Este canal não tem sessão viva.", &[], None)
                    .await;
                return Ok(());
            };
            // Numa worktree, pergunta o que fazer com ela; senão fecha, e o canal some junto.
            crate::novo::kill(app, &s, Some(canal)).await
        }
        "/ls" | "/sessoes" => {
            let _ = app
                .frontend
                .envia(Some(canal), &lista(app)?, &[], None)
                .await;
            Ok(())
        }
        // `/model` e `/effort` são comandos do frontend do Claude Code, e nenhum evento consegue
        // dispará-los. O que dá para fazer sem digitar no terminal é reiniciar a sessão com
        // `--resume`, que volta com o mesmo contexto e a flag nova.
        // Modo de permissão: mesma mecânica de /model, porque também é flag de partida.
        "/mode" | "/modo" | "/permissao" | "/permissão" => {
            let Some(s) = app.session_for_canal(canal).await? else {
                let _ = app
                    .frontend
                    .envia(Some(canal), "Este canal não tem sessão viva.", &[], None)
                    .await;
                return Ok(());
            };
            match texto.split_whitespace().nth(1) {
                Some(modo) => {
                    if let Err(e) = app.relaunch_modo(&s.session_id, modo).await {
                        let _ = app
                            .frontend
                            .envia(
                                Some(canal),
                                &format!("⚠️ {}", escapa(&format!("{e:#}"))),
                                &[],
                                None,
                            )
                            .await;
                    }
                }
                None => {
                    let atual = s.permission_mode.as_deref().unwrap_or("auto");
                    let botoes: Vec<Botao> = app
                        .agente
                        .modos()
                        .iter()
                        .filter(|m| m.no_menu)
                        .map(|m| {
                            let marca = if m.id == atual { "● " } else { "" };
                            Botao::new(format!("{marca}{}", m.rotulo), format!("pm:{}", m.id))
                        })
                        .collect();
                    let _ = app
                        .frontend
                        .envia(
                            Some(canal),
                            &format!("🔐 Modo de permissão\n<i>agora: {}</i>", escapa(atual)),
                            &botoes,
                            None,
                        )
                        .await;
                }
            }
            Ok(())
        }
        "/model" | "/modelo" | "/effort" | "/esforco" | "/esforço" => {
            let Some(s) = app.session_for_canal(canal).await? else {
                let _ = app
                    .frontend
                    .envia(Some(canal), "Este canal não tem sessão viva.", &[], None)
                    .await;
                return Ok(());
            };
            let e_modelo = comando.starts_with("/mod");
            let valor = texto.split_whitespace().nth(1);
            let Some(valor) = valor else {
                // Sem argumento, a escolha vira botões: família e depois versão.
                if e_modelo {
                    // Sem botão nenhum a escolha simplesmente não aparece, e o comando pareceria
                    // ignorado. Se o catálogo falhou, o certo é dizer isso.
                    if app.modelos().is_empty() {
                        let _ = app
                            .frontend
                            .envia(
                                Some(canal),
                                "O agente não informou o catálogo de modelos (confira a \
                                 configuração dele), ou mande o nome inteiro: \
                                 <code>/model claude-opus-4-8[1m]</code>",
                                &[],
                                None,
                            )
                            .await;
                        return Ok(());
                    }
                    let _ = app
                        .frontend
                        .envia(Some(canal), ESCOLHA_FAMILIA, &teclado_familias(app), None)
                        .await;
                } else {
                    let botoes: Vec<Botao> = app
                        .agente
                        .esforcos()
                        .iter()
                        .map(|n| Botao::new(n.to_string(), format!("ef:{n}")))
                        .collect();
                    let _ = app
                        .frontend
                        .envia(Some(canal), "⚡ Qual nível de esforço?", &botoes, None)
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
                    .frontend
                    .envia(
                        Some(canal),
                        &format!("⚠️ {}", escapa(&format!("{e:#}"))),
                        &[],
                        None,
                    )
                    .await;
            }
            Ok(())
        }
        _ => {
            // Card aberto? Então o que você escreveu é a resposta dele. A sessão está parada
            // dentro da pergunta e não leria esta mensagem agora de qualquer jeito.
            if let Some(s) = app.session_for_canal(canal).await?
                && app
                    .on_card_text(&s.session_id, texto)
                    .await
                    .unwrap_or(false)
            {
                // O card respondido já mostra o que você escreveu; manter a sua mensagem ao lado
                // deixaria a mesma resposta duas vezes seguidas no canal.
                app.frontend.apaga(msg).await;
                return Ok(());
            }
            if let Err(e) = app.on_incoming(canal, texto, de).await {
                let _ = app
                    .frontend
                    .envia(
                        Some(canal),
                        &format!("⚠️ {}", escapa(&format!("{e:#}"))),
                        &[],
                        None,
                    )
                    .await;
            }
            Ok(())
        }
    }
}

/// Mensagem com anexo: baixa, grava em disco e entrega o caminho à sessão.
///
/// O arquivo é baixado antes de a mensagem seguir, e de propósito: o id do anexo na plataforma
/// não é eterno, e uma sessão que só recebesse o id teria de falar com a API por conta própria. O
/// que ela recebe é um caminho que já existe.
async fn com_anexos(
    app: &Arc<App>,
    canal: &Canal,
    legenda: &str,
    de: &str,
    lista: Vec<Anexo>,
    // A mensagem do áudio, para o card de transcrição responder a ela.
    origem: Option<MsgId>,
) -> anyhow::Result<()> {
    let Some(s) = app.session_for_canal(canal).await? else {
        let _ = app
            .frontend
            .envia(Some(canal), "Este canal não tem sessão viva.", &[], None)
            .await;
        return Ok(());
    };

    // Áudio que vai virar card de transcrição não ganha card de anexo: o caminho do áudio no
    // meio do canal é ruído, e a transcrição já mostra o que interessa.
    let calado = app.transcritor.is_some() && lista.iter().all(|a| a.tipo.e_audio());

    let mut caminhos = Vec::new();
    for anexo in &lista {
        match crate::arquivos::recebe(app, &s.session_id, anexo).await {
            Ok(caminho) => {
                info!(sessao = %s.session_id, arquivo = %caminho.display(), "anexo recebido");
                if !calado {
                    let _ = app
                        .frontend
                        .envia(
                            Some(canal),
                            &format!(
                                "📎 <b>{}</b> · {}\n<code>{}</code>",
                                escapa(
                                    caminho
                                        .file_name()
                                        .map(|n| n.to_string_lossy())
                                        .unwrap_or_default()
                                        .as_ref()
                                ),
                                crate::arquivos::humano_u64(anexo.tamanho),
                                escapa(&caminho.to_string_lossy())
                            ),
                            &[],
                            None,
                        )
                        .await;
                }
                caminhos.push(caminho.to_string_lossy().into_owned());
            }
            Err(e) => {
                warn!(sessao = %s.session_id, erro = %e, "não consegui trazer o anexo");
                let _ = app
                    .frontend
                    .envia(
                        Some(canal),
                        &format!(
                            "⚠️ não consegui trazer {} {}: {}",
                            anexo.tipo.artigo(),
                            escapa(anexo.tipo.nome()),
                            escapa(&format!("{e:#}"))
                        ),
                        &[],
                        None,
                    )
                    .await;
            }
        }
    }

    // Nada chegou em disco: a sessão não tem o que ler, e o aviso do erro já foi para o canal.
    if caminhos.is_empty() {
        return Ok(());
    }

    // Voz é o único anexo que não se entrega sozinho: ele não diz nada à sessão, e transcrevê-lo
    // leva mais que o turno inteiro. Sai do caminho da resposta e volta como mensagem própria
    // quando ficar pronto.
    if lista.iter().all(|a| a.tipo.e_audio()) && app.transcritor.is_some() {
        transcreve_depois(app, canal.clone(), legenda, de, caminhos, origem);
        return Ok(());
    }

    app.on_incoming_com_arquivos(canal, &texto_com_anexo(legenda, &caminhos), de, caminhos)
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
}

/// O que está esperando resposta neste canal, em palavras, se há algo.
///
/// Cobre os três pedidos que param o canal: a pergunta e o pedido de permissão do Claude, e a
/// transcrição esperando seu aval. Todos têm a mesma propriedade: são uma pergunta feita a você,
/// e mandar outra coisa por cima não responde nenhuma delas.
async fn pendencia_aberta(app: &Arc<App>, canal: &Canal) -> Option<&'static str> {
    if app.confirmacoes.tem_card_na_tela(canal) {
        return Some("uma transcrição");
    }
    let s = app.session_for_canal(canal).await.ok().flatten()?;
    match app.cards.aberto_da_sessao(&s.session_id)? {
        (_, crate::cards::Kind::Pergunta) => Some("uma pergunta"),
        (_, crate::cards::Kind::Permissao) => Some("um pedido de permissão"),
    }
}

/// Sobe o card da próxima transcrição da fila, se não houver nenhum na tela.
///
/// Chamada depois de cada resolução, e é o que faz a fila andar: você decide uma, a seguinte
/// aparece. Mostrar todas de uma vez tornaria ambíguo a qual delas uma correção escrita se
/// refere, e numerar cards para desfazer essa ambiguidade seria pior que esperar a vez.
async fn mostra_proximo(app: &Arc<App>, canal: &Canal) {
    let Some(p) = app.confirmacoes.proximo_sem_card(canal) else {
        return;
    };
    let atras = app.confirmacoes.na_fila(canal).saturating_sub(1);
    let rodape = if atras > 0 {
        format!(
            "<i>Confirme, descarte, ou <b>responda a esta mensagem</b> com a correção. Mais {atras} na fila.</i>"
        )
    } else {
        "<i>Confirme, descarte, ou <b>responda a esta mensagem</b> com a correção.</i>".to_string()
    };
    let rico = format!("🎤 <b>Transcrição</b>\n\n{}\n\n{rodape}", escapa(&p.texto));
    let botoes = vec![
        Botao::new("✅ Enviar", format!("t:ok:{}", p.id)),
        Botao::new("🗑 Descartar", format!("t:no:{}", p.id)),
    ];
    // Responder ao áudio original é o que amarra o card a ele, quando há mais de uma voz no
    // canal.
    match app
        .frontend
        .envia(Some(canal), &rico, &botoes, p.origem.as_ref())
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
    let mut t = format!("🎤 <b>Transcrição</b>\n{}", escapa(transcricao.trim()));
    if let Some(r) = ratificacao.map(str::trim).filter(|r| !r.is_empty()) {
        t.push_str(&format!("\n\n✍️ <b>Ratificação</b>\n{}", escapa(r)));
    }
    t
}

/// Transcreve fora do turno e entrega o texto como se você o tivesse escrito.
///
/// Em segundo plano porque o número manda: a configuração escolhida leva dezenas de segundos
/// por minuto de fala, e o hook `Stop` desiste em 60 s. Transcrever antes de responder faria um
/// áudio de dois minutos derrubar a resposta inteira.
fn transcreve_depois(
    app: &Arc<App>,
    canal: Canal,
    legenda: &str,
    de: &str,
    caminhos: Vec<String>,
    origem: Option<MsgId>,
) {
    let app = Arc::clone(app);
    let legenda = legenda.to_string();
    let de = de.to_string();
    tokio::spawn(async move {
        let Some(transcritor) = app.transcritor.clone() else {
            return;
        };
        let aviso = app
            .frontend
            .envia(Some(&canal), "🎤 <i>transcrevendo…</i>", &[], None)
            .await;

        let mut partes = Vec::new();
        for caminho in &caminhos {
            match transcritor.transcreve(std::path::Path::new(caminho)).await {
                Ok(t) => {
                    info!(canal = %canal, segundos = t.duracao.as_secs_f32(), "voz transcrita");
                    partes.push(t.texto);
                }
                Err(e) => {
                    warn!(arquivo = %caminho, erro = %e, "não consegui transcrever");
                    let _ = app
                        .frontend
                        .envia(
                            Some(&canal),
                            &format!(
                                "⚠️ não consegui transcrever: {}\nO áudio está em <code>{}</code>.",
                                escapa(&format!("{e:#}")),
                                escapa(caminho)
                            ),
                            &[],
                            None,
                        )
                        .await;
                }
            }
        }
        if let Ok(id) = aviso {
            app.frontend.apaga(&id).await;
        }
        if partes.is_empty() {
            return;
        }

        // A transcrição NÃO vai direto para a sessão: ela erra, e a sessão agindo sobre algo
        // que o Luka não disse custa mais que o tempo que a voz economizou. Vira um card, e ele
        // decide.
        let transcrito = partes.join("\n");
        let Some(sessao) = app.session_for_canal(&canal).await.ok().flatten() else {
            return;
        };
        // Entra na fila; o card só sobe quando for a vez dele.
        app.confirmacoes.guarda(crate::confirmacao::Pendente {
            id: app.confirmacoes.novo_id(),
            session_id: sessao.session_id.clone(),
            canal: canal.clone(),
            origem,
            msg: None,
            texto: transcrito,
            legenda: legenda.clone(),
            de: de.clone(),
            arquivos: caminhos,
        });
        mostra_proximo(&app, &canal).await;
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

async fn no_principal(app: &Arc<App>, texto: &str) -> anyhow::Result<()> {
    let mut partes = texto.split_whitespace();
    let comando = partes.next().unwrap_or("");
    let resto: Vec<&str> = partes.collect();

    match comando {
        "/new" | "/nova" => {
            if resto.is_empty() {
                return crate::novo::inicio(app).await;
            }
            // Palavra que for nome de modelo ou nível de esforço sai do nome e vira flag.
            let (alvo, model, effort) = separa_flags(app.agente.as_ref(), &resto);
            let palavras: Vec<&str> = alvo.split_whitespace().collect();
            crate::novo::direto(app, &palavras, model, effort).await
        }
        "/ls" | "/sessoes" => {
            // No canal principal a lista já é o painel: em vez de mandar uma cópia que viraria
            // lixo, força o redesenho da mensagem de estado.
            app.panel.refresh();
            Ok(())
        }
        "/kill" => {
            let Some(alvo) = resto.first() else {
                crate::frontend::responde_efemero(
                    &app.frontend,
                    None,
                    "Uso: <code>/kill &lt;id da sessão&gt;</code> (ou mande /kill dentro do canal dela).",
                    TTL_RESPOSTA,
                )
                .await;
                return Ok(());
            };
            let achada = app
                .store
                .live()?
                .into_iter()
                .find(|s| s.session_id.starts_with(alvo));
            match achada {
                Some(s) => crate::novo::kill(app, &s, None).await?,
                None => {
                    crate::frontend::responde_efemero(
                        &app.frontend,
                        None,
                        "Não achei essa sessão.",
                        TTL_RESPOSTA,
                    )
                    .await;
                }
            }
            Ok(())
        }
        "/help" | "/ajuda" | "/start" => {
            crate::frontend::responde_efemero(
                &app.frontend,
                None,
                "<b>lukadispatch</b>\n\n\
                 /new: abre uma sessão (pasta, projeto, branch)\n\
                 /new &lt;projeto&gt; [branch] [opus|sonnet|fable] [high|max]: abre direto; branch que não existe é criada\n\
                 /new new-project: cria um projeto e abre a sessão nele\n\
                 /ls: lista as sessões vivas\n\
                 /kill &lt;id&gt;: fecha uma sessão\n\n\
                 Cada sessão vira um canal, e cada branch roda na worktree dela. Fale com a sessão lá dentro; /kill no canal fecha (e pergunta o que fazer com a worktree).\n\
                 Dentro do canal: /model, /effort e /mode reiniciam a sessão com o contexto inteiro.",
                TTL_TECLADO,
            )
            .await;
            Ok(())
        }
        // Texto solto no canal principal: a resposta a uma pergunta do /new, ou nada. O canal
        // não é lugar de conversa, e o texto some junto com o aviso.
        _ if !comando.starts_with('/') && crate::novo::texto(app, texto).await? => Ok(()),
        _ => {
            crate::frontend::responde_efemero(
                &app.frontend,
                None,
                "Este canal é só o painel. Use <b>/new</b> para abrir uma sessão, ou fale dentro do canal de uma.",
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
    canal: Option<&Canal>,
    msg: Option<&MsgId>,
) -> anyhow::Result<()> {
    // Transcrição esperando aval: confirmar manda para a sessão, descartar apaga sem deixar
    // rastro. Nos dois casos o card some, porque um teclado que já foi tocado só confunde.
    if let Some(resto) = dado.strip_prefix("t:")
        && let Some((acao, id)) = resto.split_once(':')
        && matches!(acao, "ok" | "no")
        && let Some(canal) = canal
    {
        let Some(p) = app.confirmacoes.tira_por_id(id) else {
            // Card de um daemon anterior, ou tocado duas vezes. Some em silêncio: dizer
            // "expirou" seria barulho sobre algo que não tem conserto.
            if let Some(m) = msg {
                app.frontend.apaga(m).await;
            }
            return Ok(());
        };
        if acao == "no" {
            // "Finge que não existiu": some tudo, e a sessão nunca soube que houve áudio.
            if let Some(m) = &p.msg {
                app.frontend.apaga(m).await;
            }
            info!(sessao = %p.session_id, "transcrição descartada");
            mostra_proximo(app, canal).await;
            return Ok(());
        }
        // O card não some: vira o registro do que foi enviado. Sem isso o canal fica com um
        // áudio seu e nenhuma pista do texto que a sessão recebeu.
        if let Some(m) = &p.msg {
            let _ = app.frontend.edita(m, &registro(&p.texto, None), &[]).await;
        }
        let texto = p.para_sessao(None);
        let r = app
            .on_incoming_com_arquivos(canal, &texto, &p.de, p.arquivos.clone())
            .await;
        mostra_proximo(app, canal).await;
        return r;
    }

    // Etapas do /new e a pergunta do /kill.
    if let Some(n) = dado.strip_prefix("e:") {
        return crate::novo::toque(app, n, canal, msg).await;
    }
    // Card de pergunta ou de permissão.
    if dado.starts_with("a:") || dado.starts_with("p:") {
        return app.on_card_touch(dado).await;
    }

    // Escolha de modelo, primeira etapa: a família vira a lista de versões, na mesma mensagem.
    if let Some(familia) = dado.strip_prefix("mf:") {
        let (Some(_canal), Some(msg)) = (canal, msg) else {
            return Ok(());
        };
        if familia == "*" {
            let _ = app
                .frontend
                .edita(msg, ESCOLHA_FAMILIA, &teclado_familias(app))
                .await;
            return Ok(());
        }
        let modelos = app.modelos();
        let mut botoes: Vec<Botao> = modelos
            .iter()
            .filter(|m| m.familia == familia)
            .map(|m| Botao::new(m.rotulo(), format!("mv:{}", m.id)))
            .collect();
        botoes.push(Botao::new("« famílias", "mf:*"));
        let _ = app
            .frontend
            .edita(
                msg,
                &format!(
                    "🧠 <b>{}</b>: qual versão?\n<i>o canal continua o mesmo, o contexto também</i>",
                    escapa(familia)
                ),
                &botoes,
            )
            .await;
        return Ok(());
    }

    // Segunda etapa: a versão escolhida reinicia a sessão.
    if let Some(id) = dado.strip_prefix("mv:") {
        return troca(app, canal, msg, Some(id), None).await;
    }
    if let Some(nivel) = dado.strip_prefix("ef:") {
        return troca(app, canal, msg, None, Some(nivel)).await;
    }
    if let Some(modo) = dado.strip_prefix("pm:") {
        let (Some(canal), Some(msg)) = (canal, msg) else {
            return Ok(());
        };
        app.frontend.apaga(msg).await;
        let Some(s) = app.session_for_canal(canal).await? else {
            return Ok(());
        };
        if let Err(e) = app.relaunch_modo(&s.session_id, modo).await {
            let _ = app
                .frontend
                .envia(
                    Some(canal),
                    &format!("⚠️ {}", escapa(&format!("{e:#}"))),
                    &[],
                    None,
                )
                .await;
        }
        return Ok(());
    }
    Ok(())
}

const ESCOLHA_FAMILIA: &str = "🧠 Qual família?\n<i>ou mande o nome inteiro, por exemplo</i> <code>/model claude-opus-4-8[1m]</code>";

fn teclado_familias(app: &Arc<App>) -> Vec<Botao> {
    let modelos = app.modelos();
    ld_core::models::por_familia(&modelos)
        .into_iter()
        .map(|(familia, _)| {
            let rotulo = familia
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string() + &familia[1..])
                .unwrap_or_else(|| familia.clone());
            Botao::new(rotulo, format!("mf:{familia}"))
        })
        .collect()
}

/// Aplica a troca e limpa o teclado.
async fn troca(
    app: &Arc<App>,
    canal: Option<&Canal>,
    msg: Option<&MsgId>,
    model: Option<&str>,
    effort: Option<&str>,
) -> anyhow::Result<()> {
    let Some(canal) = canal else { return Ok(()) };
    if let Some(msg) = msg {
        app.frontend.apaga(msg).await;
    }
    let Some(s) = app.session_for_canal(canal).await? else {
        return Ok(());
    };
    if let Err(e) = app.relaunch(&s.session_id, model, effort).await {
        let _ = app
            .frontend
            .envia(
                Some(canal),
                &format!("⚠️ {}", escapa(&format!("{e:#}"))),
                &[],
                None,
            )
            .await;
    }
    Ok(())
}

pub(crate) fn ha_quanto(epoch: i64) -> String {
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

pub(crate) async fn abrir(
    app: &Arc<App>,
    p: &Project,
    model: Option<&str>,
    effort: Option<&str>,
    retomar: Option<&str>,
) -> anyhow::Result<()> {
    // O que veio no comando ganha do padrão do projeto, que ganha do padrão do Claude Code.
    let model = model.or(p.model.as_deref());
    let effort = effort.or(p.effort.as_deref());
    info!(projeto = %p.name, ?model, ?effort, "abrindo sessão a pedido do chat");
    if let Err(e) = app.create_session(p, model, effort, retomar).await {
        // Falha de abertura precisa ser lida com calma (costuma trazer o motivo do ai-memory ou
        // do tmux), então vive mais que uma resposta comum antes de sumir.
        crate::frontend::responde_efemero(
            &app.frontend,
            None,
            &format!(
                "❌ Não consegui abrir <b>{}</b>: {}",
                escapa(&p.name),
                escapa(&format!("{e:#}"))
            ),
            TTL_TECLADO,
        )
        .await;
    }
    Ok(())
}

/// Separa `<projeto> [modelo] [esforço]`, em qualquer ordem depois do nome.
///
/// Sem isto, `/new tintim opus` procuraria um projeto chamado "tintim opus". Um projeto que se
/// chame literalmente "opus" ainda funciona pelo seletor de botões. Quem sabe se uma palavra é
/// modelo ou esforço é o agente: cada um tem os apelidos e os níveis dele.
fn separa_flags(
    agente: &dyn crate::agente::Agente,
    palavras: &[&str],
) -> (String, Option<String>, Option<String>) {
    let mut nome = Vec::new();
    let (mut model, mut effort) = (None, None);
    for p in palavras {
        let baixo = p.to_lowercase();
        if model.is_none() && agente.e_nome_de_modelo(&baixo) {
            model = Some(baixo);
        } else if effort.is_none() && agente.esforcos().contains(&baixo.as_str()) {
            effort = Some(baixo);
        } else {
            nome.push(*p);
        }
    }
    (nome.join(" "), model, effort)
}

pub(crate) fn achar(projetos: &[Project], alvo: &str) -> Option<Project> {
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
            .map(|m| format!(" · {}", escapa(m)))
            .unwrap_or_default();
        let worktree = app.store.worktree_em(&x.cwd).ok().flatten();
        s.push_str(&format!(
            "\n{dono} <b>{}</b> · {}{}{}\n<code>{}</code>",
            escapa(&crate::app::nome_do_canal(&x.project, worktree.as_ref())),
            escapa(&x.status),
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
    use crate::agente::claude_code::{ClaudeCode, Locais};

    fn p(nome: &str, caminho: &str) -> Project {
        Project {
            name: nome.into(),
            path: caminho.into(),
            permission_mode: None,
            model: None,
            effort: None,
        }
    }

    /// Um `ClaudeCode` que não toca a máquina de verdade: só o que `separa_flags` usa dele
    /// (apelido de modelo e níveis de esforço) não depende do binário nem de arquivo nenhum.
    fn agente_de_teste() -> ClaudeCode {
        let raiz = tempfile::tempdir().unwrap();
        ClaudeCode::new(
            Locais {
                cli: "/opt/ld/lukadispatch".into(),
                mcp_proxy: "/opt/ld/lukadispatch-mcp".into(),
                settings: raiz.path().join("bot-settings.json"),
                claude_json: raiz.path().join("claude.json"),
                claude_dir: raiz.path().join("claude"),
                uso_db: raiz.path().join("uso.db"),
            },
            None,
        )
    }

    #[test]
    fn separa_modelo_e_esforco_do_nome() {
        let a = agente_de_teste();
        let (nome, m, e) = separa_flags(&a, &["tintim", "opus", "high"]);
        assert_eq!(nome, "tintim");
        assert_eq!(m.as_deref(), Some("opus"));
        assert_eq!(e.as_deref(), Some("high"));
    }

    #[test]
    fn ordem_das_flags_nao_importa() {
        let a = agente_de_teste();
        let (nome, m, e) = separa_flags(&a, &["max", "meu", "projeto", "Sonnet"]);
        assert_eq!(nome, "meu projeto");
        assert_eq!(m.as_deref(), Some("sonnet"));
        assert_eq!(e.as_deref(), Some("max"));
    }

    #[test]
    fn aceita_nome_inteiro_de_modelo() {
        let a = agente_de_teste();
        let (nome, m, _) = separa_flags(&a, &["tintim", "claude-opus-4-8[1m]"]);
        assert_eq!(nome, "tintim");
        assert_eq!(m.as_deref(), Some("claude-opus-4-8[1m]"));
    }

    #[test]
    fn sem_flag_o_nome_fica_inteiro() {
        let a = agente_de_teste();
        let (nome, m, e) = separa_flags(&a, &["site", "energia", "vital"]);
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
