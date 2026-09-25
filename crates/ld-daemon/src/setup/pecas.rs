//! O passo de setup de cada implementação de cada porta.
//!
//! Uma implementação nova (codex, whatsapp, herdr, ai-jail) ganha o passo dela aqui e um braço
//! no registro da porta ([`frontend`], [`agente`], [`hospedeiro`], [`memoria`]), do mesmo
//! jeito que se registra no `da_config` da porta. O resto do setup não muda.

use anyhow::{Result, bail};
use async_trait::async_trait;

use super::arquivos::Rascunho;
use super::tela::Tela;

#[async_trait(?Send)]
pub trait Peca {
    /// O nome que aparece no config e no seletor do setup.
    fn nome(&self) -> &'static str;

    /// Confere o que esta peça precisa na máquina, pergunta o que faltar e escreve o config.
    async fn configura(&self, tela: &mut Tela<'_>, r: &mut Rascunho) -> Result<()>;

    /// Depois de o config estar gravado: liga o que a peça precisa fora dele. O que já está
    /// ligado só é conferido, a menos que `r.refazer`.
    fn ativa(&self, _tela: &mut Tela<'_>, _r: &Rascunho) -> Result<()> {
        Ok(())
    }
}

/// O frontend pelo nome. Só os que têm setup entram aqui (o nulo é para teste e offline).
pub const FRONTENDS: &[&str] = &["telegram"];

pub fn frontend(nome: &str) -> Result<Box<dyn Peca>> {
    match nome {
        "telegram" => Ok(Box::new(Telegram)),
        outro => desconhecido("--frontend", outro, FRONTENDS),
    }
}

pub fn agente(nome: &str) -> Result<Box<dyn Peca>> {
    match nome {
        // "claude" é como as pessoas chamam; "claude-code" é o nome no config.
        "claude-code" | "claude" => Ok(Box::new(ClaudeCode)),
        outro => desconhecido("--agent", outro, crate::agente::AGENTES),
    }
}

pub fn hospedeiro(nome: &str) -> Result<Box<dyn Peca>> {
    match nome {
        "tmux" => Ok(Box::new(Tmux)),
        "herdr" => Ok(Box::new(Herdr)),
        outro => desconhecido("--session", outro, crate::sessions::HOSPEDEIROS),
    }
}

/// O hospedeiro de um config novo, sem `--session`: o tmux se houver, senão o herdr. Só um dos
/// dois é necessário. Sem nenhum, fica o tmux, e o passo dele avisa que falta um dos dois.
pub fn hospedeiro_da_maquina(tem_programa: &dyn Fn(&str) -> bool) -> &'static str {
    if !tem_programa("tmux") && tem_programa("herdr") {
        "herdr"
    } else {
        "tmux"
    }
}

pub fn memoria(nome: &str) -> Result<Box<dyn Peca>> {
    match nome {
        "ai-memory" => Ok(Box::new(AiMemory)),
        "nenhuma" | "nenhum" | "none" => Ok(Box::new(Nenhuma)),
        outro => desconhecido("--memoria", outro, crate::agente::MEMORIAS),
    }
}

fn desconhecido<T>(opcao: &str, nome: &str, existentes: &[&str]) -> Result<T> {
    bail!(
        "{opcao} {nome}: não existe (disponíveis: {})",
        existentes.join(", ")
    )
}

struct Telegram;

#[async_trait(?Send)]
impl Peca for Telegram {
    fn nome(&self) -> &'static str {
        "telegram"
    }

    async fn configura(&self, tela: &mut Tela<'_>, r: &mut Rascunho) -> Result<()> {
        #[cfg(feature = "telegram")]
        {
            use super::telegram::{configura, teloxide_api::conecta};
            configura(tela, r, &conecta).await
        }
        #[cfg(not(feature = "telegram"))]
        {
            let _ = (tela, r);
            bail!("este lukadispatchd foi compilado sem o Telegram (feature \"telegram\")")
        }
    }
}

struct ClaudeCode;

/// A versão que trouxe `PermissionRequest`, `async` e `asyncRewake` nos hooks.
const CLAUDE_MINIMO: (u32, u32, u32) = (2, 1, 274);

#[async_trait(?Send)]
impl Peca for ClaudeCode {
    fn nome(&self) -> &'static str {
        "claude-code"
    }

    async fn configura(&self, tela: &mut Tela<'_>, r: &mut Rascunho) -> Result<()> {
        if !(r.tem_programa)("claude") {
            tela.diz(
                "Aviso: o Claude Code (claude) não está no PATH. Instale antes de abrir a \
                 primeira sessão: https://docs.claude.com/en/docs/claude-code",
            );
        } else if let Some(v) = versao_do_claude()
            && v >= CLAUDE_MINIMO
        {
            tela.diz(&format!("Claude Code {}.{}.{}: ok.", v.0, v.1, v.2));
        } else if let Some(v) = versao_do_claude() {
            tela.diz(&format!(
                "Aviso: o Claude Code instalado é {}.{}.{}; o lukadispatch precisa de {}.{}.{} ou \
                 mais novo. Atualize com `claude update`.",
                v.0, v.1, v.2, CLAUDE_MINIMO.0, CLAUDE_MINIMO.1, CLAUDE_MINIMO.2
            ));
        }
        r.poe(Some("agente"), "tipo", "claude-code");
        Ok(())
    }

    fn ativa(&self, tela: &mut Tela<'_>, r: &Rascunho) -> Result<()> {
        tela.passo("Os hooks do Claude Code");
        if !r.refazer && hooks_instalados() {
            tela.diz("Instalados: ok.");
            return Ok(());
        }
        tela.diz(
            "Os hooks são como o Claude Code avisa o lukadispatch do que acontece. Nas sessões \
             que você abre no terminal, entra só telemetria (para aparecerem no painel); o \
             ~/.claude/settings.json ganha um backup antes.",
        );
        if !tela.sim("Instalar os hooks agora?")? {
            tela.diz("Para instalar depois: lukadispatch install --global");
            return Ok(());
        }
        let ok = std::process::Command::new(ld_core::paths::cli())
            .args(["install", "--global"])
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            bail!("lukadispatch install --global falhou");
        }
        Ok(())
    }
}

/// Os hooks de telemetria estão no settings do Claude Code como o install os deixaria, e o
/// settings das sessões do bot existe.
fn hooks_instalados() -> bool {
    use ld_core::{hooks, paths};

    let cli = paths::cli();
    paths::bot_settings_file().is_file()
        && hooks::le_settings(&paths::claude_settings())
            .is_ok_and(|s| hooks::instalados(&s, &hooks::telemetry_hooks(&cli)))
}

/// `2.1.280 (Claude Code)` vira `(2, 1, 280)`.
pub fn le_versao(texto: &str) -> Option<(u32, u32, u32)> {
    let mut partes = texto.split_whitespace().next()?.split('.');
    let mut n = || partes.next()?.parse::<u32>().ok();
    Some((n()?, n()?, n()?))
}

fn versao_do_claude() -> Option<(u32, u32, u32)> {
    let saida = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .ok()?;
    le_versao(&String::from_utf8_lossy(&saida.stdout))
}

struct Tmux;

#[async_trait(?Send)]
impl Peca for Tmux {
    fn nome(&self) -> &'static str {
        "tmux"
    }

    async fn configura(&self, tela: &mut Tela<'_>, r: &mut Rascunho) -> Result<()> {
        if !(r.tem_programa)("tmux") {
            tela.diz(
                "Aviso: o tmux não está no PATH, e cada sessão roda dentro de um. Instale pelo \
                 gerenciador de pacotes da sua distro, ou use o herdr (setup --session herdr).",
            );
        } else {
            tela.diz("tmux: ok.");
        }
        r.poe(None, "hospedeiro", "tmux");
        Ok(())
    }
}

struct Herdr;

#[async_trait(?Send)]
impl Peca for Herdr {
    fn nome(&self) -> &'static str {
        "herdr"
    }

    async fn configura(&self, tela: &mut Tela<'_>, r: &mut Rascunho) -> Result<()> {
        if !(r.tem_programa)("herdr") {
            tela.diz(
                "Aviso: o herdr não está no PATH, e cada sessão roda dentro dele. Instale por \
                 https://herdr.dev, ou use o tmux (setup --session tmux).",
            );
        } else {
            tela.diz("herdr: ok.");
        }
        // É o `script` que dá ao agente um terminal dentro do pane e guarda a saída da sessão,
        // que é onde aparece o motivo de uma sessão que morre ao subir.
        if !(r.tem_programa)("script") {
            tela.diz(
                "Aviso: falta o script (do util-linux), que o herdr usa para rodar cada sessão. \
                 Instale pelo gerenciador de pacotes da sua distro.",
            );
        }
        match r.atual.herdr.sessao.as_deref().map(str::trim) {
            Some("default") => tela.diz(
                "As sessões sobem na sessão padrão do herdr, ao lado das suas, porque o config \
                 pede sessao = \"default\" em [herdr].",
            ),
            Some(s) if !s.is_empty() => {
                tela.diz(&format!("As sessões sobem na sessão \"{s}\" do herdr."))
            }
            _ => tela.diz(&format!(
                "As sessões sobem numa sessão própria do herdr, \"{}\", separada das suas. \
                 Para vê-las: herdr --session {}",
                crate::sessions::SESSAO_DO_BOT,
                crate::sessions::SESSAO_DO_BOT
            )),
        }
        r.poe(None, "hospedeiro", "herdr");
        Ok(())
    }
}

struct AiMemory;

#[async_trait(?Send)]
impl Peca for AiMemory {
    fn nome(&self) -> &'static str {
        "ai-memory"
    }

    async fn configura(&self, tela: &mut Tela<'_>, r: &mut Rascunho) -> Result<()> {
        let mut memoria = "ai-memory";
        if (r.tem_programa)("ai-memory") {
            tela.diz("ai-memory: ok.");
        } else {
            tela.diz(
                "O ai-memory não está no PATH. Ele dá às sessões memória entre conversas \
                 (https://github.com/akitaonrails/ai-memory), mas não é obrigatório.",
            );
            if tela.sim("Rodar as sessões sem ele por enquanto?")? {
                memoria = "nenhuma";
            } else {
                tela.diz("Instale o ai-memory antes de abrir a primeira sessão.");
            }
        }
        poe_memoria(r, memoria);
        Ok(())
    }
}

struct Nenhuma;

#[async_trait(?Send)]
impl Peca for Nenhuma {
    fn nome(&self) -> &'static str {
        "nenhuma"
    }

    async fn configura(&self, _tela: &mut Tela<'_>, r: &mut Rascunho) -> Result<()> {
        poe_memoria(r, "nenhuma");
        Ok(())
    }
}

/// Grava a memória escolhida, e tira a chave com o nome antigo (`envelope`): as duas juntas
/// fariam o config não carregar, porque uma é apelido da outra.
fn poe_memoria(r: &mut Rascunho, memoria: &str) {
    r.tira(Some("agente"), "envelope");
    r.poe(Some("agente"), "memoria", memoria);
}
