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
use ld_daemon::agente::SemMemoria;
use ld_daemon::agente::claude_code::{ClaudeCode, Locais};
use ld_daemon::app::{App, Portas};
use ld_daemon::divisor::Divisores;
use ld_daemon::frontend::Frontend;
use ld_daemon::hub::Incoming;
use ld_daemon::sessions::Tmux;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// O frontend de verdade quando a feature `telegram` está ligada, e o nulo quando não está: este
/// teste não chama a API de nenhum dos dois (token e chat falsos, ou nenhum token nenhum), então
/// qual dos dois a feature deixa disponível não muda nada aqui. É o mesmo binário que o ci.sh
/// cobre com o passo "sem telegram".
#[cfg(feature = "telegram")]
fn frontend_de_teste() -> Arc<dyn Frontend> {
    Arc::new(ld_daemon::frontend::telegram::Telegram::new(
        "0:falso".into(),
        -100,
        vec![],
    ))
}

#[cfg(not(feature = "telegram"))]
fn frontend_de_teste() -> Arc<dyn Frontend> {
    Arc::new(ld_daemon::frontend::nulo::Nulo::default())
}

fn sessao_de_teste() -> Session {
    Session {
        session_id: "s1".into(),
        project: "proj".into(),
        cwd: "/tmp/proj".into(),
        transcript_path: None,
        hospedagem: Some("ld-proj-aaaa".into()),
        canal_id: Some("7".into()),
        status: "ocioso".into(),
        status_msg_id: None,
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
    let raiz_agente = dir.join("agente");
    let agente = ClaudeCode::new(
        Locais {
            cli: "/opt/ld/lukadispatch".into(),
            mcp_proxy: "/opt/ld/lukadispatch-mcp".into(),
            settings: raiz_agente.join("bot-settings.json"),
            claude_json: raiz_agente.join("claude.json"),
            claude_dir: raiz_agente.join("claude"),
            uso_db: raiz_agente.join("uso.db"),
        },
        None,
    );
    let app = Arc::new(App::new(
        Config::default(),
        store,
        Portas {
            frontend: frontend_de_teste(),
            agente: Arc::new(agente),
            memoria: Arc::new(SemMemoria),
            transcritor: None,
            divisores: Divisores::new(vec![]),
            hospedeiro: Arc::new(Tmux),
        },
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

/// Um pedido de uma linha e a resposta dele, como o CLI faz.
async fn pergunta_ao_daemon(dir: &std::path::Path, req: &Request) -> Response {
    let mut stream = UnixStream::connect(dir.join("ld.sock")).await.unwrap();
    pede(&mut stream, req).await;
    let mut linhas = BufReader::new(stream).lines();
    serde_json::from_str(&linhas.next_line().await.unwrap().unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn o_id_curto_do_ls_serve_para_mandar_e_para_matar() {
    // O `ls` mostra os 8 primeiros caracteres do id, e é isso que você digita. Antes, o `send`
    // guardava a mensagem sob um id inexistente ("sem monitor armado") e o `kill` respondia
    // sucesso sem matar nada.
    let dir = tempfile::tempdir().unwrap();
    let app = sobe_daemon(dir.path()).await;
    let id = "379a86dc-afb4-4b22-bc65-53045b4921dc";
    app.store
        .upsert(&Session {
            session_id: id.into(),
            // Sem hospedagem nem canal: o kill não tem tmux para matar nem tópico para apagar.
            hospedagem: None,
            canal_id: None,
            ..sessao_de_teste()
        })
        .unwrap();

    let mut stream = UnixStream::connect(dir.path().join("ld.sock"))
        .await
        .unwrap();
    pede(
        &mut stream,
        &Request::Listen {
            session_id: id.into(),
        },
    )
    .await;
    for _ in 0..50 {
        if app.hub.has_listener(id) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(app.hub.has_listener(id));
    let (leitura, _escrita) = stream.into_split();
    let mut linhas = BufReader::new(leitura).lines();

    let r = pergunta_ao_daemon(
        dir.path(),
        &Request::Inject {
            session_id: "379a86dc".into(),
            text: "quanto é 2+2?".into(),
        },
    )
    .await;
    assert!(matches!(r, Response::Ok), "{r:?}");
    let chegou: Response =
        serde_json::from_str(&linhas.next_line().await.unwrap().unwrap()).unwrap();
    assert!(
        matches!(&chegou, Response::Message { text, .. } if text == "quanto é 2+2?"),
        "{chegou:?}"
    );
    assert!(
        app.store.drain("379a86dc").unwrap().is_empty(),
        "nada pode ficar na fila de um id que não existe"
    );

    // Id que não é de sessão nenhuma é erro, não sucesso calado.
    let r = pergunta_ao_daemon(
        dir.path(),
        &Request::Kill {
            session_id: "ffff0000".into(),
        },
    )
    .await;
    assert!(
        matches!(&r, Response::Error { message } if message.contains("desconhecida")),
        "{r:?}"
    );

    // Curto demais não vale, nem quando só uma sessão começa assim.
    let r = pergunta_ao_daemon(
        dir.path(),
        &Request::Kill {
            session_id: "37".into(),
        },
    )
    .await;
    assert!(
        matches!(&r, Response::Error { message } if message.contains("curto")),
        "{r:?}"
    );
    assert!(app.store.get(id).unwrap().unwrap().ended_at.is_none());

    // E o kill pelo id curto mata mesmo.
    let r = pergunta_ao_daemon(
        dir.path(),
        &Request::Kill {
            session_id: "379a86dc".into(),
        },
    )
    .await;
    assert!(matches!(r, Response::Ok), "{r:?}");
    assert!(app.store.get(id).unwrap().unwrap().ended_at.is_some());
}
