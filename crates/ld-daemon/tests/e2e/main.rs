//! A suíte e2e: o daemon inteiro, com eventos na forma que o Telegram manda.
//!
//! Tudo roda de verdade menos as duas pontas que não existem no CI: o Telegram (um Bot API de
//! mentira no próprio teste, [`bot_api`]) e o Claude Code (um dublê que dispara os ganchos do
//! settings e lê o canal pelo `listen`, `tests/e2e/claude`). No meio estão o binário do daemon, o
//! adaptador do teloxide, o socket, o CLI dos ganchos, o git e o hospedeiro, que é o tmux ou o
//! herdr. Cada cenário roda nos dois.
//!
//! O que os testes de fluxo (`tests/fluxos.rs`) não pegam e este pega: a tradução do Telegram de
//! ponta a ponta, o settings de ganchos que o daemon gera, o ambiente que chega à sessão, o
//! hospedeiro de verdade e o restart do daemon.
//!
//! Sem tmux, herdr ou python3 na máquina, o cenário é pulado, a não ser com
//! `LUKADISPATCH_E2E_EXIGE` no ambiente, que o CI liga.

// O daemon sem o frontend do Telegram não tem com quem o Bot API de mentira falar.
#![cfg(feature = "telegram")]

mod bot_api;
mod cena;

use cena::Cena;

macro_rules! nos_dois_hospedeiros {
    ($($cenario:ident),* $(,)?) => {$(
        mod $cenario {
            #[tokio::test(flavor = "multi_thread")]
            async fn tmux() {
                super::$cenario("tmux").await
            }

            #[tokio::test(flavor = "multi_thread")]
            async fn herdr() {
                super::$cenario("herdr").await
            }
        }
    )*};
}

nos_dois_hospedeiros!(
    conversa_numa_worktree_e_kill_apaga_tudo,
    trocar_o_modo_relanca_e_a_permissao_vem_pelo_card,
    o_daemon_reinicia_e_a_sessao_continua,
);

/// Abre uma sessão do `repo` na branch pelo General, e devolve o tópico dela depois de a sessão
/// responder a primeira mensagem.
async fn abre_e_conversa(c: &Cena, branch: &str) -> i32 {
    c.api.no_principal(&format!("/new repo {branch}"));
    let topico = c.espera_topico(&format!("repo · {branch}")).await;
    // O tópico nasce antes da sessão; é a apresentação dela que diz que dá para falar.
    c.espera_mensagem(Some(topico), "Pode falar").await;
    c.api.no_topico(topico, "oi, tudo bem?");
    c.espera_mensagem(Some(topico), "eco: oi, tudo bem?").await;
    topico
}

async fn conversa_numa_worktree_e_kill_apaga_tudo(hospedeiro: &'static str) {
    let Some(c) = Cena::sobe(hospedeiro).await else {
        return;
    };
    let topico = abre_e_conversa(&c, "feat").await;

    // A sessão roda na worktree da branch, e não na pasta do repositório.
    let partidas = c.partidas();
    assert_eq!(partidas.len(), 1, "{partidas:?}");
    let wt = c.worktree("feat");
    assert_eq!(partidas[0]["cwd"], wt.to_string_lossy().as_ref());
    assert!(c.existe_branch("feat"));
    let argv: Vec<&str> = partidas[0]["argv"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a.as_str())
        .collect();
    assert!(argv.contains(&"--session-id"), "{argv:?}");

    c.api.no_topico(topico, "/kill");
    c.toca("apagar worktree e branch").await;
    c.espera_topico_apagado(topico).await;
    c.espera_fim(partidas[0]["pid"].as_u64().unwrap()).await;
    c.ate("a worktree sumir", || (!wt.exists()).then_some(()))
        .await;
    assert!(!c.existe_branch("feat"), "a branch ficou");
}

async fn trocar_o_modo_relanca_e_a_permissao_vem_pelo_card(hospedeiro: &'static str) {
    let Some(c) = Cena::sobe(hospedeiro).await else {
        return;
    };
    let topico = abre_e_conversa(&c, "perm").await;

    // O modo é flag de partida: trocar relança a mesma conversa com --resume.
    c.api.no_topico(topico, "/mode perguntar");
    let partidas = c.espera_partidas(2).await;
    c.espera_fim(partidas[0]["pid"].as_u64().unwrap()).await;
    let sessao = c.partidas()[0]["argv"]
        .as_array()
        .unwrap()
        .windows(2)
        .find(|w| w[0] == "--session-id")
        .map(|w| w[1].clone())
        .expect("a primeira partida não tinha --session-id");
    let argv = partidas[1]["argv"].as_array().unwrap();
    let par = |flag: &str| argv.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone());
    assert_eq!(par("--resume"), Some(sessao), "{argv:?}");
    assert_eq!(par("--permission-mode"), Some("dontAsk".into()), "{argv:?}");

    // No modo de perguntar, o Bash para no portão (o gancho de PreToolUse, que bloqueia) até o
    // toque no card do celular.
    c.api.no_topico(topico, "rode echo feito-pelo-bash");
    c.toca("Permitir").await;
    c.espera_mensagem(Some(topico), "rodei: feito-pelo-bash")
        .await;
}

async fn o_daemon_reinicia_e_a_sessao_continua(hospedeiro: &'static str) {
    let Some(mut c) = Cena::sobe(hospedeiro).await else {
        return;
    };
    let topico = abre_e_conversa(&c, "restart").await;

    // Escrita com o daemon fora do ar (um deploy, um restart do serviço): o Telegram guarda, e o
    // daemon novo a recebe quando a sessão ainda nem voltou a ouvir.
    c.para_daemon();
    c.api.no_topico(topico, "chegou durante o restart?");
    c.inicia_daemon().await;
    c.espera_mensagem(Some(topico), "eco: chegou durante o restart?")
        .await;

    c.api.no_topico(topico, "e depois?");
    c.espera_mensagem(Some(topico), "eco: e depois?").await;

    // Continuou a mesma sessão: nada foi relançado, e o processo é o mesmo.
    let partidas = c.partidas();
    assert_eq!(partidas.len(), 1, "{partidas:?}");
    let pid = partidas[0]["pid"].as_u64().unwrap();
    assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
}
