//! Quanto da janela de contexto uma sessão está ocupando, lido do transcript `.jsonl`.
//!
//! O número autoritativo é o `usage` da última mensagem do assistente: ali, `input_tokens +
//! cache_read_input_tokens + cache_creation_input_tokens` é exatamente o que foi enviado ao
//! modelo naquela chamada, ou seja, o tamanho atual do contexto. O `output` não entra porque ele
//! ainda não está no contexto quando a chamada acontece.
//!
//! O limite vem do `modelId` que o transcript registra num `attachment` de modelo (por exemplo
//! `claude-opus-5[1m]`). É a única fonte que distingue a janela de 1M da de 200k: o campo
//! `message.model` traz só `claude-opus-5`, sem o sufixo.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Tamanhos de cauda tentados em ordem. Uma entrada de transcript pode ser enorme (resultado de
/// ferramenta), então se a primeira janela não contiver um `usage`, abre-se mais.
const CAUDAS: [u64; 3] = [256 * 1024, 4 * 1024 * 1024, u64::MAX];

pub const LIMITE_PADRAO: u64 = 200_000;
pub const LIMITE_1M: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsage {
    pub tokens: u64,
    pub limit: u64,
}

impl ContextUsage {
    pub fn pct(&self) -> f64 {
        if self.limit == 0 {
            return 0.0;
        }
        (self.tokens as f64 / self.limit as f64 * 100.0).clamp(0.0, 100.0)
    }
}

/// Lê os últimos `bytes` do arquivo, descartando a primeira linha (que pode ter sido cortada no
/// meio por causa do corte cego em bytes).
fn cauda(caminho: &Path, bytes: u64) -> Option<String> {
    let mut f = std::fs::File::open(caminho).ok()?;
    let tamanho = f.metadata().ok()?.len();
    let inicio = tamanho.saturating_sub(bytes);
    f.seek(SeekFrom::Start(inicio)).ok()?;
    let mut buf = Vec::with_capacity((tamanho - inicio) as usize);
    f.read_to_end(&mut buf).ok()?;
    let texto = String::from_utf8_lossy(&buf).into_owned();
    if inicio == 0 {
        return Some(texto);
    }
    // Corte no meio de uma linha: some com o pedaço órfão.
    match texto.find('\n') {
        Some(i) => Some(texto[i + 1..].to_string()),
        None => Some(String::new()),
    }
}

fn tokens_da_linha(linha: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(linha).ok()?;
    let usage = v.get("message")?.get("usage")?;
    let campo = |k: &str| {
        usage
            .get(k)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let total = campo("input_tokens")
        + campo("cache_read_input_tokens")
        + campo("cache_creation_input_tokens");
    (total > 0).then_some(total)
}

/// Limite da janela a partir do id do modelo. O sufixo `[1m]` é o que marca a janela grande.
pub fn limit_for_model(model_id: &str) -> u64 {
    if model_id.contains("[1m]") {
        LIMITE_1M
    } else {
        LIMITE_PADRAO
    }
}

fn model_id_da_linha(linha: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(linha).ok()?;
    let id = v
        .get("attachment")
        .and_then(|a| a.get("identity"))
        .and_then(|i| i.get("modelId"))
        .and_then(serde_json::Value::as_str)?;
    Some(id.to_string())
}

/// Contexto atual da sessão, ou `None` quando o transcript ainda não tem nenhuma resposta do
/// assistente (sessão recém-criada) ou o arquivo não existe.
pub fn read(transcript: &Path) -> Option<ContextUsage> {
    for bytes in CAUDAS {
        let texto = cauda(transcript, bytes)?;
        let mut tokens = None;
        let mut limite = None;
        // De trás para frente: interessa o estado mais recente dos dois.
        for linha in texto.lines().rev() {
            if tokens.is_none() {
                tokens = tokens_da_linha(linha);
            }
            if limite.is_none()
                && let Some(id) = model_id_da_linha(linha)
            {
                limite = Some(limit_for_model(&id));
            }
            if tokens.is_some() && limite.is_some() {
                break;
            }
        }
        if let Some(tokens) = tokens {
            return Some(ContextUsage {
                tokens,
                limit: limite.unwrap_or(LIMITE_PADRAO),
            });
        }
        if bytes == u64::MAX {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn transcript(linhas: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        for l in linhas {
            writeln!(f, "{l}").unwrap();
        }
        (dir, p)
    }

    #[test]
    fn soma_input_cache_read_e_cache_creation() {
        let (_d, p) = transcript(&[
            r#"{"type":"user"}"#,
            r#"{"type":"assistant","message":{"model":"claude-opus-5","usage":{"input_tokens":2,"cache_read_input_tokens":115632,"cache_creation_input_tokens":1771,"output_tokens":797}}}"#,
        ]);
        let c = read(&p).unwrap();
        assert_eq!(c.tokens, 117_405);
        assert_eq!(c.limit, LIMITE_PADRAO);
    }

    #[test]
    fn sufixo_1m_muda_o_limite() {
        let (_d, p) = transcript(&[
            r#"{"attachment":{"type":"model","identity":{"modelId":"claude-opus-5[1m]","marketingName":"Opus 5 (1M context)"}}}"#,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":1,"cache_read_input_tokens":499999}}}"#,
        ]);
        let c = read(&p).unwrap();
        assert_eq!(c.limit, LIMITE_1M);
        assert_eq!(c.tokens, 500_000);
        assert_eq!(c.pct(), 50.0);
    }

    #[test]
    fn pega_o_usage_mais_recente() {
        let (_d, p) = transcript(&[
            r#"{"type":"assistant","message":{"usage":{"input_tokens":10}}}"#,
            r#"{"type":"assistant","message":{"usage":{"input_tokens":20}}}"#,
        ]);
        assert_eq!(read(&p).unwrap().tokens, 20);
    }

    #[test]
    fn transcript_sem_resposta_devolve_none() {
        let (_d, p) = transcript(&[r#"{"type":"user","message":{"content":"oi"}}"#]);
        assert!(read(&p).is_none());
    }

    #[test]
    fn arquivo_inexistente_devolve_none() {
        assert!(read(Path::new("/nao/existe.jsonl")).is_none());
    }

    #[test]
    fn linha_cortada_no_inicio_da_cauda_e_descartada() {
        // Uma cauda começa no meio de um JSON quebrado; a leitura não pode explodir nem inventar.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        let lixo = "x".repeat(300 * 1024);
        writeln!(f, r#"{{"type":"assistant","lixo":"{lixo}"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":42}}}}}}"#
        )
        .unwrap();
        assert_eq!(read(&p).unwrap().tokens, 42);
    }

    #[test]
    fn limite_por_modelo() {
        assert_eq!(limit_for_model("claude-opus-5"), LIMITE_PADRAO);
        assert_eq!(limit_for_model("claude-sonnet-5[1m]"), LIMITE_1M);
    }
}
