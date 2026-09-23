//! Testes do setup: os seletores, o rascunho do config e do `.env`, o passo do Telegram contra um
//! bot roteirizado e a conversa inteira terminando num config que o daemon aceita.

use std::collections::VecDeque;
use std::io::Cursor;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use ld_core::config::Config;

use super::arquivos::{CHAVE_TOKEN, Rascunho, raizes_sugeridas};
use super::pecas::{self, Peca, le_versao};
use super::tela::Tela;
use super::telegram::{self, ApiDoBot, Direitos, InfoBot, InfoChat, Remetente, Visto};
use super::*;

const TOKEN: &str = "123456:bom";
const BOT_ID: u64 = 999;
const EU: u64 = 5550001234;
const GRUPO_VELHO: i64 = -5550009999;
const GRUPO_NOVO: i64 = -1005550009999;

fn args(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

fn selecao(s: &str) -> Selecao {
    match le_argumentos(&args(s)).unwrap() {
        Pedido::Setup(sel) => sel,
        Pedido::Ajuda => panic!("pediu ajuda"),
    }
}

#[test]
fn sem_seletor_sao_os_padroes_que_existem() {
    let sel = selecao("");
    assert_eq!(sel, Selecao::default());
    let nomes: Vec<&str> = pecas(&sel).unwrap().iter().map(|p| p.nome()).collect();
    assert_eq!(nomes, ["tmux", "claude-code", "ai-memory", "telegram"]);
}

#[test]
fn seletores_aceitam_espaco_igual_e_apelido() {
    let sel = selecao("--agent claude --envelope=nenhum --session tmux --frontend telegram");
    let nomes: Vec<&str> = pecas(&sel).unwrap().iter().map(|p| p.nome()).collect();
    assert_eq!(nomes, ["tmux", "claude-code", "nenhum", "telegram"]);
}

#[test]
fn implementacao_que_nao_existe_falha_dizendo_as_que_existem() {
    for (arg, esperado) in [
        ("--frontend whatsapp", "telegram"),
        ("--agent codex", "claude-code"),
        ("--session herdr", "tmux"),
        ("--envelope ai-jail", "ai-memory, nenhum"),
    ] {
        let e = pecas(&selecao(arg)).err().expect(arg);
        let msg = format!("{e:#}");
        let (opcao, nome) = arg.split_once(' ').unwrap();
        assert!(msg.contains(opcao) && msg.contains(nome), "{msg}");
        assert!(msg.contains(esperado), "{msg}");
    }
}

#[test]
fn opcao_errada_ou_sem_valor_e_erro_e_help_e_ajuda() {
    assert!(le_argumentos(&args("--frontnd telegram")).is_err());
    assert!(le_argumentos(&args("--agent")).is_err());
    assert_eq!(le_argumentos(&args("--help")).unwrap(), Pedido::Ajuda);
}

#[test]
fn config_novo_sai_do_exemplo_e_carrega() {
    let mut r = Rascunho::de(None, None).unwrap();
    assert!(!r.havia_config);
    r.poe(None, "usuario", "Maria");
    r.poe(Some("telegram"), "chat_id", GRUPO_NOVO);
    r.inclui_id("telegram", "allowed_user_ids", EU as i64);
    r.poe(Some("agente"), "envelope", "nenhum");
    let texto = r.config_texto();
    assert!(
        texto.contains("# Modo de permissão"),
        "os comentários do exemplo ficam: {texto}"
    );
    let cfg: Config = toml::from_str(&texto).unwrap();
    assert_eq!(cfg.usuario.as_deref(), Some("Maria"));
    assert_eq!(cfg.telegram.chat_id, GRUPO_NOVO);
    // O exemplo traz 123456789 de enfeite; o setup não pode deixar um estranho na allowlist.
    assert_eq!(cfg.telegram.allowed_user_ids, vec![EU as i64]);
    assert_eq!(cfg.agente.envelope, "nenhum");
}

#[test]
fn config_existente_e_editado_no_lugar() {
    let antes =
        "# meu comentário\nwrap_mcp = false\n\n[telegram]\nchat_id = 1\nallowed_user_ids = [42]\n";
    let mut r = Rascunho::de(Some(antes), None).unwrap();
    assert!(r.havia_config);
    r.poe(Some("telegram"), "chat_id", GRUPO_NOVO);
    r.inclui_id("telegram", "allowed_user_ids", 42);
    r.inclui_id("telegram", "allowed_user_ids", EU as i64);
    let texto = r.config_texto();
    assert!(
        texto.starts_with("# meu comentário\nwrap_mcp = false"),
        "{texto}"
    );
    let cfg: Config = toml::from_str(&texto).unwrap();
    assert!(!cfg.wrap_mcp);
    assert_eq!(cfg.telegram.allowed_user_ids, vec![42, EU as i64]);
}

#[test]
fn tabela_nova_vira_secao_e_valor_trocado_guarda_o_comentario() {
    // Medido rodando o setup contra o config real: sem cuidado, o toml_edit escreve
    // `agente = { ... }` na raiz e apaga o comentário ao lado do valor trocado.
    // E o comentário de cima de uma chave mora na chave, que um insert trocaria junto.
    let antes =
        "# cabeçalho\nusuario = \"Ana\"\n\n[telegram]\nchat_id = -1          # grupo \"Casa\"\n";
    let mut r = Rascunho::de(Some(antes), None).unwrap();
    r.poe(None, "usuario", "Maria");
    r.poe(Some("telegram"), "chat_id", GRUPO_NOVO);
    r.poe(Some("agente"), "envelope", "nenhum");
    let texto = r.config_texto();
    assert!(
        texto.contains(&format!("chat_id = {GRUPO_NOVO}          # grupo \"Casa\"")),
        "{texto}"
    );
    assert!(
        texto.contains("\n[agente]\nenvelope = \"nenhum\""),
        "{texto}"
    );
    assert!(
        texto.starts_with("# cabeçalho\nusuario = \"Maria\""),
        "{texto}"
    );

    // Tabela inline que já existe continua inline, com as chaves que tinha.
    let mut r = Rascunho::de(Some("agente = { tipo = \"claude-code\" }\n"), None).unwrap();
    r.poe(Some("agente"), "envelope", "nenhum");
    let cfg: Config = toml::from_str(&r.config_texto()).unwrap();
    assert_eq!(cfg.agente.tipo, "claude-code");
    assert_eq!(cfg.agente.envelope, "nenhum");
}

#[test]
fn config_que_nao_carrega_para_o_setup_antes_de_perguntar() {
    let e = Rascunho::de(Some("[telegram]\nchat_id = \"texto\"\n"), None)
        .err()
        .expect("config inválido");
    assert!(format!("{e:#}").contains("não carrega"), "{e:#}");
}

#[test]
fn env_troca_a_linha_do_token_sem_mexer_no_resto() {
    let mut r = Rascunho::de(
        None,
        Some("# nota\nRUST_LOG=debug\nLUKADISPATCH_TELEGRAM_TOKEN=velho\n"),
    )
    .unwrap();
    assert_eq!(r.valor_env(CHAVE_TOKEN).as_deref(), Some("velho"));
    r.poe_env(CHAVE_TOKEN, "novo");
    assert_eq!(
        r.env_texto(),
        "# nota\nRUST_LOG=debug\nLUKADISPATCH_TELEGRAM_TOKEN=novo\n"
    );

    let mut r = Rascunho::de(None, None).unwrap();
    assert_eq!(
        r.valor_env(CHAVE_TOKEN),
        None,
        "o exemplo vem com o token vazio"
    );
    r.poe_env(CHAVE_TOKEN, "novo");
    assert_eq!(
        r.env_texto()
            .matches("LUKADISPATCH_TELEGRAM_TOKEN=")
            .count(),
        1,
        "troca a linha do exemplo em vez de acrescentar outra"
    );
}

#[test]
fn raizes_sugeridas_sao_as_pastas_com_mais_repositorios() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    for repo in [
        "code/a",
        "code/b",
        "estudos/c",
        "fotos/nada",
        ".escondida/d",
    ] {
        std::fs::create_dir_all(h.join(repo)).unwrap();
    }
    for repo in ["code/a", "code/b", "estudos/c", ".escondida/d"] {
        std::fs::create_dir(h.join(repo).join(".git")).unwrap();
    }
    assert_eq!(raizes_sugeridas(h), vec![h.join("code"), h.join("estudos")]);
}

#[test]
fn versao_do_claude_se_le_da_saida_de_version() {
    assert_eq!(le_versao("2.1.280 (Claude Code)\n"), Some((2, 1, 280)));
    assert!(le_versao("2.1.273 (Claude Code)").unwrap() < (2, 1, 274));
    assert_eq!(le_versao("lixo"), None);
}

// --- o Telegram, contra um bot roteirizado ---

#[derive(Default)]
struct Roteiro {
    /// A resposta de cada `getMe` para `le_grupo_todo`; a última se repete.
    privacidade: VecDeque<bool>,
    lotes: VecDeque<Vec<Visto>>,
    forum: VecDeque<bool>,
    direitos: VecDeque<(bool, bool, bool)>,
    esperas: usize,
}

fn proximo<T: Copy>(fila: &mut VecDeque<T>) -> T {
    if fila.len() > 1 {
        fila.pop_front().unwrap()
    } else {
        *fila.front().expect("roteiro vazio")
    }
}

struct Falso {
    token: String,
    roteiro: Arc<Mutex<Roteiro>>,
}

#[async_trait]
impl ApiDoBot for Falso {
    async fn quem_sou(&self) -> Result<InfoBot> {
        if self.token != TOKEN {
            bail!("Unauthorized");
        }
        let le = proximo(&mut self.roteiro.lock().unwrap().privacidade);
        Ok(InfoBot {
            id: BOT_ID,
            usuario: "meu_bot".into(),
            le_grupo_todo: le,
        })
    }

    async fn marca_inicio(&self) -> Result<i32> {
        Ok(10)
    }

    async fn espera(&self, desde: i32) -> Result<(Vec<Visto>, i32)> {
        let mut r = self.roteiro.lock().unwrap();
        r.esperas += 1;
        let lote = r
            .lotes
            .pop_front()
            .ok_or_else(|| anyhow!("o roteiro acabou"))?;
        Ok((lote, desde + 1))
    }

    async fn chat(&self, id: i64) -> Result<InfoChat> {
        // O id velho responde pelo novo, como o Telegram faz depois da migração.
        if id != GRUPO_VELHO && id != GRUPO_NOVO {
            bail!("chat not found");
        }
        let forum = proximo(&mut self.roteiro.lock().unwrap().forum);
        Ok(InfoChat {
            id: GRUPO_NOVO,
            titulo: "Sessões".into(),
            forum,
        })
    }

    async fn direitos(&self, chat: i64, bot: u64) -> Result<Direitos> {
        assert_eq!((chat, bot), (GRUPO_NOVO, BOT_ID));
        let (admin, topicos, apagar) = proximo(&mut self.roteiro.lock().unwrap().direitos);
        Ok(Direitos {
            admin,
            topicos,
            apagar,
        })
    }
}

fn visto(chat_id: i64, grupo: bool, de: u64, bot: bool, migrou: Option<i64>) -> Visto {
    Visto {
        chat_id,
        grupo,
        titulo: "Sessões".into(),
        migrou_para: migrou,
        de: Some(Remetente {
            id: de,
            nome: "Maria Silva".into(),
            bot,
        }),
    }
}

/// O roteiro que passa por todos os laços do passo do Telegram.
fn roteiro_completo() -> Roteiro {
    Roteiro {
        privacidade: [false, true].into(),
        lotes: [
            vec![visto(EU as i64, false, EU, false, None)],
            vec![visto(GRUPO_VELHO, true, 1087968824, true, None)],
            vec![
                visto(GRUPO_VELHO, true, BOT_ID, true, None),
                visto(GRUPO_VELHO, true, EU, false, Some(GRUPO_NOVO)),
            ],
        ]
        .into(),
        forum: [false, true].into(),
        direitos: [
            (false, false, false),
            (true, true, false),
            (true, true, true),
        ]
        .into(),
        esperas: 0,
    }
}

/// Roda `f` com uma tela roteirizada e devolve o que ela escreveu.
async fn com_tela<F>(respostas: &str, f: F) -> (Result<()>, String)
where
    F: AsyncFnOnce(&mut Tela<'_>) -> Result<()>,
{
    let mut entrada = Cursor::new(respostas.as_bytes().to_vec());
    let mut saida = Vec::new();
    let r = {
        let mut tela = Tela::nova(&mut entrada, &mut saida, false);
        f(&mut tela).await
    };
    (r, String::from_utf8(saida).unwrap())
}

fn conecta_com(roteiro: &Arc<Mutex<Roteiro>>) -> impl Fn(&str) -> Arc<dyn ApiDoBot> + use<> {
    let roteiro = roteiro.clone();
    move |token: &str| {
        Arc::new(Falso {
            token: token.to_string(),
            roteiro: roteiro.clone(),
        }) as Arc<dyn ApiDoBot>
    }
}

#[tokio::test]
async fn telegram_do_zero_passa_por_todos_os_lacos() {
    let roteiro = Arc::new(Mutex::new(roteiro_completo()));
    let conecta = conecta_com(&roteiro);
    let mut r = Rascunho::de(None, None).unwrap();
    // Token errado, o certo, Enter depois do Group Privacy, do fórum e das duas rodadas de
    // direitos.
    let respostas = "errado\n123456:bom\n\n\n\n\n";
    let (res, tela) = com_tela(respostas, async |t| {
        telegram::configura(t, &mut r, &conecta).await
    })
    .await;
    res.unwrap();

    for trecho in [
        "@BotFather",
        "recusou esse token",
        "Group Privacy",
        "privado do bot",
        "admin anônimo",
        "sem tópicos",
        "Gerenciar tópicos",
        "Apagar mensagens",
        "pronto para o @meu_bot",
    ] {
        assert!(
            tela.contains(trecho),
            "faltou {trecho:?} na conversa:\n{tela}"
        );
    }
    assert!(!tela.contains(TOKEN), "o token não pode aparecer na tela");

    let cfg: Config = toml::from_str(&r.config_texto()).unwrap();
    assert_eq!(cfg.frontend, "telegram");
    assert_eq!(
        cfg.telegram.chat_id, GRUPO_NOVO,
        "vale o id depois da migração"
    );
    assert!(cfg.telegram.allowed_user_ids.contains(&(EU as i64)));
    assert!(
        !cfg.telegram.allowed_user_ids.contains(&(BOT_ID as i64)),
        "a mensagem do bot não é de quem ele obedece"
    );
    assert_eq!(r.valor_env(CHAVE_TOKEN).as_deref(), Some(TOKEN));
    assert_eq!(r.nome_sugerido.as_deref(), Some("Maria"));
}

#[tokio::test]
async fn rodar_de_novo_reaproveita_bot_e_grupo_sem_esperar_mensagem() {
    let roteiro = Arc::new(Mutex::new(Roteiro {
        privacidade: [true].into(),
        forum: [true].into(),
        direitos: [(true, true, true)].into(),
        ..Default::default()
    }));
    let conecta = conecta_com(&roteiro);
    let config = format!("[telegram]\nchat_id = {GRUPO_NOVO}\nallowed_user_ids = [{EU}]\n");
    let env = format!("LUKADISPATCH_TELEGRAM_TOKEN={TOKEN}\n");
    let mut r = Rascunho::de(Some(&config), Some(&env)).unwrap();
    let (res, tela) = com_tela("\n\n", async |t| {
        telegram::configura(t, &mut r, &conecta).await
    })
    .await;
    res.unwrap();
    assert!(
        tela.contains("Já existe um bot configurado, @meu_bot"),
        "{tela}"
    );
    assert!(tela.contains("\"Sessões\" já está configurado"), "{tela}");
    assert_eq!(roteiro.lock().unwrap().esperas, 0);
    let cfg: Config = toml::from_str(&r.config_texto()).unwrap();
    assert_eq!(cfg.telegram.allowed_user_ids, vec![EU as i64]);
}

/// O passo do Telegram de verdade, com a API roteirizada no lugar do teloxide.
struct TelegramFalso(Arc<Mutex<Roteiro>>);

#[async_trait(?Send)]
impl Peca for TelegramFalso {
    fn nome(&self) -> &'static str {
        "telegram"
    }

    async fn configura(&self, tela: &mut Tela<'_>, r: &mut Rascunho) -> Result<()> {
        telegram::configura(tela, r, &conecta_com(&self.0)).await
    }
}

#[tokio::test]
async fn a_conversa_inteira_termina_num_config_que_o_daemon_aceita() {
    let home = tempfile::tempdir().unwrap();
    let h = home.path();
    std::fs::create_dir_all(h.join("code/api/.git")).unwrap();

    let roteiro = Arc::new(Mutex::new(roteiro_completo()));
    let pecas: Vec<Box<dyn Peca>> = vec![
        pecas::hospedeiro("tmux").unwrap(),
        pecas::agente("claude").unwrap(),
        pecas::envelope("ai-memory").unwrap(),
        Box::new(TelegramFalso(roteiro.clone())),
    ];
    let mut r = Rascunho::de(None, None).unwrap();
    // Só o tmux existe nesta máquina de mentira: o claude falta (vira aviso) e o ai-memory
    // também (vira a pergunta de rodar sem ele).
    r.tem_programa = Box::new(|p| p == "tmux");

    let respostas = [
        "s",          // rodar sem o ai-memory
        "errado",     // token
        "123456:bom", // token
        "",
        "",
        "",
        "",  // Group Privacy, fórum, direitos duas vezes
        "",  // nome: fica o do Telegram
        "",  // raízes: fica a sugerida
        "2", // modo: perguntar
        "s", // desligar a transcrição, que não existe nesta home
    ]
    .join("\n")
        + "\n";
    let (res, tela) = com_tela(&respostas, async |t| conduz(t, &mut r, &pecas, h).await).await;
    res.unwrap();
    assert!(tela.contains("claude) não está no PATH"), "{tela}");

    let cfg: Config = toml::from_str(&r.config_texto()).unwrap();
    assert_eq!(cfg.frontend, "telegram");
    assert_eq!(cfg.hospedeiro, "tmux");
    assert_eq!(cfg.agente.tipo, "claude-code");
    assert_eq!(cfg.agente.envelope, "nenhum");
    assert_eq!(cfg.usuario.as_deref(), Some("Maria"));
    assert_eq!(cfg.scan.roots, vec!["~/code".to_string()]);
    assert_eq!(cfg.default_permission_mode, "perguntar");
    assert!(!cfg.transcricao.ativa);
    assert_eq!(cfg.telegram.chat_id, GRUPO_NOVO);
    assert_eq!(cfg.telegram.allowed_user_ids, vec![EU as i64]);

    // O que o daemon faz na partida com este config.
    crate::agente::da_config(&cfg).unwrap();
    crate::sessions::da_config(&cfg.hospedeiro).unwrap();
    crate::transcritor::da_config(&cfg.transcricao).unwrap();
    crate::divisor::Divisores::da_config(&cfg.arquivos).unwrap();
}

#[test]
fn grava_guarda_o_anterior_e_respeita_o_modo() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let env = dir.path().join("sub/.env");
    arquivos::grava(&env, "A=1\n", 0o600).unwrap();
    arquivos::grava(&env, "A=2\n", 0o600).unwrap();
    assert_eq!(std::fs::read_to_string(&env).unwrap(), "A=2\n");
    let bak = dir.path().join("sub/.env.bak");
    assert_eq!(std::fs::read_to_string(&bak).unwrap(), "A=1\n");
    for p in [&env, &bak] {
        let modo = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(modo, 0o600, "{}", p.display());
    }
    assert!(!Path::new(&format!("{}.novo", env.display())).exists());
}

#[cfg(feature = "telegram")]
mod payloads {
    //! A tradução de update para [`Visto`], com o JSON no formato que o Telegram manda.

    use super::super::telegram::teloxide_api::visto;
    use super::*;
    use teloxide::types::Update;

    fn update(json: &str) -> Update {
        // `from_str`, e não `from_value`: o Deserialize do UpdateKind perde a chave pelo
        // segundo caminho (docs/armadilhas.md).
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn mensagem_no_grupo() {
        let u = update(
            r#"{"update_id":11,"message":{"message_id":5,"date":1790000000,
            "chat":{"id":-5550009999,"title":"Sessões","type":"group"},
            "from":{"id":5550001234,"is_bot":false,"first_name":"Maria","last_name":"Silva"},
            "text":"oi"}}"#,
        );
        let v = visto(&u).unwrap();
        assert_eq!(v, visto_esperado(-5550009999, None));
    }

    #[test]
    fn grupo_que_virou_supergrupo_diz_o_id_novo() {
        let u = update(
            r#"{"update_id":12,"message":{"message_id":6,"date":1790000000,
            "chat":{"id":-5550009999,"title":"Sessões","type":"group"},
            "from":{"id":5550001234,"is_bot":false,"first_name":"Maria","last_name":"Silva"},
            "migrate_to_chat_id":-1005550009999}}"#,
        );
        assert_eq!(
            visto(&u).unwrap(),
            visto_esperado(-5550009999, Some(-1005550009999))
        );
    }

    #[test]
    fn mensagem_no_privado_nao_e_grupo() {
        let u = update(
            r#"{"update_id":13,"message":{"message_id":7,"date":1790000000,
            "chat":{"id":5550001234,"first_name":"Maria","type":"private"},
            "from":{"id":5550001234,"is_bot":false,"first_name":"Maria"},
            "text":"/start"}}"#,
        );
        assert!(!visto(&u).unwrap().grupo);
    }

    fn visto_esperado(chat: i64, migrou: Option<i64>) -> Visto {
        Visto {
            chat_id: chat,
            grupo: true,
            titulo: "Sessões".into(),
            migrou_para: migrou,
            de: Some(Remetente {
                id: EU,
                nome: "Maria Silva".into(),
                bot: false,
            }),
        }
    }
}
