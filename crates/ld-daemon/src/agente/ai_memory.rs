//! O ai-memory como [`Memoria`]: a sessão sobe como `ai-memory run ... <agente>`.
//!
//! Fora de worktree, cada partida ganha um workstream próprio e inédito (`--new
//! lukadispatch-<id>-<carimbo>`), como sempre foi.
//!
//! Numa worktree, três coisas mudam, e todas vêm do mesmo requisito: a memória é do PROJETO, e o
//! registro do que cada sessão fez é da WORKTREE.
//!
//! - **Marcador.** Sem ele, os hooks do ai-memory mandam só o cwd, e o servidor dá à worktree
//!   um projeto com o nome da pasta (a branch). O daemon grava um `.ai-memory.toml` na pasta que
//!   junta as worktrees do projeto, com o workspace e o projeto que o repositório principal
//!   resolve. Fica fora de qualquer checkout, e um marcador versionado no repositório continua
//!   valendo, porque o mais próximo vence.
//! - **Um workstream por worktree**, com o nome da branch. O ai-memory já separa workstream por
//!   checkout; usar sempre o mesmo dá à branch um registro contínuo, e a sessão nova numa
//!   worktree recebe o que ainda não viu dele.
//! - **Handoff.** O handoff manual (`memory_handoff_begin`) vale para o projeto inteiro e é de
//!   uso único: a sessão de uma worktree consumiria o que você deixou no terminal, e vice-versa.
//!   O ai-memory não tem como restringi-lo a um checkout. Então a sessão da worktree é instruída
//!   a guardar o "onde parei" numa página da branch, e antes de ela subir o daemon tira da fila os
//!   handoffs manuais abertos e os devolve depois que ela já passou pelo início, falando MCP por
//!   HTTP com o servidor do ai-memory.
//!
//! O que foi medido para chegar aqui está na decisão 0018.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ld_core::state::Worktree;
use serde_json::{Value, json};
use tokio::process::Command;
use tracing::{info, warn};

use super::{Guardado, Memoria, PartidaDaMemoria};

const PROGRAMA: &str = "ai-memory";

/// O nome do arquivo que o ai-memory procura subindo a partir do cwd.
pub const MARCADOR: &str = ".ai-memory.toml";

/// Quantos handoffs manuais o daemon tira da fila de uma vez. Mais que isso é acúmulo antigo, que
/// o `ai-memory handoffs --expire-all` resolve melhor.
const TETO_DE_HANDOFFS: usize = 5;

/// `ai-memory run ... <agente...>`: a sessão entra na memória de longo prazo.
#[derive(Debug, Default, Clone, Copy)]
pub struct AiMemory;

/// Nome do workstream de uma partida fora de worktree.
///
/// Único por partida: o `ai-memory run` recusa com 409 um workstream já ativo e recusa de novo
/// um nome de `--new` que já existe, e a mesma sessão sobe mais de uma vez (troca de modelo por
/// `--resume`). O prefixo `lukadispatch-<8 do id>` é o que você procura no `ai-memory`; o
/// carimbo de tempo no fim é o que o torna inédito.
pub fn workstream(session_id: &str) -> String {
    format!("lukadispatch-{}-{}", &session_id[..8], agora())
}

/// O workstream de uma worktree: o nome da branch, sem barra, que o ai-memory recusa. Não
/// colide: nomes de workstream são por checkout, e cada worktree tem uma branch só.
pub fn workstream_da_worktree(branch: &str) -> String {
    branch
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\') || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .take(64)
        .collect()
}

/// A página do wiki do projeto com o estado de uma branch: o "onde parei" das sessões dela.
pub fn pagina_da_branch(branch: &str) -> String {
    format!("worktrees/{branch}.md")
}

/// Workspace e projeto do ai-memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Escopo {
    pub workspace: String,
    pub project: String,
}

/// O escopo que o ai-memory dá ao repositório principal, pelas regras do marcador dele.
///
/// Sobe da pasta do repositório até o `$HOME` e usa o primeiro marcador que declara escopo (um
/// que só tem `[capture]` é transparente). O que o marcador não declarar fica com o padrão do
/// ai-memory: workspace `default` e o nome da pasta do repositório. Fora do `$HOME` só a própria
/// pasta conta, como no ai-memory.
pub fn escopo_do_repositorio(raiz: &Path, home: &Path) -> Escopo {
    let mut escopo = Escopo {
        workspace: "default".into(),
        project: raiz
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "default".into()),
    };
    let mut dir = Some(raiz);
    while let Some(d) = dir {
        if let Some((workspace, project)) = le_marcador(&d.join(MARCADOR)) {
            if let Some(w) = workspace {
                escopo.workspace = w;
            }
            if let Some(p) = project {
                escopo.project = p;
            }
            break;
        }
        if d == home || !d.starts_with(home) {
            break;
        }
        dir = d.parent();
    }
    escopo
}

/// `(workspace, project)` de um marcador que declara escopo; `None` se não existe ou só tem
/// `[capture]`.
fn le_marcador(caminho: &Path) -> Option<(Option<String>, Option<String>)> {
    let texto = std::fs::read_to_string(caminho).ok()?;
    let doc: toml::Table = toml::from_str(&texto).ok()?;
    let texto_de = |chave: &str| doc.get(chave).and_then(|v| v.as_str()).map(str::to_string);
    let declara = ["workspace", "project", "project_strategy"]
        .iter()
        .any(|c| doc.contains_key(*c));
    declara.then(|| (texto_de("workspace"), texto_de("project")))
}

/// O conteúdo do marcador que o daemon grava acima das worktrees de um projeto.
pub fn marcador(escopo: &Escopo, raiz: &str) -> String {
    let aspas = |s: &str| toml::Value::String(s.to_string()).to_string();
    format!(
        "# Gerado pelo lukadispatch. As worktrees abaixo daqui são do projeto em\n\
         # {raiz}, e a memória delas é a dele. Reescrito a cada sessão aberta numa delas.\n\
         workspace = {}\nproject = {}\n",
        aspas(&escopo.workspace),
        aspas(&escopo.project),
    )
}

/// A pasta que junta as worktrees do projeto: o caminho da worktree sem a branch no fim.
fn pasta_do_projeto(w: &Worktree) -> PathBuf {
    let mut pasta = PathBuf::from(&w.caminho);
    for _ in w.branch.split('/') {
        pasta.pop();
    }
    pasta
}

#[async_trait::async_trait]
impl Memoria for AiMemory {
    fn nome(&self) -> &'static str {
        "ai-memory"
    }

    async fn embrulha(
        &self,
        partida: &PartidaDaMemoria<'_>,
        argv: Vec<String>,
    ) -> Result<Vec<String>> {
        let mut linha = vec![PROGRAMA.to_string(), "run".to_string()];
        match partida.worktree {
            Some(w) if !partida.isolada => {
                let nome = workstream_da_worktree(&w.branch);
                match workstream_existe(partida.cwd, &nome).await {
                    Ok(true) => linha.extend(["--workstream".into(), nome]),
                    Ok(false) => linha.extend(["--new".into(), nome]),
                    // Sem saber se ele existe, `--new` pode esbarrar num nome já tomado, e
                    // `--workstream` num que não existe: um nome inédito sobe de qualquer jeito.
                    Err(e) => {
                        warn!(erro = %format!("{e:#}"), "não consegui listar os workstreams; esta partida usa um só dela");
                        linha.extend(["--new".into(), workstream(partida.session_id)]);
                    }
                }
            }
            Some(w) => linha.extend([
                "--new".into(),
                format!("{}-{}", workstream_da_worktree(&w.branch), agora()),
            ]),
            None => linha.extend(["--new".into(), workstream(partida.session_id)]),
        }
        linha.extend(argv);
        Ok(linha)
    }

    fn instrucoes(&self, partida: &PartidaDaMemoria<'_>) -> Option<String> {
        let w = partida.worktree?;
        Some(format!(
            "Esta sessão roda numa git worktree própria, na branch `{branch}` do projeto {projeto} \
             (o repositório principal está em {raiz}). Outras sessões podem estar em outras \
             branches do mesmo projeto, cada uma na sua worktree.\n\n\
             A memória de longo prazo (ai-memory) é a do projeto, a mesma de todas as worktrees. \
             O estado desta branch mora na página `{pagina}` dela: quando chegar o primeiro pedido, \
             antes de trabalhar nele, leia essa página com memory_read_page (se ela não existir, \
             a branch é nova). Reescreva-a com memory_write_page ao fechar um trabalho ou quando \
             pedirem para salvar o contexto: onde parou, o que falta, o que se decidiu nesta \
             branch. NÃO use memory_handoff_begin nesta sessão, mesmo que uma instrução geral \
             mande fazer handoff ao encerrar: o handoff manual vale para o projeto inteiro e \
             seria consumido pela próxima sessão de outra worktree. Aqui, a página da branch faz \
             esse papel.",
            branch = w.branch,
            projeto = w.projeto,
            raiz = w.raiz,
            pagina = pagina_da_branch(&w.branch),
        ))
    }

    async fn antes_da_partida(&self, partida: &PartidaDaMemoria<'_>) -> Result<Option<Guardado>> {
        let Some(w) = partida.worktree else {
            return Ok(None);
        };
        let escopo = escopo_do_repositorio(Path::new(&w.raiz), &ld_core::paths::home());
        let pasta = pasta_do_projeto(w);
        let arquivo = pasta.join(MARCADOR);
        let conteudo = marcador(&escopo, &w.raiz);
        if std::fs::read_to_string(&arquivo).ok().as_deref() != Some(conteudo.as_str()) {
            std::fs::create_dir_all(&pasta)
                .with_context(|| format!("criando {}", pasta.display()))?;
            std::fs::write(&arquivo, conteudo)
                .with_context(|| format!("escrevendo {}", arquivo.display()))?;
        }

        // Tirar da fila é o que protege o seu handoff do terminal; falhar aqui não pode impedir
        // a sessão de subir. O pior caso é o de antes desta proteção existir.
        match tira_handoffs(&escopo).await {
            Ok(tirados) if tirados.is_empty() => Ok(None),
            Ok(tirados) => {
                info!(quantos = tirados.len(), projeto = %escopo.project, "handoffs manuais fora da fila até a sessão subir");
                Ok(Some(Guardado(Box::new(Tirados { escopo, tirados }))))
            }
            Err(e) => {
                warn!(erro = %format!("{e:#}"), "não consegui tirar os handoffs da fila; a sessão pode consumir um");
                Ok(None)
            }
        }
    }

    async fn devolve(&self, guardado: Guardado) -> Result<()> {
        let Ok(t) = guardado.0.downcast::<Tirados>() else {
            return Ok(());
        };
        devolve_handoffs(&t.escopo, &t.tirados).await
    }

    fn ocupada(&self, saida: &str) -> bool {
        // O erro final (o 409) costuma não chegar ao espelho do tmux: o painel fecha antes de a
        // última saída ser lida. O que chega é o aviso de quando ele começa a esperar a trava, e
        // a sessão que morreu depois dele morreu esperando.
        saida.contains("another launcher owns this workstream")
            || saida.contains("workstream is already active")
            || (saida.contains("409") && saida.to_lowercase().contains("workstream"))
    }

    fn segura_a_partida(&self, partida: &PartidaDaMemoria<'_>) -> Duration {
        // Com o workstream da branch, o `ai-memory run` espera até 5 s por uma trava ocupada antes
        // de desistir com 409, e isso tem de caber na espera do hospedeiro. Workstream inédito
        // não tem trava a esperar.
        if partida.worktree.is_some() && !partida.isolada {
            Duration::from_secs(7)
        } else {
            Duration::ZERO
        }
    }

    fn a_parar(&self, pid: u32) -> Vec<u32> {
        // O `ai-memory run` só solta o workstream quando o agente dele sai: matar o próprio
        // `ai-memory` deixa a trava presa por até 90 s.
        filhos(pid)
    }

    async fn worktree_apagada(&self, worktree: &Worktree) -> Result<()> {
        let escopo = escopo_do_repositorio(Path::new(&worktree.raiz), &ld_core::paths::home());
        let saida = Command::new(PROGRAMA)
            .args([
                "delete-page",
                "--path",
                &pagina_da_branch(&worktree.branch),
                "--workspace",
                &escopo.workspace,
                "--project",
                &escopo.project,
            ])
            .output()
            .await
            .context("chamando ai-memory")?;
        if !saida.status.success() {
            let erro = String::from_utf8_lossy(&saida.stderr);
            // A branch que nunca salvou estado não tem página: não há o que apagar.
            if !erro.to_lowercase().contains("not found") {
                bail!("ai-memory delete-page: {}", erro.trim());
            }
        }
        Ok(())
    }
}

/// Os handoffs manuais tirados da fila, com o escopo de onde saíram.
struct Tirados {
    escopo: Escopo,
    tirados: Vec<Value>,
}

/// Aceita os handoffs manuais abertos do projeto, o mais novo primeiro, e devolve o conteúdo
/// deles.
///
/// O aceite vai com um cwd que não existe: handoff automático só é candidato para o cwd dele ou
/// um de dentro, então só os manuais (que valem para qualquer cwd) saem. Assim o handoff
/// automático da própria worktree fica para a sessão dela.
async fn tira_handoffs(escopo: &Escopo) -> Result<Vec<Value>> {
    let mcp = Mcp::do_ai_memory().await?;
    let ninguem = format!("/lukadispatch/nenhum/{}", uuid::Uuid::new_v4());
    let mut tirados = Vec::new();
    for _ in 0..TETO_DE_HANDOFFS {
        let r = mcp
            .chama(
                "memory_handoff_accept",
                json!({
                    "workspace": escopo.workspace,
                    "project": escopo.project,
                    "cwd": ninguem,
                }),
            )
            .await?;
        match r.get("handoff") {
            Some(h) if !h.is_null() => tirados.push(h.clone()),
            _ => break,
        }
    }
    Ok(tirados)
}

/// Recria os handoffs tirados, do mais velho para o mais novo, para a ordem entre eles ficar a
/// mesma.
async fn devolve_handoffs(escopo: &Escopo, tirados: &[Value]) -> Result<()> {
    let mcp = Mcp::do_ai_memory().await?;
    for h in tirados.iter().rev() {
        let mut args = json!({
            "workspace": escopo.workspace,
            "project": escopo.project,
            "summary": h.get("summary").and_then(Value::as_str).unwrap_or_default(),
            "open_questions": h.get("open_questions").cloned().unwrap_or(json!([])),
            "next_steps": h.get("next_steps").cloned().unwrap_or(json!([])),
            "files_touched": h.get("files_touched").cloned().unwrap_or(json!([])),
        });
        if let Some(cwd) = h.get("cwd").and_then(Value::as_str) {
            args["cwd"] = json!(cwd);
        }
        mcp.chama("memory_handoff_begin", args).await?;
    }
    info!(quantos = tirados.len(), projeto = %escopo.project, "handoffs manuais de volta à fila");
    Ok(())
}

/// O endpoint MCP do servidor do ai-memory, falado por HTTP.
///
/// Não pela ponte de stdio (`ai-memory mcp-bridge`): ela exige `CLAUDE_CODE_SESSION_ID` e sai na
/// hora sem ela ("this bridge must be launched by Claude Code"), e o daemon não é uma sessão do
/// Claude Code. O endpoint HTTP responde sem sessão (modo sem estado), e cada chamada é uma
/// requisição.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mcp {
    host: String,
    porta: u16,
    caminho: String,
    token: Option<String>,
}

/// O endpoint resolvido uma vez por vida do daemon: descobrir custa um `ai-memory status`.
static MCP: tokio::sync::OnceCell<Mcp> = tokio::sync::OnceCell::const_new();

impl Mcp {
    const PRAZO: Duration = Duration::from_secs(10);

    /// Onde o servidor escuta, como o próprio ai-memory resolve (`AI_MEMORY_SERVER_URL`, o
    /// `server_url` do config dele, senão o loopback): quem diz é o `ai-memory status --json`.
    async fn do_ai_memory() -> Result<&'static Mcp> {
        MCP.get_or_try_init(|| async {
            let saida = Command::new(PROGRAMA)
                .args(["status", "--json"])
                .output()
                .await
                .context("chamando ai-memory status")?;
            if !saida.status.success() {
                bail!(
                    "ai-memory status: {}",
                    String::from_utf8_lossy(&saida.stderr).trim()
                );
            }
            let status: Value = serde_json::from_slice(&saida.stdout)
                .context("ai-memory status respondeu fora de JSON")?;
            let cliente = &status["client"];
            let url = cliente["server_url"]
                .as_str()
                .context("ai-memory status não disse o server_url")?;
            let token = if cliente["auth"].as_bool() == Some(true) {
                Some(std::env::var("AI_MEMORY_AUTH_TOKEN").context(
                    "o servidor do ai-memory pede token, e AI_MEMORY_AUTH_TOKEN não está no ambiente do daemon",
                )?)
            } else {
                None
            };
            Mcp::da_url(url, token)
        })
        .await
    }

    /// `http://host[:porta][/prefixo]` vira o endpoint `/mcp` debaixo do prefixo.
    fn da_url(url: &str, token: Option<String>) -> Result<Self> {
        let resto = url.strip_prefix("http://").with_context(|| {
            format!("só sei falar com o ai-memory por http://, e ele está em {url}")
        })?;
        let (autoridade, prefixo) = resto.split_once('/').unwrap_or((resto, ""));
        let (host, porta) = match autoridade.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().context("porta inválida")?),
            None => (autoridade.to_string(), 80),
        };
        let prefixo = prefixo.trim_end_matches('/');
        let caminho = if prefixo.is_empty() {
            "/mcp".to_string()
        } else if prefixo.ends_with("mcp") {
            format!("/{prefixo}")
        } else {
            format!("/{prefixo}/mcp")
        };
        Ok(Self {
            host,
            porta,
            caminho,
            token,
        })
    }

    /// Chama uma ferramenta e devolve o JSON que ela respondeu.
    async fn chama(&self, ferramenta: &str, args: Value) -> Result<Value> {
        let pedido = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": ferramenta, "arguments": args},
        });
        let bruto = tokio::time::timeout(Self::PRAZO, self.post(&serde_json::to_vec(&pedido)?))
            .await
            .with_context(|| format!("o ai-memory não respondeu a {ferramenta}"))??;
        let msg = corpo_da_resposta(&bruto)?;
        if let Some(erro) = msg.get("error") {
            bail!("ai-memory recusou {ferramenta}: {erro}");
        }
        resultado_da_ferramenta(ferramenta, msg.get("result").unwrap_or(&Value::Null))
    }

    async fn post(&self, corpo: &[u8]) -> Result<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut conexao = tokio::net::TcpStream::connect((self.host.as_str(), self.porta))
            .await
            .with_context(|| format!("conectando ao ai-memory em {}:{}", self.host, self.porta))?;
        let mut cabecalho = format!(
            "POST {} HTTP/1.1\r\nHost: {}:{}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
             Connection: close\r\n",
            self.caminho,
            self.host,
            self.porta,
            corpo.len()
        );
        if let Some(t) = &self.token {
            cabecalho.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        cabecalho.push_str("\r\n");
        conexao.write_all(cabecalho.as_bytes()).await?;
        conexao.write_all(corpo).await?;
        let mut resposta = Vec::new();
        conexao.read_to_end(&mut resposta).await?;
        Ok(resposta)
    }
}

/// A mensagem JSON-RPC de uma resposta HTTP inteira: confere o status, desfaz o `chunked` e tira
/// o JSON de dentro de um evento SSE, que é como o servidor responde quando prefere streaming.
fn corpo_da_resposta(bruto: &[u8]) -> Result<Value> {
    let fim = bruto
        .windows(4)
        .position(|j| j == b"\r\n\r\n")
        .context("resposta do ai-memory sem cabeçalho")?;
    let cabecalho = String::from_utf8_lossy(&bruto[..fim]).to_lowercase();
    let mut corpo = bruto[fim + 4..].to_vec();
    let linha_de_status = cabecalho.lines().next().unwrap_or_default().to_string();
    if !linha_de_status
        .split_whitespace()
        .nth(1)
        .is_some_and(|c| c.starts_with('2'))
    {
        bail!(
            "o ai-memory respondeu {}: {}",
            linha_de_status,
            String::from_utf8_lossy(&corpo).trim()
        );
    }
    if cabecalho.contains("transfer-encoding: chunked") {
        corpo = desfaz_chunked(&corpo)?;
    }
    let texto = String::from_utf8_lossy(&corpo);
    if cabecalho.contains("text/event-stream") {
        let dados: String = texto
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(str::trim)
            .collect();
        return serde_json::from_str(&dados).context("evento do ai-memory fora de JSON");
    }
    serde_json::from_str(texto.trim()).context("resposta do ai-memory fora de JSON")
}

fn desfaz_chunked(mut resto: &[u8]) -> Result<Vec<u8>> {
    let mut saida = Vec::new();
    loop {
        let fim = resto
            .windows(2)
            .position(|j| j == b"\r\n")
            .context("pedaço chunked sem tamanho")?;
        let tamanho = usize::from_str_radix(
            String::from_utf8_lossy(&resto[..fim])
                .split(';')
                .next()
                .unwrap_or_default()
                .trim(),
            16,
        )
        .context("tamanho de pedaço chunked inválido")?;
        resto = &resto[fim + 2..];
        if tamanho == 0 {
            return Ok(saida);
        }
        let pedaco = resto.get(..tamanho).context("pedaço chunked cortado")?;
        saida.extend_from_slice(pedaco);
        resto = resto.get(tamanho + 2..).unwrap_or_default();
    }
}

/// O JSON dentro da resposta de uma ferramenta MCP: o primeiro bloco de texto do `content`.
fn resultado_da_ferramenta(ferramenta: &str, r: &Value) -> Result<Value> {
    let texto = r
        .get("content")
        .and_then(Value::as_array)
        .and_then(|c| c.iter().find_map(|b| b.get("text").and_then(Value::as_str)))
        .unwrap_or_default();
    if r.get("isError").and_then(Value::as_bool) == Some(true) {
        bail!("{ferramenta}: {texto}");
    }
    serde_json::from_str(texto).with_context(|| format!("{ferramenta} respondeu fora de JSON"))
}

/// Os workstreams deste checkout têm um com este nome?
async fn workstream_existe(cwd: &Path, nome: &str) -> Result<bool> {
    let saida = Command::new(PROGRAMA)
        // 100 é o teto que o ai-memory aceita; acima disso ele recusa o comando inteiro. Um
        // checkout é uma branch, e ela não chega perto de 100 workstreams.
        .args(["workstreams", "--json", "--limit", "100"])
        .current_dir(cwd)
        .output()
        .await
        .context("chamando ai-memory workstreams")?;
    if !saida.status.success() {
        let erro = String::from_utf8_lossy(&saida.stderr);
        // Projeto que o ai-memory ainda não viu não tem workstream: é a primeira sessão dele, e
        // ele responde 404 em vez de lista vazia.
        if erro.contains("not found in workspace") {
            return Ok(false);
        }
        bail!("ai-memory workstreams: {}", erro.trim());
    }
    let lista: Value = serde_json::from_slice(&saida.stdout)
        .context("ai-memory workstreams respondeu fora de JSON")?;
    Ok(tem_workstream(&lista, nome))
}

fn tem_workstream(lista: &Value, nome: &str) -> bool {
    lista.as_array().is_some_and(|l| {
        l.iter()
            .any(|w| w.get("name").and_then(Value::as_str) == Some(nome))
    })
}

/// Os processos filhos diretos de `pid`, pelo pai que cada um declara no `/proc/<pid>/stat`.
///
/// Varre o `/proc` em vez de ler o `children` do processo, que só existe com uma opção do
/// kernel ligada.
pub(crate) fn filhos(pid: u32) -> Vec<u32> {
    let Ok(entradas) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entradas
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&filho| pai_de(filho) == Some(pid))
        .collect()
}

/// O pai no `/proc/<pid>/stat`: o campo depois do estado, que vem depois do nome entre
/// parênteses. O nome pode ter espaço e parêntese, por isso a busca é pelo ÚLTIMO `)`.
fn pai_de(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let depois = &stat[stat.rfind(')')? + 1..];
    depois.split_whitespace().nth(1)?.parse().ok()
}

fn agora() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "ai_memory_testes.rs"]
mod testes;
