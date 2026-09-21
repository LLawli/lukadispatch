//! Tradução de `(tool_name, tool_input)` para o rótulo que aparece na mensagem de status.
//!
//! Diferente do sdispath, que escondia o comando do Bash de propósito, aqui o comando aparece:
//! o grupo é privado e o pedido foi justamente ver quais comandos estão rodando. O que continua
//! valendo é o teto de tamanho, porque a mensagem de status é editada a cada evento e o Telegram
//! cobra caro por mensagem longa (e a leitura no celular piora).

use serde_json::Value;

/// Teto de caracteres do trecho variável do rótulo (comando, caminho, consulta).
const TETO: usize = 120;

pub fn label_for_tool(tool: &str, input: &Value) -> String {
    let arg = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    match tool {
        "Bash" => {
            let cmd = arg("command");
            if cmd.is_empty() {
                "Rodando comando".into()
            } else {
                format!("$ {}", corta(cmd))
            }
        }
        "Read" => format!("Lendo {}", basename(arg("file_path"))),
        "Write" => format!("Escrevendo {}", basename(arg("file_path"))),
        "Edit" => format!("Editando {}", basename(arg("file_path"))),
        "NotebookEdit" => format!("Editando {}", basename(arg("notebook_path"))),
        "Glob" => format!("Procurando {}", corta(arg("pattern"))),
        "Grep" => format!("Buscando {}", corta(arg("pattern"))),
        "WebFetch" => format!("Abrindo {}", corta(arg("url"))),
        "WebSearch" => format!("Pesquisando {}", corta(arg("query"))),
        "Task" | "Agent" => "Rodando subagente".into(),
        "Monitor" => "Reatando o canal do Telegram".into(),
        "TodoWrite" => "Atualizando o plano".into(),
        "AskUserQuestion" => "Perguntando".into(),
        outro if outro.starts_with("mcp__") => {
            // mcp__servidor__ferramenta: mostra só a parte que identifica a ação.
            let curto = outro.rsplit("__").next().unwrap_or(outro);
            format!("MCP: {curto}")
        }
        outro => outro.to_string(),
    }
}

/// Primeira linha, sem espaços nas pontas, cortada no teto. Um comando com quebra de linha
/// viraria várias linhas na mensagem de status, então só a primeira interessa.
fn corta(s: &str) -> String {
    let uma_linha = s.lines().next().unwrap_or("").trim();
    let n = uma_linha.chars().count();
    if n <= TETO {
        return uma_linha.to_string();
    }
    let cortado: String = uma_linha.chars().take(TETO).collect();
    format!("{cortado}…")
}

fn basename(caminho: &str) -> String {
    if caminho.is_empty() {
        return "arquivo".into();
    }
    caminho
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(caminho)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bash_mostra_o_comando() {
        let l = label_for_tool("Bash", &json!({"command": "cargo test --all"}));
        assert_eq!(l, "$ cargo test --all");
    }

    #[test]
    fn comando_multilinha_vira_uma_linha_so() {
        let l = label_for_tool("Bash", &json!({"command": "echo a\necho b"}));
        assert_eq!(l, "$ echo a");
    }

    #[test]
    fn comando_longo_e_cortado() {
        let cmd = "x".repeat(400);
        let l = label_for_tool("Bash", &json!({ "command": cmd }));
        assert!(l.chars().count() <= TETO + 3, "rótulo passou do teto: {l}");
        assert!(l.ends_with('…'));
    }

    #[test]
    fn arquivo_vira_basename() {
        let l = label_for_tool("Read", &json!({"file_path": "/home/luka/x/src/main.rs"}));
        assert_eq!(l, "Lendo main.rs");
    }

    #[test]
    fn mcp_encurta() {
        let l = label_for_tool("mcp__ai-memory__memory_query", &json!({}));
        assert_eq!(l, "MCP: memory_query");
    }

    #[test]
    fn ferramenta_sem_regra_usa_o_proprio_nome() {
        assert_eq!(label_for_tool("Skill", &json!({})), "Skill");
    }
}
