//! Um daemon de verdade, isolado num tempdir, falando com o [`BotApi`] de mentira.
//!
//! Isolado quer dizer que nada dele toca a máquina de quem roda o teste: HOME, os diretórios XDG,
//! o socket, o servidor do tmux (`TMUX_TMPDIR`) e o do herdr (`XDG_CONFIG_HOME`) são todos do
//! tempdir, e o ambiente do daemon começa vazio. Sem isso, um teste rodado de dentro de uma
//! sessão do Claude Code herdaria as marcas dela, e as sessões do teste subiriam no seu tmux.
//!
//! O agente é o dublê `tests/e2e/claude`, posto no PATH com o nome `claude`. A memória é
//! `nenhuma`: o ai-memory não existe no CI.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Once;
use std::time::Duration;

use serde_json::Value;

use crate::bot_api::{BotApi, CHAT, LUKA, Mensagem, TOKEN};

/// Quanto se espera por qualquer coisa que o daemon faz. Largo: o runner do CI é lento, e uma
/// sessão leva os 3 s da espera ao subir mais o que o hospedeiro demorar.
pub const PRAZO: Duration = Duration::from_secs(45);

/// Com esta variável no ambiente, faltar tmux, herdr ou python3 é falha, e não teste pulado. É o
/// que o CI liga: lá, pular em silêncio seria um verde que não testou nada.
const EXIGE: &str = "LUKADISPATCH_E2E_EXIGE";

pub struct Cena {
    pub api: BotApi,
    pub hospedeiro: &'static str,
    dir: tempfile::TempDir,
    daemon: Option<Child>,
}

fn tem(programa: &str, arg: &str) -> bool {
    Command::new(programa)
        .arg(arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// O `lukadispatch` que os ganchos e o `listen` chamam mora ao lado do daemon, mas é de outro
/// pacote: o `cargo test` deste não o recompila. Sem isto, o teste rodaria contra um CLI velho.
fn compila_o_cli() {
    static UMA_VEZ: Once = Once::new();
    UMA_VEZ.call_once(|| {
        let ok = Command::new(env!("CARGO"))
            .args(["build", "--quiet", "-p", "ld-cli", "--bin", "lukadispatch"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("chamando o cargo")
            .success();
        assert!(ok, "o CLI não compilou");
    });
}

fn git(repo: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_clear()
        .envs(identidade_git())
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", repo)
        .stdout(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// A máquina de CI não tem identidade no git, e a sua não pode entrar (assinatura de commit com
/// chave de hardware, por exemplo).
fn identidade_git() -> Vec<(&'static str, &'static str)> {
    vec![
        ("GIT_AUTHOR_NAME", "Teste"),
        ("GIT_AUTHOR_EMAIL", "teste@exemplo"),
        ("GIT_COMMITTER_NAME", "Teste"),
        ("GIT_COMMITTER_EMAIL", "teste@exemplo"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ]
}

impl Cena {
    /// Sobe o daemon com o hospedeiro pedido e um repositório `repo` nas raízes de varredura.
    /// `None` quando falta programa na máquina e o CI não exigiu (o teste é pulado).
    pub async fn sobe(hospedeiro: &'static str) -> Option<Self> {
        let faltam: Vec<&str> = [
            ("git", "--version"),
            ("python3", "--version"),
            (
                hospedeiro,
                if hospedeiro == "tmux" {
                    "-V"
                } else {
                    "--version"
                },
            ),
        ]
        .into_iter()
        .filter(|(p, a)| !tem(p, a))
        .map(|(p, _)| p)
        .collect();
        if !faltam.is_empty() {
            assert!(
                std::env::var_os(EXIGE).is_none(),
                "{EXIGE} ligado e faltam {faltam:?}"
            );
            eprintln!("e2e pulado: faltam {faltam:?}");
            return None;
        }
        compila_o_cli();

        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        // O repositório fica sob o HOME, como os seus: a pasta das worktrees espelha o caminho a
        // partir dele.
        for sub in ["home/projetos/repo", "c/lukadispatch", "s", "d", "t", "bin"] {
            std::fs::create_dir_all(d.join(sub)).unwrap();
        }
        let r = d.join("r");
        std::fs::create_dir_all(&r).unwrap();
        std::fs::set_permissions(&r, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/e2e/claude"),
            d.join("bin/claude"),
        )
        .unwrap();

        let repo = d.join("home/projetos/repo");
        git(&repo, &["init", "--quiet", "--initial-branch=master"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        git(&repo, &["commit", "--quiet", "--allow-empty", "-m", "um"]);

        std::fs::write(
            d.join("c/lukadispatch/config.toml"),
            format!(
                r#"frontend = "telegram"
hospedeiro = "{hospedeiro}"
default_permission_mode = "auto"
trust_projects = true
wrap_mcp = false
usuario = "Luka"

[agente]
tipo = "claude-code"
memoria = "nenhuma"

[telegram]
chat_id = {CHAT}
allowed_user_ids = [{LUKA}]

[scan]
enabled = true
roots = ["{projetos}"]
depth = 1

[transcricao]
ativa = false
"#,
                projetos = d.join("home/projetos").display()
            ),
        )
        .unwrap();

        let mut c = Self {
            api: BotApi::sobe().await,
            hospedeiro,
            dir,
            daemon: None,
        };
        c.inicia_daemon().await;
        Some(c)
    }

    fn d(&self) -> &Path {
        self.dir.path()
    }

    /// O ambiente inteiro do daemon, e só ele.
    fn ambiente(&self) -> Vec<(OsString, OsString)> {
        let d = self.d();
        // O PATH de quem roda, sem as pastas que têm um `claude` de verdade: o agente aqui é só
        // o dublê, e o daemon não pode achar outro nem para ler o catálogo de modelos.
        let path = std::env::join_paths(
            std::iter::once(d.join("bin")).chain(
                std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                    .filter(|p| !p.join("claude").exists()),
            ),
        )
        .unwrap();
        let mut v: Vec<(OsString, OsString)> = vec![
            ("PATH".into(), path),
            ("HOME".into(), d.join("home").into()),
            ("XDG_CONFIG_HOME".into(), d.join("c").into()),
            ("XDG_STATE_HOME".into(), d.join("s").into()),
            ("XDG_DATA_HOME".into(), d.join("d").into()),
            ("XDG_RUNTIME_DIR".into(), d.join("r").into()),
            ("LUKADISPATCH_SOCKET".into(), d.join("r/ld.sock").into()),
            ("TMUX_TMPDIR".into(), d.join("t").into()),
            ("LD_E2E_REGISTRO".into(), d.join("registro").into()),
            ("LUKADISPATCH_TELEGRAM_TOKEN".into(), TOKEN.into()),
            (
                "LUKADISPATCH_TELEGRAM_API".into(),
                self.api.url.clone().into(),
            ),
            ("LANG".into(), "C.UTF-8".into()),
            ("TERM".into(), "xterm-256color".into()),
            ("SHELL".into(), "/bin/bash".into()),
            ("RUST_LOG".into(), "info".into()),
        ];
        v.extend(
            identidade_git()
                .into_iter()
                .map(|(k, x)| (k.into(), x.into())),
        );
        v
    }

    fn comando(&self, programa: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut c = Command::new(programa);
        c.env_clear()
            .envs(self.ambiente())
            .current_dir(self.d().join("home"));
        c
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.d().join("daemon.log")).unwrap_or_default()
    }

    pub async fn inicia_daemon(&mut self) {
        let partidas_antes = self.log().matches("socket de controle no ar").count();
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.d().join("daemon.log"))
            .unwrap();
        let daemon = self
            .comando(env!("CARGO_BIN_EXE_lukadispatchd"))
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("subindo o daemon");
        self.daemon = Some(daemon);
        // O socket de um daemon morto por SIGTERM fica para trás: existir não prova nada. É o log
        // do daemon novo que diz que ele está no ar.
        let fim = std::time::Instant::now() + PRAZO;
        loop {
            if self.log().matches("socket de controle no ar").count() > partidas_antes {
                return;
            }
            let morreu = self
                .daemon
                .as_mut()
                .and_then(|d| d.try_wait().ok().flatten());
            if morreu.is_some() || std::time::Instant::now() > fim {
                panic!(
                    "[{}] o daemon não subiu ({morreu:?}):\n{}",
                    self.hospedeiro,
                    self.log()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Mata o daemon como o systemd faria num restart ou num deploy (SIGTERM), sem subir outro.
    pub fn para_daemon(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            let _ = Command::new("kill")
                .args(["-TERM", &d.id().to_string()])
                .status();
            let _ = d.wait();
        }
    }

    /// Espera `f` devolver algo. Na falha, mostra o chat e o log do daemon.
    pub async fn ate<T>(&self, o_que: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let fim = std::time::Instant::now() + PRAZO;
        loop {
            if let Some(v) = f() {
                return v;
            }
            if std::time::Instant::now() > fim {
                panic!(
                    "[{}] esperei {o_que} por {PRAZO:?}\n--- chat\n{}--- daemon\n{}",
                    self.hospedeiro,
                    self.api.le(|c| c.conversa()),
                    self.log()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn espera_topico(&self, nome: &str) -> i32 {
        self.ate(&format!("o tópico {nome:?}"), || {
            self.api
                .le(|c| c.topico(nome).filter(|t| !t.apagado).map(|t| t.id))
        })
        .await
    }

    pub async fn espera_topico_apagado(&self, id: i32) {
        self.ate(&format!("o tópico {id} sumir"), || {
            self.api.le(|c| {
                c.topicos
                    .iter()
                    .any(|t| t.id == id && t.apagado)
                    .then_some(())
            })
        })
        .await
    }

    /// A mensagem do bot que contém `trecho`, no tópico (ou no General com `None`).
    pub async fn espera_mensagem(&self, topico: Option<i32>, trecho: &str) -> Mensagem {
        self.ate(&format!("{trecho:?} em {topico:?}"), || {
            self.api
                .le(|c| c.em(topico).find(|m| m.texto.contains(trecho)).cloned())
        })
        .await
    }

    /// Toca o botão assim que ele aparecer.
    pub async fn toca(&self, rotulo: &str) {
        self.ate(&format!("o botão {rotulo:?}"), || {
            self.api.toca(rotulo).then_some(())
        })
        .await
    }

    /// Tudo o que os dublês anotaram, na ordem.
    pub fn registro(&self) -> Vec<Value> {
        let mut v: Vec<Value> = std::fs::read_dir(self.d().join("registro"))
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|e| {
                std::fs::read_to_string(e.path())
                    .unwrap_or_default()
                    .lines()
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect::<Vec<Value>>()
            })
            .collect();
        v.sort_by(|a, b| a["em"].as_f64().partial_cmp(&b["em"].as_f64()).unwrap());
        v
    }

    /// As partidas do agente, na ordem.
    pub fn partidas(&self) -> Vec<Value> {
        self.registro()
            .into_iter()
            .filter(|e| e["evento"] == "partida")
            .collect()
    }

    pub async fn espera_partidas(&self, n: usize) -> Vec<Value> {
        self.ate(&format!("{n} partida(s) do agente"), || {
            let p = self.partidas();
            (p.len() >= n).then_some(p)
        })
        .await
    }

    /// Espera o processo do agente com este pid acabar, e confere que ele acabou mesmo.
    pub async fn espera_fim(&self, pid: u64) {
        self.ate(&format!("o agente {pid} acabar"), || {
            let anotou = self
                .registro()
                .iter()
                .any(|e| e["evento"] == "fim" && e["pid"] == pid);
            (anotou && !Path::new(&format!("/proc/{pid}")).exists()).then_some(())
        })
        .await
    }

    pub fn repo(&self) -> PathBuf {
        self.d().join("home/projetos/repo")
    }

    /// Onde a worktree de uma branch do `repo` fica.
    pub fn worktree(&self, branch: &str) -> PathBuf {
        self.d()
            .join("d/lukadispatch/worktrees/projetos/repo")
            .join(branch)
    }

    pub fn existe_branch(&self, nome: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(self.repo())
            .args(["show-ref", "--verify", "--quiet"])
            .arg(format!("refs/heads/{nome}"))
            .status()
            .unwrap()
            .success()
    }
}

impl Drop for Cena {
    fn drop(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            let _ = d.kill();
            let _ = d.wait();
        }
        // Derrubar o servidor do hospedeiro derruba os panes, e cada dublê encerra o dele.
        let _ = match self.hospedeiro {
            "tmux" => self.comando("tmux").arg("kill-server").output(),
            _ => self
                .comando("herdr")
                .args(["session", "stop", "lukadispatch", "--json"])
                .output(),
        };
        // O que ainda sobrar não pode ficar vivo na máquina de quem rodou o teste. Só mata o que
        // é mesmo um dublê: o pid anotado pode ter sido reusado.
        for e in self.registro() {
            let Some(pid) = e["pid"].as_u64() else {
                continue;
            };
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            if String::from_utf8_lossy(&cmdline).contains("bin/claude") {
                let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            }
        }
    }
}
