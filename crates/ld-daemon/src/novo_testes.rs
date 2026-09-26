//! As partes do `/new` que não precisam de chat nem de git.

use super::*;

fn p(nome: &str, caminho: &str) -> Project {
    Project {
        name: nome.into(),
        path: caminho.into(),
        permission_mode: None,
        model: None,
        effort: None,
    }
}

#[test]
fn projetos_se_agrupam_pela_raiz_e_o_resto_vai_para_os_fixados() {
    let raizes = [PathBuf::from("/h/Personal"), PathBuf::from("/h/Trabalho")];
    let pastas = agrupa(
        vec![
            p("api", "/h/Trabalho/api"),
            p("site", "/h/Personal/site"),
            p("notas", "/srv/notas"),
            p("cli", "/h/Personal/cli"),
        ],
        &raizes,
    );
    let resumo: Vec<(String, Vec<String>)> = pastas
        .into_iter()
        .map(|x| (x.nome, x.projetos.into_iter().map(|p| p.name).collect()))
        .collect();
    assert_eq!(
        resumo,
        [
            (
                "Personal".to_string(),
                vec!["site".to_string(), "cli".to_string()]
            ),
            ("Trabalho".to_string(), vec!["api".to_string()]),
            ("Fixados".to_string(), vec!["notas".to_string()]),
        ]
    );
}

#[test]
fn pasta_sem_projeto_nao_aparece_e_raiz_aninhada_nao_repete() {
    // `~/Projetos` e `~/Projetos/cliente` como raízes: o projeto fica só na primeira.
    let raizes = [
        PathBuf::from("/h/Projetos"),
        PathBuf::from("/h/Projetos/cliente"),
        PathBuf::from("/h/Vazia"),
    ];
    let pastas = agrupa(vec![p("x", "/h/Projetos/cliente/x")], &raizes);
    assert_eq!(pastas.len(), 1);
    assert_eq!(pastas[0].nome, "Projetos");
}

#[test]
fn nome_de_projeto_novo_so_com_o_que_nao_precisa_de_aspas_e_que_nao_existe() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("existe")).unwrap();
    assert!(nome_de_projeto_valido(dir.path(), "meu-app_2.0").is_ok());
    for ruim in ["", ".oculto", "-x", "com espaço", "a/b", "ação", "existe"] {
        assert!(
            nome_de_projeto_valido(dir.path(), ruim).is_err(),
            "{ruim:?}"
        );
    }
}

#[test]
fn escolha_vale_uma_vez_e_some_quando_expira() {
    let e = Estado::default();
    let dado = e.guarda(Escolha::Pastas);
    let n: u64 = dado.strip_prefix("e:").unwrap().parse().unwrap();
    assert!(matches!(e.tira(n), Some(Escolha::Pastas)));
    assert!(e.tira(n).is_none(), "tocar duas vezes não repete a ação");

    let dado = e.guarda(Escolha::Pastas);
    let n: u64 = dado.strip_prefix("e:").unwrap().parse().unwrap();
    e.escolhas.lock().unwrap().get_mut(&n).unwrap().1 = Instant::now() - VALIDADE;
    assert!(e.tira(n).is_none(), "teclado velho não age");
}

#[test]
fn o_dado_do_botao_cabe_no_teto_do_telegram() {
    let e = Estado::default();
    e.proximo.store(u64::MAX - 1, Ordering::Relaxed);
    assert!(e.guarda(Escolha::Pastas).len() <= 64);
}

#[test]
fn pendencias_viram_frases() {
    let t = descreve_pendencias(worktree::Pendencias {
        sem_commit: 2,
        sem_push: 1,
    });
    assert!(
        t.contains("2 arquivo(s)") && t.contains("1 commit(s)"),
        "{t}"
    );
    let t = descreve_pendencias(worktree::Pendencias {
        sem_commit: usize::MAX,
        sem_push: 0,
    });
    assert!(t.contains("não consegui conferir"), "{t}");
}

#[test]
fn as_palavras_do_new_viram_projeto_e_branch() {
    let projetos = vec![
        p("api", "/h/Personal/api"),
        p("meu site", "/h/Personal/meu site"),
        p("api", "/h/Trabalho/api"),
        p("notas", "/h/Personal/notas"),
    ];
    assert_eq!(
        resolve(&projetos, &["notas"]),
        Some(Alvo::Um(p("notas", "/h/Personal/notas"), None))
    );
    assert_eq!(
        resolve(&projetos, &["notas", "feat/x"]),
        Some(Alvo::Um(
            p("notas", "/h/Personal/notas"),
            Some("feat/x".into())
        ))
    );
    // Nome com espaço: as palavras todas são o nome.
    assert_eq!(
        resolve(&projetos, &["meu", "site"]),
        Some(Alvo::Um(p("meu site", "/h/Personal/meu site"), None))
    );
    // O mesmo nome em duas pastas vira pergunta, com a branch junto.
    match resolve(&projetos, &["api", "feat"]) {
        Some(Alvo::Varios(c, Some(b))) => {
            assert_eq!(c.len(), 2);
            assert_eq!(b, "feat");
        }
        outro => panic!("{outro:?}"),
    }
    assert_eq!(resolve(&projetos, &["nada"]), None);
    assert_eq!(pasta_de(&p("api", "/h/Trabalho/api")), "Trabalho");
}
