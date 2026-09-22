//! O daemon como biblioteca.
//!
//! O binário é uma casca fina sobre isto. A razão é teste: teste de integração não enxerga
//! módulo privado de um crate que só tem binário, e o canal de entrada (socket mais fila mais
//! entrega) é justamente a parte que precisa ser testada de verdade, sem Telegram no meio.

pub mod app;
pub mod arquivos;
pub mod cards;
pub mod hub;
pub mod panel;
pub mod poll;
pub mod sessions;
pub mod socket;
pub mod status;
pub mod telegram;
pub mod transcricao;
