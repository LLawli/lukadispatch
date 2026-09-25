//! Ids de canal e de mensagem opacos no banco.
//!
//! O banco guardava `topic_id` e `status_message_id` como inteiro, porque eram ids do Telegram.
//! Com o frontend trocável, eles viram texto que só o adaptador interpreta (um grupo do WhatsApp
//! é `120363...@g.us`). O banco que já existe na máquina do Luka não pode quebrar por isso: as
//! colunas continuam com o mesmo nome, e o valor inteiro que já está lá é lido como texto.

use rusqlite::Connection;

use super::*;

fn sessao(id: &str, canal: Option<&str>) -> Session {
    Session {
        session_id: id.into(),
        project: "proj".into(),
        cwd: format!("/tmp/{id}"),
        transcript_path: None,
        hospedagem: Some(format!("ld-proj-{id}")),
        canal_id: canal.map(str::to_string),
        status: "ocioso".into(),
        status_msg_id: None,
        model: None,
        effort: None,
        permission_mode: None,
        created_at: 0,
        ended_at: None,
    }
}

#[test]
fn canal_de_qualquer_frontend_vai_e_volta_igual() {
    let st = Store::open_memory().unwrap();
    st.upsert(&sessao("tg", Some("630"))).unwrap();
    st.upsert(&sessao("wa", Some("120363025246125486@g.us")))
        .unwrap();

    assert_eq!(
        st.get("tg").unwrap().unwrap().canal_id.as_deref(),
        Some("630")
    );
    assert_eq!(
        st.by_canal("120363025246125486@g.us")
            .unwrap()
            .unwrap()
            .session_id,
        "wa"
    );
    assert_eq!(st.by_canal("630").unwrap().unwrap().session_id, "tg");
    assert!(st.by_canal("631").unwrap().is_none());
}

#[test]
fn id_da_mensagem_de_status_e_texto() {
    let st = Store::open_memory().unwrap();
    st.upsert(&sessao("s1", Some("7"))).unwrap();
    st.set_status_msg("s1", Some("3EB0C767D82A1B5E9B2F"))
        .unwrap();
    assert_eq!(
        st.get("s1").unwrap().unwrap().status_msg_id.as_deref(),
        Some("3EB0C767D82A1B5E9B2F")
    );
    st.set_status_msg("s1", None).unwrap();
    assert!(st.get("s1").unwrap().unwrap().status_msg_id.is_none());
}

#[test]
fn canal_vazado_volta_como_texto_ate_ser_limpo() {
    let st = Store::open_memory().unwrap();
    st.upsert(&sessao("s2", Some("nulo-abc"))).unwrap();
    assert!(
        st.canais_vazados().unwrap().is_empty(),
        "sessão viva não vaza"
    );
    // Encerrar não tira o canal: quem limpa é quem conseguiu apagá-lo. Enquanto ele estiver no
    // banco, a varredura sabe que ainda há o que apagar.
    st.end("s2").unwrap();
    assert_eq!(
        st.canais_vazados().unwrap(),
        vec![("s2".to_string(), "nulo-abc".to_string())]
    );
    st.clear_canal("s2").unwrap();
    assert!(st.canais_vazados().unwrap().is_empty());
}

#[test]
fn rekey_do_clear_leva_o_canal_e_a_mensagem_de_status() {
    let st = Store::open_memory().unwrap();
    st.upsert(&sessao("velha", Some("7"))).unwrap();
    st.set_status_msg("velha", Some("m9")).unwrap();
    st.upsert(&sessao("nova", None)).unwrap();
    st.rekey("velha", "nova").unwrap();
    let n = st.get("nova").unwrap().unwrap();
    assert_eq!(n.canal_id.as_deref(), Some("7"));
    assert_eq!(n.status_msg_id.as_deref(), Some("m9"));
    assert_eq!(st.by_canal("7").unwrap().unwrap().session_id, "nova");
}

#[test]
fn resumo_leva_o_canal_como_texto() {
    let s = sessao("s1", Some("630"));
    assert_eq!(s.summary(None).canal_id.as_deref(), Some("630"));
}

/// O esquema como ele era antes desta mudança, com os ids do Telegram em coluna INTEGER.
const ESQUEMA_ANTIGO: &str = r#"
CREATE TABLE sessions (
    session_id        TEXT PRIMARY KEY,
    project           TEXT NOT NULL,
    cwd               TEXT NOT NULL,
    transcript_path   TEXT,
    tmux              TEXT,
    topic_id          INTEGER,
    status            TEXT NOT NULL DEFAULT 'idle',
    status_message_id INTEGER,
    model             TEXT,
    effort            TEXT,
    permission_mode   TEXT,
    pedido            INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    ended_at          INTEGER
);
CREATE TABLE queue (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    text       TEXT NOT NULL,
    from_name  TEXT NOT NULL,
    at         INTEGER NOT NULL,
    files      TEXT
);
CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO sessions (session_id, project, cwd, tmux, topic_id, status, status_message_id,
                      created_at, updated_at)
VALUES ('viva', 'proj', '/tmp/p', 'ld-proj-viva', 630, 'ocioso', 912, 1, 1);
INSERT INTO sessions (session_id, project, cwd, tmux, topic_id, status, created_at, updated_at,
                      ended_at)
VALUES ('morta', 'proj', '/tmp/q', 'ld-proj-morta', 631, 'ocioso', 1, 1, 2);
"#;

#[test]
fn banco_antigo_com_id_inteiro_continua_funcionando() {
    let dir = tempfile::tempdir().unwrap();
    let caminho = dir.path().join("state.db");
    Connection::open(&caminho)
        .unwrap()
        .execute_batch(ESQUEMA_ANTIGO)
        .unwrap();

    let st = Store::open(&caminho).unwrap();
    let viva = st.get("viva").unwrap().unwrap();
    assert_eq!(
        viva.canal_id.as_deref(),
        Some("630"),
        "inteiro lido como texto"
    );
    assert_eq!(viva.status_msg_id.as_deref(), Some("912"));
    assert_eq!(
        st.by_canal("630").unwrap().unwrap().session_id,
        "viva",
        "o tópico gravado como inteiro tem de ser achado pelo texto"
    );
    assert_eq!(
        st.canais_vazados().unwrap(),
        vec![("morta".to_string(), "631".to_string())]
    );
    assert_eq!(st.live().unwrap().len(), 1);

    // E a sessão velha continua recebendo escrita nova sem se perder.
    st.set_status_msg("viva", Some("m1")).unwrap();
    assert_eq!(
        st.get("viva").unwrap().unwrap().status_msg_id.as_deref(),
        Some("m1")
    );
    assert_eq!(st.by_canal("630").unwrap().unwrap().session_id, "viva");
}

#[test]
fn banco_antigo_com_coluna_tmux_vira_hospedagem() {
    // A coluna nasceu `tmux`. Uma sessão viva gravada nela não pode sumir na renomeação: é por
    // ela que o daemon acha o processo para matar no /kill e varrer como órfão.
    let dir = tempfile::tempdir().unwrap();
    let caminho = dir.path().join("state.db");
    Connection::open(&caminho)
        .unwrap()
        .execute_batch(ESQUEMA_ANTIGO)
        .unwrap();

    let st = Store::open(&caminho).unwrap();
    let viva = st.get("viva").unwrap().unwrap();
    assert_eq!(viva.hospedagem.as_deref(), Some("ld-proj-viva"));
    assert!(viva.owned_by_bot());
    assert!(st.hospedagem_de_sessao_morta("ld-proj-morta").unwrap());
    assert!(!st.hospedagem_de_sessao_morta("ld-proj-viva").unwrap());
    drop(st);

    // Abrir de novo (todo restart do daemon) não pode falhar nem perder o valor.
    let st = Store::open(&caminho).unwrap();
    assert_eq!(
        st.get("viva").unwrap().unwrap().hospedagem.as_deref(),
        Some("ld-proj-viva")
    );
}
