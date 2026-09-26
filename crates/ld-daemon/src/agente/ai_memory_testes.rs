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
    // O que o espelho do tmux guarda de verdade: o fim da saída se perde quando o painel fecha.
    assert!(AiMemory.ocupada(
        "ai-memory: another launcher owns this workstream; waiting briefly in case it is finalizing"
    ));
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

/// Contra o ai-memory de verdade, num projeto descartável: `cargo test -- --ignored mcp_tira`.
/// Rode sem as variáveis do Claude Code no ambiente (`env -u CLAUDE_CODE_SESSION_ID ...`), como o
/// daemon roda: foi assim que a ponte de stdio passou no teste e falhou no serviço, e é assim que o HTTP tem de passar.
/// Cria dois handoffs manuais, tira da fila, confere que a fila ficou vazia, devolve e confere
/// que eles voltaram com o conteúdo, na mesma ordem. Apaga o projeto no fim.
#[tokio::test]
#[ignore = "fala com o servidor do ai-memory desta máquina"]
async fn mcp_tira_e_devolve_handoffs_no_ai_memory_de_verdade() {
    let escopo = Escopo {
        workspace: "default".into(),
        project: format!("lukadispatch-teste-ponte-{}", agora()),
    };
    let mcp = Mcp::do_ai_memory().await.unwrap();
    for (resumo, passo) in [("primeiro", "a"), ("segundo", "b")] {
        mcp.chama(
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

#[test]
fn so_o_workstream_da_branch_segura_a_partida() {
    let w = wt("/wt/api/feat", "feat");
    let cwd = Path::new("/wt/api/feat");
    assert_eq!(
        AiMemory.segura_a_partida(&partida(cwd, Some(&w), false)),
        Duration::from_secs(7),
        "tem de caber a espera de 5 s do ai-memory por uma trava ocupada"
    );
    assert_eq!(
        AiMemory.segura_a_partida(&partida(cwd, Some(&w), true)),
        Duration::ZERO
    );
    assert_eq!(
        AiMemory.segura_a_partida(&partida(cwd, None, false)),
        Duration::ZERO
    );
}

#[test]
fn o_endpoint_sai_do_server_url_do_ai_memory() {
    let m = Mcp::da_url("http://127.0.0.1:49374", None).unwrap();
    assert_eq!(
        (m.host.as_str(), m.porta, m.caminho.as_str()),
        ("127.0.0.1", 49374, "/mcp")
    );
    assert_eq!(
        Mcp::da_url("http://mem.local/ai/", None).unwrap().caminho,
        "/ai/mcp"
    );
    assert_eq!(Mcp::da_url("http://h:1/mcp", None).unwrap().caminho, "/mcp");
    assert_eq!(Mcp::da_url("http://h", None).unwrap().porta, 80);
    assert!(
        Mcp::da_url("https://h", None).is_err(),
        "https não é falado aqui"
    );
}

#[test]
fn le_a_resposta_http_inteira_nos_tres_formatos() {
    let json = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"a\": 1}\n";
    assert_eq!(corpo_da_resposta(json).unwrap(), json!({"a": 1}));

    let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n4\r\n: 1}\r\n0\r\n\r\n";
    assert_eq!(corpo_da_resposta(chunked).unwrap(), json!({"a": 1}));

    let sse = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\nevent: message\r\ndata: {\"a\": 1}\r\n\r\n";
    assert_eq!(corpo_da_resposta(sse).unwrap(), json!({"a": 1}));

    let erro = b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 5\r\n\r\nnope!";
    let e = corpo_da_resposta(erro).unwrap_err();
    assert!(format!("{e:#}").contains("401"), "{e:#}");
}

#[tokio::test]
async fn chama_a_ferramenta_por_post_no_endpoint() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let escuta = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let porta = escuta.local_addr().unwrap().port();
    let servidor = tokio::spawn(async move {
        let (mut c, _) = escuta.accept().await.unwrap();
        let mut lido = Vec::new();
        let mut buf = [0u8; 4096];
        // Lê até o corpo inteiro chegar, pelo Content-Length do cabeçalho.
        loop {
            let n = c.read(&mut buf).await.unwrap();
            lido.extend_from_slice(&buf[..n]);
            let texto = String::from_utf8_lossy(&lido).into_owned();
            if let Some((cab, corpo)) = texto.split_once("\r\n\r\n") {
                let tamanho: usize = cab
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if corpo.len() >= tamanho {
                    break;
                }
            }
        }
        let corpo = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"handoff\": null}"}],"isError":false}}"#;
        let resposta = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{corpo}",
            corpo.len()
        );
        c.write_all(resposta.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&lido).into_owned()
    });
    let mcp = Mcp::da_url(&format!("http://127.0.0.1:{porta}"), Some("segredo".into())).unwrap();
    let r = mcp
        .chama("memory_handoff_accept", json!({"project": "x"}))
        .await
        .unwrap();
    assert_eq!(r, json!({"handoff": null}));
    let pedido = servidor.await.unwrap();
    assert!(pedido.starts_with("POST /mcp HTTP/1.1\r\n"), "{pedido}");
    assert!(pedido.contains("Authorization: Bearer segredo"), "{pedido}");
    assert!(
        pedido.contains(r#""name":"memory_handoff_accept""#),
        "{pedido}"
    );
}
