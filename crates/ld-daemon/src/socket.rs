//! O socket de controle. É por aqui que os hooks e o `lukadispatch listen` falam com o daemon.
//!
//! Sem autenticação de propósito: o socket fica no runtime dir do usuário, que já é 0700, e
//! ainda recebe 0600 na mão. Quem consegue abrir o arquivo já é o dono da máquina, e um token
//! aqui só daria a ilusão de barreira.
//!
//! Cada conexão é uma requisição e uma resposta, menos `Listen`, que fica aberta despejando
//! mensagem enquanto a sessão estiver ouvindo.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use std::path::PathBuf;

use anyhow::{Context, Result};
use ld_core::ask::Ask;
use ld_core::hooks::ASK_TIMEOUT_SECS;
use ld_core::proto::{PermissionDecision, Request, Response, line};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracing::{debug, info, warn};

use crate::app::App;

/// O mesmo teto do hook. Quem desiste é o daemon, para o caminho de desistência ser um só.
const PRAZO_RESPOSTA: std::time::Duration = std::time::Duration::from_secs(ASK_TIMEOUT_SECS);

/// `caminho` vem de fora (e não de `paths::socket()`) para o servidor não depender de estado
/// global: assim dois daemons de teste sobem lado a lado sem disputar variável de ambiente.
pub async fn serve(app: Arc<App>, caminho: PathBuf) -> Result<()> {
    if let Some(pai) = caminho.parent() {
        std::fs::create_dir_all(pai).ok();
    }

    // Socket de daemon morto fica para trás no disco e impede o bind. Só que apagar sem olhar
    // mataria o socket de um daemon vivo: por isso a sonda antes.
    if caminho.exists() {
        match UnixStream::connect(&caminho).await {
            Ok(_) => anyhow::bail!(
                "já existe um lukadispatchd atendendo em {}",
                caminho.display()
            ),
            Err(_) => {
                std::fs::remove_file(&caminho).ok();
            }
        }
    }

    let listener = UnixListener::bind(&caminho)
        .with_context(|| format!("abrindo socket em {}", caminho.display()))?;
    std::fs::set_permissions(&caminho, std::fs::Permissions::from_mode(0o600)).ok();
    info!(socket = %caminho.display(), "socket de controle no ar");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let app = app.clone();
                tokio::spawn(async move {
                    if let Err(e) = atende(app, stream).await {
                        debug!(erro = %e, "conexão encerrada com erro");
                    }
                });
            }
            Err(e) => warn!(erro = %e, "accept falhou"),
        }
    }
}

async fn atende(app: Arc<App>, stream: UnixStream) -> Result<()> {
    let (leitura, mut escrita) = stream.into_split();
    let mut linhas = BufReader::new(leitura).lines();

    while let Some(linha) = linhas.next_line().await? {
        if linha.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&linha) {
            Ok(r) => r,
            Err(e) => {
                let _ = escrita
                    .write_all(
                        line(&Response::Error {
                            message: format!("json inválido: {e}"),
                        })
                        .as_bytes(),
                    )
                    .await;
                continue;
            }
        };

        // Estas três tomam conta da conexão até o fim: não voltam para o laço.
        match req {
            Request::Listen { session_id } => {
                return escuta(app, session_id, escrita, linhas).await;
            }
            Request::Ask {
                session_id,
                questions,
                ..
            } => return pergunta(app, session_id, questions, escrita).await,
            Request::Permission {
                session_id,
                tool_name,
                tool_input,
                ..
            } => return permissao(app, session_id, tool_name, tool_input, escrita).await,
            _ => {}
        }

        let resposta = responde(&app, req).await;
        escrita.write_all(line(&resposta).as_bytes()).await?;
    }
    Ok(())
}

async fn responde(app: &Arc<App>, req: Request) -> Response {
    match req {
        Request::Ping => Response::Pong,

        Request::Register(r) => match app.on_register(&r) {
            Ok(()) => Response::Ok,
            Err(e) => erro(e),
        },

        Request::Event(ev) => match app.on_event(&ev) {
            Ok(()) => Response::Ok,
            Err(e) => erro(e),
        },

        Request::Stop(r) => match app.on_stop(&r).await {
            Ok(resp) => resp,
            Err(e) => erro(e),
        },

        Request::SessionEnd { session_id, .. } => match app.end_session(&session_id, true).await {
            Ok(()) => Response::Ok,
            Err(e) => erro(e),
        },

        Request::ListSessions => match app.summaries() {
            Ok(s) => Response::Sessions(s),
            Err(e) => erro(e),
        },

        Request::Kill { session_id } => match app.end_session(&session_id, true).await {
            Ok(()) => Response::Ok,
            Err(e) => erro(e),
        },

        Request::NewSession { project } => {
            let achado = app
                .cfg
                .projects_available()
                .into_iter()
                .find(|p| p.name == project || p.path == project);
            match achado {
                Some(p) => match app.create_session(&p).await {
                    Ok(_) => Response::Ok,
                    Err(e) => erro(e),
                },
                None => Response::Error {
                    message: format!("projeto desconhecido: {project}"),
                },
            }
        }

        // A janela do PC respondeu primeiro: resolve a pendência, e quem está esperando lá em
        // `pergunta()` apaga o card do Telegram.
        Request::LocalAnswer { ask_id, answer } => {
            app.hub.answer(&ask_id, answer);
            Response::Ok
        }

        Request::Listen { .. } | Request::Ask { .. } | Request::Permission { .. } => {
            unreachable!("tratados antes, em atende()")
        }
    }
}

fn erro(e: anyhow::Error) -> Response {
    warn!(erro = %e, "requisição falhou");
    Response::Error {
        message: e.to_string(),
    }
}

/// Pergunta do Claude: abre o card, espera a resposta de qualquer um dos dois canais.
///
/// Escreve DUAS linhas: primeiro o `ask_id`, para a janela do PC poder cancelar o card se ela
/// ganhar a corrida, e depois a resposta. Quem lê é o hook, que conhece essa ordem.
async fn pergunta(
    app: Arc<App>,
    session_id: String,
    questions: serde_json::Value,
    mut escrita: tokio::net::unix::OwnedWriteHalf,
) -> Result<()> {
    let ask = Ask::from_tool_input(&serde_json::json!({ "questions": questions }));
    let (ask_id, rx) = match app.start_ask(&session_id, ask).await {
        Ok(x) => x,
        Err(e) => {
            let resp = Response::Answer {
                answered: false,
                text: None,
                reason: Some(e.to_string()),
            };
            escrita.write_all(line(&resp).as_bytes()).await?;
            return Ok(());
        }
    };

    escrita
        .write_all(
            line(&Response::AskOpened {
                ask_id: ask_id.clone(),
            })
            .as_bytes(),
        )
        .await?;

    let bruta = tokio::time::timeout(PRAZO_RESPOSTA, rx).await;
    app.cleanup_ask(&ask_id).await;

    let resp = match bruta {
        Ok(Ok(texto)) => Response::Answer {
            answered: true,
            text: Some(app.resposta_para_claude(&texto)),
            reason: None,
        },
        _ => Response::Answer {
            answered: false,
            text: None,
            reason: Some("ninguém respondeu a tempo".into()),
        },
    };
    escrita.write_all(line(&resp).as_bytes()).await?;
    Ok(())
}

/// Pedido de permissão. Mesma corrida, decisão binária.
async fn permissao(
    app: Arc<App>,
    session_id: String,
    ferramenta: String,
    entrada: serde_json::Value,
    mut escrita: tokio::net::unix::OwnedWriteHalf,
) -> Result<()> {
    let (ask_id, rx) = match app
        .start_permission(&session_id, &ferramenta, &entrada)
        .await
    {
        Ok(x) => x,
        Err(e) => {
            // Sem card não há decisão: o Claude Code decide como decidiria sem nós.
            let resp = Response::Decision {
                decision: PermissionDecision::Undecided,
                reason: Some(e.to_string()),
            };
            escrita.write_all(line(&resp).as_bytes()).await?;
            return Ok(());
        }
    };

    escrita
        .write_all(
            line(&Response::AskOpened {
                ask_id: ask_id.clone(),
            })
            .as_bytes(),
        )
        .await?;

    let bruta = tokio::time::timeout(PRAZO_RESPOSTA, rx).await;
    app.cleanup_ask(&ask_id).await;

    let resp = match bruta.map(|r| r.map(|t| t == "allow")) {
        Ok(Ok(true)) => Response::Decision {
            decision: PermissionDecision::Allow,
            reason: Some("liberado por você no lukadispatch".into()),
        },
        Ok(Ok(false)) => Response::Decision {
            decision: PermissionDecision::Deny,
            reason: Some("negado por você no lukadispatch".into()),
        },
        _ => Response::Decision {
            decision: PermissionDecision::Undecided,
            reason: None,
        },
    };
    escrita.write_all(line(&resp).as_bytes()).await?;
    Ok(())
}

/// Fluxo de entrada de uma sessão: a fila guardada primeiro, depois o que chegar ao vivo.
///
/// O fim da conexão é detectado pelo lado da leitura (EOF quando o `lukadispatch listen` morre,
/// que é o que acontece quando o Monitor expira ou é cancelado). Sem isso, o daemon acharia que
/// a sessão continua ouvindo e nunca pediria o re-arme.
async fn escuta(
    app: Arc<App>,
    session_id: String,
    mut escrita: tokio::net::unix::OwnedWriteHalf,
    mut linhas: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
) -> Result<()> {
    info!(sessao = %session_id, "monitor armado");
    let mut rx = app.hub.listen(&session_id);

    if let Ok(guardadas) = app.store.drain(&session_id) {
        for (texto, de, at) in guardadas {
            let msg = Response::Message {
                text: texto,
                from: de,
                at,
            };
            escrita.write_all(line(&msg).as_bytes()).await?;
        }
    }

    loop {
        tokio::select! {
            recebida = rx.recv() => {
                let Some(m) = recebida else { break };
                let msg = Response::Message { text: m.text, from: m.from, at: m.at };
                if escrita.write_all(line(&msg).as_bytes()).await.is_err() {
                    break;
                }
            }
            fim = linhas.next_line() => {
                // O cliente fechou (EOF) ou morreu.
                if matches!(fim, Ok(None) | Err(_)) {
                    break;
                }
            }
        }
    }

    app.hub.unlisten(&session_id);
    info!(sessao = %session_id, "monitor caiu");
    Ok(())
}
