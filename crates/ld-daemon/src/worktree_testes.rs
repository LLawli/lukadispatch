//! O módulo de worktree contra repositórios de verdade num tempdir.

use super::*;

/// Um repositório com um commit, sem depender da configuração global do git da máquina.
async fn repo(dir: &Path) -> PathBuf {
    let raiz = dir.join("api");
    std::fs::create_dir_all(&raiz).unwrap();
    for args in [
        &["init", "--quiet", "--initial-branch=master"][..],
        &["config", "user.name", "Teste"],
        &["config", "user.email", "teste@exemplo"],
        &["config", "commit.gpgsign", "false"],
        &["commit", "--quiet", "--allow-empty", "-m", "um"],
    ] {
        git(&raiz, args).await.unwrap();
    }
    raiz
}

#[test]
fn o_caminho_segue_o_projeto_a_partir_do_home() {
    let base = Path::new("/b");
    let home = Path::new("/home/eu");
    assert_eq!(
        caminho(base, home, Path::new("/home/eu/Personal/api"), "feat/login"),
        Path::new("/b/Personal/api/feat/login")
    );
    // Dois projetos de mesmo nome em raízes diferentes não dividem pasta.
    assert_ne!(
        pasta_do_projeto(base, home, Path::new("/home/eu/Trabalho/api")),
        pasta_do_projeto(base, home, Path::new("/home/eu/Personal/api"))
    );
    assert_eq!(
        pasta_do_projeto(base, home, Path::new("/srv/api")),
        Path::new("/b/raiz/srv/api")
    );
}

#[tokio::test]
async fn cria_worktree_de_branch_nova_e_da_que_ja_existe() {
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    assert!(tem_commit(&raiz).await);
    assert_eq!(principal(&raiz).await.as_deref(), Some("master"));

    let nova = dir.path().join("wt/feat/login");
    cria(&raiz, &nova, "feat/login", Some("master"))
        .await
        .unwrap();
    assert!(nova.join(".git").is_file(), "worktree tem .git em arquivo");

    git(&raiz, &["branch", "velha"]).await.unwrap();
    let velha = dir.path().join("wt/velha");
    cria(&raiz, &velha, "velha", None).await.unwrap();

    let lista = branches(&raiz).await.unwrap();
    let achar = |n: &str| lista.iter().find(|b| b.nome == n).unwrap().clone();
    assert_eq!(achar("master").em_checkout.as_deref(), Some(raiz.as_path()));
    assert_eq!(
        achar("feat/login").em_checkout.as_deref(),
        Some(nova.as_path())
    );
    assert_eq!(achar("velha").em_checkout.as_deref(), Some(velha.as_path()));
}

#[tokio::test]
async fn pendencias_contam_o_que_se_perderia() {
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    let wt = dir.path().join("wt/x");
    cria(&raiz, &wt, "x", Some("master")).await.unwrap();
    assert!(pendencias(&wt, Some("master")).await.unwrap().nenhuma());

    std::fs::write(wt.join("a.txt"), "a").unwrap();
    assert_eq!(pendencias(&wt, Some("master")).await.unwrap().sem_commit, 1);
    git(&wt, &["add", "."]).await.unwrap();
    git(&wt, &["commit", "--quiet", "-m", "a"]).await.unwrap();
    let p = pendencias(&wt, Some("master")).await.unwrap();
    assert_eq!((p.sem_commit, p.sem_push), (0, 1));
}

#[tokio::test]
async fn apagar_tira_a_pasta_e_a_branch() {
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    let wt = dir.path().join("wt/x");
    cria(&raiz, &wt, "x", Some("master")).await.unwrap();
    std::fs::write(wt.join("sujo.txt"), "a").unwrap();

    apaga(&raiz, &wt, "x").await.unwrap();
    assert!(!wt.exists());
    assert!(!existe_branch(&raiz, "x").await);
    // De novo: o que já foi não é erro.
    apaga(&raiz, &wt, "x").await.unwrap();
}

#[tokio::test]
async fn nome_de_branch_segue_o_git_e_nao_sobe_de_pasta() {
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    assert!(nome_valido(&raiz, "feat/login").await);
    for ruim in ["", "-x", "a..b", "../fora", "com espaço", "fim/"] {
        assert!(!nome_valido(&raiz, ruim).await, "{ruim:?}");
    }
}

#[tokio::test]
async fn projeto_novo_nasce_com_commit_e_nao_sobrescreve() {
    let dir = tempfile::tempdir().unwrap();
    let pasta = dir.path().join("novo");
    // Sem identidade do git na máquina de CI o commit falharia; o teste empresta a do ambiente.
    // SAFETY: os testes deste módulo não leem estas variáveis em paralelo de outro jeito.
    unsafe {
        std::env::set_var("GIT_AUTHOR_NAME", "Teste");
        std::env::set_var("GIT_AUTHOR_EMAIL", "teste@exemplo");
        std::env::set_var("GIT_COMMITTER_NAME", "Teste");
        std::env::set_var("GIT_COMMITTER_EMAIL", "teste@exemplo");
    }
    inicia_projeto(&pasta).await.unwrap();
    assert!(tem_commit(&pasta).await);
    assert!(
        inicia_projeto(&pasta).await.is_err(),
        "pasta que existe fica"
    );
}
