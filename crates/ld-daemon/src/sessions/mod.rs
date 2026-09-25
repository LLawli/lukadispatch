//! Onde as sessões rodam: o hospedeiro, que sobe o script de partida que o agente montou e mata
//! quando pedido.
//!
//! O que compõe a partida (linha de comando, prompt, envelope) não é deste módulo: mora em
//! [`crate::agente`], que devolve uma [`crate::agente::Partida`] já pronta para rodar. Aqui fica
//! só o mecanismo que a mantém de pé: o tmux ([`Tmux`], o padrão) ou o herdr ([`Herdr`]). Os
//! dois deixam a sessão anexável no PC e sobrevivem a restart do daemon.

use anyhow::{Context, Result, bail};
use ld_core::config::{Config, Project, TOKEN_TELEGRAM};
use ld_core::paths;
use std::sync::Arc;
use tokio::process::Command;

use crate::agente::Partida;

mod herdr;
pub use herdr::Herdr;

#[derive(Debug)]
pub struct Launched {
    pub session_id: String,
    pub hospedagem: String,
}

/// Nome da sessão no hospedeiro: previsível para você achar no `tmux ls` ou na aba do herdr, e
/// único para dois projetos com o mesmo nome (ou o mesmo projeto duas vezes) não colidirem.
pub fn nome_da_sessao(projeto: &str, session_id: &str) -> String {
    let slug: String = projeto
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    let slug = if slug.is_empty() {
        "projeto".to_string()
    } else {
        slug.chars().take(24).collect()
    };
    format!("ld-{slug}-{}", &session_id[..4])
}

/// As variáveis com que o Claude Code marca os processos que nascem dentro de uma sessão dele.
///
/// Um daemon iniciado de dentro de um Claude (em desenvolvimento, tipicamente) as herda, e um
/// Claude que nasce com elas se toma por filho de outra sessão: roda com "Transcript saving is
/// off", e aí o `--resume` do próximo relançamento não tem o que retomar. A lista é explícita,
/// e não o prefixo `CLAUDE_CODE_`, porque o mesmo prefixo tem configuração que precisa passar
/// (`CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CODE_USE_BEDROCK`).
const MARCAS_DO_CLAUDE_CODE: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_BRIDGE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
];

/// Tira do comando o que o daemon tem no ambiente e não pode chegar ao hospedeiro.
///
/// O servidor do tmux ou do herdr, quando é o daemon que o sobe, copia o ambiente dele, e todo
/// pane que nascer ali herda. Sob o systemd, esse ambiente tem o token do bot (o `.env` da
/// unit): sem isto ele iria parar em cada shell do servidor, inclusive nos seus. E as
/// [`MARCAS_DO_CLAUDE_CODE`] de um daemon iniciado de dentro de um Claude desligariam os
/// transcripts de toda sessão que nascesse no servidor.
pub(super) fn limpa_ambiente(cmd: &mut Command) -> &mut Command {
    cmd.env_remove(TOKEN_TELEGRAM);
    for marca in MARCAS_DO_CLAUDE_CODE {
        cmd.env_remove(marca);
    }
    cmd
}

/// Sobe a sessão. Devolve erro sem deixar lixo se o tmux não vingar.
pub async fn launch(partida: &Partida, projeto: &Project) -> Result<Launched> {
    let tmux = nome_da_sessao(&projeto.name, &partida.session_id);

    // A catraca: a sessão sobe num invólucro que espera este arquivo antes de rodar o script.
    // Sem ela, um script que morre em milissegundos some antes de o espelho abaixo ser ligado,
    // e o erro chega sem o motivo, que é justamente o que o espelho existe para guardar. O teto
    // de ~10 s é para o caso de este processo morrer entre subir a sessão e liberá-la.
    let liberado = partida.log.with_extension("liberado");
    let _ = std::fs::remove_file(&liberado);
    const INVOLUCRO: &str = r#"i=0; while [ ! -e "$1" ] && [ "$i" -lt 200 ]; do sleep 0.05; i=$((i+1)); done; exec bash "$0""#;

    let saida = limpa_ambiente(&mut Command::new("tmux"))
        .args([
            "new-session",
            "-d",
            "-s",
            &tmux,
            "-c",
            &projeto.path,
            "-e",
            &format!("LD_SESSION={}", partida.session_id),
            // O hook roda dentro desta sessão e precisa achar o socket. O servidor tmux pode
            // ter sido iniciado com outro ambiente (sem XDG_RUNTIME_DIR, por exemplo), então o
            // caminho vai explícito em vez de depender do que ele herdou.
            "-e",
            &format!("LUKADISPATCH_SOCKET={}", paths::socket().display()),
            "sh",
            "-c",
            INVOLUCRO,
        ])
        .arg(&partida.script)
        .arg(&liberado)
        .output()
        .await
        .context("chamando tmux (ele está instalado?)")?;

    if !saida.status.success() {
        bail!(
            "tmux recusou: {}",
            String::from_utf8_lossy(&saida.stderr).trim()
        );
    }

    // Código de saída não é prova: confere que a sessão existe mesmo antes de dizer que subiu.
    if !has_session(&tmux).await {
        bail!("tmux saiu 0 mas a sessão {tmux} não existe");
    }

    // Espelha o painel no arquivo que a partida escolheu. É por `pipe-pane`, e não redirecionando
    // o comando, porque o Claude Code precisa de um terminal de verdade no stdout: com um pipe
    // ali ele entra em modo não interativo. Sem esse espelho, uma sessão que morre ao subir não
    // deixa pista nenhuma.
    let _ = Command::new("tmux")
        .args(["pipe-pane", "-o", "-t", &tmux])
        .arg(format!("cat >> {}", partida.log.display()))
        .output()
        .await;
    std::fs::write(&liberado, b"")
        .with_context(|| format!("liberando a partida em {}", liberado.display()))?;

    // Morrer logo depois de subir é o caso comum de erro (workstream ocupado, diálogo de
    // confiança, projeto inexistente), e é justamente o que passaria por "deu certo".
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    if !has_session(&tmux).await {
        bail!("a sessão morreu ao subir: {}", primeiro_erro(&partida.log));
    }
    let _ = std::fs::remove_file(&liberado);

    Ok(Launched {
        session_id: partida.session_id.clone(),
        hospedagem: tmux,
    })
}

/// A linha de erro mais útil do espelho do painel.
///
/// O arquivo tem sequências de escape do terminal misturadas ao texto; interessa a primeira
/// linha que fala de erro, que é o que explica a morte.
pub(super) fn primeiro_erro(log: &std::path::Path) -> String {
    let Ok(bruto) = std::fs::read_to_string(log) else {
        return "sem saída registrada".into();
    };
    let limpo: String = bruto
        .chars()
        .map(|c| if c == '\u{1b}' { '\n' } else { c })
        .collect();
    limpo
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("Error") || l.contains("error:") || l.contains("Caused by"))
        .map(|l| l.chars().take(200).collect())
        .unwrap_or_else(|| "sem mensagem de erro no painel".into())
}

pub async fn has_session(tmux: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", tmux])
        .output()
        .await
        .map(|s| s.status.success())
        .unwrap_or(false)
}

/// Sessões tmux que este projeto criou (prefixo `ld-`).
pub async fn nossas_sessoes() -> Vec<String> {
    let Ok(saida) = Command::new("tmux")
        .args(["list-sessions", "-F", "#S"])
        .output()
        .await
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&saida.stdout)
        .lines()
        .filter(|l| l.starts_with("ld-"))
        .map(str::to_string)
        .collect()
}

pub async fn kill(tmux: &str) -> Result<()> {
    let saida = Command::new("tmux")
        .args(["kill-session", "-t", tmux])
        .output()
        .await
        .context("chamando tmux")?;
    // Sessão já morta não é erro: o objetivo era não existir, e ela não existe.
    if !saida.status.success() && has_session(tmux).await {
        bail!(
            "não consegui matar {tmux}: {}",
            String::from_utf8_lossy(&saida.stderr).trim()
        );
    }
    Ok(())
}

/// O que o hospedeiro sabe de uma sessão que o banco dá como viva.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Situacao {
    /// De pé, onde o banco diz.
    Viva,
    /// De pé, mas o hospedeiro passou a achá-la por outra hospedagem, que é a que vem aqui. O
    /// herdr troca o terminal de todo pane num live handoff, com o processo intacto.
    Mudou(String),
    /// O processo morreu, mas o hospedeiro reiniciou e guardou o lugar da sessão: o herdr
    /// restaura o pane dela, com o mesmo rótulo, como um shell parado. Quem chama decide se a
    /// relança ali; senão, o lugar é lixo a matar.
    Restaurada,
    /// Não existe mais.
    Morta,
}

/// Onde as sessões rodam: sobe, confere se está viva, mata e lista.
///
/// É a porta que separa o ciclo de vida da sessão (que o `App` conduz) do mecanismo que a
/// mantém de pé. Hoje o mecanismo é o tmux ([`Tmux`]), escolhido porque deixa a sessão anexável
/// no PC com `tmux attach` e sobrevive a restart do daemon; o herdr ([`Herdr`]) é a outra
/// implementação. Outro multiplexador (zellij, screen) ou um contêiner por sessão entraria do
/// mesmo jeito, e os testes de fluxo usam uma de mentira para não depender de nenhum.
///
/// Os nomes que ela devolve em [`Launched::hospedagem`] e aceita nos outros métodos são opacos para
/// quem chama: é só o que identifica a sessão para o próprio hospedeiro.
#[async_trait::async_trait]
pub trait Hospedeiro: Send + Sync + 'static {
    /// Sobe a sessão a partir da [`Partida`] que o agente montou, e só volta quando ela está de
    /// pé de verdade (ou com o motivo de não estar). Não deixa lixo em caso de erro.
    async fn lanca(&self, partida: &Partida, projeto: &Project) -> Result<Launched>;

    /// A sessão com este nome existe?
    async fn vive(&self, nome: &str) -> bool;

    /// Encerra. Sessão que já não existe não é erro: o objetivo era ela não existir.
    async fn mata(&self, nome: &str) -> Result<()>;

    /// O que a reconciliação precisa saber de uma sessão que o banco dá como viva. O padrão
    /// serve a quem não renomeia sessão viva: viva ou morta, pelo [`Hospedeiro::vive`].
    async fn situacao(&self, nome: &str) -> Situacao {
        if self.vive(nome).await {
            Situacao::Viva
        } else {
            Situacao::Morta
        }
    }

    /// Os rótulos das sessões que este projeto criou e ainda estão de pé, inclusive as que o
    /// banco já esqueceu (é assim que a reconciliação acha órfãs). O rótulo é o começo da
    /// hospedagem: ela é o próprio rótulo, ou o rótulo seguido de `@` e do que mais o
    /// hospedeiro precisar. [`Hospedeiro::mata`] aceita o rótulo sozinho.
    async fn nossas(&self) -> Vec<String>;

    /// Como a sessão aparece na ficha do canal: o hospedeiro e onde achá-la nele.
    fn descreve(&self, nome: &str) -> String;

    /// A linha de comando que anexa a sessão no PC. Vai nos avisos que só se resolvem no
    /// teclado, como um diálogo de servidor MCP ou uma sessão surda.
    fn como_anexar(&self, nome: &str) -> String;
}

/// O tmux como [`Hospedeiro`]. As funções livres deste módulo são a implementação.
#[derive(Debug, Default, Clone, Copy)]
pub struct Tmux;

#[async_trait::async_trait]
impl Hospedeiro for Tmux {
    async fn lanca(&self, partida: &Partida, projeto: &Project) -> Result<Launched> {
        launch(partida, projeto).await
    }

    async fn vive(&self, nome: &str) -> bool {
        has_session(nome).await
    }

    async fn mata(&self, nome: &str) -> Result<()> {
        kill(nome).await
    }

    async fn nossas(&self) -> Vec<String> {
        nossas_sessoes().await
    }

    fn descreve(&self, nome: &str) -> String {
        format!("tmux: {nome}")
    }

    fn como_anexar(&self, nome: &str) -> String {
        format!("tmux attach -t {nome}")
    }
}

/// Os hospedeiros que existem, pelo nome que o config e o `setup --session` usam.
pub const HOSPEDEIROS: &[&str] = &["tmux", "herdr"];

/// O hospedeiro que o config pede. Nome desconhecido é erro na partida do daemon.
pub fn da_config(cfg: &Config) -> Result<Arc<dyn Hospedeiro>> {
    match cfg.hospedeiro.as_str() {
        "tmux" => Ok(Arc::new(Tmux)),
        "herdr" => Ok(Arc::new(Herdr::new(cfg.herdr.sessao.clone()))),
        outro => bail!(
            "hospedeiro desconhecido: {outro:?} (disponíveis: {})",
            HOSPEDEIROS.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_escolhe_o_hospedeiro_pelo_nome() {
        let com = |nome: &str| Config {
            hospedeiro: nome.into(),
            ..Config::default()
        };
        assert!(da_config(&com("tmux")).is_ok());
        assert!(da_config(&com("herdr")).is_ok());
        let e = da_config(&com("zellij"))
            .err()
            .expect("hospedeiro desconhecido");
        let msg = format!("{e:#}");
        assert!(
            msg.contains("zellij") && msg.contains("tmux") && msg.contains("herdr"),
            "{msg}"
        );
    }

    #[test]
    fn nome_da_sessao_e_previsivel_e_unico() {
        let id = "abcd1234-0000-0000-0000-000000000000";
        assert_eq!(nome_da_sessao("lukadispatch", id), "ld-lukadispatch-abcd");
        assert_eq!(nome_da_sessao("Meu Projeto!", id), "ld-meu-projeto-abcd");
    }

    #[test]
    fn nome_vazio_nao_gera_sessao_sem_nome() {
        let id = "abcd1234-0000-0000-0000-000000000000";
        assert_eq!(nome_da_sessao("!!!", id), "ld-projeto-abcd");
    }

    #[test]
    fn acha_o_erro_no_espelho_do_painel() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("pane.log");
        std::fs::write(
            &log,
            "\u{1b}[2J ai-memory starting\nError: opening managed workstream\nCaused by: 409\n",
        )
        .unwrap();
        assert!(primeiro_erro(&log).contains("opening managed workstream"));
    }

    /// As variáveis que o comando tira do ambiente que herdou.
    fn tiradas(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn o_token_do_bot_nao_passa_para_o_hospedeiro() {
        let mut cmd = Command::new("tmux");
        limpa_ambiente(&mut cmd);
        assert!(
            tiradas(&cmd).contains(&TOKEN_TELEGRAM.to_string()),
            "o token tem de sair do ambiente do servidor"
        );
    }

    #[test]
    fn as_marcas_de_sessao_do_claude_code_nao_passam_e_a_configuracao_passa() {
        let mut cmd = Command::new("tmux");
        limpa_ambiente(&mut cmd);
        let tiradas = tiradas(&cmd);
        for marca in [
            "CLAUDECODE",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_SESSION_ID",
        ] {
            assert!(tiradas.contains(&marca.to_string()), "{marca} passou");
        }
        assert!(!tiradas.contains(&"CLAUDE_CODE_OAUTH_TOKEN".to_string()));
    }

    #[test]
    fn espelho_ausente_nao_explode() {
        assert_eq!(
            primeiro_erro(std::path::Path::new("/nao/existe.log")),
            "sem saída registrada"
        );
    }
}

#[cfg(test)]
mod testes_hospedeiro {
    use super::*;

    fn tem_tmux() -> bool {
        std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .is_ok_and(|s| s.status.success())
    }

    fn partida_com(corpo: &str, dir: &std::path::Path, id: &str) -> (Partida, Project) {
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
    async fn tmux_roda_o_script_da_partida_e_mata_depois() {
        if !tem_tmux() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (partida, projeto) =
            partida_com("echo subiu; exec sleep 60", dir.path(), "a1b2c3d4-vive");
        let h = Tmux;
        let l = h.lanca(&partida, &projeto).await.unwrap();
        assert_eq!(l.session_id, "a1b2c3d4-vive");
        assert!(
            l.hospedagem.starts_with("ld-teste-hospedeiro-"),
            "{}",
            l.hospedagem
        );
        assert!(h.vive(&l.hospedagem).await);
        assert!(h.nossas().await.contains(&l.hospedagem));
        h.mata(&l.hospedagem).await.unwrap();
        assert!(!h.vive(&l.hospedagem).await, "a sessão sobreviveu ao mata");
    }

    #[tokio::test]
    async fn sessao_que_morre_ao_subir_e_erro_com_o_motivo() {
        // É o caso comum de falha (workstream ocupado, projeto inexistente), e é justamente o
        // que passaria por "deu certo" se o hospedeiro só olhasse o código de saída do tmux.
        if !tem_tmux() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let (partida, projeto) = partida_com(
            "echo 'Error: workstream ocupado'; exit 3",
            dir.path(),
            "e5f6a7b8-morre",
        );
        let e = Tmux.lanca(&partida, &projeto).await.unwrap_err();
        let msg = format!("{e:#}");
        // O motivo tem de estar lá: um script que morre em milissegundos já sumiu antes de o
        // espelho começar, se o hospedeiro não segurar a partida até ele estar ligado.
        assert!(
            msg.contains("morreu") && msg.contains("workstream ocupado"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn tmux_diz_que_sessao_inexistente_nao_vive_e_matar_nao_e_erro() {
        let h: std::sync::Arc<dyn Hospedeiro> = std::sync::Arc::new(Tmux);
        let nome = "ld-teste-que-nao-existe-9f3a";
        assert!(!h.vive(nome).await);
        assert!(
            h.mata(nome).await.is_ok(),
            "matar o que não existe não é erro"
        );
        assert!(!h.nossas().await.iter().any(|n| n == nome));
    }
}
