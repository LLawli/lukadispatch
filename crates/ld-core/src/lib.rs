//! Tipos e leituras compartilhados entre o daemon, o CLI/hook e a janela de pergunta.
//!
//! Este crate é deliberadamente magro: o binário de hook roda a cada chamada de ferramenta do
//! Claude, então tudo que ele carrega entra no caminho quente. Nada de Telegram e nada de GTK
//! aqui dentro.

pub mod ask;
pub mod config;
pub mod context;
pub mod hooks;
pub mod labels;
pub mod mcp;
pub mod models;
pub mod paths;
pub mod proto;
pub mod race;
pub mod state;
pub mod transcript;
pub mod trust;
pub mod usage;

pub use proto::{Request, Response};
