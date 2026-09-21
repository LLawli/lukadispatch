//! Confiança de pasta do Claude Code.
//!
//! Ao abrir uma pasta que ainda não é confiada, o Claude Code mostra um diálogo ("Is this a
//! project you created or one you trust?") e espera uma tecla. Numa sessão do lukadispatch isso
//! é a pior falha possível: a sessão sobe, não responde nada, e do celular não há como ver o
//! motivo nem como apertar a tecla.
//!
//! A confiança vale para a árvore: com `/home/luka` confiado, tudo abaixo dele já entra. Por
//! isso, na prática, isto só age em projeto fora do seu diretório pessoal.
//!
//! **Só é chamado para projeto que o próprio config oferece** (fixado no `config.toml` ou achado
//! nas raízes de varredura). O daemon nunca confia num caminho arbitrário vindo de mensagem.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// Marca o caminho como confiado. Devolve `true` quando precisou mexer no arquivo.
///
/// O `~/.claude.json` é escrito pelo próprio Claude Code enquanto trabalha, então aqui a escrita
/// é atômica (arquivo temporário e rename) e só acontece quando há mudança de verdade, que é uma
/// vez por projeto novo.
pub fn ensure_trusted(claude_json: &Path, projeto: &Path) -> Result<bool> {
    let chave = projeto.to_string_lossy().into_owned();
    let bruto = std::fs::read_to_string(claude_json).unwrap_or_else(|_| "{}".into());
    let mut doc: Value = serde_json::from_str(&bruto).unwrap_or_else(|_| json!({}));

    if ja_confiado(&doc, &chave) {
        return Ok(false);
    }

    let projetos = doc
        .as_object_mut()
        .context("~/.claude.json não é um objeto")?
        .entry("projects")
        .or_insert_with(|| json!({}));
    if !projetos.is_object() {
        *projetos = json!({});
    }
    let entrada = projetos
        .as_object_mut()
        .expect("projects vira objeto acima")
        .entry(chave)
        .or_insert_with(|| json!({}));
    if !entrada.is_object() {
        *entrada = json!({});
    }
    entrada["hasTrustDialogAccepted"] = json!(true);

    escreve_atomico(claude_json, &doc)?;
    Ok(true)
}

/// Confiado diretamente ou por herança de um diretório acima.
fn ja_confiado(doc: &Value, caminho: &str) -> bool {
    let Some(projetos) = doc.get("projects").and_then(Value::as_object) else {
        return false;
    };
    projetos.iter().any(|(k, v)| {
        v.get("hasTrustDialogAccepted") == Some(&json!(true))
            && (k == caminho || caminho.starts_with(&format!("{k}/")))
    })
}

fn escreve_atomico(destino: &Path, doc: &Value) -> Result<()> {
    if destino.exists() {
        let _ = std::fs::copy(destino, destino.with_extension("json.lukadispatch.bak"));
    }
    let temporario = destino.with_extension("json.lukadispatch.tmp");
    std::fs::write(&temporario, serde_json::to_string_pretty(doc)?)
        .with_context(|| format!("escrevendo {}", temporario.display()))?;
    std::fs::rename(&temporario, destino).context("trocando o ~/.claude.json")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arquivo(conteudo: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("claude.json");
        std::fs::write(&p, conteudo).unwrap();
        (dir, p)
    }

    #[test]
    fn confia_projeto_novo() {
        let (_d, p) = arquivo(r#"{"projects":{}}"#);
        assert!(ensure_trusted(&p, Path::new("/home/luka/x")).unwrap());
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(
            doc["projects"]["/home/luka/x"]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn nao_mexe_no_arquivo_quando_ja_confiado() {
        let (_d, p) = arquivo(r#"{"projects":{"/home/luka/x":{"hasTrustDialogAccepted":true}}}"#);
        let antes = std::fs::read_to_string(&p).unwrap();
        assert!(!ensure_trusted(&p, Path::new("/home/luka/x")).unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), antes);
    }

    #[test]
    fn confianca_do_pai_vale_para_o_filho() {
        // É como o Claude Code se comporta: com a home confiada, todo projeto dentro dela entra.
        let (_d, p) = arquivo(r#"{"projects":{"/home/luka":{"hasTrustDialogAccepted":true}}}"#);
        assert!(!ensure_trusted(&p, Path::new("/home/luka/Personal/proj")).unwrap());
    }

    #[test]
    fn prefixo_parecido_nao_conta_como_pai() {
        // /home/lukas não é filho de /home/luka.
        let (_d, p) = arquivo(r#"{"projects":{"/home/luka":{"hasTrustDialogAccepted":true}}}"#);
        assert!(ensure_trusted(&p, Path::new("/home/lukas/proj")).unwrap());
    }

    #[test]
    fn preserva_o_resto_do_arquivo() {
        let (_d, p) = arquivo(
            r#"{"numStartups":42,"projects":{"/a":{"lastCost":1.5,"hasTrustDialogAccepted":false}}}"#,
        );
        ensure_trusted(&p, Path::new("/a")).unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc["numStartups"], 42);
        assert_eq!(doc["projects"]["/a"]["lastCost"], 1.5);
        assert_eq!(doc["projects"]["/a"]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn arquivo_ausente_nao_impede() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nao-existe.json");
        assert!(ensure_trusted(&p, Path::new("/a")).unwrap());
        assert!(p.exists());
    }
}
