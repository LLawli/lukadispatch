//! Testes da porta do agente, do envelope e do Claude Code: o script de partida, o embrulho do
//! `ai-memory`, e as escolhas (modo, modelo, esforço) e leituras (histórico, contexto, uso) que
//! o Claude Code faz por baixo da trait.
//!
//! Tudo roda num tempdir: `Locais` aponta o `~/.claude.json`, o `~/.claude` e o settings para
//! lá, e nada aqui toca a máquina de verdade nem sobe o `claude`.

use std::path::Path;

use ld_core::config::{Agente as CfgAgente, Project};
use ld_core::state::Session;

use super::claude_code::{ClaudeCode, Locais};
use super::*;

const ID: &str = "0123abcd-4567-89ef-0123-456789abcdef";

fn locais(raiz: &Path) -> Locais {
    Locais {
        cli: "/opt/ld/lukadispatch".into(),
        mcp_proxy: "/opt/ld/lukadispatch-mcp".into(),
        settings: raiz.join("bot-settings.json"),
        claude_json: raiz.join("claude.json"),
        claude_dir: raiz.join("claude"),
        uso_db: raiz.join("uso.db"),
    }
}

fn claude(raiz: &Path) -> ClaudeCode {
    ClaudeCode::new(locais(raiz), None)
}

fn projeto() -> Project {
    Project {
        name: "proj".into(),
        path: "/tmp/proj".into(),
        permission_mode: None,
        model: None,
        effort: None,
    }
}

fn telegram() -> DescricaoDoChat {
    DescricaoDoChat {
        plataforma: "Telegram".into(),
        onde: "o tópico \"proj\" de um grupo do Telegram".into(),
        teto_envio: 50 * 1024 * 1024,
        renderiza_markdown: false,
    }
}

fn pedido<'a>(p: &'a Project, chat: &'a DescricaoDoChat) -> PedidoDePartida<'a> {
    PedidoDePartida {
        projeto: p,
        permission_mode: "auto",
        model: None,
        effort: None,
        resume: None,
        retomada: false,
        wrap_mcp: false,
        chat,
    }
}

/// O valor que vem logo depois de uma flag na linha de comando.
fn depois_de<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))
        .map(String::as_str)
}

// ------------------------------------------------------------------ envelope

#[test]
fn ai_memory_embrulha_com_workstream_proprio_da_sessao() {
    let argv = AiMemory.embrulha(ID, vec!["claude".into(), "-x".into()]);
    assert_eq!(&argv[..3], ["ai-memory", "run", "--new"]);
    assert!(
        argv[3].starts_with("lukadispatch-0123abcd-"),
        "o workstream tem de ser achável pelo id: {}",
        argv[3]
    );
    assert_eq!(&argv[4..], ["claude", "-x"], "o agente vem inteiro depois");
}

#[test]
fn workstream_e_unico_por_partida() {
    // A mesma sessão sobe de novo na troca de modelo, e `--new` recusa nome repetido.
    let w = workstream(ID);
    let carimbo = w.strip_prefix("lukadispatch-0123abcd-").expect(&w);
    assert!(
        !carimbo.is_empty() && carimbo.chars().all(|c| c.is_ascii_digit()),
        "{w}"
    );
}

#[test]
fn direto_nao_mexe_em_nada() {
    let argv = vec!["claude".to_string(), "--x".into()];
    assert_eq!(Direto.embrulha(ID, argv.clone()), argv);
}

#[test]
fn config_escolhe_agente_e_envelope() {
    let p = da_config(&CfgAgente::default(), None).unwrap();
    assert_eq!(p.agente.nome(), "claude-code");
    assert_eq!(p.envelope.nome(), "ai-memory");

    let p = da_config(
        &CfgAgente {
            tipo: "claude-code".into(),
            envelope: "nenhum".into(),
        },
        None,
    )
    .unwrap();
    assert_eq!(p.envelope.nome(), "nenhum");
}

#[test]
fn nome_desconhecido_falha_na_partida_dizendo_qual() {
    let e = da_config(
        &CfgAgente {
            tipo: "codex".into(),
            envelope: "ai-memory".into(),
        },
        None,
    )
    .err()
    .expect("agente desconhecido");
    assert!(format!("{e:#}").contains("codex"), "{e:#}");

    let e = da_config(
        &CfgAgente {
            tipo: "claude-code".into(),
            envelope: "docker".into(),
        },
        None,
    )
    .err()
    .expect("envelope desconhecido");
    assert!(format!("{e:#}").contains("docker"), "{e:#}");
}

// ------------------------------------------------------------------ script de partida

#[test]
fn o_script_roda_cada_argumento_exatamente_como_veio() {
    // Aspas, espaço, aspa simples e quebra de linha no prompt: se o script montasse a linha com
    // aspas erradas, o agente receberia outra coisa (ou o shell executaria um pedaço dela).
    let dir = tempfile::tempdir().unwrap();
    let inv = Invocacao {
        argv: vec![
            "printf".into(),
            "%s|".into(),
            "a b".into(),
            "it's".into(),
            "$HOME".into(),
        ],
        prompt: Some("linha 1\nlinha \"2\" e 'três'".into()),
    };
    let p = escreve_partida(dir.path(), ID, &Direto, inv).unwrap();
    assert_eq!(p.session_id, ID);
    assert_eq!(p.log, dir.path().join("pane.log"));
    assert!(p.script.starts_with(dir.path()));

    let saida = std::process::Command::new("bash")
        .arg(&p.script)
        .output()
        .unwrap();
    assert!(saida.status.success(), "{saida:?}");
    assert_eq!(
        String::from_utf8_lossy(&saida.stdout),
        "a b|it's|$HOME|linha 1\nlinha \"2\" e 'três'|"
    );
}

#[test]
fn o_script_passa_pelo_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let inv = Invocacao {
        argv: vec!["claude".into()],
        prompt: None,
    };
    let p = escreve_partida(dir.path(), ID, &AiMemory, inv).unwrap();
    let script = std::fs::read_to_string(&p.script).unwrap();
    assert!(script.starts_with("#!/usr/bin/env bash"), "{script}");
    assert!(
        script.contains("exec 'ai-memory' 'run' '--new'"),
        "{script}"
    );
    assert!(script.contains("'claude'"), "{script}");
}

// ------------------------------------------------------------------ Claude Code: partida

#[test]
fn sessao_nova_cria_com_o_id_escolhido_e_carrega_os_ganchos() {
    let raiz = tempfile::tempdir().unwrap();
    let cc = claude(raiz.path());
    let (p, chat) = (projeto(), telegram());
    let inv = cc.invocacao(&pedido(&p, &chat), ID, raiz.path()).unwrap();

    assert_eq!(inv.argv[0], "claude");
    assert_eq!(depois_de(&inv.argv, "--session-id"), Some(ID));
    assert!(
        !inv.argv.iter().any(|a| a == "--resume"),
        "os dois juntos o Claude Code recusa"
    );
    assert_eq!(
        depois_de(&inv.argv, "--settings"),
        Some(raiz.path().join("bot-settings.json").to_str().unwrap())
    );
    assert_eq!(depois_de(&inv.argv, "-n"), Some("proj"));
    assert_eq!(depois_de(&inv.argv, "--permission-mode"), Some("auto"));

    let prompt = inv.prompt.expect("sessão nova tem prompt de partida");
    assert!(
        prompt.contains("select:Monitor"),
        "o Monitor é ferramenta diferida"
    );
    assert!(
        prompt.contains(&format!("/opt/ld/lukadispatch listen --session {ID}")),
        "{prompt}"
    );
    assert!(
        prompt.contains("@arquivo:"),
        "a sessão precisa saber devolver arquivo"
    );
}

#[test]
fn o_prompt_fala_do_chat_que_esta_em_uso_e_nao_de_um_fixo() {
    let raiz = tempfile::tempdir().unwrap();
    let cc = claude(raiz.path());
    let p = projeto();
    let chat = DescricaoDoChat {
        plataforma: "WhatsApp".into(),
        onde: "o grupo \"proj\" do WhatsApp".into(),
        teto_envio: 100 * 1024 * 1024,
        renderiza_markdown: true,
    };
    let prompt = cc
        .invocacao(&pedido(&p, &chat), ID, raiz.path())
        .unwrap()
        .prompt
        .unwrap();
    assert!(prompt.contains("o grupo \"proj\" do WhatsApp"), "{prompt}");
    assert!(prompt.contains("100 MB"), "o teto vem do chat: {prompt}");
    assert!(
        !prompt.contains("Telegram"),
        "sobrou Telegram fixo no prompt: {prompt}"
    );
    assert!(!prompt.contains("NÃO renderiza Markdown"), "{prompt}");

    let tg = telegram();
    let prompt = cc
        .invocacao(&pedido(&p, &tg), ID, raiz.path())
        .unwrap()
        .prompt
        .unwrap();
    assert!(prompt.contains("50 MB"), "{prompt}");
    assert!(
        prompt.contains("NÃO renderiza Markdown"),
        "sem Markdown, a tabela tem de ir como imagem: {prompt}"
    );
}

#[test]
fn retomar_continua_a_conversa_com_o_prompt_curto() {
    let raiz = tempfile::tempdir().unwrap();
    let cc = claude(raiz.path());
    let (p, chat) = (projeto(), telegram());
    let mut ped = pedido(&p, &chat);
    ped.resume = Some(ID);

    for (retomada, abertura) in [(true, "retomada"), (false, "reiniciada")] {
        ped.retomada = retomada;
        let inv = cc.invocacao(&ped, ID, raiz.path()).unwrap();
        assert_eq!(depois_de(&inv.argv, "--resume"), Some(ID));
        assert!(!inv.argv.iter().any(|a| a == "--session-id"));
        let prompt = inv.prompt.unwrap();
        assert!(
            prompt.contains("select:Monitor"),
            "o Monitor morre com o processo"
        );
        assert!(prompt.contains(abertura), "{prompt}");
        assert!(
            !prompt.contains("@arquivo:"),
            "o contexto já voltou; repetir o manual inteiro é ruído: {prompt}"
        );
    }
}

#[test]
fn perguntar_vira_dont_ask_e_padrao_nao_passa_flag() {
    let raiz = tempfile::tempdir().unwrap();
    let cc = claude(raiz.path());
    let (p, chat) = (projeto(), telegram());
    let mut ped = pedido(&p, &chat);

    ped.permission_mode = "perguntar";
    let inv = cc.invocacao(&ped, ID, raiz.path()).unwrap();
    assert_eq!(depois_de(&inv.argv, "--permission-mode"), Some("dontAsk"));

    for padrao in ["padrao", ""] {
        ped.permission_mode = padrao;
        let inv = cc.invocacao(&ped, ID, raiz.path()).unwrap();
        assert!(
            !inv.argv.iter().any(|a| a == "--permission-mode"),
            "{padrao:?}"
        );
    }
}

#[test]
fn modelo_e_esforco_viram_flag() {
    let raiz = tempfile::tempdir().unwrap();
    let cc = claude(raiz.path());
    let (p, chat) = (projeto(), telegram());
    let mut ped = pedido(&p, &chat);
    ped.model = Some("claude-opus-4-8[1m]");
    ped.effort = Some("high");
    let inv = cc.invocacao(&ped, ID, raiz.path()).unwrap();
    assert_eq!(depois_de(&inv.argv, "--model"), Some("claude-opus-4-8[1m]"));
    assert_eq!(depois_de(&inv.argv, "--effort"), Some("high"));
}

#[test]
fn servidor_mcp_de_comando_passa_pelo_proxy() {
    let raiz = tempfile::tempdir().unwrap();
    std::fs::write(
        raiz.path().join("claude.json"),
        r#"{"mcpServers": {"whats": {"command": "wamux", "args": ["mcp"]}}}"#,
    )
    .unwrap();
    let cc = claude(raiz.path());
    let (p, chat) = (projeto(), telegram());
    let mut ped = pedido(&p, &chat);
    ped.wrap_mcp = true;
    let dir = raiz.path().join("sessao");
    std::fs::create_dir_all(&dir).unwrap();

    let inv = cc.invocacao(&ped, ID, &dir).unwrap();
    let config = depois_de(&inv.argv, "--mcp-config").expect("sem --mcp-config");
    assert!(
        inv.argv.iter().any(|a| a == "--strict-mcp-config"),
        "sem strict o servidor sobe duas vezes"
    );
    let conteudo = std::fs::read_to_string(config).unwrap();
    assert!(conteudo.contains("/opt/ld/lukadispatch-mcp"), "{conteudo}");

    // Sem servidor nenhum, nada de config de MCP.
    std::fs::write(raiz.path().join("claude.json"), "{}").unwrap();
    let inv = cc.invocacao(&ped, ID, &dir).unwrap();
    assert!(!inv.argv.iter().any(|a| a == "--mcp-config"));
}

// ------------------------------------------------------------------ Claude Code: máquina

#[test]
fn prepara_escreve_os_ganchos_apontando_para_o_cli() {
    let raiz = tempfile::tempdir().unwrap();
    claude(raiz.path()).prepara().unwrap();
    let settings = std::fs::read_to_string(raiz.path().join("bot-settings.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&settings).expect("JSON válido");
    // O gancho é o CLI configurado com `hook <evento>` nos argumentos, separados de propósito:
    // comando e argumentos sem shell no meio (ver `ld_core::hooks`).
    let comandos: Vec<&serde_json::Value> = json["hooks"]
        .as_object()
        .expect("sem seção hooks")
        .values()
        .flat_map(|grupos| grupos.as_array().into_iter().flatten())
        .flat_map(|g| g["hooks"].as_array().into_iter().flatten())
        .collect();
    assert!(!comandos.is_empty(), "nenhum gancho escrito: {settings}");
    assert!(
        comandos
            .iter()
            .all(|h| h["command"] == "/opt/ld/lukadispatch" && h["args"][0] == "hook"),
        "{settings}"
    );
}

#[test]
fn confia_marca_a_pasta_no_claude_json() {
    let raiz = tempfile::tempdir().unwrap();
    std::fs::write(raiz.path().join("claude.json"), "{}").unwrap();
    let cc = claude(raiz.path());
    assert!(cc.confia(Path::new("/tmp/proj-novo")).unwrap());
    let json = std::fs::read_to_string(raiz.path().join("claude.json")).unwrap();
    assert!(json.contains("hasTrustDialogAccepted"), "{json}");
    assert!(
        !cc.confia(Path::new("/tmp/proj-novo")).unwrap(),
        "já confiada"
    );
}

// ------------------------------------------------------------------ Claude Code: escolhas

#[test]
fn modos_do_menu_e_modos_recusados() {
    let raiz = tempfile::tempdir().unwrap();
    let cc = claude(raiz.path());
    let menu: Vec<&str> = cc
        .modos()
        .iter()
        .filter(|m| m.no_menu)
        .map(|m| m.id)
        .collect();
    assert_eq!(menu, ["auto", "perguntar", "plan", "bypassPermissions"]);

    for ok in [
        "auto",
        "perguntar",
        "plan",
        "acceptEdits",
        "bypassPermissions",
        "dontAsk",
        "padrao",
    ] {
        assert!(cc.valida_modo(ok).is_ok(), "{ok}");
    }
    // O `manual` mostra o card e ignora a decisão: o prompt continua esperando teclado no PC.
    let e = cc.valida_modo("manual").unwrap_err();
    assert!(
        format!("{e:#}").contains("perguntar"),
        "a recusa aponta a saída: {e:#}"
    );
    assert!(cc.valida_modo("xyz").is_err());
}

#[test]
fn esforcos_e_nomes_de_modelo() {
    let raiz = tempfile::tempdir().unwrap();
    let cc = claude(raiz.path());
    assert_eq!(cc.esforcos(), ["low", "medium", "high", "xhigh", "max"]);
    for m in ["opus", "Sonnet", "haiku", "fable", "claude-opus-4-8[1m]"] {
        assert!(cc.e_nome_de_modelo(m), "{m}");
    }
    for nao in ["tintim", "high", "claude"] {
        assert!(!cc.e_nome_de_modelo(nao), "{nao}");
    }
}

// ------------------------------------------------------------------ Claude Code: conversa

#[test]
fn resposta_do_turno_pula_o_anuncio_de_monitor() {
    let raiz = tempfile::tempdir().unwrap();
    let cc = claude(raiz.path());
    let t = raiz.path().join("t.jsonl");
    std::fs::write(
        &t,
        [
            r#"{"type":"user","message":{"role":"user","content":"roda os testes"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"rodei, passaram"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Monitor rearmado."}]}}"#,
        ]
        .join("\n"),
    )
    .unwrap();

    assert_eq!(
        cc.resposta_do_turno(Some("a resposta"), Some(&t))
            .as_deref(),
        Some("a resposta"),
        "a última fala, quando é resposta, vale"
    );
    assert_eq!(
        cc.resposta_do_turno(Some("Monitor rearmado."), Some(&t))
            .as_deref(),
        Some("rodei, passaram"),
        "o anúncio de re-arme não é resposta"
    );
    assert_eq!(cc.resposta_do_turno(Some("Monitor rearmado."), None), None);
    assert_eq!(cc.resposta_do_turno(None, None), None);
}

#[test]
fn leituras_sem_dado_nao_derrubam_nada() {
    let raiz = tempfile::tempdir().unwrap();
    let cc = claude(raiz.path());
    let s = Session {
        session_id: ID.into(),
        project: "proj".into(),
        cwd: "/tmp/proj".into(),
        transcript_path: Some("/nao/existe.jsonl".into()),
        tmux: None,
        canal_id: None,
        status: "ocioso".into(),
        status_msg_id: None,
        model: None,
        effort: None,
        permission_mode: None,
        created_at: 0,
        ended_at: None,
    };
    assert!(cc.historico(&s, 8).is_empty());
    assert!(cc.contexto(&s).is_none());
    assert!(cc.modelo_da_sessao(&s).is_none());
    assert!(cc.ultima_sessao("/tmp/proj").is_none());
    assert!(cc.tokens_da_sessao(ID).is_none());
    let _ = cc.uso();
}

#[test]
fn texto_injetado_nao_e_fala_digitada() {
    let raiz = tempfile::tempdir().unwrap();
    let cc = claude(raiz.path());
    assert!(cc.e_fala_digitada("roda os testes"));
    assert!(!cc.e_fala_digitada(&format!(
        "{}\nfaça isto",
        ld_core::transcript::MARCA_SISTEMA
    )));
}

#[test]
fn claude_code_cabe_atras_da_trait() {
    let raiz = tempfile::tempdir().unwrap();
    let _: Arc<dyn Agente> = Arc::new(claude(raiz.path()));
    let _: Arc<dyn Envelope> = Arc::new(AiMemory);
}
