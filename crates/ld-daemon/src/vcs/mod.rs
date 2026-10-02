//! A porta do controle de versão: a cópia de trabalho própria de cada sessão.
//!
//! Cada sessão do bot roda numa cópia própria do repositório, e não na pasta do projeto, para duas
//! sessões no mesmo projeto não pisarem uma na outra e o seu checkout no terminal ficar intocado.
//! No git a cópia é uma worktree, uma por branch ([`git::Git`]); no jj, um workspace, e a "branch"
//! é o nome dele ([`jj::Jj`]). O domínio fala "branch" nos dois casos.
//!
//! A cópia de uma branch é sempre a mesma pasta, e isso importa: o agente acha a conversa
//! anterior pelo cwd, então mudar o caminho perderia o "continuar de onde parou". Elas moram em
//! `~/.local/share/lukadispatch/worktrees/<caminho do projeto a partir do $HOME>/<branch>`: fora
//! do repositório (nenhuma ferramenta que varre o repo enxerga cópias) e fora das raízes do
//! `[scan]` (senão cada cópia viraria projeto no `/new`). O caminho do projeto, e não só o nome,
//! porque dois projetos com o mesmo nome em raízes diferentes existem (`~/Personal/api` e
//! `~/Trabalho/api`).
//!
//! Os adaptadores falam com o binário direto, e os testes rodam contra repositórios de verdade
//! num tempdir. Ver `docs/decisoes/0017-worktree-por-sessao.md` e
//! `docs/decisoes/0019-jj-como-porta-de-vcs.md`.
//!
//! ## Contrato
//!
//! - Toda leitura é de um repositório que pode estar no meio do seu trabalho: ela não pode mudar
//!   o seu checkout (no jj, `--ignore-working-copy`).
//! - Nada pede interação nem assinatura com toque: quem pediu está no celular.
//! - `apaga` só tira o que é da cópia do bot, e quem chama já perguntou o que precisava.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use ld_core::config::Config;
use ld_core::state::Worktree;
use serde_json::{Map, Value};

pub mod git;
pub mod jj;

/// Os nomes do config.
pub const VCS: &[&str] = &["git", "jj"];

/// O controle de versão do config.
pub fn da_config(cfg: &Config) -> Result<Arc<dyn Vcs>> {
    match cfg.vcs.as_str() {
        "git" => Ok(Arc::new(git::Git)),
        "jj" => Ok(Arc::new(jj::Jj::new(cfg.jj.mcp.clone()))),
        outro => bail!(
            "vcs = \"{outro}\": não existe (disponíveis: {})",
            VCS.join(", ")
        ),
    }
}

/// O adaptador que sabe mexer numa cópia já registrada: o dela, mesmo que o config tenha trocado
/// depois (uma worktree git aberta antes da troca para o jj continua sendo git).
pub fn de(atual: &Arc<dyn Vcs>, w: &Worktree) -> Arc<dyn Vcs> {
    match w.vcs.as_str() {
        n if n == atual.nome() => atual.clone(),
        "jj" => Arc::new(jj::Jj::new(Default::default())),
        _ => Arc::new(git::Git),
    }
}

/// A raiz de todas as cópias.
pub fn base() -> PathBuf {
    let dados = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => ld_core::paths::home().join(".local/share"),
    };
    dados.join("lukadispatch").join("worktrees")
}

/// A pasta que junta as cópias de um projeto: o caminho dele a partir do `$HOME`, sob a `base`.
/// Projeto fora do `$HOME` usa o caminho absoluto inteiro, sob `raiz/`.
pub fn pasta_do_projeto(base: &Path, home: &Path, raiz: &Path) -> PathBuf {
    match raiz.strip_prefix(home) {
        Ok(relativo) if !relativo.as_os_str().is_empty() => base.join(relativo),
        _ => base
            .join("raiz")
            .join(raiz.strip_prefix("/").unwrap_or(raiz)),
    }
}

/// Onde mora a cópia de uma branch. Branch com `/` vira subpasta, e isso não colide: o git não
/// deixa existir `feat` e `feat/x` ao mesmo tempo, e o jj recusa o nome que [`nome_valido`]
/// recusa.
pub fn caminho(base: &Path, home: &Path, raiz: &Path, branch: &str) -> PathBuf {
    pasta_do_projeto(base, home, raiz).join(branch)
}

/// O nome serve para branch e para pasta? É a regra do `git check-ref-format`, que vale também
/// para o jj (o bookmark é exportado para o git), mais duas nossas: nada que suba de pasta,
/// porque o nome vira caminho, e nada de aspas, porque o nome vai entre aspas num revset do jj.
pub fn nome_valido(nome: &str) -> bool {
    let partes: Vec<&str> = nome.split('/').collect();
    !nome.is_empty()
        && !nome.starts_with('-')
        && nome != "@"
        && !nome.contains("..")
        && !nome.contains("@{")
        && !nome.ends_with('.')
        && !nome.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\' | '"' | '\'')
        })
        && partes
            .iter()
            .all(|p| !p.is_empty() && !p.starts_with('.') && !p.ends_with(".lock"))
}

/// Uma linha de trabalho que o seletor do `/new` oferece.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ramo {
    pub nome: String,
    /// Não abre direto: escolhê-la cria uma branch nova a partir dela. No git, é a branch em
    /// checkout noutro lugar; no jj, todo bookmark (ele é sempre base de um workspace novo).
    pub so_como_base: bool,
}

/// O que se perderia apagando a cópia.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pendencias {
    /// Arquivos mudados e ainda sem commit (inclui os novos, fora do índice).
    pub sem_commit: usize,
    /// Commits que não estão em remoto nenhum nem na branch principal.
    pub sem_push: usize,
}

impl Pendencias {
    pub fn nenhuma(&self) -> bool {
        self.sem_commit == 0 && self.sem_push == 0
    }
}

/// Como os textos do chat chamam a cópia: "a worktree" ou "o workspace". O gênero muda o artigo,
/// então cada forma vem pronta.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Termos {
    /// `"worktree"`, `"workspace"`.
    pub nome: &'static str,
    /// `"a worktree"`, `"o workspace"`.
    pub a: &'static str,
    /// `"na worktree"`, `"no workspace"`.
    pub na: &'static str,
    /// `"da worktree"`, `"do workspace"`.
    pub da: &'static str,
    /// Rótulo de um [`Ramo::so_como_base`] no seletor, entre parênteses depois do nome.
    pub so_base: &'static str,
}

#[async_trait::async_trait]
pub trait Vcs: Send + Sync {
    /// O nome no config, e o que vai na coluna `vcs` de cada cópia.
    fn nome(&self) -> &'static str;

    fn termos(&self) -> Termos;

    /// Deixa a pasta pronta para este controle de versão, antes de qualquer outra chamada. O jj
    /// se coloca sobre um repositório que só tem git.
    async fn prepara(&self, raiz: &Path) -> Result<()> {
        let _ = raiz;
        Ok(())
    }

    /// O repositório tem de onde tirar uma cópia? Sem isso, a sessão abre na pasta do projeto.
    async fn tem_commit(&self, raiz: &Path) -> bool;

    /// A branch principal do repositório. O bot nunca trabalha nela: escolhê-la no `/new` cria
    /// uma branch nova a partir dela.
    async fn principal(&self, raiz: &Path) -> Option<String>;

    /// O que o seletor do `/new` oferece, a linha que recebeu commit mais recentemente primeiro.
    async fn ramos(&self, raiz: &Path) -> Result<Vec<Ramo>>;

    /// O nome já está em uso, e abre direto (sem base): a branch existe, no git; o workspace
    /// existe, no jj.
    async fn existe(&self, raiz: &Path, nome: &str) -> bool;

    /// De onde nasce a branch `nome`, que ainda não existe. Padrão: da principal.
    async fn base_para(
        &self,
        raiz: &Path,
        nome: &str,
        principal: Option<String>,
    ) -> Option<String> {
        let _ = (raiz, nome);
        principal
    }

    /// Garante a cópia da branch `nome` em `caminho`: a que já está lá serve; sem ela, cria (a
    /// partir de `base`, se a branch ainda não existe). Erro quando a branch está em uso noutro
    /// lugar, ou quando não existe e não há base.
    async fn garante(
        &self,
        raiz: &Path,
        caminho: &Path,
        nome: &str,
        base: Option<&str>,
    ) -> Result<()>;

    /// O que se perderia apagando a cópia.
    async fn pendencias(&self, w: &Worktree) -> Result<Pendencias>;

    /// Apagar a cópia deixa os commits dela no repositório? No jj sim, e abandoná-los é uma
    /// escolha à parte; no git, apagar leva a branch junto.
    fn apagar_preserva_commits(&self) -> bool {
        false
    }

    /// Apaga a cópia, com o que houver dentro. Com `abandona`, leva junto o que é só dela (no git,
    /// a branch, que sempre vai).
    async fn apaga(&self, w: &Worktree, abandona: bool) -> Result<()>;

    /// Cria um projeto numa pasta que ainda não existe, com um commit vazio de onde as cópias
    /// saem. O commit vai sem assinatura: a chave pode pedir um toque, e quem pediu está no
    /// celular.
    async fn inicia_projeto(&self, pasta: &Path) -> Result<()>;

    /// Antes de cada partida de uma sessão na cópia.
    async fn antes_da_partida(&self, w: &Worktree) -> Result<()> {
        let _ = w;
        Ok(())
    }

    /// O que o agente precisa saber sobre a cópia ao começar, no fim do prompt de partida.
    async fn instrucoes(&self, w: &Worktree) -> Option<String> {
        let _ = w;
        None
    }

    /// Servidores MCP que a sessão na cópia recebe, além dos do projeto, no formato do
    /// `.mcp.json`.
    fn servidores_mcp(&self) -> Map<String, Value> {
        Map::new()
    }
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn o_caminho_segue_o_projeto_a_partir_do_home() {
        let base = Path::new("/b");
        let home = Path::new("/home/eu");
        assert_eq!(
            caminho(base, home, Path::new("/home/eu/Personal/api"), "feat/login"),
            Path::new("/b/Personal/api/feat/login")
        );
        // Dois projetos de mesmo nome em raízes diferentes não dividem pasta.
        assert_ne!(
            pasta_do_projeto(base, home, Path::new("/home/eu/Trabalho/api")),
            pasta_do_projeto(base, home, Path::new("/home/eu/Personal/api"))
        );
        assert_eq!(
            pasta_do_projeto(base, home, Path::new("/srv/api")),
            Path::new("/b/raiz/srv/api")
        );
    }

    #[test]
    fn nome_segue_o_git_e_nao_sobe_de_pasta_nem_tem_aspas() {
        for bom in ["feat/login", "ld/2026-10-02-1530", "x", "a.b"] {
            assert!(nome_valido(bom), "{bom:?}");
        }
        for ruim in [
            "",
            "-x",
            "a..b",
            "../fora",
            "com espaço",
            "fim/",
            "/inicio",
            "a//b",
            ".escondido",
            "x.lock",
            "a@{1}",
            "@",
            "fim.",
            "aspas\"",
            "aspas'",
            "a:b",
            "a~1",
        ] {
            assert!(!nome_valido(ruim), "{ruim:?}");
        }
    }

    #[test]
    fn config_escolhe_o_vcs_e_recusa_o_que_nao_existe() {
        let com = |vcs: &str| Config {
            vcs: vcs.into(),
            ..Config::default()
        };
        assert_eq!(da_config(&com("git")).unwrap().nome(), "git");
        assert_eq!(da_config(&com("jj")).unwrap().nome(), "jj");
        let e = da_config(&com("hg")).err().unwrap().to_string();
        assert!(e.contains("git, jj"), "{e}");
    }

    #[test]
    fn a_copia_antiga_e_apagada_pelo_vcs_que_a_criou() {
        let jj: Arc<dyn Vcs> = Arc::new(jj::Jj::new(Default::default()));
        let w = |vcs: &str| Worktree {
            caminho: "/wt/x".into(),
            projeto: "api".into(),
            raiz: "/api".into(),
            branch: "x".into(),
            vcs: vcs.into(),
            criada_em: 0,
            usada_em: 0,
        };
        assert_eq!(de(&jj, &w("git")).nome(), "git");
        assert_eq!(de(&jj, &w("jj")).nome(), "jj");
    }
}
