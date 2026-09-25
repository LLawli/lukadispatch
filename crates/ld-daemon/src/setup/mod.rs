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
pub mod transcricao;

#[cfg(test)]
mod testes;
#[cfg(test)]
mod testes_transcricao;

use std::io::IsTerminal;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ld_core::config::Config;
use ld_core::paths;

use arquivos::Rascunho;
use pecas::Peca;
use tela::Tela;

pub const AJUDA: &str = "\
lukadispatch setup: configura o lukadispatch nesta máquina, conversando.

  --frontend <nome>   o aplicativo de chat           (padrão: telegram)
  --agent <nome>      o agente de código das sessões  (padrão: claude-code)
  --session <nome>    onde cada sessão roda           (padrão: tmux; herdr sem tmux)
  --envelope <nome>   o que envolve o agente          (padrão: ai-memory; ou nenhum)
  --refazer           pergunta de novo o que já está resolvido (trocar de bot, grupo, motor)

Rodar de novo é seguro: o que está resolvido só é conferido, e só o que falta é perguntado.
Sem flag, cada peça fica a que o config já tem; os padrões valem para config novo.";

/// Qual implementação de cada porta o setup configura. `None` é "a do config, ou a padrão".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Selecao {
    pub frontend: Option<String>,
    pub agente: Option<String>,
    pub hospedeiro: Option<String>,
    pub envelope: Option<String>,
    /// Pergunta de novo o que já está resolvido.
    pub refazer: bool,
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
        match arg.as_str() {
            "-h" | "--help" | "ajuda" => return Ok(Pedido::Ajuda),
            "--refazer" => {
                sel.refazer = true;
                continue;
            }
            _ => {}
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
        *campo = Some(valor);
    }
    Ok(Pedido::Setup(sel))
}

/// As peças da seleção, na ordem da conversa. Nome desconhecido falha aqui, antes de qualquer
/// pergunta.
///
/// Peça sem flag é a do config atual: rodar o setup de novo não pode desfazer uma escolha. Sem
/// config, é o padrão do `Config`, menos o hospedeiro, que é o que a máquina tem (o tmux, senão
/// o herdr). Os pré-requisitos locais vêm antes do frontend: é melhor saber que falta o tmux
/// antes de criar um bot do que depois.
pub fn pecas(
    sel: &Selecao,
    atual: Option<&Config>,
    tem_programa: &dyn Fn(&str) -> bool,
) -> Result<Vec<Box<dyn Peca>>> {
    let padrao = Config {
        hospedeiro: pecas::hospedeiro_da_maquina(tem_programa).into(),
        ..Config::default()
    };
    let base = atual.unwrap_or(&padrao);
    let nome = |flag: &Option<String>, doc: &str| flag.clone().unwrap_or_else(|| doc.to_string());
    Ok(vec![
        pecas::hospedeiro(&nome(&sel.hospedeiro, &base.hospedeiro))?,
        pecas::agente(&nome(&sel.agente, &base.agente.tipo))?,
        pecas::envelope(&nome(&sel.envelope, &base.agente.envelope))?,
        pecas::frontend(&nome(&sel.frontend, &base.frontend))?,
    ])
}

/// O serviço foi parado pelo setup (para escutar o bot) e ainda não voltou. É estático porque o
/// Ctrl+C, noutra thread, precisa saber se tem de religá-lo.
static PAUSADO: AtomicBool = AtomicBool::new(false);

/// O setup de verdade, no terminal.
pub async fn roda(sel: Selecao) -> Result<()> {
    let home = paths::home();
    let arquivo_config = paths::config_file();
    // Fixo em ~/.config, e não em $XDG_CONFIG_HOME: é o EnvironmentFile da unit (%h/.config).
    let arquivo_env = home.join(".config/lukadispatch/.env");

    let config = std::fs::read_to_string(&arquivo_config).ok();
    let env = std::fs::read_to_string(&arquivo_env).ok();
    let mut r = Rascunho::de(config.as_deref(), env.as_deref())?;
    let pecas = pecas(&sel, r.havia_config.then_some(&r.atual), &*r.tem_programa)?;
    r.refazer = sel.refazer;
    // O daemon e o setup escutando o mesmo bot brigam: o Telegram entrega cada update a um só, e
    // o outro recebe 409. Por isso o serviço para, mas só quando o setup precisa escutar.
    r.pausa_servico = Box::new(|| {
        if PAUSADO.load(Ordering::SeqCst) || !servico_ativo() {
            return false;
        }
        systemctl(&["stop", "lukadispatch"]);
        PAUSADO.store(true, Ordering::SeqCst);
        true
    });

    let stdin = std::io::stdin();
    let terminal = stdin.is_terminal();
    let mut entrada = stdin.lock();
    let mut saida = std::io::stdout();
    let mut tela = Tela::nova(&mut entrada, &mut saida, terminal);

    tela.diz(
        "Configurando o lukadispatch. Até o passo \"Gravando\", Ctrl+C cancela sem mudar nada.",
    );
    // Cancelar não pode deixar estrago: o serviço volta se o setup o parou, e o eco do terminal
    // volta caso o Ctrl+C tenha vindo no meio da leitura do token. Roda noutra thread do runtime,
    // porque a conversa bloqueia esta lendo o terminal.
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            if let Ok(tty) = std::fs::File::open("/dev/tty") {
                let _ = std::process::Command::new("stty")
                    .arg("echo")
                    .stdin(tty)
                    .status();
            }
            if PAUSADO.load(Ordering::SeqCst) {
                systemctl(&["start", "lukadispatch"]);
            }
            eprintln!("\nSetup cancelado.");
            std::process::exit(130);
        }
    });

    if let Err(e) = conduz(&mut tela, &mut r, &pecas, &home).await {
        if PAUSADO.load(Ordering::SeqCst) {
            systemctl(&["start", "lukadispatch"]);
        }
        return Err(e);
    }

    let (config_novo, env_novo) = (r.config_texto(), r.env_texto());
    let mudou = config.as_deref() != Some(&config_novo) || env.as_deref() != Some(&env_novo);
    if mudou {
        tela.passo("Gravando");
        arquivos::grava(&arquivo_config, &config_novo, 0o644)?;
        tela.diz(&format!("{}", arquivo_config.display()));
        arquivos::grava(&arquivo_env, &env_novo, 0o600)?;
        tela.diz(&format!("{} (só você lê)", arquivo_env.display()));
    }

    for p in &pecas {
        p.ativa(&mut tela, &r)?;
    }
    servico(&mut tela, &home, mudou).await
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
    preferencias(tela, r, home)?;
    transcricao::configura(tela, r, home, &transcricao::base_dos_pacotes()).await
}

fn preferencias(tela: &mut Tela<'_>, r: &mut Rascunho, home: &Path) -> Result<()> {
    // Com config, nome e modo já são escolhas feitas (nome ausente também: é não querer nome).
    // Só as pastas podem estar faltando de verdade.
    let tudo = !r.havia_config || r.refazer;
    if !tudo && !r.atual.scan.roots.is_empty() {
        return Ok(());
    }
    tela.passo("Preferências");
    if tudo {
        pergunta_nome(tela, r)?;
    }
    pergunta_raizes(tela, r, home)?;
    if tudo {
        pergunta_modo(tela, r)?;
    }
    Ok(())
}

fn pergunta_nome(tela: &mut Tela<'_>, r: &mut Rascunho) -> Result<()> {
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
    Ok(())
}

fn pergunta_raizes(tela: &mut Tela<'_>, r: &mut Rascunho, home: &Path) -> Result<()> {
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
    Ok(())
}

fn pergunta_modo(tela: &mut Tela<'_>, r: &mut Rascunho) -> Result<()> {
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

    Ok(())
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

/// A unit do systemd para o daemon em `daemon`. Com o daemon em `~/.local/bin` ela sai igual à
/// do pacote (com `%h`); em outro lugar, o `ExecStart` leva o caminho dele.
pub fn unit_para(daemon: &Path, home: &Path) -> String {
    let modelo = include_str!("../../../../dist/lukadispatch.service");
    if daemon == home.join(".local/bin/lukadispatchd") {
        return modelo.to_string();
    }
    modelo
        .lines()
        .map(|l| {
            if l.starts_with("ExecStart=") {
                format!("ExecStart={}", daemon.display())
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// A unit existente precisa ser refeita: o `ExecStart` não leva a um arquivo, ou leva a outro
/// que não o daemon deste setup. Dois caminhos até o mesmo arquivo (um symlink do brew) valem.
pub fn unit_pendente(texto: &str, daemon: &Path, home: &Path) -> bool {
    let Some(exec) = texto
        .lines()
        .find_map(|l| l.trim().strip_prefix("ExecStart="))
    else {
        return true;
    };
    let exec = exec
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .replace("%h", &home.to_string_lossy());
    match (std::fs::canonicalize(&exec), std::fs::canonicalize(daemon)) {
        (Ok(a), Ok(b)) => a != b,
        _ => true,
    }
}

/// A unit do systemd, se faltar ou apontar para um daemon que não existe mais (o `brew upgrade`
/// apaga a pasta da versão anterior). Devolve se escreveu.
fn garante_unit(tela: &mut Tela<'_>, home: &Path) -> Result<bool> {
    let unit = home.join(".config/systemd/user/lukadispatch.service");
    let daemon = std::path::PathBuf::from(paths::daemon());
    match std::fs::read_to_string(&unit) {
        Ok(t) if !unit_pendente(&t, &daemon, home) => return Ok(false),
        Ok(_) => tela.diz("A unit apontava para um lukadispatchd que não é este; refazendo."),
        Err(_) => {}
    }
    arquivos::grava(&unit, &unit_para(&daemon, home), 0o644)?;
    tela.diz(&format!("{}", unit.display()));
    Ok(true)
}

/// Liga, religa ou só confere o serviço. `mudou` é se o config ou o `.env` foram regravados.
async fn servico(tela: &mut Tela<'_>, home: &Path, mudou: bool) -> Result<()> {
    tela.passo("O serviço");
    if !systemctl(&["show-environment"]) {
        tela.diz("Não há systemd de usuário aqui. Para rodar o daemon à mão: lukadispatchd");
        return Ok(());
    }
    let unit_nova = garante_unit(tela, home)?;
    let pausado = PAUSADO.load(Ordering::SeqCst);
    let ativo = servico_ativo();
    if ativo && !mudou && !unit_nova {
        tela.diz("Rodando, e nada mudou: continua como está.");
        return Ok(());
    }
    // Parado pelo próprio setup, volta sem pergunta: foi o setup que o tirou do ar.
    if !pausado {
        let pergunta = if ativo {
            "Religar o serviço com a configuração nova?"
        } else {
            "Ligar o serviço agora (e a cada login)?"
        };
        if !tela.sim(pergunta)? {
            tela.diz("Para ligar depois: systemctl --user enable --now lukadispatch");
            return Ok(());
        }
    }
    systemctl(&["daemon-reload"]);
    systemctl(&["enable", "lukadispatch"]);
    PAUSADO.store(false, Ordering::SeqCst);
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
