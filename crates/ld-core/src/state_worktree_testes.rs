//! As worktrees que o bot abriu: o `/new` as lista por projeto, e a sessão acha a dela pelo cwd.

use super::*;

fn wt(raiz: &str, branch: &str) -> Worktree {
    Worktree {
        caminho: format!("/wt/{}/{branch}", raiz.trim_start_matches('/')),
        projeto: raiz.trim_start_matches('/').into(),
        raiz: raiz.into(),
        branch: branch.into(),
        criada_em: 0,
        usada_em: 0,
    }
}

#[test]
fn acha_a_worktree_pela_branch_e_pelo_caminho() {
    let st = Store::open_memory().unwrap();
    let w = wt("/api", "feat/login");
    st.registra_worktree(&w).unwrap();

    let achada = st
        .worktree_da_branch("/api", "feat/login")
        .unwrap()
        .unwrap();
    assert_eq!(achada.caminho, w.caminho);
    assert!(achada.criada_em > 0, "a data é do banco, não de quem chama");
    assert_eq!(
        st.worktree_em(&w.caminho).unwrap().unwrap().branch,
        "feat/login"
    );
    assert!(
        st.worktree_da_branch("/outro", "feat/login")
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_lista_e_por_repositorio_e_a_usada_por_ultimo_vem_primeiro() {
    let st = Store::open_memory().unwrap();
    st.registra_worktree(&wt("/api", "a")).unwrap();
    st.registra_worktree(&wt("/api", "b")).unwrap();
    st.registra_worktree(&wt("/web", "c")).unwrap();
    // Usar de novo não cria outra linha, só muda a ordem.
    st.conn()
        .execute("UPDATE worktrees SET usada_em = 1", [])
        .unwrap();
    st.registra_worktree(&wt("/api", "a")).unwrap();

    let branches: Vec<String> = st
        .worktrees_de("/api")
        .unwrap()
        .into_iter()
        .map(|w| w.branch)
        .collect();
    assert_eq!(branches, ["a", "b"]);
}

#[test]
fn esquecer_tira_so_aquela() {
    let st = Store::open_memory().unwrap();
    let a = wt("/api", "a");
    st.registra_worktree(&a).unwrap();
    st.registra_worktree(&wt("/api", "b")).unwrap();
    st.esquece_worktree(&a.caminho).unwrap();
    assert!(st.worktree_em(&a.caminho).unwrap().is_none());
    assert_eq!(st.worktrees_de("/api").unwrap().len(), 1);
}

#[test]
fn banco_de_antes_das_worktrees_ganha_a_tabela_ao_abrir() {
    // O state.db da máquina já existe sem esta tabela; abrir tem de criá-la sem mexer no resto.
    let dir = tempfile::tempdir().unwrap();
    let caminho = dir.path().join("state.db");
    {
        let c = rusqlite::Connection::open(&caminho).unwrap();
        c.execute_batch("CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        c.execute("INSERT INTO kv VALUES ('painel', '42')", [])
            .unwrap();
    }
    let st = Store::open(&caminho).unwrap();
    st.registra_worktree(&wt("/api", "a")).unwrap();
    assert_eq!(st.kv_get("painel").unwrap().as_deref(), Some("42"));
}
