//! A corrida: a mesma pergunta no Telegram e numa janela do PC, vale quem responder primeiro.
//!
//! Mora no crate comum porque dois binários precisam dela: o hook de pergunta (`lukadispatch
//! hook ask`) e o proxy de MCP (`lukadispatch-mcp`), que intercepta o diálogo próprio de um
//! servidor MCP. Os dois abrem exatamente o mesmo card e a mesma janela.
//!
//! Quem organiza a disputa é sempre um processo que roda DENTRO da sessão, com o ambiente
//! gráfico dela: um serviço systemd pode nem ter `WAYLAND_DISPLAY`.
//!
//! Qualquer falha em qualquer ponto vira "não decidi", e cabe a quem chamou escolher o caminho
//! seguro dali em diante.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ask::{Answer, Ask};
use crate::paths;
use crate::proto::{Request, Response, line};

/// Quem ganhou a corrida.
pub enum Vencedor {
    /// Veio do daemon (Telegram), já traduzido.
    Telegram(Response),
    /// Veio da janela do PC.
    Janela(Answer),
}

/// Abre os dois canais e devolve o primeiro que responder.
pub fn disputar(req: Request, pergunta: Ask, prazo: Duration) -> Option<Vencedor> {
    let (tx, rx) = mpsc::channel::<Vencedor>();
    let ask_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let pid_janela: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));

    {
        let tx = tx.clone();
        let ask_id = ask_id.clone();
        std::thread::spawn(move || {
            if let Some(r) = fala_com_daemon(req, ask_id) {
                let _ = tx.send(Vencedor::Telegram(r));
            }
        });
    }
    {
        let pid_janela = pid_janela.clone();
        std::thread::spawn(move || {
            if let Some(a) = abre_janela(pergunta, pid_janela) {
                let _ = tx.send(Vencedor::Janela(a));
            }
        });
    }

    let vencedor = rx.recv_timeout(prazo).ok()?;

    match &vencedor {
        // O Telegram ganhou: fecha a janela, senão ela fica lá pedindo uma resposta que já foi
        // dada.
        Vencedor::Telegram(_) => {
            if let Some(pid) = *pid_janela.lock().unwrap_or_else(|e| e.into_inner()) {
                mata(pid);
            }
        }
        // A janela ganhou: o daemon precisa saber para apagar o card do celular.
        Vencedor::Janela(a) => {
            let id = ask_id.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(ask_id) = id {
                let payload = serde_json::to_string(a).unwrap_or_default();
                let _ = responde_local(
                    &Request::LocalAnswer {
                        ask_id,
                        answer: payload,
                    },
                    Duration::from_secs(5),
                );
            }
        }
    }
    Some(vencedor)
}

/// Avisa o daemon de que a janela ganhou, para o card do celular sumir.
fn responde_local(req: &Request, prazo: Duration) -> Option<Response> {
    let mut stream = UnixStream::connect(paths::socket()).ok()?;
    stream.set_read_timeout(Some(prazo)).ok()?;
    stream.set_write_timeout(Some(prazo)).ok()?;
    stream.write_all(line(req).as_bytes()).ok()?;
    stream.flush().ok()?;
    let mut leitor = BufReader::new(stream);
    let mut resposta = String::new();
    leitor.read_line(&mut resposta).ok()?;
    serde_json::from_str(resposta.trim()).ok()
}

/// Conversa longa com o daemon: primeira linha traz o id do card, segunda traz a resposta.
fn fala_com_daemon(req: Request, ask_id: Arc<Mutex<Option<String>>>) -> Option<Response> {
    let mut stream = UnixStream::connect(paths::socket()).ok()?;
    // Sem prazo de leitura: a espera pode durar horas, e é isso mesmo.
    stream.set_read_timeout(None).ok()?;
    stream.write_all(line(&req).as_bytes()).ok()?;
    stream.flush().ok()?;

    let mut linhas = BufReader::new(stream).lines();
    let primeira: Response = serde_json::from_str(linhas.next()?.ok()?.trim()).ok()?;
    match primeira {
        Response::AskOpened { ask_id: id } => {
            *ask_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
        }
        // O daemon já respondeu de cara (sessão sem tópico, por exemplo): não há card nenhum.
        outra => return Some(outra),
    }
    serde_json::from_str(linhas.next()?.ok()?.trim()).ok()
}

/// Abre a janela do PC e espera a resposta dela.
fn abre_janela(pergunta: Ask, pid: Arc<Mutex<Option<u32>>>) -> Option<Answer> {
    let exe = caminho_da_janela()?;
    let mut filho = std::process::Command::new(exe)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    *pid.lock().unwrap_or_else(|e| e.into_inner()) = Some(filho.id());

    let entrada = serde_json::to_string(&pergunta).ok()?;
    filho.stdin.take()?.write_all(entrada.as_bytes()).ok()?;

    let saida = filho.wait_with_output().ok()?;
    if !saida.status.success() {
        // Janela fechada sem responder: quem decide agora é o celular.
        return None;
    }
    serde_json::from_slice(&saida.stdout).ok()
}

/// A janela fica ao lado deste binário; só depois disso vale tentar o PATH.
fn caminho_da_janela() -> Option<PathBuf> {
    Some(PathBuf::from(crate::paths::janela()))
}

/// Mata o processo da janela.
///
/// Via `kill(1)` mesmo: a alternativa seria uma dependência de libc só para uma chamada, e este
/// binário roda a cada ferramenta do Claude. O `filho` está preso no `wait_with_output` de outra
/// thread, então não dá para usar o `Child` daqui.
fn mata(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}
