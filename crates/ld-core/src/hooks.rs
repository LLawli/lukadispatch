//! Geração dos blocos de hooks que o Claude Code carrega.
//!
//! Dois conjuntos, e a diferença entre eles é deliberada:
//!
//! - **Sessão do bot** (`bot_settings`, passado em `claude --settings`): telemetria mais os
//!   hooks que respondem por você (pergunta e permissão vão para o Telegram).
//! - **Máquina inteira** (`telemetry_hooks`, acrescentado ao `~/.claude/settings.json`): só
//!   telemetria. As suas sessões de terminal entram no painel sem ter a pergunta sequestrada:
//!   lá o menu nativo do Claude Code continua sendo o certo.
//!
//! Três propriedades do formato de hook sustentam a regra de "o daemon fora do ar não pode parar
//! o Claude":
//!
//! - `async: true` roda em segundo plano e descarta a saída. Um hook assim **não tem como**
//!   bloquear a sessão, nem decidir nada. É o que toda a telemetria usa.
//! - `asyncRewake: true` roda em segundo plano e acorda o Claude quando sai com código 2. É o
//!   que o `Stop` usa para mandar re-armar o Monitor sem segurar o fim do turno.
//! - `args` dispara o binário direto, sem shell no meio, então não há nada para escapar.

use serde_json::{Value, json};

/// Ferramentas que passam pelo portão de permissão do lukadispatch: todas.
///
/// A primeira versão listava só as que escrevem ou executam, e isso tinha um buraco: no modo
/// remoto a sessão roda em `dontAsk`, que **nega por padrão**, então tudo que ficasse fora da
/// lista seria recusado em silêncio, sem card e sem você saber. Cobrindo todas, nada é negado
/// sem decisão: o daemon pergunta para o que merece pergunta e libera o resto na hora.
pub const GATE_TOOLS: &str = "";

/// Espera de resposta humana, em segundos. Seis horas: o menu nativo do Claude Code espera
/// indefinidamente, e o sdispath mediu que meia hora deixava card fantasma em sessão abandonada.
/// O teto existe só para a pendência não viver para sempre.
pub const ASK_TIMEOUT_SECS: u64 = 6 * 60 * 60;

fn async_hook(cli: &str, sub: &str) -> Value {
    json!({
        "type": "command",
        "command": cli,
        "args": ["hook", sub],
        "async": true
    })
}

fn blocking_hook(cli: &str, sub: &str, timeout: u64, status: &str) -> Value {
    json!({
        "type": "command",
        "command": cli,
        "args": ["hook", sub],
        "timeout": timeout,
        "statusMessage": status
    })
}

/// Hooks de telemetria: os que só contam o que aconteceu. Servem para qualquer sessão da
/// máquina e nunca decidem nada.
pub fn telemetry_hooks(cli: &str) -> Value {
    json!({
        "SessionStart": [ { "hooks": [ async_hook(cli, "session-start") ] } ],
        "SessionEnd":   [ { "hooks": [ async_hook(cli, "session-end")   ] } ],
        // Prompt digitado no teclado do PC (tmux attach): vira mensagem no tópico, para quem
        // está no celular não ver só a resposta, sem a pergunta.
        "UserPromptSubmit": [ { "hooks": [ async_hook(cli, "prompt") ] } ],
        "PreToolUse":   [ { "matcher": "", "hooks": [ async_hook(cli, "tool-start") ] } ],
        "PostToolUse":  [ { "matcher": "", "hooks": [ async_hook(cli, "tool-end")   ] } ],
        "PostToolUseFailure": [ { "matcher": "", "hooks": [ async_hook(cli, "tool-failed") ] } ],
        "Notification": [ { "hooks": [ async_hook(cli, "notification") ] } ],
        // Diálogo próprio de servidor MCP. Só dá para avisar: o hook não pode responder por
        // você, e sem o aviso a sessão fica parada sem explicação nenhuma no celular.
        "Elicitation":       [ { "hooks": [ async_hook(cli, "elicitation") ] } ],
        "ElicitationResult": [ { "hooks": [ async_hook(cli, "elicitation-result") ] } ],
        // Troca de modelo feita no teclado do PC (`/model`): sem isto o painel mentiria.
        "PostModelSwitch": [ { "hooks": [ async_hook(cli, "model-switch") ] } ],
    })
}

/// Settings completo de uma sessão do bot. Além da telemetria, entram os três hooks que
/// realmente conversam: `Stop` (resposta e re-arme), `AskUserQuestion` e `PermissionRequest`.
pub fn bot_settings(cli: &str) -> Value {
    let mut hooks = telemetry_hooks(cli);

    // Stop faz duas coisas num tiro só: entrega a resposta final ao Telegram e, se o Monitor da
    // sessão tiver expirado, sai com 2 para o Claude acordar e re-armar. Em segundo plano, então
    // o turno termina na hora de qualquer jeito.
    hooks["Stop"] = json!([ {
        "hooks": [ {
            "type": "command",
            "command": cli,
            "args": ["hook", "stop"],
            "asyncRewake": true,
            "timeout": 120
        } ]
    } ]);

    // A pergunta é o único hook que segura a sessão de propósito: é uma pessoa que precisa
    // responder. O menu do terminal não chega a aparecer, e é isso mesmo: a janela GTK4 é o
    // substituto dele, e assim não existe injeção de teclas em canto nenhum.
    // O portão de permissão mora no `PreToolUse`, e não no `PermissionRequest`, por medição:
    // a decisão do `PermissionRequest` é ignorada em todos os modos testados (o prompt do
    // terminal aparece assim mesmo e a sessão fica esperando teclado), enquanto a do `PreToolUse`
    // é honrada até no modo mais estrito. Ver `docs/permissoes.md`.
    //
    // O matcher é a lista do que vale perguntar. Ferramenta de leitura não entra: card a cada
    // `Read` tornaria o celular inútil, e negar leitura não protege nada.
    hooks["PreToolUse"] = json!([
        {
            "matcher": "AskUserQuestion",
            "hooks": [ blocking_hook(cli, "ask", ASK_TIMEOUT_SECS, "Perguntando no Telegram...") ]
        },
        {
            "matcher": GATE_TOOLS,
            "hooks": [ blocking_hook(cli, "permission", ASK_TIMEOUT_SECS, "Esperando você liberar...") ]
        },
        { "matcher": "", "hooks": [ async_hook(cli, "tool-start") ] }
    ]);

    json!({
        "hooks": hooks,
        // O canal da sessão é infraestrutura deste sistema, não iniciativa do agente. Sem esta
        // regra, o modo "perguntar sempre" pede autorização para o `listen` a cada re-arme, e a
        // sessão trava esperando uma tecla que ninguém vai apertar: quem está no celular não vê
        // o prompt, e o próprio canal que levaria a pergunta até lá é o que está bloqueado.
        "permissions": {
            "allow": [format!("Bash({cli} listen *)")]
        }
    })
}

/// Um hook é nosso quando ele chama o binário `lukadispatch`. Serve para instalar e desinstalar
/// sem tocar em nada que não seja nosso, inclusive quando o caminho do binário mudou entre uma
/// instalação e outra (`~/.cargo/bin` hoje, `/usr/local/bin` amanhã).
fn e_nosso(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .map(|c| c == "lukadispatch" || c.ends_with("/lukadispatch"))
        .unwrap_or(false)
}

/// Tira todos os hooks do lukadispatch de um settings, preservando o resto byte a byte.
pub fn strip(settings: &mut Value) {
    let Some(eventos) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for (_, grupos) in eventos.iter_mut() {
        let Some(lista) = grupos.as_array_mut() else {
            continue;
        };
        for grupo in lista.iter_mut() {
            if let Some(hooks) = grupo.get_mut("hooks").and_then(Value::as_array_mut) {
                hooks.retain(|h| !e_nosso(h));
            }
        }
        // Grupo que ficou sem hook nenhum era só nosso: some com ele.
        lista.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|h| !h.is_empty())
        });
    }
    eventos.retain(|_, grupos| grupos.as_array().is_none_or(|l| !l.is_empty()));
    if eventos.is_empty() {
        settings.as_object_mut().map(|o| o.remove("hooks"));
    }
}

/// Acrescenta os nossos hooks a um settings existente, sem duplicar.
///
/// Sempre faz `strip` antes: instalar duas vezes não pode gerar dois hooks iguais, e reinstalar
/// depois de mudar o caminho do binário tem que substituir o antigo, não somar.
pub fn merge_into(settings: &mut Value, nossos: &Value) {
    strip(settings);
    if !settings.is_object() {
        *settings = json!({});
    }
    let raiz = settings
        .as_object_mut()
        .expect("settings vira objeto acima");
    let destino = raiz.entry("hooks").or_insert_with(|| json!({}));
    if !destino.is_object() {
        *destino = json!({});
    }
    let destino = destino.as_object_mut().expect("hooks vira objeto acima");

    for (evento, grupos) in nossos.as_object().into_iter().flatten() {
        let alvo = destino.entry(evento).or_insert_with(|| json!([]));
        if !alvo.is_array() {
            *alvo = json!([]);
        }
        if let (Some(a), Some(b)) = (alvo.as_array_mut(), grupos.as_array()) {
            a.extend(b.iter().cloned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetria_nunca_bloqueia() {
        let h = telemetry_hooks("lukadispatch");
        for (evento, grupos) in h.as_object().unwrap() {
            for grupo in grupos.as_array().unwrap() {
                for hook in grupo["hooks"].as_array().unwrap() {
                    assert_eq!(
                        hook["async"], true,
                        "{evento}: hook de telemetria precisa ser async"
                    );
                }
            }
        }
    }

    #[test]
    fn telemetria_nao_traz_pergunta_nem_permissao() {
        // A garantia da instalação global: sessão de terminal não tem menu sequestrado.
        let h = telemetry_hooks("lukadispatch");
        assert!(h.get("PermissionRequest").is_none());
        assert!(h.get("Stop").is_none());
        let pre = h["PreToolUse"].as_array().unwrap();
        assert!(pre.iter().all(|g| g["matcher"] == ""));
    }

    #[test]
    fn settings_do_bot_tem_os_tres_canais() {
        let s = bot_settings("lukadispatch");
        let h = &s["hooks"];
        assert_eq!(h["Stop"][0]["hooks"][0]["asyncRewake"], true);
        assert_eq!(h["PreToolUse"][0]["matcher"], "AskUserQuestion");
        assert_eq!(h["PreToolUse"][0]["hooks"][0]["timeout"], ASK_TIMEOUT_SECS);
        // O portão de permissão é o segundo grupo, e mora no PreToolUse porque a decisão do
        // PermissionRequest é ignorada (medido em todos os modos).
        assert_eq!(h["PreToolUse"][1]["hooks"][0]["args"][1], "permission");
        assert_eq!(h["PreToolUse"][2]["hooks"][0]["args"][1], "tool-start");
    }

    #[test]
    fn o_canal_da_sessao_nao_pede_permissao() {
        // Sem isto, no modo "perguntar sempre" a sessão trava no re-arme do monitor: o prompt
        // aparece só no terminal, e o canal que levaria a pergunta ao celular é justamente o que
        // está esperando autorização.
        let s = bot_settings("/usr/bin/lukadispatch");
        let regras = s["permissions"]["allow"].as_array().unwrap();
        assert!(
            regras
                .iter()
                .any(|r| r.as_str() == Some("Bash(/usr/bin/lukadispatch listen *)")),
            "regra ausente: {regras:?}"
        );
    }

    #[test]
    fn stop_nao_segura_o_turno() {
        let s = bot_settings("lukadispatch");
        let stop = &s["hooks"]["Stop"][0]["hooks"][0];
        assert!(
            stop.get("async").is_none(),
            "Stop usa asyncRewake, que não é a mesma coisa que async"
        );
        assert_eq!(stop["asyncRewake"], true);
    }

    #[test]
    fn comando_vai_por_args_sem_shell() {
        let s = bot_settings("/usr/bin/lukadispatch");
        let hook = &s["hooks"]["SessionStart"][0]["hooks"][0];
        assert_eq!(hook["command"], "/usr/bin/lukadispatch");
        assert_eq!(hook["args"][0], "hook");
    }

    fn settings_do_usuario() -> Value {
        json!({
            "permissions": { "defaultMode": "auto" },
            "hooks": {
                "PostToolUse": [
                    { "hooks": [ { "type": "command", "command": "/home/luka/.claude/bin/xclaudeusage", "args": ["record"] } ] }
                ]
            }
        })
    }

    #[test]
    fn instalar_preserva_o_que_ja_existia() {
        let mut s = settings_do_usuario();
        merge_into(&mut s, &telemetry_hooks("lukadispatch"));

        assert_eq!(s["permissions"]["defaultMode"], "auto");
        let post = s["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(
            post.len(),
            2,
            "o hook do usuário continua lá, o nosso entrou"
        );
        assert_eq!(
            post[0]["hooks"][0]["command"],
            "/home/luka/.claude/bin/xclaudeusage"
        );
    }

    #[test]
    fn instalar_duas_vezes_nao_duplica() {
        let mut s = settings_do_usuario();
        merge_into(&mut s, &telemetry_hooks("lukadispatch"));
        let depois_de_uma = s.clone();
        merge_into(&mut s, &telemetry_hooks("lukadispatch"));
        assert_eq!(s, depois_de_uma);
    }

    #[test]
    fn reinstalar_com_outro_caminho_substitui() {
        let mut s = settings_do_usuario();
        merge_into(&mut s, &telemetry_hooks("lukadispatch"));
        merge_into(&mut s, &telemetry_hooks("/usr/local/bin/lukadispatch"));

        let nossos: Vec<&Value> = s["hooks"]["SessionStart"][0]["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .collect();
        assert_eq!(nossos.len(), 1);
        assert_eq!(nossos[0]["command"], "/usr/local/bin/lukadispatch");
    }

    #[test]
    fn desinstalar_devolve_o_settings_original() {
        let original = settings_do_usuario();
        let mut s = original.clone();
        merge_into(&mut s, &telemetry_hooks("lukadispatch"));
        strip(&mut s);
        assert_eq!(s, original, "sair não pode deixar rastro");
    }

    #[test]
    fn settings_vazio_aceita_instalacao() {
        let mut s = json!({});
        merge_into(&mut s, &telemetry_hooks("lukadispatch"));
        assert!(s["hooks"]["SessionStart"].is_array());
        strip(&mut s);
        assert_eq!(s, json!({}), "e volta a ficar vazio");
    }
}
