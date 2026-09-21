//! Tipos e leituras compartilhados entre o daemon, o CLI/hook e a janela de pergunta.
//!
//! Este crate é deliberadamente magro: o binário de hook roda a cada chamada de ferramenta do
//! Claude, então tudo que ele carrega entra no caminho quente. Nada de Telegram e nada de GTK
//! aqui dentro.

pub mod config;
pub mod context;
pub mod labels;
pub mod paths;
pub mod proto;
pub mod state;
pub mod usage;

pub use proto::{Request, Response};
