//! As git worktrees das sessões: uma por branch, fora do repositório.
//!
//! Cada sessão do bot roda numa worktree própria, e não na pasta do projeto, para duas sessões
//! no mesmo projeto não pisarem uma na outra e o seu checkout no terminal ficar intocado. A
//! worktree de uma branch é sempre a mesma pasta, e isso importa: o agente acha a conversa
//! anterior pelo cwd, então mudar o caminho perderia o "continuar de onde parou".
//!
//! Elas moram em `~/.local/share/lukadispatch/worktrees/<caminho do projeto a partir do
//! $HOME>/<branch>`: fora do repositório (um `git status` limpo, e nenhuma ferramenta que varre o
//! repo enxergando cópias) e fora das raízes do `[scan]` (senão cada worktree viraria projeto no
//! `/new`). O caminho do projeto, e não só o nome, porque dois projetos com o mesmo nome em
//! raízes diferentes existem (`~/Personal/api` e `~/Trabalho/api`).
//!
//! O git não é uma porta: não há outro para pôr no lugar. Por isso este módulo fala com o
//! binário direto, e os testes rodam contra repositórios de verdade num tempdir.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// A raiz de todas as worktrees.
pub fn base() -> PathBuf {
    let dados = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => ld_core::paths::home().join(".local/share"),
    };
    dados.join("lukadispatch").join("worktrees")
}

/// A pasta que junta as worktrees de um projeto: o caminho dele a partir do `$HOME`, sob a
/// `base`. Projeto fora do `$HOME` usa o caminho absoluto inteiro, sob `raiz/`.
pub fn pasta_do_projeto(base: &Path, home: &Path, raiz: &Path) -> PathBuf {
    match raiz.strip_prefix(home) {
        Ok(relativo) if !relativo.as_os_str().is_empty() => base.join(relativo),
        _ => base
            .join("raiz")
            .join(raiz.strip_prefix("/").unwrap_or(raiz)),
    }
}

/// Onde mora a worktree de uma branch. Branch com `/` vira subpasta, e isso não colide: o git
/// não deixa existir `feat` e `feat/x` ao mesmo tempo.
pub fn caminho(base: &Path, home: &Path, raiz: &Path, branch: &str) -> PathBuf {
    pasta_do_projeto(base, home, raiz).join(branch)
}

/// Uma branch local, com onde ela está em checkout agora, se estiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    pub nome: String,
    /// A pasta com esta branch em checkout: a do repositório ou a de uma worktree.
    pub em_checkout: Option<PathBuf>,
}

/// O repositório tem pelo menos um commit? Sem commit não há de onde tirar uma worktree, e a
/// sessão abre na pasta do projeto.
pub async fn tem_commit(raiz: &Path) -> bool {
    git(raiz, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .await
        .is_ok()
}

/// A branch principal do repositório: a que o `origin` aponta como padrão, senão `main` ou
/// `master` (a que existir), senão a que está em checkout na pasta do repositório.
///
/// O bot nunca trabalha nela: escolhê-la no `/new` cria uma branch nova a partir dela.
pub async fn principal(raiz: &Path) -> Option<String> {
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

pub async fn existe_branch(raiz: &Path, nome: &str) -> bool {
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

/// O nome serve para branch? É a regra do próprio git (`check-ref-format --branch`), mais uma
/// nossa: nada que suba de pasta, porque o nome vira caminho.
pub async fn nome_valido(raiz: &Path, nome: &str) -> bool {
    if nome.is_empty() || nome.starts_with('-') || nome.split('/').any(|p| p == "..") {
        return false;
    }
    git(raiz, &["check-ref-format", "--branch", nome])
        .await
        .is_ok()
}

/// Cria a worktree em `caminho`. Com `base`, cria junto a branch nova a partir dela; sem, põe
/// em checkout a branch que já existe.
pub async fn cria(raiz: &Path, caminho: &Path, branch: &str, base: Option<&str>) -> Result<()> {
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

/// O que se perderia apagando a worktree e a branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pendencias {
    /// Arquivos mudados e ainda sem commit (inclui os novos, fora do índice).
    pub sem_commit: usize,
    /// Commits da branch que não estão em remoto nenhum nem na branch principal.
    pub sem_push: usize,
}

impl Pendencias {
    pub fn nenhuma(&self) -> bool {
        self.sem_commit == 0 && self.sem_push == 0
    }
}

pub async fn pendencias(caminho: &Path, principal: Option<&str>) -> Result<Pendencias> {
    let status = git(caminho, &["status", "--porcelain"]).await?;
    let mut args = vec!["rev-list", "--count", "HEAD", "--not", "--remotes"];
    if let Some(p) = principal {
        args.push(p);
    }
    let sem_push = git(caminho, &args).await?;
    Ok(Pendencias {
        sem_commit: status.lines().filter(|l| !l.trim().is_empty()).count(),
        sem_push: sem_push.trim().parse().unwrap_or(0),
    })
}

/// Apaga a worktree e a branch dela, com o que houver dentro. Quem chama já perguntou.
pub async fn apaga(raiz: &Path, caminho: &Path, branch: &str) -> Result<()> {
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
    if existe_branch(raiz, branch).await {
        git(raiz, &["branch", "-D", branch])
            .await
            .context("apagando a branch")?;
    }
    Ok(())
}

/// Cria um projeto: a pasta, o `git init` e um commit vazio.
///
/// Sem commit a branch principal não existe de verdade, e dela não sai worktree. O commit vai
/// sem assinatura de propósito: a chave de quem assina pode pedir um toque físico, e quem pediu
/// o projeto está no celular. É o único commit que o bot faz; os das sessões são do agente.
pub async fn inicia_projeto(pasta: &Path) -> Result<()> {
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
#[path = "worktree_testes.rs"]
mod testes;
