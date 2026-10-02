//! Os workspaces do jj (Jujutsu) das sessões: um por branch, fora do repositório, e a "branch" é
//! o nome do workspace.
//!
//! O que o jj muda em relação à worktree do git, medido (decisão 0019):
//!
//! - o workspace não tem `.git`, só `.jj/`: o agente lá dentro não tem git;
//! - o mesmo bookmark serve de base a vários workspaces, então não há "em uso": todo bookmark é
//!   base de um workspace novo, e o bot não cria bookmark nenhum (o agente cria o dele ao
//!   publicar);
//! - `workspace forget` não apaga commit: abandonar o que é só do workspace é uma escolha à parte;
//! - `trunk()` só resolve bookmark remoto, daí o fallback para os locais.
//!
//! Regras de toda chamada daqui, porque o repositório é o mesmo em que você trabalha:
//! `--ignore-working-copy` em leitura (sem ela, cada leitura faz snapshot do seu checkout e grava
//! uma operação), `signing.behavior=drop` (a chave pode pedir toque), template explícito e
//! `--color=never` (alias e template da sua config não mudam a saída lida aqui), e
//! `JJ_AUTO_INIT=0` (um wrapper que inicializa o jj sozinho não age por efeito colateral). O
//! `workspace add` é a exceção da primeira regra: o jj o recusa sem o checkout, então ele faz o
//! snapshot do repositório principal, como qualquer comando seu faria.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ld_core::state::Worktree;
use serde_json::{Map, Value};
use tokio::process::Command;
use tracing::info;

use super::{Pendencias, Ramo, Termos, Vcs};

pub struct Jj {
    /// Os servidores MCP do `[jj] mcp` do config.
    mcp: BTreeMap<String, Value>,
}

impl Jj {
    pub fn new(mcp: BTreeMap<String, Value>) -> Self {
        Self { mcp }
    }
}

/// O que no repositório do git indica uma operação pela metade: colocar o jj por cima no meio
/// dela deixaria as duas ferramentas discordando do estado.
const GIT_NO_MEIO: &[&str] = &[
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "BISECT_LOG",
    "rebase-merge",
    "rebase-apply",
];

/// Folga entre a criação do workspace no jj e o registro dele no banco, que é a hora que o
/// abandono usa para separar o trabalho da sessão da base de onde ela saiu.
const FOLGA_DA_CRIACAO: i64 = 60;

#[async_trait::async_trait]
impl Vcs for Jj {
    fn nome(&self) -> &'static str {
        "jj"
    }

    fn termos(&self) -> Termos {
        Termos {
            nome: "workspace",
            a: "o workspace",
            na: "no workspace",
            da: "do workspace",
            so_base: "workspace novo a partir dele",
        }
    }

    /// Coloca o jj num repositório que só tem git. Pasta sem `.git`, ou com `.git` em arquivo
    /// (worktree ou submódulo do git, onde o jj não se coloca), fica como está.
    async fn prepara(&self, raiz: &Path) -> Result<()> {
        if raiz.join(".jj").is_dir() {
            return Ok(());
        }
        let git = raiz.join(".git");
        if !git.is_dir() {
            return Ok(());
        }
        if let Some(f) = GIT_NO_MEIO.iter().find(|f| git.join(f).exists()) {
            bail!(
                "o git de {} está no meio de uma operação ({f}): termine-a antes de abrir uma \
                 sessão com o jj",
                raiz.display()
            );
        }
        roda(
            None,
            &["git", "init", "--colocate", &raiz.to_string_lossy()],
        )
        .await
        .context("colocando o jj sobre o git")?;
        info!(repositorio = %raiz.display(), "jj colocado sobre o git");
        Ok(())
    }

    /// Há algum commit com conteúdo ou descrição? Um repositório recém-criado só tem o `@` vazio.
    async fn tem_commit(&self, raiz: &Path) -> bool {
        le(
            raiz,
            &[
                "log",
                "--no-graph",
                "--limit",
                "1",
                "-r",
                "::visible_heads() ~ root() ~ (empty() & description(exact:\"\"))",
                "-T",
                "\"x\"",
            ],
        )
        .await
        .is_ok_and(|s| !s.is_empty())
    }

    /// O bookmark local do `trunk()`, senão `main`, `master` ou `trunk`, o que existir.
    async fn principal(&self, raiz: &Path) -> Option<String> {
        let locais = bookmarks(raiz).await.ok()?;
        let do_trunk = le(
            raiz,
            &[
                "log",
                "--no-graph",
                "-r",
                "trunk()",
                "-T",
                "if(!root, remote_bookmarks.map(|b| b.name()).join(\"\\n\"))",
            ],
        )
        .await
        .unwrap_or_default();
        do_trunk
            .lines()
            .map(str::trim)
            .chain(["main", "master", "trunk"])
            .find(|n| locais.iter().any(|l| l == n))
            .map(str::to_string)
    }

    async fn ramos(&self, raiz: &Path) -> Result<Vec<Ramo>> {
        Ok(bookmarks(raiz)
            .await?
            .into_iter()
            .map(|nome| Ramo {
                nome,
                so_como_base: true,
            })
            .collect())
    }

    async fn existe(&self, raiz: &Path, nome: &str) -> bool {
        workspaces(raiz)
            .await
            .is_ok_and(|l| l.iter().any(|(n, _)| n == nome))
    }

    /// O bookmark de mesmo nome, se há um: `/new api feat/x` com o bookmark `feat/x` é trabalhar
    /// nele.
    async fn base_para(
        &self,
        raiz: &Path,
        nome: &str,
        principal: Option<String>,
    ) -> Option<String> {
        match bookmarks(raiz).await {
            Ok(b) if b.iter().any(|b| b == nome) => Some(nome.to_string()),
            _ => principal,
        }
    }

    async fn garante(
        &self,
        raiz: &Path,
        caminho: &Path,
        nome: &str,
        base: Option<&str>,
    ) -> Result<()> {
        let lista = workspaces(raiz).await?;
        if let Some((_, onde)) = lista.iter().find(|(n, _)| n == nome) {
            let vivo = onde.join(".jj").is_dir();
            if vivo && onde == caminho {
                return Ok(());
            }
            if vivo {
                bail!("o workspace {nome} já existe em {}", onde.display());
            }
            // A pasta sumiu por fora, e o jj ainda tem o workspace e o commit dele. Recria no
            // mesmo lugar em cima do que ele tinha: o `@`, se tinha conteúdo; senão, os pais.
            let pais = le(
                raiz,
                &[
                    "log",
                    "--no-graph",
                    "-r",
                    &workspace(nome),
                    "-T",
                    "if(empty, parents.map(|c| c.commit_id()).join(\" \"), commit_id)",
                ],
            )
            .await?;
            le(raiz, &["workspace", "forget", nome]).await?;
            let pais: Vec<&str> = pais.split_whitespace().collect();
            return cria(raiz, caminho, nome, &pais).await;
        }
        let Some(base) = base else {
            bail!("o workspace não existe e não há de onde criá-lo");
        };
        cria(raiz, caminho, nome, &[&simbolo(base)]).await
    }

    /// O que está no `@` do workspace, e os commits que não estão no remoto nem na principal.
    /// A conta é contra `remote_bookmarks()`, e não contra `bookmarks()`: o trabalho num bookmark
    /// local ainda não publicado é o que se perderia.
    async fn pendencias(&self, w: &Worktree) -> Result<Pendencias> {
        let raiz = Path::new(&w.raiz);
        snapshot(Path::new(&w.caminho)).await?;
        let ws = workspace(&w.branch);
        let publicado = match self.principal(raiz).await {
            Some(p) => format!("trunk() | remote_bookmarks() | {}", simbolo(&p)),
            None => "trunk() | remote_bookmarks()".into(),
        };
        let arquivos = le(raiz, &["diff", "--summary", "-r", &ws]).await?;
        let commits = le(
            raiz,
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("(::{ws} ~ ::({publicado})) ~ empty() ~ {ws}"),
                "-T",
                "\"x\\n\"",
            ],
        )
        .await?;
        Ok(Pendencias {
            sem_commit: arquivos.lines().filter(|l| !l.trim().is_empty()).count(),
            sem_push: commits.lines().filter(|l| !l.trim().is_empty()).count(),
        })
    }

    fn apagar_preserva_commits(&self) -> bool {
        true
    }

    /// Grava o que estava sem commit, esquece o workspace e apaga a pasta. Com `abandona`, antes
    /// abandona os commits que são só dele (e os bookmarks que apontam para eles vão junto).
    async fn apaga(&self, w: &Worktree, abandona: bool) -> Result<()> {
        let raiz = Path::new(&w.raiz);
        let caminho = Path::new(&w.caminho);
        if caminho.exists() {
            if !caminho.join(".jj").is_dir() {
                bail!(
                    "{} não é um workspace do jj, e não apago o que não é meu",
                    caminho.display()
                );
            }
            snapshot(caminho)
                .await
                .context("gravando o que o workspace tinha sem commit")?;
        }
        if workspaces(raiz).await?.iter().any(|(n, _)| *n == w.branch) {
            if abandona {
                let principal = self.principal(raiz).await;
                let ids = le(
                    raiz,
                    &[
                        "log",
                        "--no-graph",
                        "-r",
                        &so_dele(&w.branch, principal.as_deref(), w.criada_em),
                        "-T",
                        "commit_id ++ \"\\n\"",
                    ],
                )
                .await?;
                let ids: Vec<&str> = ids.split_whitespace().collect();
                if !ids.is_empty() {
                    let mut args = vec!["abandon"];
                    for id in &ids {
                        args.extend(["-r", id]);
                    }
                    le(raiz, &args).await.context("abandonando os commits")?;
                    info!(workspace = %w.branch, quantos = ids.len(), "commits abandonados");
                }
            }
            le(raiz, &["workspace", "forget", &w.branch])
                .await
                .context("esquecendo o workspace")?;
        }
        if caminho.exists() {
            std::fs::remove_dir_all(caminho)
                .with_context(|| format!("apagando {}", caminho.display()))?;
        }
        Ok(())
    }

    /// `jj git init --colocate`, um commit vazio descrito e o bookmark `main` nele.
    async fn inicia_projeto(&self, pasta: &Path) -> Result<()> {
        if pasta.exists() {
            bail!("{} já existe", pasta.display());
        }
        std::fs::create_dir_all(pasta).with_context(|| format!("criando {}", pasta.display()))?;
        roda(
            None,
            &["git", "init", "--colocate", &pasta.to_string_lossy()],
        )
        .await?;
        roda(Some(pasta), &["describe", "-m", "chore: início do projeto"])
            .await
            .context("fazendo o commit inicial")?;
        roda(Some(pasta), &["new"]).await?;
        roda(Some(pasta), &["bookmark", "create", "main", "-r", "@-"]).await?;
        Ok(())
    }

    /// Um workspace fica "stale" quando o commit dele é reescrito de outro lugar e o jj não
    /// consegue absorver sozinho. Sem nada a recuperar, é um no-op.
    async fn antes_da_partida(&self, w: &Worktree) -> Result<()> {
        let caminho = Path::new(&w.caminho);
        if caminho.join(".jj").is_dir() {
            roda(Some(caminho), &["workspace", "update-stale"]).await?;
        }
        Ok(())
    }

    async fn instrucoes(&self, w: &Worktree) -> Option<String> {
        let repo = repositorio_no_github(Path::new(&w.raiz)).await;
        Some(instrucoes(w, repo.as_deref(), !self.mcp.is_empty()))
    }

    fn servidores_mcp(&self) -> Map<String, Value> {
        self.mcp.clone().into_iter().collect()
    }
}

/// O que o agente precisa saber de um workspace do jj, que ele não descobre sozinho: não há git
/// ali, e o `gh` não acha o repositório.
pub fn instrucoes(w: &Worktree, repo: Option<&str>, tem_mcp: bool) -> String {
    let gh = match repo {
        Some(r) => format!(
            "O `gh` não acha o repositório sem `.git`: passe `--repo {r}`, e no `gh pr create` \
             também `--head <bookmark> --title <título> --body <corpo>`."
        ),
        None => "O `gh` não acha o repositório sem `.git`: passe `--repo <dono>/<repo>`, e no \
                 `gh pr create` também `--head <bookmark> --title <título> --body <corpo>`."
            .to_string(),
    };
    let mcp = if tem_mcp {
        " Prefira as ferramentas MCP de jj desta sessão ao jj pelo shell."
    } else {
        ""
    };
    format!(
        "Esta cópia é o workspace `{nome}` do jj (Jujutsu), do repositório em {raiz}. Não há \
         `.git` nesta pasta: use o jj, não o git.{mcp} O working copy é o commit `@`, e todo \
         arquivo mudado entra nele sozinho. Nenhum bookmark foi criado para este workspace: crie \
         o seu quando for publicar (`jj bookmark create <nome> -r @-`, ou `jj bookmark set` para \
         avançar um que já existe) e empurre com `jj git push --bookmark <nome>`. Se o jj disser \
         que o working copy está stale, rode `jj workspace update-stale`. {gh}",
        nome = w.branch,
        raiz = w.raiz,
    )
}

/// `dono/repo` do remoto no GitHub (o `origin`, senão o primeiro).
async fn repositorio_no_github(raiz: &Path) -> Option<String> {
    let saida = le(raiz, &["git", "remote", "list"]).await.ok()?;
    let remotos: Vec<(&str, &str)> = saida
        .lines()
        .filter_map(|l| l.split_once(char::is_whitespace))
        .map(|(n, u)| (n, u.trim()))
        .collect();
    let url = remotos
        .iter()
        .find(|(n, _)| *n == "origin")
        .or(remotos.first())?
        .1;
    dono_e_repo(url)
}

/// `dono/repo` de uma URL do GitHub, por SSH ou HTTPS.
pub fn dono_e_repo(url: &str) -> Option<String> {
    let resto = [
        "git@github.com:",
        "ssh://git@github.com/",
        "https://github.com/",
    ]
    .iter()
    .find_map(|p| url.strip_prefix(p))?;
    let resto = resto.trim_end_matches('/');
    let resto = resto.strip_suffix(".git").unwrap_or(resto);
    let (dono, repo) = resto.split_once('/')?;
    (!dono.is_empty() && !repo.is_empty() && !repo.contains('/')).then(|| format!("{dono}/{repo}"))
}

/// Os commits que são só do workspace: os dele que não estão no remoto, na principal nem em
/// outro working copy, feitos depois de ele nascer. A data separa o trabalho da sessão do
/// bookmark local de onde ela saiu, que não é dela.
pub fn so_dele(nome: &str, principal: Option<&str>, criado_em: i64) -> String {
    let ws = workspace(nome);
    let principal = principal
        .map(|p| format!(" | {}", simbolo(p)))
        .unwrap_or_default();
    let desde = time::OffsetDateTime::from_unix_timestamp(criado_em - FOLGA_DA_CRIACAO)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".into());
    format!(
        "(::{ws} ~ ::(trunk() | remote_bookmarks(){principal} | (working_copies() ~ {ws}))) \
         & committer_date(after:\"{desde}\")"
    )
}

/// Um nome como símbolo de revset, entre aspas: `ld/2026-10-02-1530` sem elas seria lido como
/// uma conta.
pub fn simbolo(nome: &str) -> String {
    format!("\"{}\"", nome.replace('\\', "\\\\").replace('"', "\\\""))
}

/// O `@` de um workspace num revset.
pub fn workspace(nome: &str) -> String {
    format!("{}@", simbolo(nome))
}

/// Os bookmarks locais, o de commit mais recente primeiro. Bookmark em conflito fica de fora:
/// não há um commit só de onde partir.
async fn bookmarks(raiz: &Path) -> Result<Vec<String>> {
    Ok(le(
        raiz,
        &[
            "bookmark",
            "list",
            "--sort",
            "committer-date-",
            "-T",
            "if(!remote && normal_target, name ++ \"\\n\")",
        ],
    )
    .await?
    .lines()
    .map(str::trim)
    .filter(|n| !n.is_empty())
    .map(str::to_string)
    .collect())
}

/// Os workspaces do repositório, com a pasta de cada um.
async fn workspaces(raiz: &Path) -> Result<Vec<(String, PathBuf)>> {
    Ok(le(
        raiz,
        &[
            "workspace",
            "list",
            "-T",
            "name ++ \"\\t\" ++ self.root() ++ \"\\n\"",
        ],
    )
    .await?
    .lines()
    .filter_map(|l| l.split_once('\t'))
    .map(|(n, p)| (n.to_string(), PathBuf::from(p)))
    .collect())
}

/// Cria o workspace `nome` em `caminho`, com o `@` em cima de `revs`.
async fn cria(raiz: &Path, caminho: &Path, nome: &str, revs: &[&str]) -> Result<()> {
    if let Some(pai) = caminho.parent() {
        std::fs::create_dir_all(pai).with_context(|| format!("criando {}", pai.display()))?;
    }
    let destino = caminho.to_string_lossy().into_owned();
    let mut args = vec!["workspace", "add", destino.as_str(), "--name", nome];
    for r in revs {
        args.extend(["-r", r]);
    }
    roda(Some(raiz), &args)
        .await
        .with_context(|| format!("criando o workspace {nome}"))?;
    Ok(())
}

/// Grava no `@` do workspace o que está na pasta. Pasta que sumiu não tem o que gravar.
async fn snapshot(caminho: &Path) -> Result<()> {
    if caminho.join(".jj").is_dir() {
        roda(Some(caminho), &["workspace", "update-stale"]).await?;
        roda(Some(caminho), &["util", "snapshot"]).await?;
    }
    Ok(())
}

/// Uma chamada que não toca no working copy de quem está em `dir`.
async fn le(dir: &Path, args: &[&str]) -> Result<String> {
    let mut com = vec!["--ignore-working-copy"];
    com.extend_from_slice(args);
    roda(Some(dir), &com).await
}

/// Roda `jj [-R <dir>] <args>` e devolve o stdout. Código de saída diferente de zero é erro, com
/// o stderr do jj como motivo.
async fn roda(dir: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("jj");
    if let Some(d) = dir {
        cmd.arg("-R").arg(d);
    }
    let saida = cmd
        .args([
            "--color=never",
            "--no-pager",
            "--config",
            "signing.behavior=drop",
        ])
        .args(args)
        .env("JJ_AUTO_INIT", "0")
        .output()
        .await
        .context("chamando jj (ele está instalado?)")?;
    if !saida.status.success() {
        let sub = args
            .iter()
            .find(|a| !a.starts_with('-'))
            .copied()
            .unwrap_or_default();
        bail!(
            "jj {sub}: {}",
            String::from_utf8_lossy(&saida.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&saida.stdout).into_owned())
}

#[cfg(test)]
#[path = "jj_testes.rs"]
mod testes;
