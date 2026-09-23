//! Os ganchos do Claude Code: `lukadispatch hook claude <evento>` (ou só `hook <evento>`).
//!
//! Traduz o JSON que o Claude Code manda em cada gancho para o protocolo neutro do socket
//! (`ld_core::proto`). É o lado de entrada da porta do agente: o daemon não sabe o formato do
//! payload do Claude Code, só recebe `RegisterSession`, `SessionEvent`, `StopReport`, pergunta e
//! permissão.
//!
//! Todos seguem a mesma regra: falam com o daemon e **saem com 0**. A única exceção é o `stop`,
//! que sai com 2 de propósito quando o monitor precisa voltar, porque é assim que o
//! `asyncRewake` acorda o Claude.
//!
//! Nada aqui imprime no stdout: em hook de decisão, stdout é canal de protocolo, e sujeira lá
//! vira comportamento estranho na sessão.

use std::time::Duration;

use ld_core::labels::label_for_tool;
use ld_core::proto::{EventKind, RegisterSession, Request, Response, SessionEvent, StopReport};
use serde_json::Value;

use super::Ganchos;
use crate::client;

/// O `stop` manda a resposta final para o chat, então espera a ida e volta da API. Ele roda
/// em `asyncRewake`, fora do caminho do turno, então essa espera não atrasa você.
const PRAZO_STOP: Duration = Duration::from_secs(60);

/// O Claude Code como fonte de ganchos.
pub struct Claude;

impl Ganchos for Claude {
    fn nome(&self) -> &'static str {
        "claude"
    }

    fn trata(&self, evento: &str, ev: Value) -> i32 {
        trata(evento, ev)
    }
}

fn trata(evento: &str, ev: Value) -> i32 {
    let Some(session_id) = texto(&ev, "session_id") else {
        return 0;
    };

    match evento {
        "session-start" => {
            client::send(&Request::Register(RegisterSession {
                session_id,
                cwd: texto(&ev, "cwd").unwrap_or_default(),
                transcript_path: texto(&ev, "transcript_path").unwrap_or_default(),
                reason: texto(&ev, "session_start_reason").unwrap_or_else(|| "startup".into()),
                model: texto(&ev, "model"),
            }));
        }

        "session-end" => {
            client::send(&Request::SessionEnd {
                session_id,
                reason: texto(&ev, "reason").unwrap_or_default(),
            });
        }

        "tool-start" => {
            let tool = texto(&ev, "tool_name").unwrap_or_default();
            let entrada = ev.get("tool_input").cloned().unwrap_or(Value::Null);
            client::send(&Request::Event(SessionEvent {
                session_id,
                event: EventKind::ToolStart {
                    label: label_for_tool(&tool, &entrada),
                    tool,
                    effort: texto(&ev, "effort"),
                },
            }));
        }

        "tool-end" | "tool-failed" => {
            let tool = texto(&ev, "tool_name").unwrap_or_default();
            client::send(&Request::Event(SessionEvent {
                session_id,
                event: EventKind::ToolEnd {
                    ok: evento == "tool-end",
                    tool,
                },
            }));
        }

        "notification" => {
            let texto_ev = texto(&ev, "message")
                .or_else(|| texto(&ev, "notification_type"))
                .unwrap_or_default();
            // O aviso de "esperando permissão" já vira card próprio; repetir aqui só polui.
            if texto_ev.is_empty() || texto_ev.contains("permission") {
                return 0;
            }
            client::send(&Request::Event(SessionEvent {
                session_id,
                event: EventKind::Notification { text: texto_ev },
            }));
        }

        "prompt" => {
            // O campo é `prompt`. A documentação chama de `user_prompt`, e não é o que chega:
            // medido no evento real, que traz `cwd`, `hook_event_name`, `permission_mode`,
            // `prompt`, `prompt_id`, `session_id` e `transcript_path`. O nome documentado fica
            // como segunda tentativa, para uma versão futura não quebrar isto em silêncio.
            let Some(texto_prompt) = texto(&ev, "prompt").or_else(|| texto(&ev, "user_prompt"))
            else {
                return 0;
            };
            // O evento dispara para TUDO que entra como prompt: inclusive os prompts que o
            // daemon injeta e as notificações de evento do Monitor. Espelhar isso encheria o
            // tópico de encanamento assinado como se fosse você.
            if !ld_core::transcript::e_fala_digitada(&texto_prompt) {
                return 0;
            }
            client::send(&Request::Event(SessionEvent {
                session_id,
                event: EventKind::UserPrompt { text: texto_prompt },
            }));
        }

        "elicitation" => {
            // Os nomes dos campos não estão documentados; lemos os prováveis e caímos para o
            // JSON cru, que é melhor que não avisar nada.
            let servidor = texto(&ev, "mcp_server")
                .or_else(|| texto(&ev, "server"))
                .unwrap_or_else(|| "MCP".into());
            let pedido = texto(&ev, "elicitation_request")
                .or_else(|| texto(&ev, "message"))
                .or_else(|| texto(&ev, "request"))
                .or_else(|| texto(&ev, "mcp_tool"))
                .unwrap_or_else(|| "confirmação".into());
            client::send(&Request::Event(SessionEvent {
                session_id,
                event: EventKind::Elicitation { servidor, pedido },
            }));
        }

        "elicitation-result" => {
            client::send(&Request::Event(SessionEvent {
                session_id,
                event: EventKind::ElicitationFim,
            }));
        }

        "model-switch" => {
            let Some(para) = texto(&ev, "to_model") else {
                return 0;
            };
            client::send(&Request::Event(SessionEvent {
                session_id,
                event: EventKind::ModelSwitch { model: para },
            }));
        }

        "stop" => return stop(&ev, session_id),

        "ask" => return crate::ask::ask(&ev, session_id),

        "permission" => return crate::ask::permission(&ev, session_id),

        _ => {}
    }
    0
}

/// Fim de turno: entrega a resposta e, se o monitor tiver caído, acorda o Claude para re-armar.
fn stop(ev: &Value, session_id: String) -> i32 {
    let req = Request::Stop(StopReport {
        session_id,
        transcript_path: texto(ev, "transcript_path"),
        last_assistant_message: texto(ev, "last_assistant_message"),
    });

    let Some(Response::Listener {
        alive: false,
        rearm_command: Some(cmd),
        ..
    }) = client::call(&req, PRAZO_STOP)
    else {
        return 0;
    };

    // stderr, e não stdout: é o que o asyncRewake mostra ao Claude como lembrete.
    eprintln!(
        "O canal de mensagens desta sessão está sem monitor armado, então mensagens novas não \
         chegam até você. Arme-o agora: chame ToolSearch com query \"select:Monitor\" e depois \
         Monitor com command \"{cmd}\", description \"mensagens do chat\" e timeout_ms \
         1800000. Faça isso antes de qualquer outra coisa e não responda mais nada depois."
    );
    2
}

fn texto(v: &Value, chave: &str) -> Option<String> {
    v.get(chave)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn texto_ignora_vazio_e_tipo_errado() {
        let v = json!({"a": "", "b": 3, "c": "ok"});
        assert!(texto(&v, "a").is_none());
        assert!(texto(&v, "b").is_none());
        assert_eq!(texto(&v, "c").as_deref(), Some("ok"));
    }

    #[test]
    fn evento_sem_campo_util_nao_vira_pedido() {
        // `run` não entra aqui de propósito: ele lê o stdin, e sob `cargo test` isso pendura o
        // processo de teste para sempre (sete deles ficaram vivos na máquina até eu perceber).
        // O que dá para verificar sem efeito colateral é a leitura, que é onde mora a decisão de
        // sair cedo.
        let vazio = json!({});
        assert!(texto(&vazio, "session_id").is_none());
        assert!(texto(&json!({"session_id": ""}), "session_id").is_none());
    }
}
