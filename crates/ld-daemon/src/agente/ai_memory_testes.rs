//! O ai-memory como memória: o que dá para provar sem o servidor dele no ar.

use super::*;

fn wt(caminho: &str, branch: &str) -> Worktree {
    Worktree {
        caminho: caminho.into(),
        projeto: "api".into(),
        raiz: "/home/eu/Personal/api".into(),
        branch: branch.into(),
        criada_em: 0,
        usada_em: 0,
    }
}

fn partida<'a>(
    cwd: &'a Path,
    worktree: Option<&'a Worktree>,
    isolada: bool,
) -> PartidaDaMemoria<'a> {
    PartidaDaMemoria {
        session_id: "0123abcd-ffff",
        cwd,
        worktree,
        isolada,
    }
}

#[tokio::test]
async fn fora_de_worktree_cada_partida_tem_workstream_inedito() {
    let argv = AiMemory
        .embrulha(
            &partida(Path::new("/p"), None, false),
            vec!["claude".into()],
        )
        .await
        .unwrap();
    assert_eq!(&argv[..3], ["ai-memory", "run", "--new"]);
    assert!(argv[3].starts_with("lukadispatch-0123abcd-"), "{argv:?}");
    assert_eq!(argv[4], "claude", "o agente vem inteiro depois");
}

#[tokio::test]
async fn worktree_presa_usa_workstream_proprio_com_o_nome_da_branch() {
    let w = wt("/wt/api/feat/login", "feat/login");
    let argv = AiMemory
        .embrulha(
            &partida(Path::new(&w.caminho), Some(&w), true),
            vec!["claude".into()],
        )
        .await
        .unwrap();
    assert_eq!(argv[2], "--new");
    assert!(argv[3].starts_with("feat-login-"), "{argv:?}");
}

#[test]
fn o_nome_do_workstream_e_a_branch_sem_barra() {
    assert_eq!(workstream_da_worktree("feat/login"), "feat-login");
    assert_eq!(workstream_da_worktree("a\\b"), "a-b");
    assert_eq!(workstream_da_worktree(&"x".repeat(100)).len(), 64);
}

#[test]
fn a_pasta_do_projeto_e_a_worktree_sem_a_branch() {
    assert_eq!(
        pasta_do_projeto(&wt("/wt/Personal/api/feat/login", "feat/login")),
        Path::new("/wt/Personal/api")
    );
    assert_eq!(
        pasta_do_projeto(&wt("/wt/Personal/api/x", "x")),
        Path::new("/wt/Personal/api")
    );
}

#[test]
fn sem_marcador_o_escopo_e_o_padrao_do_ai_memory() {
    let home = tempfile::tempdir().unwrap();
    let raiz = home.path().join("Personal/api");
    std::fs::create_dir_all(&raiz).unwrap();
    assert_eq!(
        escopo_do_repositorio(&raiz, home.path()),
        Escopo {
            workspace: "default".into(),
            project: "api".into()
        }
    );
}

#[test]
fn o_marcador_mais_proximo_que_declara_escopo_vence() {
    let home = tempfile::tempdir().unwrap();
    let raiz = home.path().join("Trabalho/cliente/api");
    std::fs::create_dir_all(&raiz).unwrap();
    std::fs::write(
        home.path().join("Trabalho/.ai-memory.toml"),
        "workspace = \"trabalho\"\n",
    )
    .unwrap();
    // Só `[capture]` não declara escopo: é transparente.
    std::fs::write(
        home.path().join("Trabalho/cliente/.ai-memory.toml"),
        "[capture]\nignore_paths = [\"x\"]\n",
    )
    .unwrap();
    assert_eq!(
        escopo_do_repositorio(&raiz, home.path()),
        Escopo {
            workspace: "trabalho".into(),
            project: "api".into()
        }
    );

    std::fs::write(
        raiz.join(".ai-memory.toml"),
        "workspace = \"cliente\"\nproject = \"portal\"\n",
    )
    .unwrap();
    assert_eq!(
        escopo_do_repositorio(&raiz, home.path()),
        Escopo {
            workspace: "cliente".into(),
            project: "portal".into()
        }
    );
}

#[test]
fn o_marcador_gravado_diz_o_escopo_que_o_ai_memory_vai_ler() {
    let dir = tempfile::tempdir().unwrap();
    let escopo = Escopo {
        workspace: "default".into(),
        project: "nome \"estranho\"".into(),
    };
    let arquivo = dir.path().join(MARCADOR);
    std::fs::write(&arquivo, marcador(&escopo, "/home/eu/api")).unwrap();
    assert_eq!(
        le_marcador(&arquivo),
        Some((Some("default".into()), Some("nome \"estranho\"".into())))
    );
}

#[test]
fn as_instrucoes_so_existem_em_worktree_e_trocam_o_handoff_pela_pagina() {
    assert!(
        AiMemory
            .instrucoes(&partida(Path::new("/p"), None, false))
            .is_none()
    );
    let w = wt("/wt/api/feat/login", "feat/login");
    let texto = AiMemory
        .instrucoes(&partida(Path::new(&w.caminho), Some(&w), false))
        .unwrap();
    assert!(texto.contains("worktrees/feat/login.md"), "{texto}");
    assert!(texto.contains("NÃO use memory_handoff_begin"), "{texto}");
}

#[test]
fn reconhece_o_workstream_preso_pelo_que_o_painel_mostrou() {
    assert!(AiMemory.ocupada("Error: opening managed workstream\nCaused by: 409"));
    assert!(AiMemory.ocupada("workstream is already active: feat-login"));
    assert!(!AiMemory.ocupada("Error: connection refused"));
    assert!(!SemMemoriaDeTeste.ocupada("workstream is already active"));
}

/// Uma memória que só tem o obrigatório: prova os padrões da trait.
struct SemMemoriaDeTeste;

#[async_trait::async_trait]
impl Memoria for SemMemoriaDeTeste {
    fn nome(&self) -> &'static str {
        "teste"
    }

    async fn embrulha(&self, _: &PartidaDaMemoria<'_>, argv: Vec<String>) -> Result<Vec<String>> {
        Ok(argv)
    }
}

#[test]
fn le_a_resposta_de_uma_ferramenta_mcp() {
    let ok =
        json!({"content": [{"type": "text", "text": "{\"handoff\": null}"}], "isError": false});
    assert_eq!(
        resultado_da_ferramenta("x", &ok).unwrap(),
        json!({"handoff": null})
    );
    let erro = json!({"content": [{"type": "text", "text": "sem projeto"}], "isError": true});
    let e = resultado_da_ferramenta("x", &erro).unwrap_err();
    assert!(format!("{e:#}").contains("sem projeto"));
}

#[test]
fn acha_o_workstream_pelo_nome_na_lista() {
    let lista = json!([{"name": "default"}, {"name": "feat-login"}]);
    assert!(tem_workstream(&lista, "feat-login"));
    assert!(!tem_workstream(&lista, "feat"));
    assert!(!tem_workstream(&json!({}), "feat"));
}

#[test]
fn a_parada_vai_para_o_agente_e_nao_para_o_ai_memory() {
    let mut filho = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let meu = std::process::id();
    assert!(AiMemory.a_parar(meu).contains(&filho.id()));
    assert_eq!(SemMemoriaDeTeste.a_parar(meu), vec![meu]);
    let _ = filho.kill();
    let _ = filho.wait();
}

/// Contra o ai-memory de verdade, num projeto descartável: `cargo test -- --ignored ponte`.
/// Cria dois handoffs manuais, tira da fila, confere que a fila ficou vazia, devolve e confere
/// que eles voltaram com o conteúdo, na mesma ordem. Apaga o projeto no fim.
#[tokio::test]
#[ignore = "fala com o servidor do ai-memory desta máquina"]
async fn ponte_tira_e_devolve_handoffs_no_ai_memory_de_verdade() {
    let escopo = Escopo {
        workspace: "default".into(),
        project: format!("lukadispatch-teste-ponte-{}", agora()),
    };
    let mut ponte = Ponte::abre().await.unwrap();
    for (resumo, passo) in [("primeiro", "a"), ("segundo", "b")] {
        ponte
            .chama(
                "memory_handoff_begin",
                json!({
                    "workspace": escopo.workspace,
                    "project": escopo.project,
                    "summary": resumo,
                    "next_steps": [passo],
                    "cwd": "/home/eu/terminal",
                }),
            )
            .await
            .unwrap();
        // O ai-memory ordena por criação; dois no mesmo instante empatariam.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let tirados = tira_handoffs(&escopo).await.unwrap();
    let resumos: Vec<&str> = tirados
        .iter()
        .map(|h| h["summary"].as_str().unwrap())
        .collect();
    assert_eq!(resumos, ["segundo", "primeiro"], "o mais novo sai primeiro");
    assert!(
        tira_handoffs(&escopo).await.unwrap().is_empty(),
        "a fila ficou vazia"
    );

    devolve_handoffs(&escopo, &tirados).await.unwrap();
    let de_volta = tira_handoffs(&escopo).await.unwrap();
    let resumos: Vec<&str> = de_volta
        .iter()
        .map(|h| h["summary"].as_str().unwrap())
        .collect();
    assert_eq!(
        resumos,
        ["segundo", "primeiro"],
        "a ordem entre eles não mudou"
    );
    assert_eq!(de_volta[0]["next_steps"], json!(["b"]));
    assert_eq!(de_volta[0]["cwd"], json!("/home/eu/terminal"));

    let _ = Command::new(PROGRAMA)
        .args([
            "purge-project",
            "--workspace",
            &escopo.workspace,
            "--project",
            &escopo.project,
            "--confirm",
        ])
        .output()
        .await;
}

/// A lista de workstreams contra o ai-memory de verdade: o comando que o daemon monta tem de ser
/// aceito por ele (um `--limit` fora da faixa derrubava a lista inteira).
#[tokio::test]
#[ignore = "chama o ai-memory desta máquina"]
async fn lista_de_workstreams_aceita_pelo_ai_memory_de_verdade() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!workstream_existe(dir.path(), "nao-existe").await.unwrap());
}
