//! Nascimento e morte de uma sessão: o tmux, o script de partida e o prompt de bootstrap.
//!
//! A sessão sobe como `ai-memory run claude` dentro de um tmux próprio, então continua entrando
//! na memória de longo prazo e dá para anexar no PC com `tmux attach`. O prompt inicial é
//! argumento do `claude`, não tecla injetada: é a única "injeção" do projeto e ela acontece
//! antes de a sessão existir.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use ld_core::config::Project;
use ld_core::paths;
use tokio::process::Command;

pub struct Launched {
    pub session_id: String,
    pub tmux: String,
}

/// Nome de sessão tmux: previsível para você achar no `tmux ls`, e único para dois projetos com
/// o mesmo nome (ou o mesmo projeto duas vezes) não colidirem.
pub fn tmux_name(projeto: &str, session_id: &str) -> String {
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

/// O que a sessão lê antes de qualquer outra coisa.
///
/// Ele precisa ser explícito em três pontos, e cada um deles já foi motivo de bug em ponte de
/// agente: (1) o Monitor é ferramenta diferida, então sem `ToolSearch` antes o agente não
/// consegue chamá-lo; (2) a resposta vai sozinha pelo hook, senão o agente tenta "mandar" a
/// mensagem por conta própria e inventa um curl; (3) o monitor expira e precisa voltar.
pub fn bootstrap_prompt(session_id: &str, projeto: &str) -> String {
    let cli = paths::cli();
    format!(
        r#"Você está rodando dentro do lukadispatch. O seu usuário (Luka) fala com você pelo tópico "{projeto}" de um grupo do Telegram, e NÃO por este terminal. Ninguém está lendo esta tela.

Faça agora, nesta ordem, e nada além disso:

1. Chame ToolSearch com query "select:Monitor" para carregar o schema da ferramenta Monitor.
2. Chame Monitor com exatamente estes argumentos:
   command: {cli} listen --session {session_id}
   description: mensagens do Telegram
   timeout_ms: 1800000
3. Pare. Não escreva relatório, não explore o projeto, não chame mais nenhuma ferramenta. Fique em silêncio até chegar o primeiro evento do monitor.

Como funciona daqui em diante:

- Cada linha que o monitor emitir é uma mensagem do Luka, em JSON: {{"kind":"message","text":"...","from":"...","at":0}}. Trate o campo "text" exatamente como se ele tivesse acabado de digitar aquilo para você, e trabalhe normalmente.
- VOCÊ NÃO PRECISA ENVIAR NADA DE VOLTA. Um hook pega a sua resposta final e entrega no Telegram sozinho. Nunca chame curl, nunca use a API do Telegram, nunca tente "mandar mensagem": isso duplicaria tudo.
- Perguntas e pedidos de permissão também saem sozinhos: use AskUserQuestion normalmente, que ela aparece no celular e numa janela no PC ao mesmo tempo.
- O monitor expira a cada 30 minutos. Quando isso acontecer, arme-o de novo com a mesma chamada do passo 2. Se você terminar um turno sem monitor armado, um lembrete vai chegar: cumpra-o na hora, senão a sessão fica surda.
"#
    )
}

/// Nome do workstream gerenciado do ai-memory.
///
/// Precisa ser único por sessão: o `ai-memory run` recusa com 409 quando o workstream do projeto
/// já está ativo (é o que acontece se você já tem uma sessão gerenciada ali), e recusa de novo se
/// o nome pedido em `--new` já existir. Com o id da sessão no nome, nenhum dos dois acontece.
pub fn workstream_name(session_id: &str) -> String {
    format!("lukadispatch-{}", &session_id[..8])
}

/// Escreve o script de partida da sessão e devolve o caminho.
///
/// Existe um script em vez de uma linha de comando montada na hora por dois motivos: o prompt
/// sai do `ps` (ele é longo e vaza o nome do projeto para qualquer um que liste processos), e dá
/// para ler depois exatamente o que foi lançado quando algo der errado.
fn write_launch_script(
    session_id: &str,
    projeto: &Project,
    permission_mode: &str,
) -> Result<PathBuf> {
    let dir = paths::state_dir().join("sessions").join(session_id);
    std::fs::create_dir_all(&dir).with_context(|| format!("criando {}", dir.display()))?;

    let prompt = dir.join("prompt.txt");
    std::fs::write(&prompt, bootstrap_prompt(session_id, &projeto.name))?;

    let script = dir.join("launch.sh");
    let settings = paths::bot_settings_file();
    std::fs::write(
        &script,
        format!(
            r#"#!/usr/bin/env bash
# Gerado pelo lukadispatch para a sessão {session_id}. Editar aqui não muda nada:
# o arquivo é reescrito a cada sessão nova.
set -u
exec ai-memory run --new {workstream} claude \
  --session-id {session_id} \
  --settings {settings} \
  --permission-mode {permission_mode} \
  "$(cat {prompt})"
"#,
            settings = settings.display(),
            prompt = prompt.display(),
            workstream = workstream_name(session_id),
        ),
    )?;
    Ok(script)
}

/// Sobe a sessão. Devolve erro sem deixar lixo se o tmux não vingar.
pub async fn launch(projeto: &Project, permission_mode: &str) -> Result<Launched> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let tmux = tmux_name(&projeto.name, &session_id);
    let script = write_launch_script(&session_id, projeto, permission_mode)?;

    let saida = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &tmux,
            "-c",
            &projeto.path,
            "-e",
            &format!("LD_SESSION={session_id}"),
            // O hook roda dentro desta sessão e precisa achar o socket. O servidor tmux pode
            // ter sido iniciado com outro ambiente (sem XDG_RUNTIME_DIR, por exemplo), então o
            // caminho vai explícito em vez de depender do que ele herdou.
            "-e",
            &format!("LUKADISPATCH_SOCKET={}", paths::socket().display()),
            "bash",
        ])
        .arg(&script)
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

    // Espelha o painel num arquivo. É por `pipe-pane`, e não redirecionando o comando, porque o
    // Claude Code precisa de um terminal de verdade no stdout: com um pipe ali ele entra em modo
    // não interativo. Sem esse espelho, uma sessão que morre ao subir não deixa pista nenhuma.
    let log = script.with_file_name("pane.log");
    let _ = Command::new("tmux")
        .args(["pipe-pane", "-o", "-t", &tmux])
        .arg(format!("cat >> {}", log.display()))
        .output()
        .await;

    // Morrer logo depois de subir é o caso comum de erro (workstream ocupado, diálogo de
    // confiança, projeto inexistente), e é justamente o que passaria por "deu certo".
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    if !has_session(&tmux).await {
        bail!("a sessão morreu ao subir: {}", primeiro_erro(&log));
    }

    Ok(Launched { session_id, tmux })
}

/// A linha de erro mais útil do espelho do painel.
///
/// O arquivo tem sequências de escape do terminal misturadas ao texto; interessa a primeira
/// linha que fala de erro, que é o que explica a morte.
fn primeiro_erro(log: &std::path::Path) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nome_de_tmux_e_previsivel_e_unico() {
        let id = "abcd1234-0000-0000-0000-000000000000";
        assert_eq!(tmux_name("lukadispatch", id), "ld-lukadispatch-abcd");
        assert_eq!(tmux_name("Meu Projeto!", id), "ld-meu-projeto-abcd");
    }

    #[test]
    fn nome_vazio_nao_gera_sessao_sem_nome() {
        let id = "abcd1234-0000-0000-0000-000000000000";
        assert_eq!(tmux_name("!!!", id), "ld-projeto-abcd");
    }

    #[test]
    fn workstream_e_unico_por_sessao() {
        // O ai-memory recusa com 409 se o workstream do projeto já estiver ativo, então dois
        // lançamentos não podem pedir o mesmo nome.
        let a = workstream_name("abcd1234-0000-0000-0000-000000000000");
        let b = workstream_name("ffff9999-0000-0000-0000-000000000000");
        assert_ne!(a, b);
        assert_eq!(a, "lukadispatch-abcd1234");
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

    #[test]
    fn espelho_ausente_nao_explode() {
        assert_eq!(
            primeiro_erro(std::path::Path::new("/nao/existe.log")),
            "sem saída registrada"
        );
    }

    #[test]
    fn prompt_carrega_o_monitor_antes_de_usar() {
        let p = bootstrap_prompt("sid-123", "proj");
        let pos_toolsearch = p
            .find("ToolSearch")
            .expect("precisa mandar carregar o schema");
        let pos_monitor = p
            .find("Chame Monitor")
            .expect("precisa mandar armar o monitor");
        assert!(
            pos_toolsearch < pos_monitor,
            "Monitor é ferramenta diferida: o ToolSearch tem que vir antes"
        );
        assert!(p.contains("listen --session sid-123"));
        assert!(p.contains("1800000"));
    }

    #[test]
    fn prompt_proibe_o_agente_de_enviar_sozinho() {
        let p = bootstrap_prompt("sid", "proj");
        assert!(p.contains("NÃO PRECISA ENVIAR NADA DE VOLTA"));
        assert!(p.contains("curl"));
    }
}
