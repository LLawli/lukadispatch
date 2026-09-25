//! O herdr como [`Hospedeiro`]: cada sessão é uma aba no workspace do projeto, anexável no PC
//! com `herdr terminal attach` e visível ao lado das suas no herdr que você já usa.
//!
//! Fala direto com o socket do servidor (JSON por linha), e não pela CLI, porque o único jeito
//! de abrir um pane já rodando um comando com ambiente próprio é o método `layout.apply`, que a
//! CLI não expõe. Pela CLI seria abrir um shell e digitar o comando nele.
//!
//! A hospedagem gravada no banco é `rótulo@terminal`: o rótulo (`ld-projeto-abcd`) é o nome que
//! você acha na aba, e o `terminal_id` é o que identifica o processo. Um restart do servidor do
//! herdr mata os processos e restaura os panes como shells com o mesmo rótulo e terminal novo;
//! pelo terminal, o daemon enxerga isso como a sessão que morreu, e não como a mesma viva.

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

use super::{Hospedeiro, Launched, nome_da_sessao, primeiro_erro};
use crate::agente::Partida;

/// Quem assina os metadados que o daemon põe no pane (o título).
const FONTE: &str = "custom:lukadispatch";

/// Quanto uma chamada ao socket pode levar. As do hospedeiro são todas de uma linha e voltam em
/// milissegundos; passar disto é servidor travado, e travar o daemon junto não ajuda ninguém.
const PRAZO: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct Herdr {
    /// Sessão nomeada do herdr. `None` é a padrão.
    sessao: Option<String>,
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

#[derive(Debug, Deserialize)]
struct SessaoListada {
    name: String,
    default: bool,
    running: bool,
    socket_path: PathBuf,
}

impl Herdr {
    pub fn new(sessao: Option<String>) -> Self {
        Self {
            sessao: sessao.filter(|s| !s.trim().is_empty()),
            log_do_servidor: paths::state_dir().join("herdr-servidor.log"),
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

    /// A sessão configurada, como o próprio herdr a lista: onde fica o socket e se está de pé.
    async fn listada(&self) -> Result<Option<SessaoListada>> {
        #[derive(Deserialize)]
        struct Lista {
            sessions: Vec<SessaoListada>,
        }
        let saida = Command::new("herdr")
            .args(["session", "list", "--json"])
            .output()
            .await
            .context("chamando herdr (ele está instalado?)")?;
        if !saida.status.success() {
            bail!(
                "herdr session list falhou: {}",
                String::from_utf8_lossy(&saida.stderr).trim()
            );
        }
        let lista: Lista =
            serde_json::from_slice(&saida.stdout).context("lendo a lista de sessões do herdr")?;
        Ok(lista.sessions.into_iter().find(|s| match &self.sessao {
            Some(nome) => &s.name == nome,
            None => s.default,
        }))
    }

    /// O socket da sessão, se o servidor dela está de pé. Não sobe nada: é o que `vive`, `mata`
    /// e `nossas` usam, e conferir não pode ter o efeito colateral de ligar um servidor.
    async fn socket_se_de_pe(&self) -> Option<PathBuf> {
        match self.listada().await {
            Ok(Some(s)) if s.running => Some(s.socket_path),
            _ => None,
        }
    }

    /// O socket da sessão, subindo o servidor se ele não estiver de pé.
    ///
    /// Ao contrário do `tmux new-session`, a CLI do herdr não sobe servidor sozinha. O servidor
    /// sobe em grupo de processo próprio, para sobreviver ao restart do daemon (a unit usa
    /// `KillMode=process` pelo mesmo motivo). A saída de erro dele vai para um arquivo: o
    /// servidor que não sobe (socket com caminho longo demais, sessão corrompida) diz o motivo
    /// ali, e só ali.
    async fn garante_servidor(&self) -> Result<PathBuf> {
        if let Some(s) = self.socket_se_de_pe().await {
            return Ok(s);
        }
        let log = &self.log_do_servidor;
        if let Some(pai) = log.parent() {
            let _ = std::fs::create_dir_all(pai);
        }
        let erro = std::fs::File::create(log)
            .map(std::process::Stdio::from)
            .unwrap_or_else(|_| std::process::Stdio::null());
        let mut servidor = Command::new("herdr")
            .args(self.flag_sessao())
            .arg("server")
            .env_remove("HERDR_SOCKET_PATH")
            .env_remove("HERDR_SESSION")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(erro)
            .process_group(0)
            .spawn()
            .context("subindo o servidor do herdr")?;
        let nome = self.sessao.as_deref().unwrap_or("sessão padrão");
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Some(s) = self.socket_se_de_pe().await
                && chama(&s, "ping", json!({})).await.is_ok()
            {
                return Ok(s);
            }
            // Morto não sobe mais: esperar o resto do prazo só atrasaria o erro.
            if let Ok(Some(status)) = servidor.try_wait() {
                bail!(
                    "o servidor do herdr ({nome}) saiu ao subir ({status}): {}",
                    ultima_linha(log)
                );
            }
        }
        bail!(
            "o servidor do herdr ({nome}) não subiu em 5 s: {}",
            ultima_linha(log)
        )
    }

    async fn paineis(&self, socket: &Path) -> Result<Vec<Painel>> {
        let r = chama(socket, "pane.list", json!({})).await?;
        serde_json::from_value(r["panes"].clone()).context("lendo pane.list do herdr")
    }

    /// O pane que roda este terminal, se ele ainda existe.
    async fn painel_do_terminal(&self, terminal: &str) -> Option<(PathBuf, Painel)> {
        let socket = self.socket_se_de_pe().await?;
        let painel = self
            .paineis(&socket)
            .await
            .ok()?
            .into_iter()
            .find(|p| p.terminal_id == terminal)?;
        Some((socket, painel))
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
    let linha = tokio::time::timeout(PRAZO, ida)
        .await
        .with_context(|| format!("o herdr não respondeu {metodo} em {PRAZO:?}"))??;
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

/// A última linha não vazia de um arquivo de log, ou um aviso de que ele não diz nada.
fn ultima_linha(log: &Path) -> String {
    std::fs::read_to_string(log)
        .ok()
        .and_then(|t| {
            t.lines()
                .rev()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(str::to_string)
        })
        .map(|l| l.chars().take(300).collect())
        .unwrap_or_else(|| format!("sem mensagem ({})", log.display()))
}

/// `rótulo@terminal`, o formato da hospedagem.
fn parte(hospedagem: &str) -> (&str, &str) {
    hospedagem.rsplit_once('@').unwrap_or(("", hospedagem))
}

#[async_trait::async_trait]
impl Hospedeiro for Herdr {
    async fn lanca(&self, partida: &Partida, projeto: &Project) -> Result<Launched> {
        let rotulo = nome_da_sessao(&projeto.name, &partida.session_id);
        let socket = self.garante_servidor().await?;
        let alvo = self.alvo_no_projeto(&socket, projeto).await?;

        // O `script` dá ao agente um terminal de verdade (com pipe no stdout o Claude Code vira
        // não interativo) e espelha a saída no log desde o primeiro byte. É o papel do
        // `pipe-pane` no tmux, sem a catraca: lá o espelho só liga depois de a sessão existir, e
        // aqui ele nasce junto com ela. O caminho do script vai por ambiente para não passar
        // por aspas de shell.
        let mut pedido = json!({
            "tab_label": rotulo,
            "focus": false,
            "root": {
                "type": "pane",
                "label": rotulo,
                "cwd": projeto.path,
                "command": [
                    "script", "-q", "-f", "-a", "-e",
                    "-c", r#"exec bash "$LD_PARTIDA""#,
                    partida.log,
                ],
                "env": {
                    "LD_SESSION": partida.session_id,
                    "LUKADISPATCH_SOCKET": paths::socket(),
                    "LD_PARTIDA": partida.script,
                },
            },
        });
        pedido
            .as_object_mut()
            .expect("objeto")
            .extend(alvo.as_object().expect("objeto").clone());
        let r = chama(&socket, "layout.apply", pedido).await?;
        let pane = r["layout"]["root"]["pane_id"]
            .as_str()
            .context("layout.apply sem pane_id")?
            .to_string();

        // Morrer em milissegundos fecha o pane antes desta leitura: aí o motivo está no log.
        let terminal = match chama(&socket, "pane.get", json!({ "pane_id": pane })).await {
            Ok(p) => p["pane"]["terminal_id"]
                .as_str()
                .context("pane.get sem terminal_id")?
                .to_string(),
            Err(_) => bail!("a sessão morreu ao subir: {}", primeiro_erro(&partida.log)),
        };
        let hospedagem = format!("{rotulo}@{terminal}");

        // O título é o nome do tópico, para você saber no herdr qual conversa é qual. É só
        // enfeite: falhar aqui não derruba a sessão.
        let _ = chama(
            &socket,
            "pane.report_metadata",
            json!({ "pane_id": pane, "source": FONTE, "title": projeto.name }),
        )
        .await;

        // Mesmo motivo do tmux: morrer logo depois de subir é o caso comum de erro, e é o que
        // passaria por "deu certo".
        tokio::time::sleep(Duration::from_secs(3)).await;
        if !self.vive(&hospedagem).await {
            bail!("a sessão morreu ao subir: {}", primeiro_erro(&partida.log));
        }

        Ok(Launched {
            session_id: partida.session_id.clone(),
            hospedagem,
        })
    }

    async fn vive(&self, nome: &str) -> bool {
        self.painel_do_terminal(parte(nome).1).await.is_some()
    }

    async fn mata(&self, nome: &str) -> Result<()> {
        let Some((socket, painel)) = self.painel_do_terminal(parte(nome).1).await else {
            return Ok(());
        };
        let fechou = chama(&socket, "pane.close", json!({ "pane_id": painel.pane_id })).await;
        if let Err(e) = fechou
            && self.vive(nome).await
        {
            return Err(e.context(format!("não consegui matar {nome}")));
        }
        Ok(())
    }

    async fn nossas(&self) -> Vec<String> {
        let Some(socket) = self.socket_se_de_pe().await else {
            return Vec::new();
        };
        self.paineis(&socket)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| {
                let rotulo = p.label.filter(|l| l.starts_with("ld-"))?;
                Some(format!("{rotulo}@{}", p.terminal_id))
            })
            .collect()
    }

    fn descreve(&self, nome: &str) -> String {
        match &self.sessao {
            Some(s) => format!("herdr ({s}): {}", parte(nome).0),
            None => format!("herdr: {}", parte(nome).0),
        }
    }

    fn como_anexar(&self, nome: &str) -> String {
        let mut cmd = vec!["herdr".to_string()];
        cmd.extend(self.flag_sessao());
        cmd.extend(["terminal".into(), "attach".into(), parte(nome).1.into()]);
        cmd.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hospedagem_separa_rotulo_e_terminal() {
        assert_eq!(
            parte("ld-proj-abcd@term_65c4"),
            ("ld-proj-abcd", "term_65c4")
        );
        // Sem rótulo (não deveria acontecer), o valor inteiro ainda serve de terminal.
        assert_eq!(parte("term_65c4"), ("", "term_65c4"));
    }

    #[test]
    fn anexar_aponta_o_terminal_e_a_sessao_nomeada() {
        let h = "ld-proj-abcd@term_65c4";
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
}

/// Contra o herdr de verdade, numa sessão nomeada só do teste: nunca a sua sessão padrão.
#[cfg(test)]
mod testes_hospedeiro {
    use super::*;

    fn tem_herdr() -> bool {
        std::process::Command::new("herdr")
            .arg("--version")
            .output()
            .is_ok_and(|s| s.status.success())
    }

    /// Para e apaga a sessão do teste mesmo se ele falhar no meio. O log do servidor fica num
    /// diretório do teste, e não no estado do daemon de verdade.
    struct SessaoDeTeste(String, tempfile::TempDir);

    impl SessaoDeTeste {
        fn nova(sufixo: &str) -> Self {
            Self::com_nome(format!("ldteste-{}-{sufixo}", std::process::id()))
        }
        fn com_nome(nome: String) -> Self {
            Self(nome, tempfile::tempdir().unwrap())
        }
        fn herdr(&self) -> Herdr {
            Herdr {
                log_do_servidor: self.1.path().join("herdr-servidor.log"),
                ..Herdr::new(Some(self.0.clone()))
            }
        }
    }

    impl Drop for SessaoDeTeste {
        fn drop(&mut self) {
            for acao in ["stop", "delete"] {
                let _ = std::process::Command::new("herdr")
                    .args(["session", acao, &self.0, "--json"])
                    .output();
            }
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
        assert!(h.vive(&l.hospedagem).await);
        assert!(h.nossas().await.contains(&l.hospedagem));

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

        let socket = h.socket_se_de_pe().await.unwrap();
        let r = chama(&socket, "workspace.list", json!({})).await.unwrap();
        let ws: Vec<Workspace> = serde_json::from_value(r["workspaces"].clone()).unwrap();
        let do_projeto: Vec<_> = ws
            .iter()
            .filter(|w| w.label.as_deref() == Some("teste-hospedeiro"))
            .collect();
        assert_eq!(do_projeto.len(), 1, "{ws:?}");
        let abas = chama(
            &socket,
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
        // Um nome de sessão longo o bastante estoura o caminho do socket unix (~108 bytes), e o
        // servidor morre ao subir. O motivo é do herdr, e tem de chegar em quem chamou.
        let sessao =
            SessaoDeTeste::com_nome(format!("ldteste-{}-{}", std::process::id(), "x".repeat(90)));
        let dir = tempfile::tempdir().unwrap();
        let (partida, projeto) = partida_com("exec sleep 60", dir.path(), "c0ffee00-longo");
        let inicio = std::time::Instant::now();
        let e = sessao.herdr().lanca(&partida, &projeto).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("saiu ao subir"), "{msg}");
        assert!(!msg.contains("sem mensagem"), "o motivo se perdeu: {msg}");
        assert!(
            inicio.elapsed() < Duration::from_secs(4),
            "esperou o prazo inteiro por um servidor que já tinha morrido"
        );
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
        assert!(
            h.socket_se_de_pe().await.is_none(),
            "conferir subiu servidor"
        );
    }
}
