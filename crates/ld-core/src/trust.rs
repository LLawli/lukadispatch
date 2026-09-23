//! Confiança de pasta do Claude Code.
//!
//! Ao abrir uma pasta que ainda não é confiada, o Claude Code mostra um diálogo ("Is this a
//! project you created or one you trust?") e espera uma tecla. Numa sessão do lukadispatch isso
//! é a pior falha possível: a sessão sobe, não responde nada, e do celular não há como ver o
//! motivo nem como apertar a tecla.
//!
//! A confiança é herdada de um diretório acima, **mas só até a raiz do repositório git**. Medido
//! no binário do Claude Code 2.1.280: a subida começa na pasta aberta e para no primeiro
//! diretório com `.git` (diretório ou arquivo, nunca symlink); sem `.git`, vai até a raiz do
//! disco. Então uma home confiada cobre pasta solta, mas não cobre projeto nenhum com `.git`, e
//! todo projeto da varredura tem um. Tratar a home como suficiente já travou sessão no diálogo.
//!
//! **Só é chamado para projeto que o próprio config oferece** (fixado no `config.toml` ou achado
//! nas raízes de varredura). O daemon nunca confia num caminho arbitrário vindo de mensagem.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

/// Marca o caminho como confiado. Devolve `true` quando precisou mexer no arquivo.
///
/// O `~/.claude.json` é escrito pelo próprio Claude Code enquanto trabalha, então aqui a escrita
/// é atômica (arquivo temporário e rename) e só acontece quando há mudança de verdade, que é uma
/// vez por projeto novo.
pub fn ensure_trusted(claude_json: &Path, projeto: &Path) -> Result<bool> {
    // A chave do Claude Code é o cwd do processo, que já vem com os symlinks resolvidos.
    let projeto = std::fs::canonicalize(projeto).unwrap_or_else(|_| projeto.to_path_buf());
    let chave = projeto.to_string_lossy().into_owned();
    let bruto = std::fs::read_to_string(claude_json).unwrap_or_else(|_| "{}".into());
    let mut doc: Value = serde_json::from_str(&bruto).unwrap_or_else(|_| json!({}));

    if ja_confiado(&doc, &projeto, raiz_git(&projeto).as_deref()) {
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

/// Confiado diretamente ou por herança de um diretório acima, sem passar da raiz git.
fn ja_confiado(doc: &Value, caminho: &Path, raiz: Option<&Path>) -> bool {
    let Some(projetos) = doc.get("projects").and_then(Value::as_object) else {
        return false;
    };
    let confiado = |p: &Path| {
        projetos
            .get(p.to_string_lossy().as_ref())
            .and_then(|v| v.get("hasTrustDialogAccepted"))
            == Some(&json!(true))
    };
    for p in caminho.ancestors() {
        if confiado(p) {
            return true;
        }
        if Some(p) == raiz {
            return false;
        }
    }
    false
}

/// O primeiro diretório, subindo a partir de `caminho`, que tem um `.git` diretório ou arquivo.
///
/// Arquivo cobre worktree e submódulo. Symlink não conta, porque o Claude Code também não conta.
fn raiz_git(caminho: &Path) -> Option<PathBuf> {
    caminho
        .ancestors()
        .find(|p| {
            std::fs::symlink_metadata(p.join(".git"))
                .map(|m| m.is_dir() || m.is_file())
                .unwrap_or(false)
        })
        .map(Path::to_path_buf)
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
    use std::path::PathBuf;

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
    fn confianca_do_pai_vale_para_o_filho_fora_de_repositorio() {
        // Pasta solta, sem `.git` acima: o Claude Code sobe até a raiz do disco procurando.
        let (_d, p) = arquivo(r#"{"projects":{"/home/luka":{"hasTrustDialogAccepted":true}}}"#);
        assert!(!ensure_trusted(&p, Path::new("/home/luka/Personal/proj")).unwrap());
    }

    /// `home/` confiada no claude.json, e `home/<nome>` com um `.git` do tipo pedido.
    fn home_confiada_com_repo(git: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap().join("home");
        let repo = home.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        match git {
            "dir" => std::fs::create_dir(repo.join(".git")).unwrap(),
            "arquivo" => std::fs::write(repo.join(".git"), "gitdir: /outro/lugar\n").unwrap(),
            "link" => std::os::unix::fs::symlink(dir.path(), repo.join(".git")).unwrap(),
            _ => unreachable!(),
        }
        let claude = dir.path().join("claude.json");
        let doc = json!({"projects": {home.to_str().unwrap(): {"hasTrustDialogAccepted": true}}});
        std::fs::write(&claude, doc.to_string()).unwrap();
        (dir, claude, repo)
    }

    #[test]
    fn confianca_acima_do_repositorio_nao_vale() {
        // O caso que travou uma sessão: home confiada, projeto com `.git`. O Claude Code 2.1.280
        // para de subir na raiz do repositório, então a home não conta e o diálogo aparece.
        let (_d, claude, repo) = home_confiada_com_repo("dir");
        assert!(ensure_trusted(&claude, &repo).unwrap());
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
        assert_eq!(
            doc["projects"][repo.to_str().unwrap()]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn git_em_arquivo_tambem_e_raiz() {
        // Worktree e submódulo têm `.git` como arquivo apontando para outro lugar.
        let (_d, claude, repo) = home_confiada_com_repo("arquivo");
        assert!(ensure_trusted(&claude, &repo).unwrap());
    }

    #[test]
    fn git_que_e_symlink_nao_e_raiz() {
        // O Claude Code ignora `.git` que é link, e continua subindo; aqui também.
        let (_d, claude, repo) = home_confiada_com_repo("link");
        assert!(!ensure_trusted(&claude, &repo).unwrap());
    }

    #[test]
    fn dentro_do_repositorio_a_raiz_confiada_vale() {
        let (_d, claude, repo) = home_confiada_com_repo("dir");
        ensure_trusted(&claude, &repo).unwrap();
        let sub = repo.join("crates").join("x");
        std::fs::create_dir_all(&sub).unwrap();
        assert!(!ensure_trusted(&claude, &sub).unwrap());
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
