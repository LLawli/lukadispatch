//! Configuração do daemon: `~/.config/lukadispatch/config.toml` mais variáveis de ambiente.
//!
//! O token do bot NUNCA mora no arquivo de config (que é versionável e fácil de copiar por
//! engano): ele vem sempre de `LUKADISPATCH_TELEGRAM_TOKEN`, carregado do `.env` do serviço.
//!
//! A lista de projetos é a soma das duas fontes que o usuário pediu: os fixados no arquivo,
//! na ordem em que ele escreveu, e a varredura automática das raízes por diretórios com `.git`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub telegram: Telegram,
    pub scan: Scan,
    /// Projetos fixados: aparecem primeiro no seletor e podem trazer regra própria.
    pub projects: Vec<Project>,
    /// Modo de permissão usado quando o projeto não declara o dele.
    pub default_permission_mode: String,
    /// Marcar a pasta do projeto como confiada antes de abrir a sessão.
    ///
    /// Sem isso, projeto fora de uma árvore já confiada trava no diálogo de confiança do Claude
    /// Code, e do celular isso aparece como uma sessão muda. Vale só para os projetos que este
    /// config oferece, nunca para um caminho arbitrário.
    pub trust_projects: bool,
    /// Quantas falas do histórico o tópico recebe ao retomar uma conversa.
    pub history_lines: usize,
    /// Caminho do binário do Claude Code, quando a descoberta automática não servir.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claude_binary: Option<String>,
    /// Passar os servidores MCP pelo proxy, para o diálogo deles caber no celular.
    ///
    /// Desligue se algum servidor seu não gostar de ter um processo no meio do cano: as sessões
    /// voltam a falar direto com eles, e o diálogo volta a só dar para responder no PC.
    pub wrap_mcp: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            telegram: Telegram::default(),
            scan: Scan::default(),
            projects: Vec::new(),
            default_permission_mode: "auto".into(),
            trust_projects: true,
            history_lines: 8,
            claude_binary: None,
            wrap_mcp: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Telegram {
    /// Id do supergrupo com tópicos ligados.
    pub chat_id: i64,
    /// Só estes usuários são obedecidos. Vazio significa "ninguém", de propósito: um bot de
    /// controle de máquina que aceita qualquer um é um backdoor.
    pub allowed_user_ids: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Scan {
    pub enabled: bool,
    /// Raízes varridas em busca de repositórios. `~` é expandido.
    pub roots: Vec<String>,
    /// Profundidade da varredura: 1 = filhos diretos da raiz.
    pub depth: usize,
}

impl Default for Scan {
    fn default() -> Self {
        Self {
            enabled: true,
            roots: vec!["~/Personal".into(), "~/Projetos".into()],
            depth: 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    /// Modelo e esforço padrão deste projeto. O `/new` sem argumento usa estes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl Config {
    pub fn load(caminho: &Path) -> Result<Self> {
        if !caminho.exists() {
            return Ok(Self::default());
        }
        let texto = std::fs::read_to_string(caminho)
            .with_context(|| format!("lendo config em {}", caminho.display()))?;
        let mut cfg: Config = toml::from_str(&texto)
            .with_context(|| format!("config inválida em {}", caminho.display()))?;
        cfg.telegram.chat_id = match std::env::var("LUKADISPATCH_CHAT_ID") {
            Ok(v) if !v.is_empty() => v.parse().unwrap_or(cfg.telegram.chat_id),
            _ => cfg.telegram.chat_id,
        };
        Ok(cfg)
    }

    /// Token do bot. Só do ambiente, nunca do arquivo.
    pub fn telegram_token() -> Result<String> {
        let t = std::env::var("LUKADISPATCH_TELEGRAM_TOKEN")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .context("LUKADISPATCH_TELEGRAM_TOKEN não está no ambiente (veja o .env.example)")?;
        Ok(t.trim().to_string())
    }

    pub fn allows(&self, user_id: i64) -> bool {
        self.telegram.allowed_user_ids.contains(&user_id)
    }

    pub fn permission_mode_for(&self, path: &str) -> String {
        self.projects
            .iter()
            .find(|p| p.path == path)
            .and_then(|p| p.permission_mode.clone())
            .unwrap_or_else(|| self.default_permission_mode.clone())
    }

    /// Projetos oferecidos no seletor: primeiro os fixados, na ordem do arquivo, depois os
    /// encontrados na varredura, em ordem alfabética. Sem repetição por caminho.
    pub fn projects_available(&self) -> Vec<Project> {
        let mut vistos: BTreeSet<String> = BTreeSet::new();
        let mut saida = Vec::new();
        for p in &self.projects {
            let caminho = expand_tilde(&p.path).to_string_lossy().into_owned();
            if vistos.insert(caminho.clone()) {
                saida.push(Project {
                    name: p.name.clone(),
                    path: caminho,
                    permission_mode: p.permission_mode.clone(),
                    model: p.model.clone(),
                    effort: p.effort.clone(),
                });
            }
        }
        if self.scan.enabled {
            for achado in scan_repos(&self.scan) {
                let caminho = achado.to_string_lossy().into_owned();
                if vistos.insert(caminho.clone()) {
                    let nome = achado
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| caminho.clone());
                    saida.push(Project {
                        name: nome,
                        path: caminho,
                        permission_mode: None,
                        model: None,
                        effort: None,
                    });
                }
            }
        }
        saida
    }
}

pub fn expand_tilde(caminho: &str) -> PathBuf {
    if let Some(resto) = caminho.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(resto);
    }
    PathBuf::from(caminho)
}

/// Diretórios com `.git` sob as raízes, até a profundidade pedida, em ordem alfabética.
fn scan_repos(scan: &Scan) -> Vec<PathBuf> {
    let mut achados = BTreeSet::new();
    for raiz in &scan.roots {
        varre(&expand_tilde(raiz), scan.depth, &mut achados);
    }
    achados.into_iter().collect()
}

fn varre(dir: &Path, profundidade: usize, achados: &mut BTreeSet<PathBuf>) {
    if profundidade == 0 {
        return;
    }
    let Ok(entradas) = std::fs::read_dir(dir) else {
        return;
    };
    for entrada in entradas.flatten() {
        let caminho = entrada.path();
        if !caminho.is_dir() {
            continue;
        }
        // Diretório escondido não é projeto de trabalho, e varrer .cache/.local é desperdício.
        if caminho
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with('.'))
        {
            continue;
        }
        if caminho.join(".git").exists() {
            achados.insert(caminho);
            continue; // repositório achado: não desce para submódulo.
        }
        varre(&caminho, profundidade - 1, achados);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_ausente_vira_padrao() {
        let c = Config::load(Path::new("/nao/existe/config.toml")).unwrap();
        assert_eq!(c.default_permission_mode, "auto");
        assert!(c.scan.enabled);
    }

    #[test]
    fn allowlist_vazia_nega_todo_mundo() {
        let c = Config::default();
        assert!(!c.allows(1));
        assert!(!c.allows(0));
    }

    #[test]
    fn le_toml_com_projetos() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
default_permission_mode = "acceptEdits"

[telegram]
chat_id = -1001
allowed_user_ids = [42]

[scan]
enabled = false

[[projects]]
name = "um"
path = "/tmp/um"
permission_mode = "bypassPermissions"
"#,
        )
        .unwrap();
        let c = Config::load(&p).unwrap();
        assert!(c.allows(42));
        assert!(!c.allows(43));
        assert_eq!(c.permission_mode_for("/tmp/um"), "bypassPermissions");
        assert_eq!(c.permission_mode_for("/tmp/outro"), "acceptEdits");
        let disponiveis = c.projects_available();
        assert_eq!(disponiveis.len(), 1, "varredura desligada: só o fixado");
    }

    #[test]
    fn varredura_acha_repositorio_e_ignora_escondido() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projA/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("naorepo")).unwrap();
        std::fs::create_dir_all(dir.path().join(".escondido/.git")).unwrap();

        let scan = Scan {
            enabled: true,
            roots: vec![dir.path().to_string_lossy().into_owned()],
            depth: 1,
        };
        let achados = scan_repos(&scan);
        assert_eq!(achados.len(), 1);
        assert!(achados[0].ends_with("projA"));
    }

    #[test]
    fn fixado_nao_duplica_com_o_varrido() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("projA/.git")).unwrap();
        let caminho_a = dir.path().join("projA").to_string_lossy().into_owned();

        let c = Config {
            projects: vec![Project {
                name: "apelido".into(),
                path: caminho_a.clone(),
                permission_mode: None,
                model: None,
                effort: None,
            }],
            scan: Scan {
                enabled: true,
                roots: vec![dir.path().to_string_lossy().into_owned()],
                depth: 1,
            },
            ..Default::default()
        };
        let d = c.projects_available();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "apelido", "o fixado vence e mantém o apelido");
    }
}
