//! `lukadispatch`: o binário que os hooks chamam, o que o Monitor lê e o que você usa no PC.
//!
//! Três papéis num executável só para o hook não ter que achar três caminhos diferentes, e para
//! a instalação ser um arquivo. O parser de argumentos é feito na mão: são seis subcomandos, e
//! uma dependência de linha de comando aqui pesaria em cada chamada de ferramenta do Claude.

mod ask;
mod client;
mod hook;
mod listen;

use std::process::ExitCode;

use ld_core::paths;
use ld_core::proto::{Request, Response};

const USO: &str = "\
lukadispatch

  listen --session <uuid>   fluxo de mensagens do chat (é o que o Monitor da sessão lê)
  hook [agente] <evento>    ponte dos ganchos do agente (lê o evento no stdin); sem agente,
                            é o Claude Code, o único por enquanto (`hook claude <evento>`)
  ls                        sessões vivas
  models                    catálogo de modelos lido do binário do Claude Code
  kill <id>                 fecha uma sessão
  new <projeto> [--continuar]   abre uma sessão (--continuar retoma a última conversa)
  send <id> <texto>         entrega uma mensagem a uma sessão sem passar pelo Telegram
  send-file <caminho> [--legenda <texto>] [--como-arquivo] [--session <id>]
                            manda um arquivo do disco para o tópico da sessão (a sessão em si
                            não precisa do --session: ela já tem LD_SESSION no ambiente)
  model <id> <modelo>       troca o modelo reiniciando com o contexto inteiro
  effort <id> <nível>       idem para o esforço (low, medium, high, xhigh, max)
  setup [--frontend telegram] [--agent claude-code] [--session tmux] [--envelope ai-memory]
                            configura tudo conversando: o bot, o grupo, o config, os hooks e o
                            serviço (os padrões são os que existem hoje; --help lista)
  install [--global]        escreve os hooks; --global acrescenta a telemetria ao settings do
                            Claude Code, para as suas sessões de terminal entrarem no painel
  uninstall                 remove os hooks do settings do Claude Code
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("");

    let codigo = match cmd {
        "listen" => listen::run(valor(&args, "--session").as_deref()),
        "hook" => hook::run(args.get(1..).unwrap_or_default()),
        "ls" => ls(),
        "models" | "modelos" => modelos(),
        "kill" => kill(args.get(1).map(String::as_str)),
        "send" => send(
            args.get(1).map(String::as_str),
            args.get(2..).map(|r| r.join(" ")).unwrap_or_default(),
        ),
        "send-file" | "sendfile" | "enviar" => send_file(&args),
        "model" | "effort" => trocar(
            cmd,
            args.get(1).map(String::as_str),
            args.get(2).map(String::as_str),
        ),
        "new" => {
            let continuar = args.iter().any(|a| a == "--continuar");
            let nome: Vec<String> = args[1..]
                .iter()
                .filter(|a| *a != "--continuar")
                .cloned()
                .collect();
            new(nome.join(" "), continuar)
        }
        "setup" => setup(&args[1..]),
        "install" => install(args.iter().any(|a| a == "--global")),
        "uninstall" => uninstall(),
        "-h" | "--help" | "help" | "" => {
            print!("{USO}");
            0
        }
        outro => {
            eprintln!("subcomando desconhecido: {outro}\n\n{USO}");
            2
        }
    };
    ExitCode::from(codigo as u8)
}

fn valor(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn ls() -> i32 {
    match client::call(&Request::ListSessions, client::PRAZO_LOCAL) {
        Some(Response::Sessions { sessions }) if sessions.is_empty() => {
            println!("nenhuma sessão viva");
            0
        }
        Some(Response::Sessions { sessions }) => {
            for x in sessions {
                let ctx = match (x.context_tokens, x.context_limit) {
                    (Some(t), Some(l)) => format!("{}k/{}k", t / 1000, l / 1000),
                    _ => "-".into(),
                };
                let dono = if x.owned_by_bot { "bot" } else { "pc " };
                println!(
                    "{dono}  {:8}  {:<20} {:<10} {ctx}",
                    &x.session_id[..8.min(x.session_id.len())],
                    x.project,
                    x.status
                );
            }
            0
        }
        _ => {
            eprintln!("daemon não respondeu");
            1
        }
    }
}

/// Mostra o catálogo extraído do binário do Claude Code, para conferir o que o bot vai oferecer.
fn modelos() -> i32 {
    let Some(bin) = ld_core::models::claude_binary() else {
        eprintln!("não achei o binário `claude` no PATH");
        return 1;
    };
    let catalogo = ld_core::models::catalog(&bin);
    if catalogo.is_empty() {
        eprintln!("nenhum modelo encontrado em {}", bin.display());
        return 1;
    }
    println!("{} ({} modelos)", bin.display(), catalogo.len());
    for (familia, modelos) in ld_core::models::por_familia(&catalogo) {
        println!("\n{familia}:");
        for m in modelos {
            println!("  {:<22} {}", m.id, m.rotulo());
        }
    }
    0
}

fn kill(id: Option<&str>) -> i32 {
    let Some(id) = id else {
        eprintln!("uso: lukadispatch kill <id>");
        return 2;
    };
    match client::call(
        &Request::Kill {
            session_id: id.to_string(),
        },
        client::PRAZO_LOCAL,
    ) {
        Some(Response::Ok) => 0,
        Some(Response::Error { message }) => {
            eprintln!("{message}");
            1
        }
        _ => {
            eprintln!("daemon não respondeu");
            1
        }
    }
}

fn new(projeto: String, continuar: bool) -> i32 {
    if projeto.is_empty() {
        eprintln!("uso: lukadispatch new <projeto>");
        return 2;
    }
    match client::call(
        &Request::NewSession {
            project: projeto,
            resume_last: continuar,
        },
        client::PRAZO_NEW,
    ) {
        Some(Response::Ok) => 0,
        Some(Response::Error { message }) => {
            eprintln!("{message}");
            1
        }
        _ => {
            eprintln!("daemon não respondeu");
            1
        }
    }
}

fn send(id: Option<&str>, texto: String) -> i32 {
    let (Some(id), false) = (id, texto.is_empty()) else {
        eprintln!("uso: lukadispatch send <id> <texto>");
        return 2;
    };
    match client::call(
        &Request::Inject {
            session_id: id.to_string(),
            text: texto,
        },
        client::PRAZO_LOCAL,
    ) {
        Some(Response::Ok) => 0,
        Some(Response::Error { message }) => {
            eprintln!("{message}");
            1
        }
        _ => {
            eprintln!("daemon não respondeu");
            1
        }
    }
}

/// Devolve um arquivo pelo Telegram, do tópico da própria sessão.
///
/// É o sentido contrário do anexo que chega: quem chama é o agente, com um caminho que ele
/// acabou de produzir. A sessão não precisa dizer quem é, porque o `LD_SESSION` já está no
/// ambiente do tmux dela; o `--session` existe para você mandar do seu terminal.
fn send_file(args: &[String]) -> i32 {
    let legenda = valor(args, "--legenda").or_else(|| valor(args, "--caption"));
    let como_arquivo = args
        .iter()
        .any(|a| a == "--como-arquivo" || a == "--as-file");
    let sessao = valor(args, "--session").or_else(|| {
        std::env::var("LD_SESSION")
            .ok()
            .filter(|v| !v.trim().is_empty())
    });

    // Sobra tudo o que não é flag nem valor de flag: é o caminho.
    let mut caminho = None;
    let mut pular = false;
    for a in args.iter().skip(1) {
        if pular {
            pular = false;
            continue;
        }
        if a.starts_with("--") {
            pular = matches!(a.as_str(), "--legenda" | "--caption" | "--session");
            continue;
        }
        if caminho.is_none() {
            caminho = Some(a.clone());
        }
    }

    let (Some(caminho), Some(sessao)) = (caminho, sessao) else {
        eprintln!(
            "uso: lukadispatch send-file <caminho> [--legenda <texto>] [--como-arquivo] [--session <id>]"
        );
        return 2;
    };

    // O daemon abre o arquivo pelo caminho que receber, e o diretório dele não é o seu: caminho
    // relativo sem isto viraria "não achei" num lugar que existe.
    let absoluto = std::fs::canonicalize(&caminho)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(caminho);

    match client::call(
        &Request::SendFile {
            session_id: sessao,
            path: absoluto,
            caption: legenda,
            como_arquivo,
        },
        client::PRAZO_ENVIO,
    ) {
        Some(Response::Done { detail }) => {
            println!("{detail}");
            0
        }
        Some(Response::Ok) => 0,
        Some(Response::Error { message }) => {
            eprintln!("{message}");
            1
        }
        _ => {
            eprintln!("daemon não respondeu");
            1
        }
    }
}

/// Troca modelo ou esforço de uma sessão viva.
///
/// `/model` e `/effort` são comandos do frontend do Claude Code: nada nem ninguém consegue
/// dispará-los por evento. O daemon reinicia a sessão com `--resume`, que volta com o mesmo
/// transcript, então na prática a conversa não sente.
fn trocar(qual: &str, id: Option<&str>, valor: Option<&str>) -> i32 {
    let (Some(id), Some(valor)) = (id, valor) else {
        eprintln!("uso: lukadispatch {qual} <id> <valor>");
        return 2;
    };
    let (model, effort) = match qual {
        "model" => (Some(valor.to_string()), None),
        _ => (None, Some(valor.to_string())),
    };
    match client::call(
        &Request::Relaunch {
            session_id: id.to_string(),
            model,
            effort,
        },
        client::PRAZO_NEW,
    ) {
        Some(Response::Ok) => 0,
        Some(Response::Error { message }) => {
            eprintln!("{message}");
            1
        }
        _ => {
            eprintln!("daemon não respondeu");
            1
        }
    }
}

/// Escreve o settings das sessões do bot e, com `--global`, acrescenta a telemetria ao settings
/// do Claude Code do usuário.
///
/// A separação é o ponto: o global leva **só** telemetria. Uma sessão sua de terminal entra no
/// painel, mas continua com o menu nativo de pergunta e o fluxo de permissão normal. Sequestrar
/// isso numa sessão em que você já está na frente do teclado seria pior que não ter painel.
/// O setup mora no daemon, que é quem conhece o Telegram. Aqui só se troca de processo, para o
/// terminal (e o Ctrl+C) passar direto para ele.
fn setup(args: &[String]) -> i32 {
    use std::os::unix::process::CommandExt;

    let daemon = paths::daemon();
    let erro = std::process::Command::new(&daemon)
        .arg("setup")
        .args(args)
        .exec();
    eprintln!("não consegui rodar {daemon} setup: {erro}");
    1
}

fn install(global: bool) -> i32 {
    let cli = paths::cli();

    let destino = paths::bot_settings_file();
    if let Some(pai) = destino.parent()
        && let Err(e) = std::fs::create_dir_all(pai)
    {
        eprintln!("não consegui criar {}: {e}", pai.display());
        return 1;
    }
    let conteudo = serde_json::to_string_pretty(&ld_core::hooks::bot_settings(&cli))
        .expect("settings sempre serializa");
    if let Err(e) = std::fs::write(&destino, conteudo) {
        eprintln!("não consegui escrever {}: {e}", destino.display());
        return 1;
    }
    println!("settings das sessões: {}", destino.display());

    if global {
        let alvo = paths::claude_settings();
        let mut settings = ler_json(&alvo);
        ld_core::hooks::merge_into(&mut settings, &ld_core::hooks::telemetry_hooks(&cli));
        if let Err(e) = escrever_json(&alvo, &settings) {
            eprintln!("não consegui escrever {}: {e}", alvo.display());
            return 1;
        }
        println!("telemetria instalada em {}", alvo.display());
        println!("(vale a partir da próxima sessão do Claude Code)");
    }
    0
}

fn uninstall() -> i32 {
    let alvo = paths::claude_settings();
    let mut settings = ler_json(&alvo);
    ld_core::hooks::strip(&mut settings);
    if let Err(e) = escrever_json(&alvo, &settings) {
        eprintln!("não consegui escrever {}: {e}", alvo.display());
        return 1;
    }
    println!("hooks removidos de {}", alvo.display());
    0
}

fn ler_json(caminho: &std::path::Path) -> serde_json::Value {
    std::fs::read_to_string(caminho)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Escreve com backup do que estava lá.
///
/// O settings do Claude Code é arquivo do usuário, com coisas que não são nossas. Se um bug
/// nosso o corromper, o `.bak` é a diferença entre "restaura" e "reconfigura tudo de novo".
fn escrever_json(caminho: &std::path::Path, v: &serde_json::Value) -> std::io::Result<()> {
    if let Some(pai) = caminho.parent() {
        std::fs::create_dir_all(pai)?;
    }
    if caminho.exists() {
        let _ = std::fs::copy(caminho, caminho.with_extension("json.bak"));
    }
    std::fs::write(caminho, format!("{}\n", serde_json::to_string_pretty(v)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valor_le_a_flag_seguinte() {
        let args: Vec<String> = ["listen", "--session", "abc"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(valor(&args, "--session").as_deref(), Some("abc"));
        assert!(valor(&args, "--outra").is_none());
    }

    #[test]
    fn flag_sem_valor_nao_explode() {
        let args: Vec<String> = ["listen", "--session"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(valor(&args, "--session").is_none());
    }
}
