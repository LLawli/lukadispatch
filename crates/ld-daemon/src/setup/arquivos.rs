//! O que o setup vai gravar, montado em memória: o `config.toml` e o `.env`.
//!
//! Nada vai para o disco até o fim, e um config que já existe é editado no lugar (comentários,
//! ordem e chaves que o setup não conhece ficam como estavam). Só as chaves que alguma peça
//! pediu para mudar mudam.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ld_core::config::Config;
use toml_edit::{Array, DocumentMut, Item, TableLike, Value};

/// Base de um config novo: o exemplo comentado que vai no pacote.
const CONFIG_EXEMPLO: &str = include_str!("../../../../dist/config.example.toml");
/// Base de um `.env` novo.
const ENV_EXEMPLO: &str = include_str!("../../../../.env.example");

pub const CHAVE_TOKEN: &str = ld_core::config::TOKEN_TELEGRAM;

pub struct Rascunho {
    config: DocumentMut,
    env: Vec<String>,
    /// O config que existia, já carregado; o padrão quando não havia nenhum.
    pub atual: Config,
    /// Se havia config antes. Sem ele, os valores de `atual` são os padrões, e não escolhas.
    pub havia_config: bool,
    /// Nome de quem vai usar, quando alguma peça o descobre (o Telegram sabe o seu).
    pub nome_sugerido: Option<String>,
    /// `true` quando algum programa está no PATH. Nos testes, a máquina é de mentira.
    pub tem_programa: Box<dyn Fn(&str) -> bool>,
    /// Perguntar de novo o que já está resolvido (`setup --refazer`).
    pub refazer: bool,
    /// Para o serviço antes de o setup escutar o bot, e diz se ele estava rodando. Nos testes (e
    /// por padrão) não faz nada: só o setup de verdade mexe no systemd.
    pub pausa_servico: Box<dyn Fn() -> bool>,
}

impl Rascunho {
    /// A partir do que existe em disco (`None` para arquivo ausente).
    pub fn de(config: Option<&str>, env: Option<&str>) -> Result<Self> {
        let havia_config = config.is_some();
        let atual = match config {
            Some(t) => toml::from_str(t).context(
                "o config.toml atual não carrega; conserte ou mova-o antes de rodar o setup",
            )?,
            None => Config::default(),
        };
        let config = config
            .unwrap_or(CONFIG_EXEMPLO)
            .parse::<DocumentMut>()
            .context("lendo o config.toml")?;
        let env = env
            .unwrap_or(ENV_EXEMPLO)
            .lines()
            .map(str::to_string)
            .collect();
        Ok(Self {
            config,
            env,
            atual,
            havia_config,
            nome_sugerido: None,
            tem_programa: Box::new(no_path),
            refazer: false,
            pausa_servico: Box::new(|| false),
        })
    }

    /// Põe `chave = valor` na raiz (`tabela = None`) ou numa tabela, criando-a se faltar.
    ///
    /// A tabela nova nasce como seção (`[agente]`), e não inline na raiz, que é o que o
    /// toml_edit faria sozinho. O valor trocado herda o comentário do antigo, na mesma linha.
    pub fn poe(&mut self, tabela: Option<&str>, chave: &str, valor: impl Into<Value>) {
        let mut valor = valor.into();
        let alvo: &mut dyn TableLike = match tabela {
            None => self.config.as_table_mut(),
            Some(t) => {
                // Inline ou seção, a que existe fica: trocá-la perderia as outras chaves.
                if !self.config.get(t).is_some_and(Item::is_table_like) {
                    self.config[t] = toml_edit::table();
                }
                self.config[t]
                    .as_table_like_mut()
                    .expect("a tabela acabou de ser garantida")
            }
        };
        // Troca só o valor, no lugar: a chave carrega o comentário das linhas de cima, e um
        // `insert` a substituiria junto.
        match alvo.get_mut(chave) {
            Some(item) => {
                if let Some(antigo) = item.as_value() {
                    *valor.decor_mut() = antigo.decor().clone();
                }
                *item = Item::Value(valor);
            }
            None => {
                alvo.insert(chave, Item::Value(valor));
            }
        }
    }

    /// Tira uma chave, se ela existir.
    pub fn tira(&mut self, tabela: Option<&str>, chave: &str) {
        let alvo = match tabela {
            None => Some(self.config.as_table_mut() as &mut dyn TableLike),
            Some(t) => self.config.get_mut(t).and_then(Item::as_table_like_mut),
        };
        if let Some(alvo) = alvo {
            alvo.remove(chave);
        }
    }

    /// Acrescenta `id` a uma lista de inteiros, sem repetir e sem tirar quem já estava.
    ///
    /// Num config novo a lista começa vazia: a que vem no exemplo é enfeite, e somar a ela
    /// poria um id de mentira na allowlist do bot.
    pub fn inclui_id(&mut self, tabela: &str, chave: &str, id: i64) {
        let mut lista = if self.havia_config {
            self.config[tabela][chave]
                .as_array()
                .cloned()
                .unwrap_or_else(Array::new)
        } else {
            Array::new()
        };
        if !lista.iter().any(|v| v.as_integer() == Some(id)) {
            lista.push(id);
        }
        self.config[tabela][chave] = Item::Value(Value::Array(lista));
    }

    pub fn valor_env(&self, chave: &str) -> Option<String> {
        let prefixo = format!("{chave}=");
        self.env
            .iter()
            .find_map(|l| l.strip_prefix(&prefixo))
            .map(|v| v.trim().trim_matches('"').to_string())
            .filter(|v| !v.is_empty())
    }

    /// Troca a linha da chave no `.env`, ou acrescenta no fim.
    pub fn poe_env(&mut self, chave: &str, valor: &str) {
        let prefixo = format!("{chave}=");
        let linha = format!("{chave}={valor}");
        match self.env.iter_mut().find(|l| l.starts_with(&prefixo)) {
            Some(l) => *l = linha,
            None => self.env.push(linha),
        }
    }

    pub fn config_texto(&self) -> String {
        self.config.to_string()
    }

    pub fn env_texto(&self) -> String {
        let mut t = self.env.join("\n");
        t.push('\n');
        t
    }
}

fn no_path(programa: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p)
            .any(|d| std::fs::metadata(d.join(programa)).is_ok_and(|m| m.is_file()))
    })
}

/// Pastas da home que têm projetos com `.git` dentro, as com mais projetos primeiro.
///
/// É o palpite para as raízes da varredura: quase todo mundo guarda o código numa ou duas pastas
/// (`~/Projetos`, `~/code`, `~/src`), e perguntar sem sugerir obriga a pessoa a lembrar o nome.
pub fn raizes_sugeridas(home: &Path) -> Vec<PathBuf> {
    let Ok(entradas) = std::fs::read_dir(home) else {
        return Vec::new();
    };
    let mut achadas: Vec<(usize, PathBuf)> = entradas
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|p| {
            let n = std::fs::read_dir(&p)
                .ok()?
                .flatten()
                .filter(|f| f.path().join(".git").exists())
                .count();
            (n > 0).then_some((n, p))
        })
        .collect();
    achadas.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    achadas.into_iter().map(|(_, p)| p).collect()
}

/// `~/x` para um caminho sob a home, que é como o config guarda e como se lê melhor.
pub fn com_til(caminho: &Path, home: &Path) -> String {
    match caminho.strip_prefix(home) {
        Ok(resto) => format!("~/{}", resto.display()),
        Err(_) => caminho.display().to_string(),
    }
}

/// Grava `conteudo` em `destino`, guardando o anterior em `<destino>.bak`. `modo` é o do
/// arquivo novo (o `.env` nasce 0600, porque carrega o token).
pub fn grava(destino: &Path, conteudo: &str, modo: u32) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(pai) = destino.parent() {
        std::fs::create_dir_all(pai).with_context(|| format!("criando {}", pai.display()))?;
    }
    if destino.exists() {
        let bak = PathBuf::from(format!("{}.bak", destino.display()));
        std::fs::copy(destino, &bak).with_context(|| format!("copiando para {}", bak.display()))?;
        std::fs::set_permissions(&bak, std::fs::Permissions::from_mode(modo))?;
    }
    let temporario = PathBuf::from(format!("{}.novo", destino.display()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(modo)
        .open(&temporario)
        .with_context(|| format!("escrevendo {}", temporario.display()))?;
    std::io::Write::write_all(&mut f, conteudo.as_bytes())?;
    std::fs::set_permissions(&temporario, std::fs::Permissions::from_mode(modo))?;
    std::fs::rename(&temporario, destino)
        .with_context(|| format!("trocando {}", destino.display()))?;
    Ok(())
}
