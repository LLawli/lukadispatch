//! A porta da transcrição: áudio entra, texto sai.
//!
//! Quem pede a transcrição (o fluxo do card de confirmação) só conhece a trait [`Transcritor`].
//! Qual motor roda por baixo é escolha de [`da_config`], a partir de `[transcricao] motor` no
//! `config.toml`.
//!
//! Há dois níveis de troca, e vale saber qual se quer antes de escrever código:
//!
//! - **Outro programa local** (outro modelo do whisper.cpp, sherpa-onnx, faster-whisper): não
//!   precisa de código. O motor `"processo"` ([`processo::ProcessoExterno`]) chama qualquer
//!   comando, com marcadores para o áudio, o modelo e a saída. Trocar é editar o config.
//! - **Outra natureza de motor** (uma API remota, uma biblioteca embutida no binário): aí sim é
//!   uma implementação nova desta trait, registrada em [`da_config`] com um nome novo de motor.
//!
//! O contrato que toda implementação cumpre:
//!
//! - **Sucesso é texto não vazio.** Motor que "termina bem" sem produzir nada devolve erro, e
//!   não string vazia: mensagem vazia some sem rastro, erro pelo menos aparece no canal.
//! - **Não bloqueia o runtime.** Transcrever leva dezenas de segundos; quem chama já roda isto
//!   fora do turno, mas a implementação não pode segurar uma thread do tokio fazendo isso.
//! - **Tem prazo.** Um motor travado não pode prender a fila para sempre.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use async_trait::async_trait;
use ld_core::config::Transcricao;

pub mod processo;

#[cfg(test)]
mod testes;

pub use processo::ProcessoExterno;

/// O que o motor produziu, com o tempo que levou (vai para o log, e ajuda a perceber quando uma
/// troca de modelo saiu cara).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcrito {
    pub texto: String,
    pub duracao: Duration,
}

#[async_trait]
pub trait Transcritor: Send + Sync + 'static {
    /// Nome curto para o log.
    fn nome(&self) -> &str;

    /// Transcreve um áudio qualquer que chegou pelo chat (Opus, MP3, M4A...). A conversão de
    /// formato, se o motor precisar, é problema da implementação.
    async fn transcreve(&self, audio: &Path) -> Result<Transcrito>;
}

/// O transcritor que o config pede, ou `None` se a transcrição está desligada.
///
/// Config inválido (motor desconhecido, `saida` que não existe) é erro aqui, na partida do
/// daemon, e não no primeiro áudio, horas depois.
pub fn da_config(cfg: &Transcricao) -> Result<Option<Arc<dyn Transcritor>>> {
    if !cfg.ativa {
        return Ok(None);
    }
    match cfg.motor.as_str() {
        "processo" => Ok(Some(Arc::new(processo::ProcessoExterno::new(cfg)?))),
        outro => bail!("motor de transcrição desconhecido: {outro:?} (disponíveis: \"processo\")"),
    }
}
