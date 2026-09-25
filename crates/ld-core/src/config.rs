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

/// A variável do `.env` com o token do bot. O daemon a lê e o setup a grava: um nome só.
pub const TOKEN_TELEGRAM: &str = "LUKADISPATCH_TELEGRAM_TOKEN";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Qual frontend de chat o daemon usa. Cada valor corresponde a uma implementação da trait
    /// `Frontend` do daemon; hoje existe `"telegram"`. O modo offline (`LUKADISPATCH_OFFLINE`)
    /// passa por cima disto e usa o frontend nulo.
    pub frontend: String,
    /// Onde cada sessão roda. Cada valor corresponde a uma implementação da trait `Hospedeiro`
    /// do daemon; hoje existem `"tmux"` e `"herdr"`.
    pub hospedeiro: String,
    /// Opções do hospedeiro herdr, lidas só quando `hospedeiro = "herdr"`.
    pub herdr: Herdr,
    /// Qual agente de código roda nas sessões, e com que memória de longo prazo.
    pub agente: Agente,
    pub telegram: Telegram,
    pub scan: Scan,
    /// Como o áudio que chega vira texto. O motor é trocável sem recompilar.
    pub transcricao: Transcricao,
    /// Como arquivo grande demais para o frontend é partido antes de sair.
    pub arquivos: Arquivos,
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
    /// Ferramentas que pedem card no modo "perguntar no celular".
    ///
    /// O portão é consultado para TODA ferramenta (senão o `dontAsk` por baixo negaria em
    /// silêncio o que ficasse de fora); esta lista é o que vira pergunta. O resto é liberado na
    /// hora, sem card. Prefixo `mcp__` cobre servidor MCP inteiro.
    pub ask_tools: Vec<String>,

    /// Passar os servidores MCP pelo proxy, para o diálogo deles caber no celular.
    ///
    /// Desligue se algum servidor seu não gostar de ter um processo no meio do cano: as sessões
    /// voltam a falar direto com eles, e o diálogo volta a só dar para responder no PC.
    pub wrap_mcp: bool,

    /// Como a sessão chama você no prompt de partida. Sem nome, ela diz "o seu usuário".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usuario: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            frontend: "telegram".into(),
            hospedeiro: "tmux".into(),
            herdr: Herdr::default(),
            agente: Agente::default(),
            telegram: Telegram::default(),
            scan: Scan::default(),
            transcricao: Transcricao::default(),
            arquivos: Arquivos::default(),
            projects: Vec::new(),
            default_permission_mode: "auto".into(),
            trust_projects: true,
            history_lines: 8,
            claude_binary: None,
            ask_tools: [
                "Bash",
                "Write",
                "Edit",
                "MultiEdit",
                "NotebookEdit",
                "WebFetch",
                "mcp__",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            wrap_mcp: true,
            usuario: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Herdr {
    /// A sessão nomeada do herdr onde as sessões do bot sobem. Sem valor, é a sessão padrão (a
    /// do `herdr` sem argumentos), e as sessões do bot aparecem ao lado das suas.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessao: Option<String>,
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

/// Como o áudio que chega vira texto.
///
/// O motor mora aqui e não no código porque a escolha é do hardware, não do projeto. O padrão é
/// o que ganhou o benchmark neste notebook (Ryzen 5700U, Radeon Vega sem VRAM dedicada):
/// whisper.cpp com large-v3-turbo quantizado em q5_0, no backend Vulkan. Ali ele fez 12,9% de
/// erro nos áudios reais a 44 s por minuto de fala, usando ~1 GB.
///
/// O `lukadispatch setup` instala um de dois motores e escreve estes campos. Trocar à mão é
/// editar `comando`, `modelo` e `saida`. Os dois que o setup oferece, e o terceiro medido:
///
/// ```toml
/// # whisper.cpp large-v3-turbo q5_0 (o padrão): 12,9% de erro, 44 s por minuto no Vulkan.
/// comando = ["~/.local/share/lukadispatch/asr/whisper-cli", "-m", "{modelo}",
///            "-f", "{audio}", "-l", "pt", "-t", "8", "-otxt", "-of", "{saida}", "-nt"]
/// modelo = "~/.local/share/lukadispatch/asr/modelos/ggml-large-v3-turbo-q5_0.bin"
/// saida = "arquivo"
///
/// # O mesmo na CPU (whisper-cli-cpu): mesmo erro, 67 s por minuto, 155 MB a menos.
///
/// # FastConformer-pt no sherpa-onnx: 8 s por minuto e 417 MB, com 21,6% de erro. Bom para
/// # fala corrida, ruim para jargão, nome próprio e palavra em inglês.
/// comando = ["~/.local/share/lukadispatch/asr/sherpa-onnx-offline",
///            "--encoder={modelo}/encoder.int8.onnx", "--decoder={modelo}/decoder.int8.onnx",
///            "--joiner={modelo}/joiner.int8.onnx", "--tokens={modelo}/tokens.txt",
///            "--model-type=nemo_transducer", "--num-threads=8", "{audio}"]
/// modelo = "~/.local/share/lukadispatch/asr/modelos/sherpa-onnx-nemo-transducer-stt_pt_fastconformer_hybrid_large_pc-int8"
/// saida = "json"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Transcricao {
    /// Desligada, áudio continua chegando como arquivo, só não vira texto.
    pub ativa: bool,
    /// Qual implementação da trait `Transcritor` do daemon roda. Hoje existe `"processo"`, que
    /// chama o comando abaixo como processo filho; um motor novo (API remota, biblioteca
    /// embutida) entra como outro valor aqui, sem mexer em quem pede a transcrição.
    pub motor: String,
    /// O comando, já dividido em argumentos (nada de shell no meio).
    ///
    /// Marcadores: `{audio}` é o WAV mono 16 kHz que o daemon prepara, `{modelo}` é o campo
    /// abaixo e `{saida}` é o prefixo do arquivo de texto, sem extensão.
    pub comando: Vec<String>,
    /// Caminho do modelo (ou do diretório dele). `~` é expandido. Vazio quando o comando já
    /// sabe onde está o seu.
    pub modelo: String,
    /// Onde o comando deixa o texto: `"arquivo"` (escreve `{saida}.txt`), `"stdout"` (texto
    /// puro) ou `"json"` (uma linha JSON com o texto no campo `text`, como o sherpa-onnx faz).
    pub saida: String,
    /// Teto de tempo por áudio. Transcrever é lento aqui: um minuto de fala leva de 8 s a 128 s
    /// dependendo do motor, e um áudio longo multiplica isso.
    pub timeout_s: u64,
    /// Por quantos dias o `.oga` original fica em disco depois de transcrito.
    ///
    /// Guardar tem um motivo concreto: quando a transcrição sai estranha, o áudio é a única
    /// forma de saber se o erro foi do modelo ou da gravação. Zero desliga a expiração.
    pub guardar_audio_dias: u64,
}

impl Default for Transcricao {
    fn default() -> Self {
        Self {
            ativa: true,
            motor: "processo".into(),
            comando: [
                "~/.local/share/lukadispatch/asr/whisper-cli-vulkan",
                "-m",
                "{modelo}",
                "-f",
                "{audio}",
                "-l",
                "pt",
                "-t",
                "8",
                "-otxt",
                "-of",
                "{saida}",
                "-nt",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            modelo: "~/.local/share/lukadispatch/asr/modelos/ggml-large-v3-turbo-q5_0.bin".into(),
            saida: "arquivo".into(),
            timeout_s: 900,
            guardar_audio_dias: 7,
        }
    }
}

/// O agente de código das sessões.
///
/// São duas escolhas independentes. `tipo` é o agente em si (hoje `"claude-code"`): quem sabe
/// montar a linha de comando, o prompt de partida, os hooks e onde fica a conversa gravada.
/// `memoria` é a memória de longo prazo, que embrulha essa linha de comando antes de rodar:
/// `"ai-memory"` sobe o agente como `ai-memory run ... <agente...>`, para a sessão entrar na
/// memória de longo prazo; `"nenhuma"` roda o agente direto.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Agente {
    pub tipo: String,
    /// A chave nasceu `envelope` (com o valor `"nenhum"`), e é assim que está nos configs
    /// instalados antes da troca de nome.
    #[serde(alias = "envelope")]
    pub memoria: String,
}

impl Default for Agente {
    fn default() -> Self {
        Self {
            tipo: "claude-code".into(),
            memoria: "ai-memory".into(),
        }
    }
}

/// Como um arquivo que não cabe numa mensagem é partido.
///
/// São duas peças, e a ordem entre elas é fixa: vídeo tenta primeiro o corte por tempo (cada
/// trecho toca sozinho no celular), e o que sobrar, ou falhar, cai no divisor genérico.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Arquivos {
    /// Divisor genérico, por volumes: `"7z"` ou `"rar"`. Cada valor corresponde a uma
    /// implementação da trait `Divisor` do daemon.
    pub divisor: String,
    /// Vídeo grande é cortado em trechos pelo ffmpeg antes de cair nos volumes.
    pub cortar_video: bool,
}

impl Default for Arquivos {
    fn default() -> Self {
        Self {
            divisor: "7z".into(),
            cortar_video: true,
        }
    }
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
        let t = std::env::var(TOKEN_TELEGRAM)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .with_context(|| {
                format!("{TOKEN_TELEGRAM} não está no ambiente (rode lukadispatch setup)")
            })?;
        Ok(t.trim().to_string())
    }

    pub fn allows(&self, user_id: i64) -> bool {
        self.telegram.allowed_user_ids.contains(&user_id)
    }

    /// `true` quando esta ferramenta merece um card no modo de perguntar.
    pub fn pergunta_por(&self, ferramenta: &str) -> bool {
        self.ask_tools
            .iter()
            .any(|regra| ferramenta == regra || ferramenta.starts_with(regra.as_str()))
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
    fn o_exemplo_distribuido_carrega() {
        // O config.example.toml vai no pacote da release e o install.sh o copia para quem
        // instala: se ele parar de carregar, o primeiro contato de alguém é um erro de parse.
        let cfg: Config = toml::from_str(include_str!("../../../dist/config.example.toml"))
            .expect("dist/config.example.toml precisa carregar");
        assert!(cfg.trust_projects);
        assert_eq!(cfg.telegram.chat_id, -1001234567890);
        assert_eq!(cfg.usuario, None, "o nome fica comentado no exemplo");
    }

    #[test]
    fn config_ausente_vira_padrao() {
        let c = Config::load(Path::new("/nao/existe/config.toml")).unwrap();
        assert_eq!(c.default_permission_mode, "auto");
        assert!(c.scan.enabled);
    }

    #[test]
    fn a_lista_de_perguntar_cobre_escrita_e_mcp_mas_nao_leitura() {
        let c = Config::default();
        assert!(c.pergunta_por("Bash"));
        assert!(c.pergunta_por("Write"));
        assert!(c.pergunta_por("mcp__wamux-omarchy__whatsapp_send"));
        assert!(!c.pergunta_por("Read"));
        assert!(!c.pergunta_por("Glob"));
        assert!(
            !c.pergunta_por("TodoWrite"),
            "TodoWrite não escreve no disco"
        );
    }

    #[test]
    fn seletores_das_pecas_tem_padrao_e_aceitam_troca_pelo_toml() {
        // Config antigo, sem as chaves novas, tem de continuar subindo com o que já rodava.
        let c = Config::default();
        assert_eq!(c.frontend, "telegram");
        assert_eq!(c.agente.tipo, "claude-code");
        assert_eq!(c.agente.memoria, "ai-memory");
        assert_eq!(c.transcricao.motor, "processo");
        assert_eq!(c.arquivos.divisor, "7z");
        assert!(c.arquivos.cortar_video);
        assert_eq!(c.hospedeiro, "tmux");
        assert_eq!(
            c.herdr.sessao, None,
            "sem sessão nomeada, é a padrão do herdr"
        );

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            r#"
frontend = "whatsapp"
hospedeiro = "herdr"

[herdr]
sessao = "bot"

[agente]
memoria = "nenhuma"

[transcricao]
motor = "api"

[arquivos]
divisor = "rar"
cortar_video = false
"#,
        )
        .unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.frontend, "whatsapp");
        assert_eq!(c.agente.memoria, "nenhuma");
        assert_eq!(
            c.agente.tipo, "claude-code",
            "a chave que faltou mantém o padrão"
        );
        assert_eq!(c.transcricao.motor, "api");
        assert!(c.transcricao.ativa, "a chave que faltou mantém o padrão");
        assert_eq!(c.arquivos.divisor, "rar");
        assert!(!c.arquivos.cortar_video);
        assert_eq!(c.hospedeiro, "herdr");
        assert_eq!(c.herdr.sessao.as_deref(), Some("bot"));
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

    #[test]
    fn config_de_antes_da_troca_de_nome_ainda_escolhe_a_memoria() {
        // Configs instalados antes da troca de nome dizem `envelope = "nenhum"`.
        let c: Config = toml::from_str("[agente]\nenvelope = \"nenhum\"\n").unwrap();
        assert_eq!(c.agente.memoria, "nenhum");
    }
}
