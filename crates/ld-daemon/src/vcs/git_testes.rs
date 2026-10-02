//! O adaptador git contra repositórios de verdade num tempdir.

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

fn registro(raiz: &Path, caminho: &Path, branch: &str) -> Worktree {
    Worktree {
        caminho: caminho.to_string_lossy().into_owned(),
        projeto: "api".into(),
        raiz: raiz.to_string_lossy().into_owned(),
        branch: branch.into(),
        vcs: "git".into(),
        criada_em: 0,
        usada_em: 0,
    }
}

#[tokio::test]
async fn cria_worktree_de_branch_nova_e_da_que_ja_existe() {
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    assert!(Git.tem_commit(&raiz).await);
    assert_eq!(Git.principal(&raiz).await.as_deref(), Some("master"));

    let nova = dir.path().join("wt/feat/login");
    Git.garante(&raiz, &nova, "feat/login", Some("master"))
        .await
        .unwrap();
    assert!(nova.join(".git").is_file(), "worktree tem .git em arquivo");

    git(&raiz, &["branch", "velha"]).await.unwrap();
    let velha = dir.path().join("wt/velha");
    // A branch que existe abre sem base, mesmo que uma seja passada.
    Git.garante(&raiz, &velha, "velha", Some("master"))
        .await
        .unwrap();

    let lista = branches(&raiz).await.unwrap();
    let achar = |n: &str| lista.iter().find(|b| b.nome == n).unwrap().clone();
    assert_eq!(achar("master").em_checkout.as_deref(), Some(raiz.as_path()));
    assert_eq!(
        achar("feat/login").em_checkout.as_deref(),
        Some(nova.as_path())
    );
    assert_eq!(achar("velha").em_checkout.as_deref(), Some(velha.as_path()));

    // Garantir de novo onde ela já está não é erro; noutro lugar é.
    Git.garante(&raiz, &velha, "velha", None).await.unwrap();
    let outro = dir.path().join("wt/outro");
    assert!(Git.garante(&raiz, &outro, "velha", None).await.is_err());
    assert!(
        Git.garante(&raiz, &outro, "nao-existe", None)
            .await
            .is_err(),
        "branch que não existe sem base"
    );
}

#[tokio::test]
async fn ramo_em_checkout_noutro_lugar_so_serve_de_base() {
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    git(&raiz, &["branch", "livre"]).await.unwrap();
    let ramos = Git.ramos(&raiz).await.unwrap();
    let achar = |n: &str| ramos.iter().find(|r| r.nome == n).unwrap().clone();
    assert!(achar("master").so_como_base, "em checkout no repositório");
    assert!(!achar("livre").so_como_base);
    assert!(Git.existe(&raiz, "livre").await);
    assert!(!Git.existe(&raiz, "nada").await);
}

#[tokio::test]
async fn pendencias_contam_o_que_se_perderia() {
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    let wt = dir.path().join("wt/x");
    Git.garante(&raiz, &wt, "x", Some("master")).await.unwrap();
    let w = registro(&raiz, &wt, "x");
    assert!(Git.pendencias(&w).await.unwrap().nenhuma());

    std::fs::write(wt.join("a.txt"), "a").unwrap();
    assert_eq!(Git.pendencias(&w).await.unwrap().sem_commit, 1);
    git(&wt, &["add", "."]).await.unwrap();
    git(&wt, &["commit", "--quiet", "-m", "a"]).await.unwrap();
    let p = Git.pendencias(&w).await.unwrap();
    assert_eq!((p.sem_commit, p.sem_push), (0, 1));
}

#[tokio::test]
async fn apagar_tira_a_pasta_e_a_branch() {
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    let wt = dir.path().join("wt/x");
    Git.garante(&raiz, &wt, "x", Some("master")).await.unwrap();
    std::fs::write(wt.join("sujo.txt"), "a").unwrap();
    let w = registro(&raiz, &wt, "x");

    assert!(!Git.apagar_preserva_commits());
    Git.apaga(&w, false).await.unwrap();
    assert!(!wt.exists());
    assert!(!Git.existe(&raiz, "x").await);
    // De novo: o que já foi não é erro.
    Git.apaga(&w, false).await.unwrap();
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
    Git.inicia_projeto(&pasta).await.unwrap();
    assert!(Git.tem_commit(&pasta).await);
    assert!(
        Git.inicia_projeto(&pasta).await.is_err(),
        "pasta que existe fica"
    );
}
