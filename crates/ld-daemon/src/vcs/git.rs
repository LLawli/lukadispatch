//! As git worktrees das sessões: uma por branch, fora do repositório.
//!
//! O que o git impõe, e que molda o resto: a mesma branch não fica em checkout em duas worktrees,
//! e a worktree é uma raiz git própria (um arquivo `.git`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ld_core::state::Worktree;
use tokio::process::Command;

use super::{Pendencias, Ramo, Termos, Vcs};

pub struct Git;

#[async_trait::async_trait]
impl Vcs for Git {
    fn nome(&self) -> &'static str {
        "git"
    }

    fn termos(&self) -> Termos {
        Termos {
            nome: "worktree",
            a: "a worktree",
            na: "na worktree",
            da: "da worktree",
            so_base: "em uso: nova a partir dela",
        }
    }

    async fn tem_commit(&self, raiz: &Path) -> bool {
        git(raiz, &["rev-parse", "--verify", "--quiet", "HEAD"])
            .await
            .is_ok()
    }

    /// A que o `origin` aponta como padrão, senão `main` ou `master` (a que existir), senão a que
    /// está em checkout na pasta do repositório.
    async fn principal(&self, raiz: &Path) -> Option<String> {
        principal(raiz).await
    }

    async fn ramos(&self, raiz: &Path) -> Result<Vec<Ramo>> {
        Ok(branches(raiz)
            .await?
            .into_iter()
            .map(|b| Ramo {
                so_como_base: b.em_checkout.is_some(),
                nome: b.nome,
            })
            .collect())
    }

    async fn existe(&self, raiz: &Path, nome: &str) -> bool {
        existe_branch(raiz, nome).await
    }

    async fn garante(
        &self,
        raiz: &Path,
        caminho: &Path,
        nome: &str,
        base: Option<&str>,
    ) -> Result<()> {
        let existente = branches(raiz).await?.into_iter().find(|b| b.nome == nome);
        match existente.as_ref().and_then(|b| b.em_checkout.as_ref()) {
            // Já está em checkout exatamente onde a queremos (o banco a esqueceu, o git não).
            Some(onde) if *onde == caminho => Ok(()),
            Some(onde) => bail!("a branch está em checkout em {}", onde.display()),
            None => {
                let base = if existente.is_some() { None } else { base };
                if existente.is_none() && base.is_none() {
                    bail!("a branch não existe e não há de onde criá-la");
                }
                cria(raiz, caminho, nome, base).await
            }
        }
    }

    async fn pendencias(&self, w: &Worktree) -> Result<Pendencias> {
        let principal = principal(Path::new(&w.raiz)).await;
        let caminho = Path::new(&w.caminho);
        let status = git(caminho, &["status", "--porcelain"]).await?;
        let mut args = vec!["rev-list", "--count", "HEAD", "--not", "--remotes"];
        if let Some(p) = principal.as_deref() {
            args.push(p);
        }
        let sem_push = git(caminho, &args).await?;
        Ok(Pendencias {
            sem_commit: status.lines().filter(|l| !l.trim().is_empty()).count(),
            sem_push: sem_push.trim().parse().unwrap_or(0),
        })
    }

    /// Apaga a worktree e a branch dela: no git, uma não vai sem a outra.
    async fn apaga(&self, w: &Worktree, _abandona: bool) -> Result<()> {
        let raiz = Path::new(&w.raiz);
        let caminho = Path::new(&w.caminho);
        if caminho.exists() {
            git(
                raiz,
                &["worktree", "remove", "--force", &caminho.to_string_lossy()],
            )
            .await
            .context("apagando a worktree")?;
        }
        // Pasta que sumiu por fora deixa o registro do git para trás.
        let _ = git(raiz, &["worktree", "prune"]).await;
        if existe_branch(raiz, &w.branch).await {
            git(raiz, &["branch", "-D", &w.branch])
                .await
                .context("apagando a branch")?;
        }
        Ok(())
    }

    /// A pasta, o `git init` e um commit vazio: sem commit a branch principal não existe de
    /// verdade, e dela não sai worktree.
    async fn inicia_projeto(&self, pasta: &Path) -> Result<()> {
        if pasta.exists() {
            bail!("{} já existe", pasta.display());
        }
        std::fs::create_dir_all(pasta).with_context(|| format!("criando {}", pasta.display()))?;
        git(pasta, &["init", "--quiet"]).await?;
        git(
            pasta,
            &[
                "commit",
                "--quiet",
                "--allow-empty",
                "--no-gpg-sign",
                "-m",
                "chore: início do projeto",
            ],
        )
        .await
        .context("fazendo o commit inicial")?;
        Ok(())
    }
}

/// Uma branch local, com onde ela está em checkout agora, se estiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub nome: String,
    /// A pasta com esta branch em checkout: a do repositório ou a de uma worktree.
    pub em_checkout: Option<PathBuf>,
}

async fn principal(raiz: &Path) -> Option<String> {
    if let Ok(r) = git(
        raiz,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await
        && let Some(nome) = r.trim().strip_prefix("origin/")
        && existe_branch(raiz, nome).await
    {
        return Some(nome.to_string());
    }
    for nome in ["main", "master"] {
        if existe_branch(raiz, nome).await {
            return Some(nome.to_string());
        }
    }
    git(raiz, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn existe_branch(raiz: &Path, nome: &str) -> bool {
    git(
        raiz,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{nome}"),
        ],
    )
    .await
    .is_ok()
}

/// As branches locais, a que recebeu commit mais recentemente primeiro, cada uma com a pasta em
/// que está em checkout.
pub async fn branches(raiz: &Path) -> Result<Vec<Branch>> {
    let nomes = git(
        raiz,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/heads",
        ],
    )
    .await?;
    let checkouts = checkouts(raiz).await?;
    Ok(nomes
        .lines()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(|nome| Branch {
            nome: nome.to_string(),
            em_checkout: checkouts
                .iter()
                .find(|(_, b)| b == nome)
                .map(|(p, _)| p.clone()),
        })
        .collect())
}

/// Cada pasta com branch em checkout: a do repositório e as das worktrees.
async fn checkouts(raiz: &Path) -> Result<Vec<(PathBuf, String)>> {
    let saida = git(raiz, &["worktree", "list", "--porcelain"]).await?;
    let mut lista = Vec::new();
    let mut pasta: Option<PathBuf> = None;
    for linha in saida.lines() {
        if let Some(p) = linha.strip_prefix("worktree ") {
            pasta = Some(PathBuf::from(p));
        } else if let Some(r) = linha.strip_prefix("branch refs/heads/")
            && let Some(p) = pasta.take()
        {
            lista.push((p, r.to_string()));
        }
    }
    Ok(lista)
}

/// Cria a worktree em `caminho`. Com `base`, cria junto a branch nova a partir dela; sem, põe
/// em checkout a branch que já existe.
async fn cria(raiz: &Path, caminho: &Path, branch: &str, base: Option<&str>) -> Result<()> {
    if let Some(pai) = caminho.parent() {
        std::fs::create_dir_all(pai).with_context(|| format!("criando {}", pai.display()))?;
    }
    let destino = caminho.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "add"];
    match base {
        Some(b) => args.extend(["-b", branch, destino.as_str(), b]),
        None => args.extend([destino.as_str(), branch]),
    }
    git(raiz, &args)
        .await
        .with_context(|| format!("criando a worktree de {branch}"))?;
    Ok(())
}

/// Roda `git -C <dir> <args>` e devolve o stdout. Código de saída diferente de zero é erro, com
/// o stderr do git como motivo.
async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let saida = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        // Um git que pare para perguntar travaria o daemon: não há terminal para responder.
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .context("chamando git (ele está instalado?)")?;
    if !saida.status.success() {
        bail!(
            "git {}: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&saida.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&saida.stdout).into_owned())
}

#[cfg(test)]
#[path = "git_testes.rs"]
mod testes;
