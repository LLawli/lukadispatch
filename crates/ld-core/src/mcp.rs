//! Monta a configuração de MCP das sessões do bot, com cada servidor passando pelo proxy.
//!
//! O proxy (`lukadispatch-mcp`) só entra em ação se as sessões falarem com ele em vez de falarem
//! direto com o servidor. Como quem lança a sessão é o daemon, dá para gerar um arquivo de
//! configuração próprio e passá-lo em `--mcp-config`.
//!
//! **Por que junta três fontes.** O Claude Code lê servidores de mais de um lugar, e o
//! `--strict-mcp-config` (necessário para o original não subir junto com o embrulhado) ignora
//! todos eles. Então o que não for copiado para cá simplesmente some da sessão. As fontes são:
//!
//! 1. `mcpServers` no topo do `~/.claude.json` (escopo do usuário);
//! 2. `projects.<cwd>.mcpServers` do mesmo arquivo (escopo do projeto);
//! 3. `.mcp.json` na raiz do projeto (escopo do repositório).
//!
//! Servidor que não é de linha de comando (HTTP, SSE) passa **sem embrulho**: o proxy é um cano
//! de stdio, e não teria onde se meter.

use std::path::Path;

use serde_json::{Map, Value, json};

/// Os servidores MCP que valem para um diretório, já mesclados.
///
/// A ordem de precedência é a do próprio Claude Code: o mais específico ganha do mais geral.
/// `projeto` é a chave do projeto no `~/.claude.json`, e `cwd`, onde está o `.mcp.json`. São a
/// mesma pasta, menos numa worktree: o Claude Code guarda os servidores pelo caminho do
/// repositório, e a worktree, noutra pasta, não tem entrada lá.
pub fn servidores_do_projeto(claude_json: &Path, projeto: &str, cwd: &str) -> Map<String, Value> {
    let mut saida = Map::new();

    let doc: Value = std::fs::read_to_string(claude_json)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));

    if let Some(m) = doc.get("mcpServers").and_then(Value::as_object) {
        saida.extend(m.clone());
    }
    if let Some(m) = doc
        .get("projects")
        .and_then(|p| p.get(projeto))
        .and_then(|p| p.get("mcpServers"))
        .and_then(Value::as_object)
    {
        saida.extend(m.clone());
    }

    let do_repo: Value = std::fs::read_to_string(Path::new(cwd).join(".mcp.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    if let Some(m) = do_repo.get("mcpServers").and_then(Value::as_object) {
        saida.extend(m.clone());
    }

    saida
}

/// O arquivo de configuração que a sessão vai carregar, com os servidores de stdio embrulhados.
pub fn config_embrulhada(servidores: &Map<String, Value>, session_id: &str, proxy: &str) -> Value {
    let mut saida = Map::new();
    for (nome, entrada) in servidores {
        saida.insert(nome.clone(), embrulha(entrada, session_id, proxy));
    }
    json!({ "mcpServers": Value::Object(saida) })
}

/// Nomes dos servidores que passaram pelo proxy, para o log dizer o que mudou.
pub fn embrulhados(servidores: &Map<String, Value>) -> Vec<String> {
    servidores
        .iter()
        .filter(|(_, e)| e_stdio(e))
        .map(|(n, _)| n.clone())
        .collect()
}

fn e_stdio(entrada: &Value) -> bool {
    let tipo = entrada.get("type").and_then(Value::as_str);
    let tem_comando = entrada.get("command").and_then(Value::as_str).is_some();
    // `type` costuma vir como "stdio", mas é opcional: uma entrada com `command` é de processo.
    matches!(tipo, Some("stdio") | None) && tem_comando
}

fn embrulha(entrada: &Value, session_id: &str, proxy: &str) -> Value {
    if !e_stdio(entrada) {
        return entrada.clone();
    }
    let comando = entrada
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = entrada
        .get("args")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut novos = vec![
        json!("--session"),
        json!(session_id),
        json!("--"),
        json!(comando),
    ];
    novos.extend(args);

    let mut saida = entrada.clone();
    let obj = saida.as_object_mut().expect("entrada de servidor é objeto");
    obj.insert("type".into(), json!("stdio"));
    obj.insert("command".into(), json!(proxy));
    obj.insert("args".into(), Value::Array(novos));
    saida
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escreve(dir: &Path, nome: &str, v: Value) -> std::path::PathBuf {
        let p = dir.join(nome);
        std::fs::write(&p, v.to_string()).unwrap();
        p
    }

    #[test]
    fn junta_as_tres_fontes_com_o_mais_especifico_ganhando() {
        let dir = tempfile::tempdir().unwrap();
        let projeto = dir.path().join("proj");
        std::fs::create_dir_all(&projeto).unwrap();
        let cwd = projeto.to_string_lossy().into_owned();

        let claude = escreve(
            dir.path(),
            "claude.json",
            json!({
                "mcpServers": {"global": {"command": "g"}, "ambos": {"command": "do-global"}},
                "projects": { cwd.clone(): {"mcpServers": {"ambos": {"command": "do-projeto"}}} }
            }),
        );
        escreve(
            &projeto,
            ".mcp.json",
            json!({"mcpServers": {"repo": {"command": "r"}}}),
        );

        let s = servidores_do_projeto(&claude, &cwd, &cwd);
        assert_eq!(sorted(&s), vec!["ambos", "global", "repo"]);
        assert_eq!(s["ambos"]["command"], "do-projeto");
    }

    #[test]
    fn embrulha_stdio_preservando_comando_args_e_env() {
        let entrada = json!({
            "type": "stdio",
            "command": "ai-memory",
            "args": ["mcp-bridge", "--server-url", "http://127.0.0.1:49374/mcp"],
            "env": {"TOKEN": "x"}
        });
        let e = embrulha(&entrada, "s1", "/bin/lukadispatch-mcp");
        assert_eq!(e["command"], "/bin/lukadispatch-mcp");
        assert_eq!(
            e["args"],
            json!([
                "--session",
                "s1",
                "--",
                "ai-memory",
                "mcp-bridge",
                "--server-url",
                "http://127.0.0.1:49374/mcp"
            ])
        );
        assert_eq!(
            e["env"]["TOKEN"], "x",
            "o ambiente do servidor não pode sumir"
        );
    }

    #[test]
    fn servidor_sem_processo_passa_intacto() {
        // Proxy de stdio não tem onde se meter num servidor HTTP.
        let http = json!({"type": "http", "url": "https://exemplo/mcp"});
        assert_eq!(embrulha(&http, "s1", "/bin/proxy"), http);
        assert!(!e_stdio(&http));
    }

    #[test]
    fn entrada_sem_type_mas_com_comando_conta_como_processo() {
        let sem_tipo = json!({"command": "servidor", "args": []});
        assert!(e_stdio(&sem_tipo));
        assert_eq!(embrulha(&sem_tipo, "s1", "/p")["command"], "/p");
    }

    #[test]
    fn config_gerada_tem_todos_os_servidores() {
        let mut m = Map::new();
        m.insert("a".into(), json!({"command": "a"}));
        m.insert("b".into(), json!({"type": "http", "url": "u"}));
        let c = config_embrulhada(&m, "s1", "/p");
        assert_eq!(sorted(c["mcpServers"].as_object().unwrap()), vec!["a", "b"]);
        assert_eq!(embrulhados(&m), vec!["a"]);
    }

    #[test]
    fn sem_arquivo_nenhum_devolve_vazio() {
        let dir = tempfile::tempdir().unwrap();
        let s = servidores_do_projeto(&dir.path().join("nao-existe.json"), "/tmp/x", "/tmp/x");
        assert!(s.is_empty());
    }

    fn sorted(m: &Map<String, Value>) -> Vec<String> {
        let mut v: Vec<String> = m.keys().cloned().collect();
        v.sort();
        v
    }

    #[test]
    fn worktree_herda_os_servidores_do_repositorio_e_le_o_mcp_json_dela() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude.json");
        std::fs::write(
            &claude,
            r#"{"projects": {"/repo": {"mcpServers": {"do-projeto": {"command": "a"}}}}}"#,
        )
        .unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".mcp.json"),
            r#"{"mcpServers": {"do-repo": {"command": "b"}}}"#,
        )
        .unwrap();
        let s = servidores_do_projeto(&claude, "/repo", &wt.to_string_lossy());
        assert!(
            s.contains_key("do-projeto") && s.contains_key("do-repo"),
            "{s:?}"
        );
    }
}
