//! Onde cada coisa mora. Tudo respeita XDG, e o socket fica no runtime dir (tmpfs, some no
//! reboot, permissão do próprio usuário), que é exatamente o que um socket de controle quer.

use std::path::PathBuf;

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME não definido"))
}

fn xdg(var: &str, default: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(default),
    }
}

/// Socket de controle. O CLI, os hooks e a janela de pergunta falam com o daemon por aqui.
///
/// Fica no runtime dir para herdar o modo 0700 do diretório: não há token de autenticação no
/// protocolo justamente porque o acesso já é do dono da sessão. `LUKADISPATCH_SOCKET` existe
/// para teste (os testes de integração sobem um daemon próprio num tempdir).
pub fn socket() -> PathBuf {
    if let Some(v) = std::env::var_os("LUKADISPATCH_SOCKET") {
        return PathBuf::from(v);
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v).join("lukadispatch.sock"),
        // Sem runtime dir (cron, serviço sem sessão), cai para o diretório de estado.
        _ => state_dir().join("lukadispatch.sock"),
    }
}

/// Banco de estado, log do daemon, settings gerado para as sessões do bot.
pub fn state_dir() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state").join("lukadispatch")
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("lukadispatch")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn state_db() -> PathBuf {
    state_dir().join("state.db")
}

/// Arquivo de settings passado em `claude --settings`. É um só, compartilhado por todas as
/// sessões do bot: o hook descobre de qual sessão se trata pelo `session_id` que já vem no
/// stdin, então não há motivo para gerar um arquivo por sessão.
pub fn bot_settings_file() -> PathBuf {
    state_dir().join("bot-settings.json")
}

/// Caminho do binário `lukadispatch`, do jeito que outro processo precisa dele.
///
/// Procura ao lado do executável atual antes de confiar no PATH, e isso importa em dois lugares
/// onde o PATH não é o seu: um serviço systemd tem PATH mínimo, e o comando que o Monitor roda
/// dentro da sessão herda o ambiente do Claude Code. Caminho absoluto resolve os dois.
pub fn cli() -> String {
    vizinho("lukadispatch")
}

/// Um binário do projeto, procurado ao lado do executável atual antes do PATH.
fn vizinho(nome: &str) -> String {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let candidato = dir.join(nome);
        if candidato.is_file() {
            return candidato.to_string_lossy().into_owned();
        }
    }
    nome.to_string()
}

/// Caminho do proxy de MCP, ao lado dos outros binários.
pub fn mcp_proxy() -> String {
    vizinho("lukadispatch-mcp")
}

/// Diretório do Claude Code do usuário (transcripts, settings global, banco de uso).
pub fn claude_dir() -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home().join(".claude"),
    }
}

pub fn claude_settings() -> PathBuf {
    claude_dir().join("settings.json")
}

/// `~/.claude.json`: onde o Claude Code guarda, entre outras coisas, a confiança por pasta.
pub fn claude_json() -> PathBuf {
    home().join(".claude.json")
}

/// Banco do XClaudeUsage: quem escreve é o statusline/hook do usuário, aqui só lemos.
pub fn usage_db() -> PathBuf {
    claude_dir().join("data").join("xclaude-usage.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_respeita_override() {
        // SAFETY: teste de processo único; nenhuma outra thread lê env aqui.
        unsafe { std::env::set_var("LUKADISPATCH_SOCKET", "/tmp/x.sock") };
        assert_eq!(socket(), PathBuf::from("/tmp/x.sock"));
        unsafe { std::env::remove_var("LUKADISPATCH_SOCKET") };
    }
}
