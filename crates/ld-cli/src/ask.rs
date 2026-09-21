//! A corrida: a mesma pergunta no Telegram e numa janela do PC, vale quem responder primeiro.
//!
//! O árbitro é este processo, e não o daemon, por um motivo prático: o hook roda dentro do tmux,
//! com o ambiente gráfico da sua sessão, então é ele que consegue abrir uma janela. Um serviço
//! systemd pode nem ter `WAYLAND_DISPLAY`.
//!
//! O desenho é duas threads e um canal. A primeira fala com o daemon (que desenha o card no
//! Telegram e fica esperando), a segunda abre a janela. A primeira resposta que chegar ganha, e
//! o perdedor é desfeito: se a janela ganhou, o daemon recebe `LocalAnswer` e apaga o card; se o
//! Telegram ganhou, a janela leva um sinal.
//!
//! Qualquer falha em qualquer ponto vira "não decidi": o hook sai com 0 sem escrever nada, e o
//! Claude Code segue pelo caminho normal dele. Nunca travar a sessão vale mais que responder.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ld_core::ask::{Answer, Ask, Opt, Question};
use ld_core::hooks::ASK_TIMEOUT_SECS;
use ld_core::labels::label_for_tool;
use ld_core::paths;
use ld_core::proto::{PermissionDecision, Request, Response, line};
use serde_json::{Value, json};

/// Um pouco mais que o teto do daemon, para quem desiste primeiro ser sempre ele: assim o
/// caminho de desistência é um só, com card apagado e tudo.
const PRAZO: Duration = Duration::from_secs(ASK_TIMEOUT_SECS + 60);

enum Vencedor {
    /// Veio do daemon (Telegram), já traduzido.
    Telegram(Response),
    /// Veio da janela do PC.
    Janela(Answer),
}

/// Hook `PreToolUse` do AskUserQuestion.
pub fn ask(ev: &Value, session_id: String) -> i32 {
    let entrada = ev.get("tool_input").cloned().unwrap_or(Value::Null);
    let pergunta = Ask::from_tool_input(&entrada);
    if pergunta.is_empty() {
        return 0;
    }

    let req = Request::Ask {
        session_id,
        tool_use_id: ev
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        questions: entrada.get("questions").cloned().unwrap_or(json!([])),
    };

    let texto = match disputar(req, pergunta) {
        Some(Vencedor::Telegram(Response::Answer {
            answered: true,
            text: Some(t),
            ..
        })) => t,
        Some(Vencedor::Janela(a)) => a.to_claude(),
        _ => return 0,
    };

    // O canal de volta é o motivo do `deny`: o `permissionDecisionReason` é o único texto que o
    // hook consegue entregar ao Claude. O texto diz, em letras claras, que isto é resposta e não
    // recusa (é o que `Answer::to_claude` garante).
    println!(
        "{}",
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": texto
            }
        })
    );
    0
}

/// Hook `PermissionRequest`.
pub fn permission(ev: &Value, session_id: String) -> i32 {
    let ferramenta = ev
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let entrada = ev.get("tool_input").cloned().unwrap_or(Value::Null);

    let req = Request::Permission {
        session_id,
        tool_name: ferramenta.clone(),
        tool_input: entrada.clone(),
        tool_use_id: ev
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    };

    // A janela mostra a mesma coisa que o card: duas opções.
    let pergunta = Ask {
        questions: vec![Question {
            question: format!("Permitir {ferramenta}?"),
            header: "Permissão".into(),
            multi_select: false,
            options: vec![
                Opt {
                    label: "Permitir".into(),
                    description: label_for_tool(&ferramenta, &entrada),
                    preview: None,
                },
                Opt {
                    label: "Negar".into(),
                    description: String::new(),
                    preview: None,
                },
            ],
        }],
    };

    let permitir = match disputar(req, pergunta) {
        Some(Vencedor::Telegram(Response::Decision { decision, .. })) => match decision {
            PermissionDecision::Allow => true,
            PermissionDecision::Deny => false,
            PermissionDecision::Undecided => return 0,
        },
        Some(Vencedor::Janela(a)) => a
            .items
            .first()
            .and_then(|i| i.answers.first())
            .map(|r| r.eq_ignore_ascii_case("permitir"))
            .unwrap_or(false),
        _ => return 0,
    };

    println!(
        "{}",
        json!({
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "permissionDecision": if permitir { "allow" } else { "deny" },
                "permissionDecisionReason": if permitir {
                    "Liberado por você no lukadispatch."
                } else {
                    "Negado por você no lukadispatch."
                }
            }
        })
    );
    0
}

/// Abre os dois canais e devolve o primeiro que responder.
fn disputar(req: Request, pergunta: Ask) -> Option<Vencedor> {
    let (tx, rx) = mpsc::channel::<Vencedor>();
    let ask_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let pid_janela: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

    {
        let tx = tx.clone();
        let ask_id = ask_id.clone();
        std::thread::spawn(move || {
            if let Some(r) = fala_com_daemon(req, ask_id) {
                let _ = tx.send(Vencedor::Telegram(r));
            }
        });
    }
    {
        let pid_janela = pid_janela.clone();
        std::thread::spawn(move || {
            if let Some(a) = abre_janela(pergunta, pid_janela) {
                let _ = tx.send(Vencedor::Janela(a));
            }
        });
    }

    let vencedor = rx.recv_timeout(PRAZO).ok()?;

    match &vencedor {
        // O Telegram ganhou: fecha a janela, senão ela fica lá pedindo uma resposta que já foi
        // dada.
        Vencedor::Telegram(_) => {
            if let Some(pid) = *pid_janela.lock().unwrap_or_else(|e| e.into_inner()) {
                mata(pid);
            }
        }
        // A janela ganhou: o daemon precisa saber para apagar o card do celular.
        Vencedor::Janela(a) => {
            let id = ask_id.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(ask_id) = id {
                let payload = serde_json::to_string(a).unwrap_or_default();
                let _ = crate::client::call(
                    &Request::LocalAnswer {
                        ask_id,
                        answer: payload,
                    },
                    Duration::from_secs(5),
                );
            }
        }
    }
    Some(vencedor)
}

/// Conversa longa com o daemon: primeira linha traz o id do card, segunda traz a resposta.
fn fala_com_daemon(req: Request, ask_id: Arc<Mutex<Option<String>>>) -> Option<Response> {
    let mut stream = UnixStream::connect(paths::socket()).ok()?;
    // Sem prazo de leitura: a espera pode durar horas, e é isso mesmo.
    stream.set_read_timeout(None).ok()?;
    stream.write_all(line(&req).as_bytes()).ok()?;
    stream.flush().ok()?;

    let mut linhas = BufReader::new(stream).lines();
    let primeira: Response = serde_json::from_str(linhas.next()?.ok()?.trim()).ok()?;
    match primeira {
        Response::AskOpened { ask_id: id } => {
            *ask_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
        }
        // O daemon já respondeu de cara (sessão sem tópico, por exemplo): não há card nenhum.
        outra => return Some(outra),
    }
    serde_json::from_str(linhas.next()?.ok()?.trim()).ok()
}

/// Abre a janela do PC e espera a resposta dela.
fn abre_janela(pergunta: Ask, pid: Arc<Mutex<Option<u32>>>) -> Option<Answer> {
    let exe = caminho_da_janela()?;
    let mut filho = std::process::Command::new(exe)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    *pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(filho.id());

    let entrada = serde_json::to_string(&pergunta).ok()?;
    filho.stdin.take()?.write_all(entrada.as_bytes()).ok()?;

    let saida = filho.wait_with_output().ok()?;
    if !saida.status.success() {
        // Janela fechada sem responder: quem decide agora é o celular.
        return None;
    }
    serde_json::from_slice(&saida.stdout).ok()
}

/// A janela fica ao lado deste binário; só depois disso vale tentar o PATH.
fn caminho_da_janela() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let vizinho = dir.join("lukadispatch-ask");
        if vizinho.is_file() {
            return Some(vizinho);
        }
    }
    Some(PathBuf::from("lukadispatch-ask"))
}

/// Mata o processo da janela.
///
/// Via `kill(1)` mesmo: a alternativa seria uma dependência de libc só para uma chamada, e este
/// binário roda a cada ferramenta do Claude. O `filho` está preso no `wait_with_output` de outra
/// thread, então não dá para usar o `Child` daqui.
fn mata(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prazo_do_hook_e_maior_que_o_do_daemon() {
        // Quem desiste primeiro tem que ser o daemon: é ele que apaga o card.
        assert!(PRAZO.as_secs() > ASK_TIMEOUT_SECS);
    }

    #[test]
    fn permissao_le_a_escolha_da_janela() {
        let a = Answer {
            items: vec![ld_core::ask::AnswerItem {
                header: "Permissão".into(),
                question: "Permitir Bash?".into(),
                answers: vec!["Permitir".into()],
            }],
        };
        let permitir = a
            .items
            .first()
            .and_then(|i| i.answers.first())
            .map(|r| r.eq_ignore_ascii_case("permitir"))
            .unwrap_or(false);
        assert!(permitir);
    }

    #[test]
    fn resposta_da_janela_vira_texto_para_o_claude() {
        let a = Answer {
            items: vec![ld_core::ask::AnswerItem {
                header: "Banco".into(),
                question: "Qual banco?".into(),
                answers: vec!["SQLite".into()],
            }],
        };
        let t = a.to_claude();
        assert!(t.contains("- Banco: SQLite"));
        assert!(t.contains("não precisa rodar"));
    }
}
