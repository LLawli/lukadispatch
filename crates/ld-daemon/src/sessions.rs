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

/// Como subir uma sessão.
///
/// `resume` é o que permite trocar o modelo de uma sessão viva sem perder a conversa: mata-se o
/// processo e sobe-se de novo com `--resume <id>`, que continua o mesmo transcript. É a única
/// forma de atender `/model` e `/effort` pelo Telegram, porque esses comandos são do frontend do
/// Claude Code e nenhum evento consegue dispará-los.
pub struct Spec<'a> {
    pub projeto: &'a Project,
    pub permission_mode: &'a str,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub resume: Option<&'a str>,
    /// `true` quando o `resume` é "continuar de onde parou" (e não uma troca de modelo). Muda só
    /// a primeira frase do prompt, para a sessão saber por que voltou.
    pub retomada: bool,
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
    // A marca é a primeira linha de todo prompt injetado: é por ela que o replay sabe que este
    // texto é do sistema, e não uma fala sua.
    let marca = ld_core::transcript::MARCA_SISTEMA;
    format!(
        r#"{marca}
Você está rodando dentro do lukadispatch. O seu usuário (Luka) fala com você pelo tópico "{projeto}" de um grupo do Telegram, e NÃO por este terminal. Ninguém está lendo esta tela.

Faça agora, nesta ordem, e nada além disso:

1. Chame ToolSearch com query "select:Monitor" para carregar o schema da ferramenta Monitor.
2. Chame Monitor com exatamente estes argumentos:
   command: {cli} listen --session {session_id}
   description: mensagens do Telegram
   timeout_ms: 1800000
3. Pare. Não escreva relatório, não explore o projeto, não chame mais nenhuma ferramenta. Fique em silêncio até chegar o primeiro evento do monitor.

Como funciona daqui em diante:

- Cada linha que o monitor emitir é uma mensagem do Luka, em JSON: {{"kind":"message","text":"...","from":"...","at":0}}. Trate o campo "text" exatamente como se ele tivesse acabado de digitar aquilo para você, e trabalhe normalmente. O campo "from" diz de ONDE a mensagem saiu (o nome de quem escreveu, quando veio do Telegram, ou "pc" quando foi injetada aqui da máquina), e não muda em nada o que você deve fazer.
- VOCÊ NÃO PRECISA ENVIAR NADA DE VOLTA. Um hook pega a sua resposta final e entrega no Telegram sozinho. Nunca chame curl, nunca use a API do Telegram, nunca tente "mandar mensagem": isso duplicaria tudo.
- Perguntas e pedidos de permissão também saem sozinhos: use AskUserQuestion normalmente, que ela aparece no celular e numa janela no PC ao mesmo tempo.
- O monitor expira a cada 30 minutos. Quando isso acontecer, arme-o de novo com a mesma chamada do passo 2. Se você terminar um turno sem monitor armado, um lembrete vai chegar: cumpra-o na hora, senão a sessão fica surda.
"#
    )
}

/// O que a sessão lê quando volta por `--resume` (troca de modelo ou de esforço).
///
/// Curto de propósito: o contexto todo já está de volta com ela, e a única coisa que se perdeu
/// no caminho foi o Monitor, que morre junto com o processo anterior.
pub fn rearm_prompt(session_id: &str, retomada: bool) -> String {
    let cli = paths::cli();
    let marca = ld_core::transcript::MARCA_SISTEMA;
    let abertura = if retomada {
        "Esta conversa foi retomada pelo lukadispatch e agora está ligada a um tópico do Telegram. Tudo o que vocês já conversaram continua aqui; o Luka acabou de receber as últimas falas no celular."
    } else {
        "A sua sessão foi reiniciada pelo lukadispatch (troca de modelo ou de esforço). O contexto continua o mesmo; o que se perdeu foi o canal do Telegram."
    };
    format!(
        r#"{marca}
{abertura}

Faça só isto, agora:

1. Chame ToolSearch com query "select:Monitor".
2. Chame Monitor com command "{cli} listen --session {session_id}", description "mensagens do Telegram" e timeout_ms 1800000.
3. Pare e fique em silêncio até chegar o próximo evento do monitor. Não retome o que estava fazendo por conta própria, não resuma nada e não pergunte se pode continuar: se o Luka quiser seguir, ele manda.
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

/// Igual ao de cima, mais um carimbo de tempo.
///
/// Uma sessão pode subir mais de uma vez (troca de modelo por `--resume`), e `--new` recusa nome
/// repetido. Sem o carimbo, a segunda partida da mesma sessão morreria com 409.
fn workstream_name_unico(session_id: &str) -> String {
    let agora = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{}-{agora}", workstream_name(session_id))
}

/// Escreve o script de partida da sessão e devolve o caminho.
///
/// Existe um script em vez de uma linha de comando montada na hora por dois motivos: o prompt
/// sai do `ps` (ele é longo e vaza o nome do projeto para qualquer um que liste processos), e dá
/// para ler depois exatamente o que foi lançado quando algo der errado.
fn write_launch_script(session_id: &str, spec: &Spec<'_>) -> Result<PathBuf> {
    let dir = paths::state_dir().join("sessions").join(session_id);
    std::fs::create_dir_all(&dir).with_context(|| format!("criando {}", dir.display()))?;

    let prompt = dir.join("prompt.txt");
    let texto = match spec.resume {
        Some(_) => rearm_prompt(session_id, spec.retomada),
        None => bootstrap_prompt(session_id, &spec.projeto.name),
    };
    std::fs::write(&prompt, texto)?;

    let script = dir.join("launch.sh");
    let settings = paths::bot_settings_file();

    // `--session-id` cria; `--resume` continua. Os dois juntos o Claude Code recusa.
    let selecao = match spec.resume {
        Some(id) => format!("--resume {id}"),
        None => format!("--session-id {session_id}"),
    };
    let mut extras = String::new();
    if let Some(m) = spec.model {
        extras.push_str(&format!("  --model {m} \\\n"));
    }
    if let Some(e) = spec.effort {
        extras.push_str(&format!("  --effort {e} \\\n"));
    }

    std::fs::write(
        &script,
        format!(
            r#"#!/usr/bin/env bash
# Gerado pelo lukadispatch para a sessão {session_id}. Editar aqui não muda nada:
# o arquivo é reescrito a cada partida da sessão.
set -u
exec ai-memory run --new {workstream} claude \
  {selecao} \
  --settings {settings} \
  --permission-mode {permission_mode} \
{extras}  -n {nome} \
  "$(cat {prompt})"
"#,
            settings = settings.display(),
            prompt = prompt.display(),
            permission_mode = spec.permission_mode,
            workstream = workstream_name_unico(session_id),
            nome = shell_quote(&spec.projeto.name),
        ),
    )?;
    Ok(script)
}

/// Aspas simples para um argumento de shell, com o truque padrão para a própria aspa.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Sobe a sessão. Devolve erro sem deixar lixo se o tmux não vingar.
pub async fn launch(spec: &Spec<'_>) -> Result<Launched> {
    let session_id = match spec.resume {
        Some(id) => id.to_string(),
        None => uuid::Uuid::new_v4().to_string(),
    };
    let tmux = tmux_name(&spec.projeto.name, &session_id);
    let script = write_launch_script(&session_id, spec)?;

    let saida = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &tmux,
            "-c",
            &spec.projeto.path,
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
    fn todo_prompt_injetado_leva_a_marca() {
        // Sem ela, o replay mostraria estes textos como se você os tivesse escrito.
        assert!(bootstrap_prompt("sid", "proj").starts_with(ld_core::transcript::MARCA_SISTEMA));
        assert!(rearm_prompt("sid", true).starts_with(ld_core::transcript::MARCA_SISTEMA));
        assert!(rearm_prompt("sid", false).starts_with(ld_core::transcript::MARCA_SISTEMA));
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
    fn prompt_de_rearme_nao_manda_continuar_sozinho() {
        // Voltar de um --resume com a sessão retomando tarefa sozinha seria surpresa ruim: quem
        // decide continuar é quem está do outro lado.
        let p = rearm_prompt("sid", false);
        assert!(p.contains("Monitor"));
        assert!(p.contains("não retome") || p.contains("Não retome"));
    }

    #[test]
    fn workstream_de_relancamento_nao_repete() {
        let a = workstream_name_unico("abcd1234-0000-0000-0000-000000000000");
        assert!(a.starts_with("lukadispatch-abcd1234-"));
        assert_ne!(a, workstream_name("abcd1234-0000-0000-0000-000000000000"));
    }

    #[test]
    fn nome_com_aspa_nao_quebra_o_script() {
        assert_eq!(shell_quote("meu'projeto"), "'meu'\\''projeto'");
    }

    #[test]
    fn prompt_proibe_o_agente_de_enviar_sozinho() {
        let p = bootstrap_prompt("sid", "proj");
        assert!(p.contains("NÃO PRECISA ENVIAR NADA DE VOLTA"));
        assert!(p.contains("curl"));
    }
}
