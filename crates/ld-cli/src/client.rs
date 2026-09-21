//! Cliente do socket de controle, síncrono.
//!
//! Regra de ouro do projeto mora aqui: **nada disto pode travar o Claude**. Toda chamada tem
//! prazo, e qualquer falha (daemon fora, socket velho, resposta estranha) vira `None`. Quem
//! chama trata `None` como "não decidi nada" e sai com 0.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use ld_core::paths;
use ld_core::proto::{Request, Response, line};

/// Prazo das chamadas de telemetria. Elas rodam como hook `async`, então nem seriam esperadas
/// pelo Claude; o teto existe para o processo não ficar pendurado à toa.
pub const PRAZO_CURTO: Duration = Duration::from_millis(1500);

/// Comandos que você digita no PC podem esperar um pouco: quem está na frente vê o terminal
/// parado e entende.
pub const PRAZO_LOCAL: Duration = Duration::from_secs(5);

/// Subir arquivo para o Telegram depende da rede e do tamanho (o teto é 50 MB), então este
/// prazo é o do upload, não o de uma conversa com o daemon.
pub const PRAZO_ENVIO: Duration = Duration::from_secs(180);

/// Abrir sessão envolve criar tópico no Telegram e subir tmux: é a operação mais lenta do
/// sistema, e falhar por impaciência deixaria tópico órfão.
pub const PRAZO_NEW: Duration = Duration::from_secs(90);

pub fn connect(prazo: Duration) -> Option<UnixStream> {
    let stream = UnixStream::connect(paths::socket()).ok()?;
    stream.set_read_timeout(Some(prazo)).ok()?;
    stream.set_write_timeout(Some(prazo)).ok()?;
    Some(stream)
}

/// Manda uma requisição e lê uma resposta. `None` em qualquer problema.
pub fn call(req: &Request, prazo: Duration) -> Option<Response> {
    let stream = match connect(prazo) {
        Some(s) => s,
        None => {
            autostart();
            return None;
        }
    };
    call_on(stream, req, prazo)
}

pub fn call_on(mut stream: UnixStream, req: &Request, _prazo: Duration) -> Option<Response> {
    stream.write_all(line(req).as_bytes()).ok()?;
    stream.flush().ok()?;
    let mut leitor = BufReader::new(stream);
    let mut resposta = String::new();
    leitor.read_line(&mut resposta).ok()?;
    serde_json::from_str(resposta.trim()).ok()
}

/// Dispara e esquece: manda a requisição sem esperar resposta. Usado pela telemetria, onde
/// esperar não muda nada.
pub fn send(req: &Request) {
    let Some(mut stream) = connect(PRAZO_CURTO) else {
        autostart();
        return;
    };
    let _ = stream.write_all(line(req).as_bytes());
    let _ = stream.flush();
}

/// Sobe o daemon quando o socket não responde.
///
/// É a sonda do socket fazendo o papel da sonda de porta do sdispath: se dá para conectar, o
/// daemon está de pé e não há nada a fazer; se não dá, pede ao systemd. Desanexado e sem
/// esperar, porque quem chama é um hook. Uma sessão do Claude nunca deve parar por isto, então
/// falha aqui é silenciosa por construção.
fn autostart() {
    if std::env::var_os("LUKADISPATCH_NO_AUTOSTART").is_some() {
        return;
    }
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "--no-block", "start", "lukadispatch.service"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
