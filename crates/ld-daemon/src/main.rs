//! `lukadispatchd`: o daemon.
//!
//! Sobe três coisas e nada mais: o socket de controle (por onde os hooks falam), o roteador de
//! eventos do frontend e o arquivo de settings que as sessões do bot carregam. A partida falha
//! cedo e alto se a configuração estiver errada, porque descobrir isso na primeira mensagem,
//! horas depois, é pior.
//!
//! Qual frontend, transcritor, divisor e hospedeiro sobem é decidido só aqui: o resto do daemon
//! fala com as traits, nunca com o nome escolhido no config.

use std::sync::Arc;

use anyhow::Result;
use ld_core::config::Config;
use ld_core::paths;
use tracing::info;
use tracing_subscriber::EnvFilter;

use ld_daemon::agente;
use ld_daemon::app::{App, Portas};
use ld_daemon::divisor::Divisores;
use ld_daemon::frontend::Frontend;
use ld_daemon::frontend::nulo::Nulo;
use ld_daemon::sessions::Tmux;
use ld_daemon::{roteador, socket, transcritor};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cfg = Config::load(&paths::config_file())?;

    let pecas = agente::da_config(&cfg)?;
    pecas.agente.prepara()?;

    let store = ld_core::state::Store::open(&paths::state_db())?;

    let frontend = monta_frontend(&cfg)?;
    // O nulo é barato de sobra (não fala com rede nenhuma); os outros custam uma chamada de
    // verdade, e o erro na partida é o que evita descobrir token errado na primeira mensagem.
    if frontend.nome() != "nulo" {
        let eu = frontend.confere().await?;
        info!(frontend = frontend.nome(), quem = %eu, "frontend conectado");
    } else {
        info!("modo offline: frontend nulo, só socket e sessões");
    }

    let transcritor = transcritor::da_config(&cfg.transcricao)?;
    info!(
        transcritor = transcritor
            .as_ref()
            .map(|t| t.nome())
            .unwrap_or("desligado"),
        "transcrição configurada"
    );

    let divisores = Divisores::da_config(&cfg.arquivos)?;
    info!(divisores = ?divisores.nomes(), "divisores configurados");

    info!(
        agente = pecas.agente.nome(),
        envelope = pecas.envelope.nome(),
        "agente configurado"
    );

    let app = Arc::new(App::new(
        cfg,
        store,
        Portas {
            frontend,
            agente: pecas.agente,
            envelope: pecas.envelope,
            transcritor,
            divisores,
            hospedeiro: Arc::new(Tmux),
        },
    ));

    // Lê o catálogo já na partida: assim um binário do Claude Code que não dá para varrer
    // aparece no log do serviço, e não seis horas depois, quando você mandar /model e o teclado
    // vier vazio.
    let quantos = app.modelos().len();
    if quantos == 0 {
        tracing::warn!("sem catálogo de modelos: /model vai pedir o nome inteiro");
    }

    // Antes de qualquer coisa: o que sobrou de antes do restart pode já estar morto.
    match app.reconcile().await {
        Ok(n) if n > 0 => info!(mortas = n, "sessões órfãs encerradas na partida"),
        Ok(_) => {}
        Err(e) => tracing::warn!(erro = %e, "reconciliação falhou"),
    }

    let socket_app = app.clone();
    let caminho_socket = paths::socket();
    let socket = tokio::spawn(async move { socket::serve(socket_app, caminho_socket).await });
    let roteador_app = app.clone();
    let roteador = tokio::spawn(async move { roteador::run(roteador_app).await });

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
        _ = roteador => {}
        _ = tokio::signal::ctrl_c() => { info!("sinal recebido, saindo"); }
    }

    std::fs::remove_file(paths::socket()).ok();
    Ok(())
}

/// Escolhe o frontend pelo config (`[daemon] frontend` / `frontend.rs`), com o modo offline
/// passando por cima de tudo.
fn monta_frontend(cfg: &Config) -> Result<Arc<dyn Frontend>> {
    if std::env::var_os("LUKADISPATCH_OFFLINE").is_some() {
        return Ok(Arc::new(Nulo::default()));
    }
    match cfg.frontend.as_str() {
        "telegram" => monta_telegram(cfg),
        outro => anyhow::bail!(
            "frontend desconhecido no config: {outro:?} (disponível: \"telegram\", ou \
             LUKADISPATCH_OFFLINE=1 para o frontend nulo)"
        ),
    }
}

#[cfg(feature = "telegram")]
fn monta_telegram(cfg: &Config) -> Result<Arc<dyn Frontend>> {
    use ld_daemon::frontend::telegram::Telegram;

    let token = Config::telegram_token()?;
    if cfg.telegram.chat_id == 0 {
        anyhow::bail!(
            "falta o chat_id: ponha em {} ou em LUKADISPATCH_CHAT_ID",
            paths::config_file().display()
        );
    }
    if cfg.telegram.allowed_user_ids.is_empty() {
        anyhow::bail!(
            "allowed_user_ids está vazio em {}: sem isso o bot não obedeceria ninguém",
            paths::config_file().display()
        );
    }
    Ok(Arc::new(Telegram::new(
        token,
        cfg.telegram.chat_id,
        cfg.telegram.allowed_user_ids.clone(),
    )))
}

#[cfg(not(feature = "telegram"))]
fn monta_telegram(_cfg: &Config) -> Result<Arc<dyn Frontend>> {
    anyhow::bail!(
        "frontend \"telegram\" pedido no config, mas este binário foi compilado sem a feature \
         \"telegram\""
    )
}
