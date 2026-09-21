//! O canal de entrada ponta a ponta, sem Telegram no meio: socket, fila e entrega ao vivo.
//!
//! É o teste que mais importa do projeto. Se ele passar, o caminho "mensagem chega no daemon ->
//! vira linha no stdout do `listen` -> vira evento no Monitor" está de pé; o que sobra do lado
//! do Telegram é transporte.

use std::sync::Arc;
use std::time::Duration;

use ld_core::config::Config;
use ld_core::proto::{Request, Response, line};
use ld_core::state::{Session, Store};
use ld_daemon::app::App;
use ld_daemon::hub::Incoming;
use ld_daemon::telegram::Tg;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn sessao_de_teste() -> Session {
    Session {
        session_id: "s1".into(),
        project: "proj".into(),
        cwd: "/tmp/proj".into(),
        transcript_path: None,
        tmux: Some("ld-proj-aaaa".into()),
        topic_id: Some(7),
        status: "ocioso".into(),
        status_message_id: None,
        model: Some("opus".into()),
        effort: None,
        permission_mode: Some("auto".into()),
        created_at: 0,
        ended_at: None,
    }
}

async fn sobe_daemon(dir: &std::path::Path) -> Arc<App> {
    let socket = dir.join("ld.sock");

    let store = Store::open_memory().unwrap();
    store.upsert(&sessao_de_teste()).unwrap();
    // Token e chat falsos: nada neste caminho chama a API do Telegram.
    let app = Arc::new(App::new(
        Config::default(),
        store,
        Tg::new("0:falso".into(), -100),
    ));

    let servidor = app.clone();
    let caminho = socket.clone();
    tokio::spawn(async move { ld_daemon::socket::serve(servidor, caminho).await });

    for _ in 0..100 {
        if socket.exists() {
            return app;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("o socket não apareceu");
}

async fn pede(stream: &mut UnixStream, req: &Request) {
    stream.write_all(line(req).as_bytes()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn fila_antiga_sai_antes_da_mensagem_nova() {
    let dir = tempfile::tempdir().unwrap();
    let app = sobe_daemon(dir.path()).await;

    // Chegou com a sessão surda: tem que ficar guardada.
    app.store
        .enqueue("s1", "mensagem da fila", "luka", &[])
        .unwrap();

    let mut stream = UnixStream::connect(dir.path().join("ld.sock"))
        .await
        .unwrap();
    pede(
        &mut stream,
        &Request::Listen {
            session_id: "s1".into(),
        },
    )
    .await;

    let (leitura, _escrita) = stream.into_split();
    let mut linhas = BufReader::new(leitura).lines();

    let primeira: Response =
        serde_json::from_str(&linhas.next_line().await.unwrap().unwrap()).unwrap();
    match primeira {
        Response::Message { text, .. } => assert_eq!(text, "mensagem da fila"),
        outra => panic!("esperava a mensagem guardada, veio {outra:?}"),
    }

    // Agora ao vivo: o daemon só entrega direto porque há um listener.
    for _ in 0..50 {
        if app.hub.has_listener("s1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        app.hub
            .deliver("s1", Incoming::texto("mensagem ao vivo", "luka", 0))
    );

    let segunda: Response =
        serde_json::from_str(&linhas.next_line().await.unwrap().unwrap()).unwrap();
    match segunda {
        Response::Message { text, .. } => assert_eq!(text, "mensagem ao vivo"),
        outra => panic!("esperava a mensagem ao vivo, veio {outra:?}"),
    }

    // A fila esvaziou ao ser entregue: reentregar a mesma mensagem seria pior que perdê-la.
    assert!(app.store.drain("s1").unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn mensagem_com_quebra_de_linha_continua_sendo_um_evento_so() {
    // O Monitor corta eventos por linha: um texto com \n não pode virar dois eventos.
    let dir = tempfile::tempdir().unwrap();
    let app = sobe_daemon(dir.path()).await;

    let mut stream = UnixStream::connect(dir.path().join("ld.sock"))
        .await
        .unwrap();
    pede(
        &mut stream,
        &Request::Listen {
            session_id: "s1".into(),
        },
    )
    .await;
    let (leitura, _e) = stream.into_split();
    let mut linhas = BufReader::new(leitura).lines();

    for _ in 0..50 {
        if app.hub.has_listener("s1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    app.hub
        .deliver("s1", Incoming::texto("primeira\nsegunda", "luka", 0));

    let bruta = linhas.next_line().await.unwrap().unwrap();
    let r: Response = serde_json::from_str(&bruta).unwrap();
    match r {
        Response::Message { text, .. } => assert_eq!(text, "primeira\nsegunda"),
        outra => panic!("veio {outra:?}"),
    }
}

/// O caminho do anexo tem que sobreviver à fila: o arquivo chega em disco antes de a sessão
/// estar ouvindo, e é só o caminho que espera no banco.
#[tokio::test(flavor = "multi_thread")]
async fn anexo_guardado_chega_com_o_caminho() {
    let dir = tempfile::tempdir().unwrap();
    let app = sobe_daemon(dir.path()).await;

    app.store
        .enqueue(
            "s1",
            "[arquivo recebido: /data/nota.pdf]",
            "luka",
            &["/data/nota.pdf".to_string()],
        )
        .unwrap();

    let mut stream = UnixStream::connect(dir.path().join("ld.sock"))
        .await
        .unwrap();
    pede(
        &mut stream,
        &Request::Listen {
            session_id: "s1".into(),
        },
    )
    .await;
    let (leitura, _e) = stream.into_split();
    let mut linhas = BufReader::new(leitura).lines();

    let r: Response = serde_json::from_str(&linhas.next_line().await.unwrap().unwrap()).unwrap();
    match r {
        Response::Message { text, files, .. } => {
            assert_eq!(files, vec!["/data/nota.pdf".to_string()]);
            assert!(
                text.contains("/data/nota.pdf"),
                "o texto também leva o caminho"
            );
        }
        outra => panic!("veio {outra:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_responde_pong() {
    let dir = tempfile::tempdir().unwrap();
    let _app = sobe_daemon(dir.path()).await;

    let mut stream = UnixStream::connect(dir.path().join("ld.sock"))
        .await
        .unwrap();
    pede(&mut stream, &Request::Ping).await;
    let (leitura, _e) = stream.into_split();
    let mut linhas = BufReader::new(leitura).lines();
    let r: Response = serde_json::from_str(&linhas.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(r, Response::Pong);
}

#[tokio::test(flavor = "multi_thread")]
async fn queda_do_listener_e_notada() {
    // É o que o hook Stop consulta para saber se precisa mandar re-armar o monitor.
    let dir = tempfile::tempdir().unwrap();
    let app = sobe_daemon(dir.path()).await;

    let mut stream = UnixStream::connect(dir.path().join("ld.sock"))
        .await
        .unwrap();
    pede(
        &mut stream,
        &Request::Listen {
            session_id: "s1".into(),
        },
    )
    .await;
    for _ in 0..50 {
        if app.hub.has_listener("s1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(app.hub.has_listener("s1"));

    drop(stream); // o Monitor expirou e matou o processo
    for _ in 0..100 {
        if !app.hub.has_listener("s1") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("o daemon não percebeu que o listener caiu");
}
