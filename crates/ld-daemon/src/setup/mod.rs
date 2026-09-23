//! `lukadispatch setup`: do binário instalado até o bot respondendo no grupo, perguntando só o
//! que não dá para descobrir sozinho.
//!
//! O setup é uma sequência de peças, uma por porta, escolhidas pelos seletores:
//!
//! ```text
//! lukadispatch setup --frontend telegram --agent claude-code --session tmux --envelope ai-memory
//! ```
//!
//! Os padrões são as implementações que existem hoje. Cada implementação traz o próprio passo
//! (ver [`pecas`]): o do Telegram cria o bot e o grupo, o do ai-memory confere se ele está
//! instalado. Depois das peças vêm as preferências (seu nome, onde ficam os projetos, o modo de
//! permissão), a gravação do config e do `.env`, e por fim os hooks e o serviço, cada um
//! perguntado antes.
//!
//! Mora no daemon, e não no CLI, porque o passo do Telegram fala com a API do Telegram, e o CLI
//! não conhece frontend nenhum (é std síncrono de propósito: roda a cada hook). O
//! `lukadispatch setup` só repassa para o `lukadispatchd setup`.

pub mod arquivos;
pub mod pecas;
pub mod tela;
pub mod telegram;

#[cfg(test)]
mod testes;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ld_core::paths;

use arquivos::Rascunho;
use pecas::Peca;
use tela::Tela;

pub const AJUDA: &str = "\
lukadispatch setup: configura o lukadispatch nesta máquina, conversando.

  --frontend <nome>   o aplicativo de chat           (padrão: telegram)
  --agent <nome>      o agente de código das sessões  (padrão: claude-code)
  --session <nome>    onde cada sessão roda           (padrão: tmux)
  --envelope <nome>   o que envolve o agente          (padrão: ai-memory; ou nenhum)

Rodar de novo é seguro: o que já está configurado é oferecido como padrão, e o config.toml
existente é editado no lugar, com backup.";

/// Qual implementação de cada porta o setup configura.
#[derive(Debug, Clone, PartialEq)]
pub struct Selecao {
    pub frontend: String,
    pub agente: String,
    pub hospedeiro: String,
    pub envelope: String,
}

impl Default for Selecao {
    fn default() -> Self {
        Self {
            frontend: "telegram".into(),
            agente: "claude-code".into(),
            hospedeiro: "tmux".into(),
            envelope: "ai-memory".into(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Pedido {
    Setup(Selecao),
    Ajuda,
}

/// Lê os argumentos depois de `setup`. Aceita `--opcao valor` e `--opcao=valor`.
pub fn le_argumentos(args: &[String]) -> Result<Pedido> {
    let mut sel = Selecao::default();
    let mut resto = args.iter();
    while let Some(arg) = resto.next() {
        if matches!(arg.as_str(), "-h" | "--help" | "ajuda") {
            return Ok(Pedido::Ajuda);
        }
        let (opcao, valor) = match arg.split_once('=') {
            Some((o, v)) => (o, v.to_string()),
            None => {
                let v = resto
                    .next()
                    .with_context(|| format!("{arg} precisa de um valor"))?;
                (arg.as_str(), v.clone())
            }
        };
        let campo = match opcao {
            "--frontend" => &mut sel.frontend,
            "--agent" | "--agente" => &mut sel.agente,
            "--session" | "--sessao" => &mut sel.hospedeiro,
            "--envelope" => &mut sel.envelope,
            outro => bail!("opção desconhecida: {outro}\n\n{AJUDA}"),
        };
        *campo = valor;
    }
    Ok(Pedido::Setup(sel))
}

/// As peças da seleção, na ordem da conversa. Nome desconhecido falha aqui, antes de qualquer
/// pergunta.
///
/// Os pré-requisitos locais vêm antes do frontend: é melhor saber que falta o tmux antes de
/// criar um bot do que depois.
pub fn pecas(sel: &Selecao) -> Result<Vec<Box<dyn Peca>>> {
    Ok(vec![
        pecas::hospedeiro(&sel.hospedeiro)?,
        pecas::agente(&sel.agente)?,
        pecas::envelope(&sel.envelope)?,
        pecas::frontend(&sel.frontend)?,
    ])
}

/// O setup de verdade, no terminal.
pub async fn roda(sel: Selecao) -> Result<()> {
    let pecas = pecas(&sel)?;
    let home = paths::home();
    let arquivo_config = paths::config_file();
    // Fixo em ~/.config, e não em $XDG_CONFIG_HOME: é o EnvironmentFile da unit (%h/.config).
    let arquivo_env = home.join(".config/lukadispatch/.env");

    let config = std::fs::read_to_string(&arquivo_config).ok();
    let env = std::fs::read_to_string(&arquivo_env).ok();
    let mut r = Rascunho::de(config.as_deref(), env.as_deref())?;

    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    let mut entrada = stdin.lock();
    let mut saida = std::io::stdout();
    let mut tela = Tela::nova(&mut entrada, &mut saida, terminal);

    tela.diz(
        "Configurando o lukadispatch. Até o passo \"Gravando\", Ctrl+C cancela sem mudar nada.",
    );
    let estava_ativo = servico_ativo();
    if estava_ativo {
        // O daemon e o setup puxando mensagens do mesmo bot brigam: o Telegram entrega cada
        // update a um só, e o outro recebe 409.
        tela.diz("O serviço está rodando; ele fica parado enquanto o setup conversa com o bot.");
        systemctl(&["stop", "lukadispatch"]);
    }
    // Cancelar não pode deixar estrago: o serviço volta como estava, e o eco do terminal volta
    // caso o Ctrl+C tenha vindo no meio da leitura do token. Roda noutra thread do runtime,
    // porque a conversa bloqueia esta lendo o terminal.
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            if let Ok(tty) = std::fs::File::open("/dev/tty") {
                let _ = std::process::Command::new("stty")
                    .arg("echo")
                    .stdin(tty)
                    .status();
            }
            if estava_ativo {
                systemctl(&["start", "lukadispatch"]);
            }
            eprintln!("\nSetup cancelado.");
            std::process::exit(130);
        }
    });

    if let Err(e) = conduz(&mut tela, &mut r, &pecas, &home).await {
        if estava_ativo {
            systemctl(&["start", "lukadispatch"]);
        }
        return Err(e);
    }

    tela.passo("Gravando");
    arquivos::grava(&arquivo_config, &r.config_texto(), 0o644)?;
    tela.diz(&format!("{}", arquivo_config.display()));
    arquivos::grava(&arquivo_env, &r.env_texto(), 0o600)?;
    tela.diz(&format!("{} (só você lê)", arquivo_env.display()));

    for p in &pecas {
        p.ativa(&mut tela)?;
    }
    servico(&mut tela, &home, estava_ativo).await
}

/// As peças e as preferências, sem tocar no disco. É o que os testes exercitam.
pub async fn conduz(
    tela: &mut Tela<'_>,
    r: &mut Rascunho,
    pecas: &[Box<dyn Peca>],
    home: &Path,
) -> Result<()> {
    tela.passo("Esta máquina");
    for p in pecas {
        p.configura(tela, r).await?;
    }
    preferencias(tela, r, home)
}

fn preferencias(tela: &mut Tela<'_>, r: &mut Rascunho, home: &Path) -> Result<()> {
    tela.passo("Preferências");

    let nome_padrao = r
        .atual
        .usuario
        .clone()
        .or_else(|| r.nome_sugerido.clone())
        .unwrap_or_default();
    let nome = tela.pergunta("Como a sessão deve chamar você", &nome_padrao)?;
    if !nome.is_empty() {
        r.poe(None, "usuario", nome.as_str());
    }

    let raizes_padrao = if !r.havia_config || r.atual.scan.roots.is_empty() {
        let sugeridas: Vec<String> = arquivos::raizes_sugeridas(home)
            .iter()
            .take(3)
            .map(|p| arquivos::com_til(p, home))
            .collect();
        if sugeridas.is_empty() {
            "~/Projetos".to_string()
        } else {
            sugeridas.join(", ")
        }
    } else {
        r.atual.scan.roots.join(", ")
    };
    tela.diz("Todo repositório git dentro destas pastas aparece no /new.");
    let raizes = tela.pergunta(
        "Pastas com os seus projetos, separadas por vírgula",
        &raizes_padrao,
    )?;
    let lista: toml_edit::Array = raizes
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    r.poe(Some("scan"), "enabled", true);
    r.poe(Some("scan"), "roots", lista);

    const MODOS: [(&str, &str); 3] = [
        (
            "auto",
            "o Claude Code decide sozinho o que é seguro, e pergunta pouco",
        ),
        (
            "perguntar",
            "toda edição e todo comando viram um card para você aprovar no celular",
        ),
        (
            "acceptEdits",
            "edições de arquivo passam direto; comandos perguntam",
        ),
    ];
    let mut opcoes = MODOS.to_vec();
    if let Some(i) = opcoes
        .iter()
        .position(|(m, _)| *m == r.atual.default_permission_mode)
    {
        let atual = opcoes.remove(i);
        opcoes.insert(0, atual);
    }
    let i = tela.escolhe(
        "Modo de permissão das sessões (/mode troca depois, por sessão):",
        &opcoes,
    )?;
    r.poe(None, "default_permission_mode", opcoes[i].0);

    let t = &r.atual.transcricao;
    if t.ativa
        && let Some(exe) = t.comando.first()
    {
        let exe = expande(exe, home);
        if exe.is_absolute() && !exe.exists() {
            tela.diz(&format!(
                "A transcrição de voz está configurada para {}, que não existe nesta máquina. \
                 Sem ela, mensagem de voz chega à sessão como arquivo de áudio, sem texto. \
                 Para ligar depois: docs/decisoes/0007-transcricao.md.",
                exe.display()
            ));
            if tela.sim("Desligar a transcrição por enquanto?")? {
                r.poe(Some("transcricao"), "ativa", false);
            }
        }
    }
    Ok(())
}

fn expande(caminho: &str, home: &Path) -> PathBuf {
    match caminho.strip_prefix("~/") {
        Some(resto) => home.join(resto),
        None => PathBuf::from(caminho),
    }
}

fn systemctl(args: &[&str]) -> bool {
    std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn servico_ativo() -> bool {
    systemctl(&["is-active", "--quiet", "lukadispatch"])
}

/// A unit do systemd, se ainda não existir (quem instalou por `cargo install` não tem).
fn garante_unit(tela: &mut Tela<'_>, home: &Path) -> Result<()> {
    let unit = home.join(".config/systemd/user/lukadispatch.service");
    if unit.exists() {
        return Ok(());
    }
    let dir = std::env::current_exe()?
        .parent()
        .context("diretório do lukadispatchd")?
        .to_path_buf();
    let modelo = include_str!("../../../../dist/lukadispatch.service");
    let texto = if dir == home.join(".local/bin") {
        modelo.to_string()
    } else {
        modelo.replace("%h/.local/bin", &dir.to_string_lossy())
    };
    arquivos::grava(&unit, &texto, 0o644)?;
    tela.diz(&format!("{}", unit.display()));
    Ok(())
}

async fn servico(tela: &mut Tela<'_>, home: &Path, estava_ativo: bool) -> Result<()> {
    tela.passo("O serviço");
    if !systemctl(&["show-environment"]) {
        tela.diz("Não há systemd de usuário aqui. Para rodar o daemon à mão: lukadispatchd");
        return Ok(());
    }
    garante_unit(tela, home)?;
    let pergunta = if estava_ativo {
        "Religar o serviço com a configuração nova?"
    } else {
        "Ligar o serviço agora (e a cada login)?"
    };
    if !tela.sim(pergunta)? {
        tela.diz("Para ligar depois: systemctl --user enable --now lukadispatch");
        return Ok(());
    }
    systemctl(&["daemon-reload"]);
    systemctl(&["enable", "lukadispatch"]);
    if !systemctl(&["restart", "lukadispatch"]) {
        bail!("o serviço não subiu; veja: journalctl --user -u lukadispatch -e");
    }
    // O socket de controle é a última coisa que o daemon abre: com ele no ar, token, grupo e
    // config passaram pela partida.
    for _ in 0..30 {
        if servico_ativo() && paths::socket().exists() {
            tela.diz("\nPronto. No grupo, abra o tópico General e mande /new para abrir a primeira sessão.");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("o serviço não ficou de pé em 15 s; veja: journalctl --user -u lukadispatch -e")
}
