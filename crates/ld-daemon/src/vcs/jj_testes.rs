//! O adaptador jj contra repositórios de verdade num tempdir.
//!
//! Sem o jj no PATH os testes que precisam dele retornam cedo, menos com
//! `LUKADISPATCH_E2E_EXIGE` no ambiente (o CI liga): lá, pular seria um verde que não testou nada.

use std::sync::Once;

use super::*;

/// O jj existe? Também isola os testes da config da máquina (alias, assinatura, wrapper): o jj
/// lê só um arquivo com a identidade de teste.
fn tem_jj() -> bool {
    static CONFIG: Once = Once::new();
    CONFIG.call_once(|| {
        let arquivo =
            std::env::temp_dir().join(format!("lukadispatch-jj-teste-{}.toml", std::process::id()));
        std::fs::write(
            &arquivo,
            "user.name = \"Teste\"\nuser.email = \"teste@exemplo\"\n",
        )
        .unwrap();
        // SAFETY: só os testes deste módulo leem estas variáveis, e todos passam por aqui antes.
        unsafe {
            std::env::set_var("JJ_CONFIG", &arquivo);
            std::env::set_var("GIT_AUTHOR_NAME", "Teste");
            std::env::set_var("GIT_AUTHOR_EMAIL", "teste@exemplo");
            std::env::set_var("GIT_COMMITTER_NAME", "Teste");
            std::env::set_var("GIT_COMMITTER_EMAIL", "teste@exemplo");
        }
    });
    let ok = std::process::Command::new("jj")
        .arg("--version")
        .output()
        .is_ok_and(|s| s.status.success());
    if !ok && std::env::var_os("LUKADISPATCH_E2E_EXIGE").is_some() {
        panic!("o jj não está no PATH, e LUKADISPATCH_E2E_EXIGE pede que esteja");
    }
    ok
}

/// Um repositório jj colocado com um commit e o bookmark `main` nele.
async fn repo(dir: &Path) -> PathBuf {
    let raiz = dir.join("api");
    Jj::new(Default::default())
        .inicia_projeto(&raiz)
        .await
        .unwrap();
    std::fs::write(raiz.join("a.txt"), "a").unwrap();
    roda(Some(&raiz), &["commit", "-m", "um"]).await.unwrap();
    roda(Some(&raiz), &["bookmark", "set", "main", "-r", "@-"])
        .await
        .unwrap();
    raiz
}

fn jj() -> Jj {
    Jj::new(Default::default())
}

fn registro(raiz: &Path, caminho: &Path, nome: &str) -> Worktree {
    Worktree {
        caminho: caminho.to_string_lossy().into_owned(),
        projeto: "api".into(),
        raiz: raiz.to_string_lossy().into_owned(),
        branch: nome.into(),
        vcs: "jj".into(),
        criada_em: time::OffsetDateTime::now_utc().unix_timestamp(),
        usada_em: 0,
    }
}

/// Quantos commits o revset devolve, vistos da raiz.
async fn conta(raiz: &Path, revset: &str) -> usize {
    le(raiz, &["log", "--no-graph", "-r", revset, "-T", "\"x\\n\""])
        .await
        .unwrap()
        .lines()
        .count()
}

#[tokio::test]
async fn projeto_novo_tem_commit_e_principal_e_nao_sobrescreve() {
    if !tem_jj() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let pasta = dir.path().join("novo");
    jj().inicia_projeto(&pasta).await.unwrap();
    assert!(pasta.join(".jj").is_dir() && pasta.join(".git").is_dir());
    assert!(
        jj().tem_commit(&pasta).await,
        "o commit vazio descrito conta"
    );
    assert_eq!(jj().principal(&pasta).await.as_deref(), Some("main"));
    assert!(jj().inicia_projeto(&pasta).await.is_err());
}

#[tokio::test]
async fn repositorio_recem_criado_nao_tem_commit() {
    if !tem_jj() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    roda(None, &["git", "init", &dir.path().to_string_lossy()])
        .await
        .unwrap();
    assert!(!jj().tem_commit(dir.path()).await);
    assert_eq!(jj().principal(dir.path()).await, None);
    assert!(!jj().tem_commit(&dir.path().join("nao-existe")).await);
}

#[tokio::test]
async fn prepara_coloca_o_jj_num_repositorio_so_git_sem_quebrar_a_worktree() {
    if !tem_jj() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let raiz = dir.path().join("g");
    std::fs::create_dir_all(&raiz).unwrap();
    let git = |args: &[&str]| {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(&raiz)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "--quiet", "--initial-branch=main"]);
    git(&[
        "-c",
        "commit.gpgsign=false",
        "commit",
        "--quiet",
        "--allow-empty",
        "-m",
        "um",
    ]);
    let velha = dir.path().join("velha");
    git(&[
        "worktree",
        "add",
        "--quiet",
        "-b",
        "velha",
        &velha.to_string_lossy(),
    ]);

    std::fs::write(raiz.join(".git/MERGE_HEAD"), "x").unwrap();
    let e = jj().prepara(&raiz).await.unwrap_err().to_string();
    assert!(e.contains("MERGE_HEAD"), "{e}");
    assert!(!raiz.join(".jj").exists(), "no meio de um merge não mexe");
    std::fs::remove_file(raiz.join(".git/MERGE_HEAD")).unwrap();

    jj().prepara(&raiz).await.unwrap();
    assert!(raiz.join(".jj").is_dir());
    assert!(jj().tem_commit(&raiz).await);
    assert_eq!(jj().principal(&raiz).await.as_deref(), Some("main"));
    git(&["-C", &velha.to_string_lossy(), "status", "--short"]);
    // De novo é no-op, e pasta sem repositório fica como está.
    jj().prepara(&raiz).await.unwrap();
    let solta = dir.path().join("solta");
    std::fs::create_dir_all(&solta).unwrap();
    jj().prepara(&solta).await.unwrap();
    assert!(!solta.join(".jj").exists());
}

#[tokio::test]
async fn garante_cria_o_workspace_e_o_acha_de_novo() {
    if !tem_jj() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    let ws = dir.path().join("wt/feat/login");
    assert!(!jj().existe(&raiz, "feat/login").await);
    jj().garante(&raiz, &ws, "feat/login", Some("main"))
        .await
        .unwrap();
    assert!(ws.join(".jj").is_dir());
    assert!(!ws.join(".git").exists(), "o workspace do jj não tem git");
    assert!(ws.join("a.txt").exists(), "nasceu em cima da main");
    assert!(jj().existe(&raiz, "feat/login").await);

    jj().garante(&raiz, &ws, "feat/login", None).await.unwrap();
    let outro = dir.path().join("wt/outro");
    let e = jj()
        .garante(&raiz, &outro, "feat/login", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("já existe"), "{e}");
    assert!(
        jj().garante(&raiz, &outro, "nada", None).await.is_err(),
        "sem base não há de onde criar"
    );
}

#[tokio::test]
async fn pasta_apagada_por_fora_volta_com_o_que_tinha() {
    if !tem_jj() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    let ws = dir.path().join("wt/x");
    jj().garante(&raiz, &ws, "x", Some("main")).await.unwrap();
    std::fs::write(ws.join("b.txt"), "b").unwrap();
    snapshot(&ws).await.unwrap();
    std::fs::remove_dir_all(&ws).unwrap();

    jj().garante(&raiz, &ws, "x", None).await.unwrap();
    assert_eq!(std::fs::read_to_string(ws.join("b.txt")).unwrap(), "b");
}

#[tokio::test]
async fn todo_bookmark_e_base_e_o_de_mesmo_nome_vira_base() {
    if !tem_jj() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    roda(Some(&raiz), &["bookmark", "create", "feat/x", "-r", "main"])
        .await
        .unwrap();
    let ramos = jj().ramos(&raiz).await.unwrap();
    assert!(ramos.iter().all(|r| r.so_como_base));
    let nomes: Vec<&str> = ramos.iter().map(|r| r.nome.as_str()).collect();
    assert!(
        nomes.contains(&"main") && nomes.contains(&"feat/x"),
        "{nomes:?}"
    );
    let principal = Some("main".to_string());
    assert_eq!(
        jj().base_para(&raiz, "feat/x", principal.clone())
            .await
            .as_deref(),
        Some("feat/x")
    );
    assert_eq!(
        jj().base_para(&raiz, "nova", principal).await.as_deref(),
        Some("main")
    );
}

#[tokio::test]
async fn pendencias_contam_o_sem_commit_e_o_nao_publicado() {
    if !tem_jj() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    let ws = dir.path().join("wt/x");
    jj().garante(&raiz, &ws, "x", Some("main")).await.unwrap();
    let w = registro(&raiz, &ws, "x");
    assert!(jj().pendencias(&w).await.unwrap().nenhuma());

    std::fs::write(ws.join("b.txt"), "b").unwrap();
    let p = jj().pendencias(&w).await.unwrap();
    assert_eq!((p.sem_commit, p.sem_push), (1, 0));

    roda(Some(&ws), &["commit", "-m", "b"]).await.unwrap();
    // Bookmark local não publicado continua sendo o que se perderia.
    roda(Some(&ws), &["bookmark", "create", "feat-b", "-r", "@-"])
        .await
        .unwrap();
    let p = jj().pendencias(&w).await.unwrap();
    assert_eq!((p.sem_commit, p.sem_push), (0, 1));
}

#[tokio::test]
async fn apagar_sem_abandonar_mantem_os_commits_e_o_que_estava_sem_commit() {
    if !tem_jj() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    let ws = dir.path().join("wt/x");
    jj().garante(&raiz, &ws, "x", Some("main")).await.unwrap();
    std::fs::write(ws.join("b.txt"), "b").unwrap();
    roda(Some(&ws), &["commit", "-m", "b"]).await.unwrap();
    // Escrito depois do último comando: só o snapshot do apagar o grava.
    std::fs::write(ws.join("c.txt"), "c").unwrap();
    let w = registro(&raiz, &ws, "x");

    assert!(jj().apagar_preserva_commits());
    jj().apaga(&w, false).await.unwrap();
    assert!(!ws.exists());
    assert!(!jj().existe(&raiz, "x").await);
    assert_eq!(conta(&raiz, "description(exact:\"b\\n\")").await, 1);
    assert_eq!(
        conta(&raiz, "files(root:\"c.txt\") ~ empty()").await,
        1,
        "o arquivo sem commit virou o @ que ficou"
    );
    // De novo: o que já foi não é erro.
    jj().apaga(&w, false).await.unwrap();
}

#[tokio::test]
async fn abandonar_leva_o_que_e_so_dele_e_poupa_a_base() {
    if !tem_jj() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    // Um bookmark seu, local, de antes do workspace: é a base dele, e não é dele.
    std::fs::write(raiz.join("base.txt"), "base").unwrap();
    roda(Some(&raiz), &["commit", "-m", "base"]).await.unwrap();
    roda(Some(&raiz), &["bookmark", "create", "minha", "-r", "@-"])
        .await
        .unwrap();
    // Some do working copy do repositório, para só o bookmark proteger a base.
    roda(Some(&raiz), &["new", "main"]).await.unwrap();
    // A data do commit tem resolução de segundos, e a separação é por ela.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let mut w = registro(&raiz, &dir.path().join("wt/x"), "x");
    w.criada_em = time::OffsetDateTime::now_utc().unix_timestamp() + FOLGA_DA_CRIACAO;
    let ws = PathBuf::from(&w.caminho);
    jj().garante(&raiz, &ws, "x", Some("minha")).await.unwrap();
    std::fs::write(ws.join("b.txt"), "b").unwrap();
    roda(Some(&ws), &["commit", "-m", "do workspace"])
        .await
        .unwrap();
    roda(Some(&ws), &["bookmark", "create", "dele", "-r", "@-"])
        .await
        .unwrap();

    jj().apaga(&w, true).await.unwrap();
    assert!(!ws.exists());
    assert_eq!(
        conta(&raiz, "description(exact:\"do workspace\\n\")").await,
        0
    );
    assert_eq!(conta(&raiz, "description(exact:\"base\\n\")").await, 1);
    let bookmarks = bookmarks(&raiz).await.unwrap();
    assert!(bookmarks.contains(&"minha".to_string()));
    assert!(!bookmarks.contains(&"dele".to_string()), "{bookmarks:?}");
}

#[tokio::test]
async fn antes_da_partida_nao_falha_sem_nada_a_recuperar() {
    if !tem_jj() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let raiz = repo(dir.path()).await;
    let ws = dir.path().join("wt/x");
    jj().garante(&raiz, &ws, "x", Some("main")).await.unwrap();
    let w = registro(&raiz, &ws, "x");
    jj().antes_da_partida(&w).await.unwrap();
    roda(
        Some(&raiz),
        &[
            "git",
            "remote",
            "add",
            "origin",
            "git@github.com:Dono/api.git",
        ],
    )
    .await
    .unwrap();
    let texto = jj().instrucoes(&w).await.unwrap();
    assert!(texto.contains("--repo Dono/api"), "{texto}");
}

#[test]
fn dono_e_repo_sai_de_ssh_e_https() {
    for url in [
        "git@github.com:LLawli/lukadispatch.git",
        "https://github.com/LLawli/lukadispatch",
        "https://github.com/LLawli/lukadispatch.git/",
        "ssh://git@github.com/LLawli/lukadispatch.git",
    ] {
        assert_eq!(
            dono_e_repo(url).as_deref(),
            Some("LLawli/lukadispatch"),
            "{url}"
        );
    }
    assert_eq!(dono_e_repo("https://gitlab.com/a/b"), None);
    assert_eq!(dono_e_repo("git@github.com:so-dono"), None);
}

#[test]
fn instrucoes_dizem_que_nao_ha_git_e_como_publicar() {
    let w = Worktree {
        caminho: "/wt/api/x".into(),
        projeto: "api".into(),
        raiz: "/api".into(),
        branch: "x".into(),
        vcs: "jj".into(),
        criada_em: 0,
        usada_em: 0,
    };
    let t = instrucoes(&w, None, false);
    assert!(
        t.contains("workspace `x`") && t.contains("Não há `.git`"),
        "{t}"
    );
    assert!(
        t.contains("--repo <dono>/<repo>") && !t.contains("MCP"),
        "{t}"
    );
    assert!(instrucoes(&w, Some("a/b"), true).contains("MCP"));
}

#[test]
fn nomes_vao_entre_aspas_no_revset() {
    assert_eq!(simbolo("ld/2026-10-02-1530"), "\"ld/2026-10-02-1530\"");
    assert_eq!(workspace("feat/x"), "\"feat/x\"@");
    let r = so_dele("x", Some("main"), 120);
    assert!(r.contains("| \"main\""), "{r}");
    assert!(
        r.contains("committer_date(after:\"1970-01-01T00:01:00Z\")"),
        "{r}"
    );
}
