//! Os dois hooks que fazem uma pergunta ao usuário: `AskUserQuestion` e `PermissionRequest`.
//!
//! A mecânica da corrida (card no Telegram e janela no PC ao mesmo tempo, vale quem responder
//! primeiro) mora em `ld_core::race`, porque o proxy de MCP também precisa dela. Aqui fica só o
//! que é específico de hook: montar a pergunta e traduzir a resposta para o formato que o Claude
//! Code entende.
//!
//! Qualquer falha em qualquer ponto vira "não decidi": o hook sai com 0 sem escrever nada, e o
//! Claude Code segue pelo caminho normal dele. Nunca travar a sessão vale mais que responder.

use std::time::Duration;

use ld_core::ask::{Ask, Opt, Question};
use ld_core::hooks::ASK_TIMEOUT_SECS;
use ld_core::labels::label_for_tool;
use ld_core::proto::{PermissionDecision, Request, Response};
use ld_core::race::{Vencedor, disputar};
use serde_json::{Value, json};

/// Um pouco mais que o teto do daemon, para quem desiste primeiro ser sempre ele: assim o
/// caminho de desistência é um só, com card apagado e tudo.
const PRAZO: Duration = Duration::from_secs(ASK_TIMEOUT_SECS + 60);

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

    let texto = match disputar(req, pergunta, PRAZO) {
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

/// O portão de permissão, no `PreToolUse`.
///
/// Fica neste evento por medição, não por gosto: a decisão do `PermissionRequest` é ignorada
/// (o prompt do terminal aparece assim mesmo), e a do `PreToolUse` é honrada até no modo mais
/// estrito. Como `PreToolUse` dispara antes de existir pergunta, quem decide se vale perguntar é
/// o daemon, que conhece o modo da sessão: fora do modo remoto ele responde "não decidi" na hora
/// e a sessão segue o caminho normal.
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

    let permitir = match disputar(req, pergunta, PRAZO) {
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
                "hookEventName": "PreToolUse",
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

#[cfg(test)]
mod tests {
    use super::*;
    use ld_core::ask::Answer;

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
