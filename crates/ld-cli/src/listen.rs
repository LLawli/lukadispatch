//! `lukadispatch listen`: o fluxo de entrada da sessão.
//!
//! Quem roda isto é a ferramenta Monitor, dentro do Claude. Cada linha impressa aqui vira um
//! evento na sessão, então três coisas são obrigatórias:
//!
//! 1. **Uma linha por mensagem.** O JSON que o daemon manda já é de uma linha só.
//! 2. **Flush a cada linha.** O stdout do Rust é bufferizado em bloco quando não é terminal, e
//!    aqui ele é um pipe: sem flush, a mensagem ficaria presa no buffer até o processo morrer,
//!    que é exatamente o contrário de tempo real.
//! 3. **Nada de ruído no stdout.** Aviso e erro vão para stderr, que o Monitor guarda no arquivo
//!    de saída sem transformar em evento.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use ld_core::paths;
use ld_core::proto::{Request, Response, line};

/// Quantas falhas seguidas de conexão antes de desistir. O daemon reiniciando leva uns
/// segundos; passar de meio minuto já é outra coisa, e aí é melhor o monitor morrer e o hook
/// Stop cobrar o re-arme do que ficar um processo fantasma pendurado.
const TENTATIVAS: u32 = 15;
const ESPERA: Duration = Duration::from_secs(2);

pub fn run(session: Option<&str>) -> i32 {
    let Some(sid) = session else {
        eprintln!("uso: lukadispatch listen --session <uuid>");
        return 2;
    };

    let mut falhas = 0;
    loop {
        match ouvir(sid) {
            // Substituído por um monitor mais novo: sair é o certo. Reconectar aqui derrubaria o
            // registro do novo e a sessão entraria num laço de re-armar.
            Ok(Fim::Substituido) => {
                eprintln!("outro monitor assumiu esta sessão; saindo");
                return 0;
            }
            Ok(Fim::ConexaoCaiu(mensagens)) => {
                // Conexão encerrada pelo daemon (restart, por exemplo). Reconectar é o certo:
                // perder o canal por causa de um `systemctl restart` deixaria a sessão surda.
                falhas = 0;
                eprintln!("conexão encerrada depois de {mensagens} mensagens; reconectando");
            }
            Err(e) => {
                falhas += 1;
                eprintln!("sem daemon ({e}); tentativa {falhas}/{TENTATIVAS}");
                if falhas >= TENTATIVAS {
                    eprintln!("desisti de falar com o daemon");
                    return 1;
                }
            }
        }
        std::thread::sleep(ESPERA);
    }
}

/// Por que o `ouvir` terminou.
enum Fim {
    /// O daemon fechou (restart, por exemplo). Vale reconectar.
    ConexaoCaiu(u64),
    /// Outro monitor assumiu o lugar deste. Não vale reconectar.
    Substituido,
}

fn ouvir(session_id: &str) -> std::io::Result<Fim> {
    let mut stream = UnixStream::connect(paths::socket())?;
    // Sem prazo de leitura: esta conexão existe justamente para ficar esperando.
    stream.set_read_timeout(None)?;
    stream.write_all(
        line(&Request::Listen {
            session_id: session_id.to_string(),
        })
        .as_bytes(),
    )?;
    stream.flush()?;

    let leitor = BufReader::new(stream);
    let mut saida = std::io::stdout().lock();
    let mut contador = 0;
    for linha in leitor.lines() {
        let linha = linha?;
        if linha.trim().is_empty() {
            continue;
        }
        // A despedida não é evento para a sessão: é ordem de sair.
        if let Ok(Response::Bye { .. }) = serde_json::from_str::<Response>(&linha) {
            return Ok(Fim::Substituido);
        }
        writeln!(saida, "{linha}")?;
        saida.flush()?;
        contador += 1;
    }
    Ok(Fim::ConexaoCaiu(contador))
}
