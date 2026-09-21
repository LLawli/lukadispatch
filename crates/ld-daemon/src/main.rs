//! `lukadispatchd`: o daemon.
//!
//! Sobe três coisas e nada mais: o socket de controle (por onde os hooks falam), o laço de
//! polling do Telegram e o arquivo de settings que as sessões do bot carregam. A partida falha
//! cedo e alto se o token ou o grupo estiverem errados, porque descobrir isso na primeira
//! mensagem, horas depois, é pior.

use std::sync::Arc;

use anyhow::{Context, Result};
use ld_core::config::Config;
use ld_core::{hooks, paths};
use tracing::info;
use tracing_subscriber::EnvFilter;

use ld_daemon::app::App;
use ld_daemon::telegram::Tg;
use ld_daemon::{poll, socket};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cfg = Config::load(&paths::config_file())?;
    // Modo offline: sobe só o socket e as sessões. Serve para testar o canal de entrada numa
    // máquina que ainda não tem bot, sem abrir mão de nada do resto (tmux, hooks, Monitor).
    let offline = std::env::var_os("LUKADISPATCH_OFFLINE").is_some();
    let token = if offline {
        String::new()
    } else {
        Config::telegram_token()?
    };
    if !offline && cfg.telegram.chat_id == 0 {
        anyhow::bail!(
            "falta o chat_id: ponha em {} ou em LUKADISPATCH_CHAT_ID",
            paths::config_file().display()
        );
    }
    if !offline && cfg.telegram.allowed_user_ids.is_empty() {
        anyhow::bail!(
            "allowed_user_ids está vazio em {}: sem isso o bot não obedeceria ninguém",
            paths::config_file().display()
        );
    }

    escreve_settings_das_sessoes()?;

    let store = ld_core::state::Store::open(&paths::state_db())?;
    let tg = Tg::new(token, cfg.telegram.chat_id);
    if offline {
        info!("modo offline: sem Telegram, só socket e sessões");
    } else {
        let eu = tg.preflight().await?;
        info!(bot = %eu, chat = cfg.telegram.chat_id, "conectado ao Telegram");
    }

    let app = Arc::new(App::new(cfg, store, tg));

    // Antes de qualquer coisa: o que sobrou de antes do restart pode já estar morto.
    match app.reconcile().await {
        Ok(n) if n > 0 => info!(mortas = n, "sessões órfãs encerradas na partida"),
        Ok(_) => {}
        Err(e) => tracing::warn!(erro = %e, "reconciliação falhou"),
    }

    let socket_app = app.clone();
    let caminho_socket = paths::socket();
    let socket = tokio::spawn(async move { socket::serve(socket_app, caminho_socket).await });
    let poll_app = app.clone();
    let poll = tokio::spawn(async move {
        if offline {
            // Nada de long polling sem token: ficaria batendo em 401 para sempre.
            std::future::pending::<()>().await;
        }
        poll::run(poll_app).await
    });

    // Relógio do painel: a contagem para o reset das janelas envelhece sozinha, então ele
    // precisa se redesenhar mesmo quando nada acontece.
    let painel_app = app.clone();
    tokio::spawn(async move {
        let mut tique = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tique.tick().await;
            // Uma sessão pode morrer a qualquer momento (crash, `exit` no PC): sem esta varredura
            // o painel mostraria uma sessão viva que não existe mais.
            if let Err(e) = painel_app.reconcile().await {
                tracing::warn!(erro = %e, "reconciliação falhou");
            }
            painel_app.panel.refresh();
        }
    });

    // Encerrar limpo importa: o socket é um arquivo, e deixá-lo para trás faz a próxima partida
    // ter que decidir se ele é lixo ou um daemon vivo.
    tokio::select! {
        r = socket => { r??; }
        _ = poll => {}
        _ = tokio::signal::ctrl_c() => { info!("sinal recebido, saindo"); }
    }

    std::fs::remove_file(paths::socket()).ok();
    Ok(())
}

/// Grava o `bot-settings.json` que as sessões carregam com `claude --settings`.
///
/// É reescrito a cada partida de propósito: assim, atualizar o lukadispatch atualiza os hooks
/// das próximas sessões sem você ter que lembrar de nada.
fn escreve_settings_das_sessoes() -> Result<()> {
    let destino = paths::bot_settings_file();
    if let Some(pai) = destino.parent() {
        std::fs::create_dir_all(pai)?;
    }
    let cli = paths::cli();
    let json = serde_json::to_string_pretty(&hooks::bot_settings(&cli))?;
    std::fs::write(&destino, json).with_context(|| format!("escrevendo {}", destino.display()))?;
    info!(settings = %destino.display(), cli = %cli, "settings das sessões atualizado");
    Ok(())
}
