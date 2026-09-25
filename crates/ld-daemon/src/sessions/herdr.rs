//! O herdr como [`Hospedeiro`]: cada sessão é uma aba no workspace do projeto, anexável no PC
//! com `herdr terminal attach` e visível ao lado das suas no herdr que você já usa.
//!
//! Fala direto com o socket do servidor (JSON por linha), e não pela CLI, porque o único jeito
//! de abrir um pane já rodando um comando com ambiente próprio é o método `layout.apply`, que a
//! CLI não expõe. Pela CLI seria abrir um shell e digitar o comando nele.
//!
//! A hospedagem gravada no banco é `rótulo@terminal@pid:início` (ver [`Hospedagem`]). O rótulo
//! (`ld-projeto-abcd`) é o nome que você acha na aba e o que acha o pane; o terminal é o que o
//! `terminal attach` pede; o processo é o que diz se a sessão vive. O terminal não serve para
//! isso: ele muda num restart do servidor, que mata a sessão, e também num live handoff, que a
//! mantém de pé.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ld_core::config::Project;
use ld_core::paths;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

use super::{Hospedeiro, Launched, Situacao, limpa_ambiente, nome_da_sessao, primeiro_erro};
use crate::agente::Partida;

/// Quem assina os metadados que o daemon põe no pane (o título).
const FONTE: &str = "custom:lukadispatch";

/// Quanto uma chamada ao socket pode levar. As do hospedeiro são todas de uma linha e voltam em
/// milissegundos; passar disto é servidor travado, e travar o daemon junto não ajuda ninguém.
const PRAZO: Duration = Duration::from_secs(10);

/// Quanto o `ping` que confere se o servidor está de pé pode levar. A reconciliação confere cada
/// sessão viva a cada minuto, e um servidor que não responde em um segundo conta como fora do
/// ar naquela volta, em vez de segurar a varredura inteira por dez.
const PRAZO_DE_CONFERIR: Duration = Duration::from_secs(1);

/// O `sun_path` de um socket unix no Linux: 108 bytes, contando o NUL do fim.
const SUN_PATH: usize = 108;

#[derive(Debug, Clone)]
pub struct Herdr {
    /// Sessão nomeada do herdr. `None` é a padrão.
    sessao: Option<String>,
    /// `XDG_CONFIG_HOME` imposto ao servidor que o daemon sobe. `None` fora dos testes: o daemon
    /// acha o socket onde o herdr do usuário o põe, pelo mesmo ambiente.
    config_home: Option<PathBuf>,
    /// O socket da sessão, calculado uma vez: o caminho não muda enquanto o daemon vive.
    socket: PathBuf,
    /// Para onde vai a saída de erro do servidor que o daemon sobe.
    log_do_servidor: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Painel {
    pane_id: String,
    terminal_id: String,
    #[serde(default)]
    label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Workspace {
    workspace_id: String,
    #[serde(default)]
    label: Option<String>,
}

/// O diretório de config do herdr, na ordem do herdr 0.8.2 (`src/config/io.rs`): o
/// `XDG_CONFIG_HOME`, se existir (mesmo vazio), senão `~/.config`, senão o temporário. O
/// `HERDR_CONFIG_PATH` não entra: ele troca só o caminho do `config.toml`.
fn dir_do_herdr(xdg_config_home: Option<String>, home: Option<String>) -> PathBuf {
    match (xdg_config_home, home) {
        (Some(xdg), _) => PathBuf::from(xdg).join("herdr"),
        (None, Some(home)) => PathBuf::from(home).join(".config").join("herdr"),
        (None, None) => std::env::temp_dir().join("herdr"),
    }
}

/// Onde o servidor de uma sessão escuta (`src/session.rs` do herdr): a padrão em
/// `<dir>/herdr.sock`, a nomeada em `<dir>/sessions/<nome>/herdr.sock`. O nome `default` é a
/// padrão, como o herdr o normaliza.
///
/// Calculado, e não perguntado ao `herdr session list`: aquilo era um processo por conferência,
/// e a reconciliação confere cada sessão viva a cada minuto. O `HERDR_SOCKET_PATH` do ambiente
/// fica de fora de propósito: um daemon iniciado de dentro de um pane herdaria o da sessão
/// daquele pane.
fn socket_da_sessao(dir: &Path, sessao: Option<&str>) -> PathBuf {
    match sessao.filter(|s| *s != "default") {
        Some(nome) => dir.join("sessions").join(nome).join("herdr.sock"),
        None => dir.join("herdr.sock"),
    }
}

impl Herdr {
    pub fn new(sessao: Option<String>) -> Self {
        Self::com(sessao, None, paths::state_dir().join("herdr-servidor.log"))
    }

    fn com(sessao: Option<String>, config_home: Option<PathBuf>, log: PathBuf) -> Self {
        let sessao = sessao.filter(|s| !s.trim().is_empty());
        let dir = match &config_home {
            Some(c) => c.join("herdr"),
            None => dir_do_herdr(
                std::env::var("XDG_CONFIG_HOME").ok(),
                std::env::var("HOME").ok(),
            ),
        };
        Self {
            socket: socket_da_sessao(&dir, sessao.as_deref()),
            sessao,
            config_home,
            log_do_servidor: log,
        }
    }

    /// `--session <nome>` quando a sessão é nomeada. Explícito sempre: o `herdr` resolve o
    /// socket primeiro pela flag e só depois pelo ambiente, e o daemon pode ter herdado o
    /// `HERDR_SOCKET_PATH` de outra sessão se foi iniciado de dentro de um pane.
    fn flag_sessao(&self) -> Vec<String> {
        match &self.sessao {
            Some(s) => vec!["--session".into(), s.clone()],
            None => Vec::new(),
        }
    }

    /// O servidor da sessão responde? Não sobe nada: é o que `vive`, `mata` e `nossas` usam, e
    /// conferir não pode ter o efeito colateral de ligar um servidor.
    ///
    /// Responder ao `ping`, e não só aceitar a conexão: um socket que ficou de um servidor morto,
    /// ou de um travado, não pode passar por sessão de pé.
    async fn de_pe(&self) -> bool {
        self.socket.exists()
            && chama_em(&self.socket, "ping", json!({}), PRAZO_DE_CONFERIR)
                .await
                .is_ok()
    }

    /// Sobe o servidor da sessão, se ele não estiver de pé.
    ///
    /// Ao contrário do `tmux new-session`, a CLI do herdr não sobe servidor sozinha. O servidor
    /// sobe em grupo de processo próprio, para sobreviver ao restart do daemon (a unit usa
    /// `KillMode=process` pelo mesmo motivo). A saída de erro dele vai para um arquivo: o
    /// servidor que não sobe (nome de sessão inválido, sessão corrompida) diz o motivo ali, e só
    /// ali.
    async fn garante_servidor(&self) -> Result<()> {
        if self.de_pe().await {
            return Ok(());
        }
        // Com o caminho longo demais o servidor morre ao subir, e o motivo que ele deixa não diz
        // qual caminho. Aparece com um XDG_CONFIG_HOME fundo.
        if self.socket.as_os_str().len() >= SUN_PATH {
            bail!(
                "o socket do herdr ficaria em {} ({} bytes), e um socket unix aceita até {}: \
                 encurte o nome da sessão em [herdr] sessao ou o XDG_CONFIG_HOME",
                self.socket.display(),
                self.socket.as_os_str().len(),
                SUN_PATH - 1
            );
        }
        let log = &self.log_do_servidor;
        if let Some(pai) = log.parent() {
            let _ = std::fs::create_dir_all(pai);
        }
        let erro = std::fs::File::create(log)
            .map(std::process::Stdio::from)
            .unwrap_or_else(|_| std::process::Stdio::null());
        let mut comando = Command::new("herdr");
        limpa_ambiente(&mut comando)
            .args(self.flag_sessao())
            .arg("server")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_SESSION")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(erro)
            .process_group(0);
        if let Some(c) = &self.config_home {
            comando.env("XDG_CONFIG_HOME", c);
        }
        let mut servidor = comando.spawn().context("subindo o servidor do herdr")?;
        let nome = self.sessao.as_deref().unwrap_or("sessão padrão");
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if self.de_pe().await {
                return Ok(());
            }
            // Morto não sobe mais: esperar o resto do prazo só atrasaria o erro.
            if let Ok(Some(status)) = servidor.try_wait() {
                bail!(
                    "o servidor do herdr ({nome}) saiu ao subir ({status}): {}",
                    motivo_no_log(log)
                );
            }
        }
        bail!(
            "o servidor do herdr ({nome}) não subiu em 5 s: {}",
            motivo_no_log(log)
        )
    }

    async fn paineis(&self, socket: &Path) -> Result<Vec<Painel>> {
        let r = chama(socket, "pane.list", json!({})).await?;
        serde_json::from_value(r["panes"].clone()).context("lendo pane.list do herdr")
    }

    /// O pane que roda este terminal, se ele ainda existe.
    async fn painel_do_terminal(&self, terminal: &str) -> Option<Painel> {
        if !self.de_pe().await {
            return None;
        }
        self.paineis(&self.socket)
            .await
            .ok()?
            .into_iter()
            .find(|p| p.terminal_id == terminal)
    }

    /// Os panes com este rótulo. Mais de um só sobra de um relançamento interrompido no meio.
    /// Sem servidor de pé, nenhum.
    async fn paineis_do_rotulo(&self, rotulo: &str) -> Vec<Painel> {
        if rotulo.is_empty() || !self.de_pe().await {
            return Vec::new();
        }
        self.paineis(&self.socket)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.label.as_deref() == Some(rotulo))
            .collect()
    }

    /// O workspace do projeto: o que já tem o nome dele (inclusive um aberto por você) ou um
    /// novo. Devolve o alvo do `layout.apply`: a aba a substituir, num workspace novo, para ele
    /// não ficar com um shell sobrando ao lado da sessão.
    async fn alvo_no_projeto(&self, socket: &Path, projeto: &Project) -> Result<Value> {
        let r = chama(socket, "workspace.list", json!({})).await?;
        let lista: Vec<Workspace> = serde_json::from_value(r["workspaces"].clone())
            .context("lendo workspace.list do herdr")?;
        if let Some(w) = lista
            .into_iter()
            .find(|w| w.label.as_deref() == Some(projeto.name.as_str()))
        {
            return Ok(json!({ "workspace_id": w.workspace_id }));
        }
        let r = chama(
            socket,
            "workspace.create",
            json!({ "cwd": projeto.path, "label": projeto.name, "focus": false }),
        )
        .await?;
        let aba = r["tab"]["tab_id"]
            .as_str()
            .context("workspace.create sem tab_id")?;
        Ok(json!({ "tab_id": aba }))
    }
}

/// Uma chamada ao socket: um JSON por linha na ida, um na volta.
async fn chama(socket: &Path, metodo: &str, params: Value) -> Result<Value> {
    chama_em(socket, metodo, params, PRAZO).await
}

/// [`chama`] com um prazo próprio.
async fn chama_em(socket: &Path, metodo: &str, params: Value, prazo: Duration) -> Result<Value> {
    let ida = async {
        let mut s = UnixStream::connect(socket)
            .await
            .with_context(|| format!("conectando no herdr em {}", socket.display()))?;
        let pedido = json!({ "id": "lukadispatch", "method": metodo, "params": params });
        s.write_all(format!("{pedido}\n").as_bytes()).await?;
        let mut linha = String::new();
        BufReader::new(s).read_line(&mut linha).await?;
        anyhow::Ok(linha)
    };
    let linha = tokio::time::timeout(prazo, ida)
        .await
        .with_context(|| format!("o herdr não respondeu {metodo} em {prazo:?}"))??;
    let v: Value = serde_json::from_str(&linha)
        .with_context(|| format!("resposta ilegível do herdr para {metodo}"))?;
    if let Some(e) = v.get("error") {
        bail!(
            "herdr recusou {metodo}: {} ({})",
            e["message"].as_str().unwrap_or("?"),
            e["code"].as_str().unwrap_or("?")
        );
    }
    Ok(v["result"].clone())
}

/// O motivo no log do servidor: a última linha de erro, ou a última não vazia, ou um aviso de
/// que ele não diz nada.
///
/// A última linha sozinha não serve: quando a CLI do herdr recusa (nome de sessão inválido), ela
/// fecha com `run 'herdr --help' for usage`, e o motivo está na linha `error:` de antes.
fn motivo_no_log(log: &Path) -> String {
    let texto = std::fs::read_to_string(log).unwrap_or_default();
    let linhas: Vec<&str> = texto
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    linhas
        .iter()
        .rev()
        .find(|l| l.to_ascii_lowercase().starts_with("error"))
        .or(linhas.last())
        .map(|l| l.chars().take(300).collect())
        .unwrap_or_else(|| format!("sem mensagem ({})", log.display()))
}

/// A hospedagem de uma sessão do herdr: `rótulo@terminal@pid:início`.
///
/// Lida de volta do banco, ela aceita também o formato antigo, `rótulo@terminal`, de sessão
/// lançada antes de o processo ir junto, e o rótulo sozinho, que é o que [`Herdr::nossas`] devolve.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hospedagem<'a> {
    rotulo: &'a str,
    terminal: &'a str,
    /// `None` no formato antigo: aí a sessão vive enquanto o terminal existir, como era.
    processo: Option<Processo>,
}

impl<'a> Hospedagem<'a> {
    fn le(nome: &'a str) -> Self {
        let mut partes = nome.splitn(3, '@');
        let rotulo = partes.next().unwrap_or_default();
        let terminal = partes.next().unwrap_or_default();
        let processo = partes.next().and_then(|p| {
            let (pid, inicio) = p.split_once(':')?;
            Some(Processo {
                pid: pid.parse().ok()?,
                inicio: inicio.parse().ok()?,
            })
        });
        Self {
            rotulo,
            terminal,
            processo,
        }
    }
}

impl std::fmt::Display for Hospedagem<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.rotulo, self.terminal)?;
        if let Some(p) = self.processo {
            write!(f, "@{}:{}", p.pid, p.inicio)?;
        }
        Ok(())
    }
}

/// O processo que vira o agente da sessão: o pid e a hora em que ele começou.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Processo {
    pid: u32,
    /// Em tiques de relógio desde o boot (campo 22 do `/proc/<pid>/stat`). É o que distingue o
    /// nosso processo de outro que ganhou o mesmo pid depois, num reboot por exemplo.
    inicio: u64,
}

impl Processo {
    /// O processo com este pid, se ele está vivo.
    fn de(pid: u32) -> Option<Self> {
        inicio_do_processo(pid).map(|inicio| Self { pid, inicio })
    }

    fn vivo(&self) -> bool {
        inicio_do_processo(self.pid) == Some(self.inicio)
    }
}

/// Quando o processo começou, se ele está vivo. Zumbi não conta: já saiu, só falta alguém o
/// recolher.
fn inicio_do_processo(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // O nome do comando vem entre parênteses e pode ter espaço: os campos contam depois dele, a
    // partir do terceiro (o estado).
    let mut campos = stat.rsplit_once(')')?.1.split_whitespace();
    if campos.next()? == "Z" {
        return None;
    }
    campos.nth(18)?.parse().ok()
}

#[async_trait::async_trait]
impl Hospedeiro for Herdr {
    async fn lanca(&self, partida: &Partida, projeto: &Project) -> Result<Launched> {
        let rotulo = nome_da_sessao(&projeto.name, &partida.session_id);
        self.garante_servidor().await?;
        let socket = self.socket.as_path();
        let alvo = self.alvo_no_projeto(socket, projeto).await?;

        // O `script` dá ao agente um terminal de verdade (com pipe no stdout o Claude Code vira
        // não interativo) e espelha a saída no log desde o primeiro byte. É o papel do
        // `pipe-pane` no tmux, sem a catraca: lá o espelho só liga depois de a sessão existir, e
        // aqui ele nasce junto com ela. Os caminhos vão por ambiente para não passar por aspas
        // de shell.
        //
        // O `$$` do shell que o `script` abre é o processo que vira o agente: cada `exec` dali
        // em diante troca o programa e mantém o pid. É por ele que o daemon sabe se a sessão
        // vive.
        //
        // Sem `HERDR_ENV` e `HERDR_PANE_ID`, a integração do Claude Code do herdr não registra
        // este pane como agente, e um restart do servidor não religa o Claude aqui sozinho: o
        // religado seria só `claude --resume`, sem os hooks do bot. Quem relança é o daemon
        // ([`Situacao::Restaurada`]). O herdr não os deixa tirar pelo `env` do `layout.apply`,
        // que ele aplica antes da identidade do pane.
        let arquivo_pid = partida.log.with_file_name("processo.pid");
        let _ = std::fs::remove_file(&arquivo_pid);
        let mut pedido = json!({
            "tab_label": rotulo,
            "focus": false,
            "root": {
                "type": "pane",
                "label": rotulo,
                "cwd": projeto.path,
                "command": [
                    "script", "-q", "-f", "-a", "-e",
                    "-c",
                    r#"unset HERDR_ENV HERDR_PANE_ID; echo $$ > "$LD_PID"; exec bash "$LD_PARTIDA""#,
                    partida.log,
                ],
                "env": {
                    "LD_SESSION": partida.session_id,
                    "LUKADISPATCH_SOCKET": paths::socket(),
                    "LD_PARTIDA": partida.script,
                    "LD_PID": arquivo_pid,
                },
            },
        });
        pedido
            .as_object_mut()
            .expect("objeto")
            .extend(alvo.as_object().expect("objeto").clone());
        let r = chama(socket, "layout.apply", pedido).await?;
        let pane = r["layout"]["root"]["pane_id"]
            .as_str()
            .context("layout.apply sem pane_id")?
            .to_string();

        // Morrer em milissegundos fecha o pane antes desta leitura: aí o motivo está no log.
        let terminal = match chama(socket, "pane.get", json!({ "pane_id": pane })).await {
            Ok(p) => p["pane"]["terminal_id"]
                .as_str()
                .context("pane.get sem terminal_id")?
                .to_string(),
            Err(_) => bail!("a sessão morreu ao subir: {}", primeiro_erro(&partida.log)),
        };

        // O título é o nome do tópico, para você saber no herdr qual conversa é qual. É só
        // enfeite: falhar aqui não derruba a sessão.
        let _ = chama(
            socket,
            "pane.report_metadata",
            json!({ "pane_id": pane, "source": FONTE, "title": projeto.name }),
        )
        .await;

        // Mesmo motivo do tmux: morrer logo depois de subir é o caso comum de erro, e é o que
        // passaria por "deu certo". O pid chega nesse meio tempo: o shell o escreve antes de
        // qualquer outra coisa.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let processo = std::fs::read_to_string(&arquivo_pid)
            .ok()
            .and_then(|pid| pid.trim().parse().ok())
            .and_then(Processo::de);
        let Some(processo) = processo else {
            bail!("a sessão morreu ao subir: {}", primeiro_erro(&partida.log));
        };
        let hospedagem = Hospedagem {
            rotulo: &rotulo,
            terminal: &terminal,
            processo: Some(processo),
        }
        .to_string();

        Ok(Launched {
            session_id: partida.session_id.clone(),
            hospedagem,
        })
    }

    /// Pelo processo, sem perguntar ao servidor: um servidor lento, ou fora do ar por um
    /// instante, não mata sessão nenhuma.
    async fn vive(&self, nome: &str) -> bool {
        let h = Hospedagem::le(nome);
        match h.processo {
            Some(p) => p.vivo(),
            None => self.painel_do_terminal(h.terminal).await.is_some(),
        }
    }

    /// Com o processo morto, sobe o servidor se ele estiver fora do ar: é o restore dele que
    /// devolve o lugar da sessão, e depois de um reboot ninguém mais o sobe. Só aqui, e não no
    /// [`Hospedeiro::vive`]: conferir uma sessão viva não pode ligar servidor.
    async fn situacao(&self, nome: &str) -> Situacao {
        let h = Hospedagem::le(nome);
        if !self.vive(nome).await {
            if let Err(e) = self.garante_servidor().await {
                tracing::warn!(erro = %format!("{e:#}"), "não consegui subir o herdr para procurar a sessão");
                return Situacao::Morta;
            }
            return if self.paineis_do_rotulo(h.rotulo).await.is_empty() {
                Situacao::Morta
            } else {
                Situacao::Restaurada
            };
        }
        if h.processo.is_none() {
            return Situacao::Viva;
        }
        // Viva. Um live handoff troca o terminal de todo pane e mantém o processo, e o terminal
        // é o que o `terminal attach` pede.
        let paineis = self.paineis_do_rotulo(h.rotulo).await;
        match paineis.as_slice() {
            [p] if p.terminal_id != h.terminal => Situacao::Mudou(
                Hospedagem {
                    terminal: &p.terminal_id,
                    ..h
                }
                .to_string(),
            ),
            _ => Situacao::Viva,
        }
    }

    /// Fecha o pane da sessão. Pelo terminal gravado, se ele ainda existe; senão pelo rótulo,
    /// porque depois de um restart do servidor o pane é o mesmo e o terminal é outro.
    async fn mata(&self, nome: &str) -> Result<()> {
        let h = Hospedagem::le(nome);
        let paineis = self.paineis_do_rotulo(h.rotulo).await;
        let alvos: Vec<&Painel> = match paineis.iter().find(|p| p.terminal_id == h.terminal) {
            Some(p) => vec![p],
            None => paineis.iter().collect(),
        };
        let mut falha = None;
        for p in alvos {
            if let Err(e) = chama(&self.socket, "pane.close", json!({ "pane_id": p.pane_id })).await
            {
                falha = Some(e);
            }
        }
        if let Some(e) = falha
            && (self.vive(nome).await || !self.paineis_do_rotulo(h.rotulo).await.is_empty())
        {
            return Err(e.context(format!("não consegui matar {nome}")));
        }
        Ok(())
    }

    /// O processo gravado na hospedagem, se ainda é o nosso.
    async fn pid(&self, nome: &str) -> Option<u32> {
        Hospedagem::le(nome)
            .processo
            .filter(Processo::vivo)
            .map(|p| p.pid)
    }

    async fn nossas(&self) -> Vec<String> {
        if !self.de_pe().await {
            return Vec::new();
        }
        let mut rotulos: Vec<String> = self
            .paineis(&self.socket)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| p.label.filter(|l| l.starts_with("ld-")))
            .collect();
        rotulos.sort();
        rotulos.dedup();
        rotulos
    }

    fn descreve(&self, nome: &str) -> String {
        let rotulo = Hospedagem::le(nome).rotulo;
        match &self.sessao {
            Some(s) => format!("herdr ({s}): {rotulo}"),
            None => format!("herdr: {rotulo}"),
        }
    }

    fn como_anexar(&self, nome: &str) -> String {
        let mut cmd = vec!["herdr".to_string()];
        cmd.extend(self.flag_sessao());
        cmd.extend([
            "terminal".into(),
            "attach".into(),
            Hospedagem::le(nome).terminal.into(),
        ]);
        cmd.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hospedagem_vai_e_volta_pelo_banco() {
        let h = Hospedagem::le("ld-proj-abcd@term_65c4@4242:987654");
        assert_eq!(h.rotulo, "ld-proj-abcd");
        assert_eq!(h.terminal, "term_65c4");
        assert_eq!(
            h.processo,
            Some(Processo {
                pid: 4242,
                inicio: 987654
            })
        );
        assert_eq!(h.to_string(), "ld-proj-abcd@term_65c4@4242:987654");

        // A gravada antes de o processo ir junto, e o rótulo sozinho que o `nossas` devolve.
        let antiga = Hospedagem::le("ld-proj-abcd@term_65c4");
        assert_eq!((antiga.terminal, antiga.processo), ("term_65c4", None));
        let so_rotulo = Hospedagem::le("ld-proj-abcd");
        assert_eq!((so_rotulo.rotulo, so_rotulo.terminal), ("ld-proj-abcd", ""));
    }

    #[test]
    fn processo_vivo_e_o_mesmo_pid_com_o_mesmo_inicio() {
        let eu = Processo::de(std::process::id()).expect("o próprio processo existe");
        assert!(eu.vivo());
        let outro = Processo {
            inicio: eu.inicio + 1,
            ..eu
        };
        assert!(
            !outro.vivo(),
            "pid reaproveitado passou pelo nosso processo"
        );
        assert!(Processo::de(u32::MAX).is_none());
    }

    #[test]
    fn processo_que_saiu_nao_vive() {
        let mut filho = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let p = Processo::de(filho.id()).unwrap();
        assert!(p.vivo());
        filho.kill().unwrap();
        // Antes de recolher: zumbi também não é vivo.
        std::thread::sleep(Duration::from_millis(100));
        assert!(!p.vivo(), "zumbi passou por vivo");
        filho.wait().unwrap();
        assert!(!p.vivo());
    }

    #[test]
    fn anexar_aponta_o_terminal_e_a_sessao_nomeada() {
        let h = "ld-proj-abcd@term_65c4@4242:987654";
        assert_eq!(
            Herdr::new(None).como_anexar(h),
            "herdr terminal attach term_65c4"
        );
        assert_eq!(
            Herdr::new(Some("bot".into())).como_anexar(h),
            "herdr --session bot terminal attach term_65c4"
        );
        assert_eq!(Herdr::new(None).descreve(h), "herdr: ld-proj-abcd");
    }

    #[test]
    fn sessao_vazia_no_config_e_a_padrao() {
        assert!(Herdr::new(Some("  ".into())).flag_sessao().is_empty());
    }

    #[test]
    fn diretorio_do_herdr_na_ordem_do_herdr() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            dir_do_herdr(s("/x/config"), s("/home/u")),
            Path::new("/x/config/herdr")
        );
        assert_eq!(
            dir_do_herdr(None, s("/home/u")),
            Path::new("/home/u/.config/herdr")
        );
        // Vazio conta como definido, como no herdr (`std::env::var` devolve Ok("")).
        assert_eq!(dir_do_herdr(s(""), s("/home/u")), Path::new("herdr"));
        assert_eq!(dir_do_herdr(None, None), std::env::temp_dir().join("herdr"));
    }

    #[test]
    fn o_motivo_e_a_linha_de_erro_e_nao_a_dica_de_uso() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("servidor.log");
        // O que o herdr 0.8.2 escreve ao recusar um nome de sessão.
        std::fs::write(
            &log,
            "error: session name may only contain ASCII letters, numbers, '.', '_' and '-'\n\
             run 'herdr --help' for usage\n",
        )
        .unwrap();
        assert!(
            motivo_no_log(&log).starts_with("error: session name"),
            "{}",
            motivo_no_log(&log)
        );

        std::fs::write(&log, "subindo\nsocket busy at /x\n\n").unwrap();
        assert_eq!(
            motivo_no_log(&log),
            "socket busy at /x",
            "sem linha de erro, a última"
        );

        std::fs::write(&log, "").unwrap();
        assert!(motivo_no_log(&log).starts_with("sem mensagem"));
    }

    #[test]
    fn socket_da_padrao_e_da_nomeada() {
        let dir = Path::new("/c/herdr");
        assert_eq!(
            socket_da_sessao(dir, None),
            Path::new("/c/herdr/herdr.sock")
        );
        assert_eq!(
            socket_da_sessao(dir, Some("bot")),
            Path::new("/c/herdr/sessions/bot/herdr.sock")
        );
        assert_eq!(
            socket_da_sessao(dir, Some("default")),
            Path::new("/c/herdr/herdr.sock"),
            "o herdr trata `default` como a sessão padrão"
        );
    }
}

/// Contra o herdr de verdade, numa sessão nomeada só do teste e com um `XDG_CONFIG_HOME` só
/// dele: nem a sua sessão padrão nem o seu `~/.config/herdr` são tocados.
#[cfg(test)]
mod testes_hospedeiro {
    use super::*;

    fn tem_herdr() -> bool {
        std::process::Command::new("herdr")
            .arg("--version")
            .output()
            .is_ok_and(|s| s.status.success())
    }

    /// Para o servidor do teste mesmo se ele falhar no meio. A config do herdr e o log do
    /// servidor ficam num diretório do teste, que some junto.
    struct SessaoDeTeste {
        nome: String,
        dir: tempfile::TempDir,
        config_home: PathBuf,
    }

    impl SessaoDeTeste {
        fn nova(sufixo: &str) -> Self {
            Self::com_nome(format!("ldteste-{}-{sufixo}", std::process::id()))
        }
        fn com_nome(nome: String) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let config_home = dir.path().to_path_buf();
            Self {
                nome,
                dir,
                config_home,
            }
        }
        /// A config num diretório fundo o bastante para o socket passar do `sun_path`.
        fn funda(mut self) -> Self {
            self.config_home = self.dir.path().join("x".repeat(100));
            self
        }
        fn herdr(&self) -> Herdr {
            Herdr::com(
                Some(self.nome.clone()),
                Some(self.config_home.clone()),
                self.dir.path().join("herdr-servidor.log"),
            )
        }
        /// A CLI do herdr apontada para a config do teste.
        fn cli(&self) -> std::process::Command {
            let mut c = std::process::Command::new("herdr");
            c.env("XDG_CONFIG_HOME", &self.config_home)
                .env_remove("HERDR_SOCKET_PATH")
                .env_remove("HERDR_SESSION");
            c
        }
    }

    impl Drop for SessaoDeTeste {
        fn drop(&mut self) {
            let _ = self
                .cli()
                .args(["session", "stop", &self.nome, "--json"])
                .output();
        }
    }

    fn partida_com(corpo: &str, dir: &Path, id: &str) -> (Partida, Project) {
        let script = dir.join("launch.sh");
        std::fs::write(&script, format!("#!/usr/bin/env bash\n{corpo}\n")).unwrap();
        let partida = Partida {
            session_id: id.into(),
            script,
            log: dir.join("pane.log"),
        };
        let projeto = Project {
            name: "teste-hospedeiro".into(),
            path: dir.to_string_lossy().into_owned(),
            permission_mode: None,
            model: None,
            effort: None,
        };
        (partida, projeto)
    }

    #[tokio::test]
    async fn herdr_roda_o_script_com_o_ambiente_da_sessao_e_mata_depois() {
        if !tem_herdr() {
            return;
        }
        let sessao = SessaoDeTeste::nova("vive");
        let h = sessao.herdr();
        let dir = tempfile::tempdir().unwrap();
        let (partida, projeto) = partida_com(
            r#"echo "sessao=$LD_SESSION socket=$LUKADISPATCH_SOCKET"; [ -t 1 ] && echo tem-terminal; exec sleep 60"#,
            dir.path(),
            "a1b2c3d4-vive",
        );
        let l = h.lanca(&partida, &projeto).await.unwrap();
        assert_eq!(l.session_id, "a1b2c3d4-vive");
        assert!(
            l.hospedagem.starts_with("ld-teste-hospedeiro-a1b2@term_"),
            "{}",
            l.hospedagem
        );
        let processo = Hospedagem::le(&l.hospedagem)
            .processo
            .expect("a hospedagem leva o processo");
        assert!(
            std::fs::read_to_string(format!("/proc/{}/cmdline", processo.pid))
                .unwrap()
                .starts_with("sleep"),
            "o processo gravado não é o que virou o agente"
        );
        assert!(h.vive(&l.hospedagem).await);
        assert_eq!(h.situacao(&l.hospedagem).await, Situacao::Viva);
        assert!(
            h.nossas()
                .await
                .contains(&"ld-teste-hospedeiro-a1b2".into())
        );

        let log = std::fs::read_to_string(&partida.log).unwrap();
        assert!(log.contains("sessao=a1b2c3d4-vive"), "{log}");
        assert!(log.contains("socket=/"), "{log}");
        assert!(
            log.contains("tem-terminal"),
            "sem terminal de verdade: {log}"
        );

        h.mata(&l.hospedagem).await.unwrap();
        assert!(!h.vive(&l.hospedagem).await, "a sessão sobreviveu ao mata");
        assert!(
            h.mata(&l.hospedagem).await.is_ok(),
            "matar de novo não é erro"
        );
    }

    #[tokio::test]
    async fn live_handoff_mantem_a_sessao_viva_e_troca_o_terminal() {
        if !tem_herdr() {
            return;
        }
        let sessao = SessaoDeTeste::nova("handoff");
        let h = sessao.herdr();
        let dir = tempfile::tempdir().unwrap();
        let (partida, projeto) = partida_com("exec sleep 60", dir.path(), "4a4d0000-handoff");
        let l = h.lanca(&partida, &projeto).await.unwrap();

        let saida = sessao
            .cli()
            .args(["--session", &sessao.nome, "server", "live-handoff"])
            .output()
            .unwrap();
        assert!(saida.status.success(), "{saida:?}");

        // O processo é o mesmo, e é ele que diz que a sessão vive: o terminal mudou.
        assert!(h.vive(&l.hospedagem).await, "o handoff matou a sessão");
        let Situacao::Mudou(nova) = h.situacao(&l.hospedagem).await else {
            panic!("o terminal não mudou no handoff, ou a sessão sumiu");
        };
        let (velha, nova_h) = (Hospedagem::le(&l.hospedagem), Hospedagem::le(&nova));
        assert_eq!(
            (nova_h.rotulo, nova_h.processo),
            (velha.rotulo, velha.processo)
        );
        assert_ne!(nova_h.terminal, velha.terminal);
        assert_eq!(h.situacao(&nova).await, Situacao::Viva);

        h.mata(&nova).await.unwrap();
        assert!(!h.vive(&nova).await, "a sessão sobreviveu ao mata");
    }

    #[tokio::test]
    async fn restart_do_servidor_devolve_o_lugar_da_sessao_sem_o_agente() {
        if !tem_herdr() {
            return;
        }
        let sessao = SessaoDeTeste::nova("restart");
        let h = sessao.herdr();
        let dir = tempfile::tempdir().unwrap();
        let (partida, projeto) = partida_com(
            r#"echo "herdr_env=[${HERDR_ENV:-}] pane=[${HERDR_PANE_ID:-}]"; exec sleep 300"#,
            dir.path(),
            "7e57a000-restart",
        );
        let l = h.lanca(&partida, &projeto).await.unwrap();
        // É o que impede a integração do Claude Code de registrar o pane, e o herdr de religar
        // o agente sozinho no restore.
        let log = std::fs::read_to_string(&partida.log).unwrap();
        assert!(log.contains("herdr_env=[] pane=[]"), "{log}");

        let parou = sessao
            .cli()
            .args(["--session", &sessao.nome, "server", "stop"])
            .output()
            .unwrap();
        assert!(parou.status.success(), "{parou:?}");
        for _ in 0..50 {
            if !h.vive(&l.hospedagem).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            !h.vive(&l.hospedagem).await,
            "a sessão sobreviveu ao restart"
        );

        // Com o processo morto e o servidor fora do ar, é a situação que sobe o servidor, e o
        // restore dele devolve o pane com o mesmo rótulo.
        assert_eq!(h.situacao(&l.hospedagem).await, Situacao::Restaurada);
        assert!(h.de_pe().await);
        assert!(
            h.nossas()
                .await
                .contains(&"ld-teste-hospedeiro-7e57".into())
        );

        h.mata(&l.hospedagem).await.unwrap();
        assert_eq!(h.situacao(&l.hospedagem).await, Situacao::Morta);
        assert!(
            h.nossas().await.is_empty(),
            "o lugar restaurado ficou para trás"
        );
    }

    #[tokio::test]
    async fn sessoes_do_mesmo_projeto_dividem_o_workspace() {
        if !tem_herdr() {
            return;
        }
        let sessao = SessaoDeTeste::nova("ws");
        let h = sessao.herdr();
        let dir = tempfile::tempdir().unwrap();
        let (p1, projeto) = partida_com("exec sleep 60", dir.path(), "aaaa0000-um");
        let (p2, _) = partida_com("exec sleep 60", dir.path(), "bbbb0000-dois");
        let l1 = h.lanca(&p1, &projeto).await.unwrap();
        let l2 = h.lanca(&p2, &projeto).await.unwrap();

        let socket = &h.socket;
        let r = chama(socket, "workspace.list", json!({})).await.unwrap();
        let ws: Vec<Workspace> = serde_json::from_value(r["workspaces"].clone()).unwrap();
        let do_projeto: Vec<_> = ws
            .iter()
            .filter(|w| w.label.as_deref() == Some("teste-hospedeiro"))
            .collect();
        assert_eq!(do_projeto.len(), 1, "{ws:?}");
        let abas = chama(
            socket,
            "tab.list",
            json!({ "workspace_id": do_projeto[0].workspace_id }),
        )
        .await
        .unwrap();
        assert_eq!(
            abas["tabs"].as_array().unwrap().len(),
            2,
            "uma aba por sessão, sem shell sobrando: {abas}"
        );

        h.mata(&l1.hospedagem).await.unwrap();
        h.mata(&l2.hospedagem).await.unwrap();
    }

    #[tokio::test]
    async fn sessao_que_morre_ao_subir_e_erro_com_o_motivo() {
        if !tem_herdr() {
            return;
        }
        let sessao = SessaoDeTeste::nova("morre");
        let dir = tempfile::tempdir().unwrap();
        let (partida, projeto) = partida_com(
            "echo 'Error: workstream ocupado'; exit 3",
            dir.path(),
            "e5f6a7b8-morre",
        );
        let e = sessao.herdr().lanca(&partida, &projeto).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(
            msg.contains("morreu") && msg.contains("workstream ocupado"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn servidor_que_nao_sobe_diz_o_motivo() {
        if !tem_herdr() {
            return;
        }
        // O herdr recusa nome de sessão com caractere fora de [A-Za-z0-9._-] e morre ao subir.
        // O motivo é do herdr, e tem de chegar em quem chamou.
        let sessao = SessaoDeTeste::com_nome(format!("ldteste-{}-inválido", std::process::id()));
        let dir = tempfile::tempdir().unwrap();
        let (partida, projeto) = partida_com("exec sleep 60", dir.path(), "c0ffee00-invalido");
        let inicio = std::time::Instant::now();
        let e = sessao.herdr().lanca(&partida, &projeto).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("saiu ao subir"), "{msg}");
        assert!(msg.contains("session name"), "o motivo se perdeu: {msg}");
        assert!(
            inicio.elapsed() < Duration::from_secs(4),
            "esperou o prazo inteiro por um servidor que já tinha morrido"
        );
    }

    #[tokio::test]
    async fn socket_longo_demais_e_recusado_antes_de_subir() {
        if !tem_herdr() {
            return;
        }
        let sessao = SessaoDeTeste::nova("longo").funda();
        let h = sessao.herdr();
        assert!(
            h.socket.as_os_str().len() >= SUN_PATH,
            "{}",
            h.socket.display()
        );
        let dir = tempfile::tempdir().unwrap();
        let (partida, projeto) = partida_com("exec sleep 60", dir.path(), "c0ffee00-longo");
        let e = h.lanca(&partida, &projeto).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("socket unix aceita até 107"), "{msg}");
        assert!(msg.contains(&*h.socket.to_string_lossy()), "{msg}");
        assert!(
            !sessao.dir.path().join("herdr-servidor.log").exists(),
            "tentou subir o servidor mesmo assim"
        );
    }

    #[tokio::test]
    async fn o_socket_calculado_e_o_que_o_proprio_herdr_lista() {
        if !tem_herdr() {
            return;
        }
        let sessao = SessaoDeTeste::nova("lista");
        let h = sessao.herdr();
        let dir = tempfile::tempdir().unwrap();
        let (partida, projeto) = partida_com("exec sleep 60", dir.path(), "d00d0000-lista");
        let l = h.lanca(&partida, &projeto).await.unwrap();

        let saida = sessao
            .cli()
            .args(["session", "list", "--json"])
            .output()
            .unwrap();
        assert!(saida.status.success(), "{saida:?}");
        let lista: Value = serde_json::from_slice(&saida.stdout).unwrap();
        let dela = lista["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == sessao.nome.as_str())
            .unwrap_or_else(|| panic!("a sessão do teste não está na lista: {lista}"));
        assert_eq!(dela["running"], true, "{dela}");
        assert_eq!(
            Path::new(dela["socket_path"].as_str().unwrap()),
            h.socket,
            "o daemon calculou um socket diferente do que o herdr usa"
        );

        h.mata(&l.hospedagem).await.unwrap();
    }

    #[tokio::test]
    async fn socket_que_nao_responde_nao_esta_de_pe() {
        // Não depende do herdr: o que se testa é o daemon diante de um socket sem servidor que
        // responda.
        let sessao = SessaoDeTeste::nova("mudo");
        let h = sessao.herdr();
        std::fs::create_dir_all(h.socket.parent().unwrap()).unwrap();

        // Aceita a conexão (o kernel enfileira) e nunca responde: é o servidor travado.
        let mudo = std::os::unix::net::UnixListener::bind(&h.socket).unwrap();
        let inicio = std::time::Instant::now();
        assert!(!h.de_pe().await, "servidor travado passou por de pé");
        assert!(h.nossas().await.is_empty());
        assert!(
            inicio.elapsed() < Duration::from_secs(4),
            "a conferência esperou o prazo longo: {:?}",
            inicio.elapsed()
        );

        // Sem ninguém escutando, o arquivo que sobrou é só um socket velho.
        drop(mudo);
        assert!(h.socket.exists());
        assert!(!h.de_pe().await, "socket velho passou por de pé");
    }

    #[tokio::test]
    async fn sem_servidor_nada_vive_e_matar_nao_e_erro() {
        if !tem_herdr() {
            return;
        }
        // Conferir não pode subir servidor: a sessão do teste nunca é criada.
        let sessao = SessaoDeTeste::nova("vazia");
        let h = sessao.herdr();
        let nome = "ld-teste-que-nao-existe@term_0000";
        assert!(!h.vive(nome).await);
        assert!(h.mata(nome).await.is_ok());
        assert!(h.nossas().await.is_empty());
        assert!(!h.de_pe().await, "conferir subiu servidor");
        assert!(!h.socket.exists(), "conferir subiu servidor");
    }
}
