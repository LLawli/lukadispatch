//! Onde cada coisa mora. Tudo respeita XDG, e o socket fica no runtime dir (tmpfs, some no
//! reboot, permissão do próprio usuário), que é exatamente o que um socket de controle quer.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
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

/// Onde ficam os arquivos que chegaram pelo Telegram para uma sessão.
///
/// Fora do projeto e fora do `/tmp`, e cada uma das duas coisas por um motivo: dentro do projeto
/// o anexo entraria num `git add -A` sem ninguém pedir, e o `/tmp` desta máquina é tmpfs, ou
/// seja, um PDF de 15 MB largado ali é RAM que não volta. O diretório é por sessão porque é a
/// sessão que é apagada no fim.
pub fn arquivos_dir(session_id: &str) -> PathBuf {
    arquivos_base().join(session_id)
}

/// A raiz de todos os diretórios de sessão. É o que a varredura de órfãos percorre.
pub fn arquivos_base() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
        .join("lukadispatch")
        .join("arquivos")
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
    let Ok(exe) = std::env::current_exe() else {
        return nome.to_string();
    };
    let invocado = std::env::args_os().next().map(PathBuf::from);
    let path = std::env::var_os("PATH");
    escolhe_vizinho(nome, &exe, invocado.as_deref(), path.as_deref())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| nome.to_string())
}

/// O caminho mais estável até o binário `nome` que mora ao lado de `exe`.
///
/// O `current_exe` resolve symlinks: sob o Homebrew ele devolve
/// `Cellar/lukadispatch/<versão>/bin`, e o que se grava com esse caminho (a unit do systemd, os
/// hooks do Claude Code) quebra no próximo `brew upgrade`, que apaga a pasta da versão. Por isso
/// a preferência é, nesta ordem: o diretório pelo qual este processo foi chamado (o systemd e os
/// hooks chamam por caminho absoluto), a entrada do PATH que leva ao mesmo arquivo (o shell chama
/// pelo nome), e só então o caminho real. `None` quando não há `nome` ao lado de `exe`.
pub fn escolhe_vizinho(
    nome: &str,
    exe: &Path,
    invocado: Option<&Path>,
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    let real = exe.parent()?.join(nome);
    let alvo = std::fs::canonicalize(&real).ok()?;
    let mesmo = |c: &Path| c.is_file() && std::fs::canonicalize(c).is_ok_and(|x| x == alvo);

    if let Some(dir) = invocado.filter(|i| i.is_absolute()).and_then(Path::parent) {
        let c = dir.join(nome);
        if mesmo(&c) {
            return Some(c);
        }
    }
    for dir in path.map(std::env::split_paths).into_iter().flatten() {
        let c = dir.join(nome);
        if dir.is_absolute() && mesmo(&c) {
            return Some(c);
        }
    }
    Some(real)
}

/// Caminho do daemon, ao lado dos outros binários. É por ele que o `lukadispatch setup` passa.
pub fn daemon() -> String {
    vizinho("lukadispatchd")
}

/// Caminho da janela de pergunta do PC, ao lado dos outros binários.
pub fn janela() -> String {
    vizinho("lukadispatch-ask")
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

    /// A árvore do Homebrew: o binário mora numa pasta com a versão, e o que fica no PATH e na
    /// unit é um symlink que o `brew upgrade` troca de alvo.
    fn arvore_do_brew() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let raiz = tempfile::tempdir().unwrap();
        let cellar = raiz.path().join("Cellar/lukadispatch/0.1.0/bin");
        let bin = raiz.path().join("bin");
        std::fs::create_dir_all(&cellar).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        for nome in ["lukadispatch", "lukadispatchd"] {
            std::fs::write(cellar.join(nome), "#!/bin/sh\n").unwrap();
            std::os::unix::fs::symlink(cellar.join(nome), bin.join(nome)).unwrap();
        }
        (raiz, cellar, bin)
    }

    #[test]
    fn vizinho_prefere_o_caminho_pelo_qual_foi_chamado() {
        // O systemd e os hooks chamam por caminho absoluto; o current_exe resolve o symlink até
        // a pasta da versão, que some no próximo upgrade.
        let (_r, cellar, bin) = arvore_do_brew();
        let exe = cellar.join("lukadispatchd");
        let achado = escolhe_vizinho("lukadispatch", &exe, Some(&bin.join("lukadispatchd")), None);
        assert_eq!(achado, Some(bin.join("lukadispatch")));
    }

    #[test]
    fn vizinho_chamado_pelo_nome_se_acha_pelo_path() {
        let (_r, cellar, bin) = arvore_do_brew();
        let exe = cellar.join("lukadispatch");
        let path = std::env::join_paths(["/nao/existe", bin.to_str().unwrap()]).unwrap();
        let achado = escolhe_vizinho(
            "lukadispatchd",
            &exe,
            Some(std::path::Path::new("lukadispatch")),
            Some(&path),
        );
        assert_eq!(achado, Some(bin.join("lukadispatchd")));
    }

    #[test]
    fn sem_caminho_estavel_fica_o_real() {
        let (_r, cellar, _bin) = arvore_do_brew();
        let exe = cellar.join("lukadispatch");
        assert_eq!(
            escolhe_vizinho("lukadispatchd", &exe, None, None),
            Some(cellar.join("lukadispatchd"))
        );
        assert_eq!(escolhe_vizinho("nao-existe", &exe, None, None), None);
    }

    #[test]
    fn socket_respeita_override() {
        // SAFETY: teste de processo único; nenhuma outra thread lê env aqui.
        unsafe { std::env::set_var("LUKADISPATCH_SOCKET", "/tmp/x.sock") };
        assert_eq!(socket(), PathBuf::from("/tmp/x.sock"));
        unsafe { std::env::remove_var("LUKADISPATCH_SOCKET") };
    }
}
