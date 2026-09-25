//! O daemon como biblioteca.
//!
//! O binário é uma casca fina sobre isto. A razão é teste: teste de integração não enxerga
//! módulo privado de um crate que só tem binário, e o canal de entrada (socket mais fila mais
//! entrega) é justamente a parte que precisa ser testada de verdade, sem Telegram no meio.

pub mod agente;
pub mod app;
pub mod arquivos;
pub mod cards;
pub mod confirmacao;
pub mod divisor;
pub mod frontend;
pub mod hub;
pub mod novo;
pub mod panel;
pub mod roteador;
pub mod sessions;
pub mod setup;
pub mod socket;
pub mod status;
pub mod transcritor;
pub mod worktree;
