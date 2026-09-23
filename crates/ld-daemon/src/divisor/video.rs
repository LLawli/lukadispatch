//! Vídeo cortado por tempo com `ffmpeg -c copy`, sem recodificar: cada trecho toca sozinho.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::frontend::nome_simples;

use super::{Divisor, MAX_PARTES, Partes};
use crate::frontend::Midia;

#[derive(Debug, Default, Clone, Copy)]
pub struct TrechosDeVideo;

fn tem_ffmpeg() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn tem_ffprobe() -> bool {
    std::process::Command::new("ffprobe")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

async fn duracao_de(caminho: &Path) -> Result<f64> {
    let saida = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(caminho)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("rodando o ffprobe: {e}"))?;
    String::from_utf8_lossy(&saida.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| anyhow::anyhow!("duração ilegível"))
}

/// Uma passada do `ffmpeg` cortando por tempo. Devolve os trechos em ordem.
async fn segmenta(
    caminho: &Path,
    dir: &Path,
    base: &str,
    ext: &str,
    segundos: f64,
) -> Result<Vec<PathBuf>> {
    // Rodar de novo por cima do que já existe misturaria as duas tentativas.
    let _ = tokio::fs::remove_dir_all(dir).await;
    tokio::fs::create_dir_all(dir).await?;

    let molde = dir.join(format!("{base}-parte-%03d.{ext}"));
    let saida = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(caminho)
        .args(["-map", "0", "-c", "copy", "-f", "segment"])
        .arg("-segment_time")
        .arg(format!("{segundos:.3}"))
        .args(["-reset_timestamps", "1"])
        .arg(&molde)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("rodando o ffmpeg: {e}"))?;

    let mut arquivos = Vec::new();
    let mut entradas = tokio::fs::read_dir(dir).await?;
    while let Some(e) = entradas.next_entry().await? {
        if e.path().is_file() {
            arquivos.push(e.path());
        }
    }
    arquivos.sort();
    // Saída zero não prova nada: o que vale é ter trecho em disco, e nenhum deles vazio.
    if arquivos.is_empty()
        || arquivos
            .iter()
            .any(|p| p.metadata().is_ok_and(|m| m.len() == 0))
    {
        bail!(
            "o ffmpeg não produziu trecho utilizável: {}",
            String::from_utf8_lossy(&saida.stderr).trim()
        );
    }
    Ok(arquivos)
}

#[async_trait]
impl Divisor for TrechosDeVideo {
    fn nome(&self) -> &str {
        "video"
    }

    fn disponivel(&self) -> bool {
        tem_ffmpeg() && tem_ffprobe()
    }

    fn aceita(&self, caminho: &Path) -> bool {
        caminho
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "mp4" | "mkv" | "mov" | "webm" | "m4v" | "avi" | "ts" | "mpg" | "mpeg"
                )
            })
    }

    fn anuncio(&self, _teto: u64) -> String {
        "estou cortando em trechos que tocam sozinhos".to_string()
    }

    /// Corta um vídeo em trechos que caibam em `teto`, sem recodificar.
    ///
    /// `-c copy` copia os fluxos como estão: é rápido (segundos para centenas de MB), não perde
    /// qualidade e mantém cada trecho sendo um vídeo de verdade, que toca sozinho no celular. É
    /// essa a diferença para os volumes de 7z/rar, onde nenhuma parte serve para nada até
    /// estarem todas juntas.
    async fn divide(&self, caminho: &Path, dir: PathBuf, teto: u64) -> Result<Partes> {
        let tamanho = std::fs::metadata(caminho)?.len();
        let duracao = duracao_de(caminho).await?;
        if duracao <= 0.0 {
            bail!("não consegui ler a duração do vídeo");
        }

        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| anyhow::anyhow!("criando {}: {e}", dir.display()))?;

        let ext = caminho
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp4")
            .to_ascii_lowercase();
        let base = nome_simples(
            caminho
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("video"),
        );

        // Alvo de cada trecho: 80% do teto, porque o corte acontece no keyframe mais próximo, e
        // não no ponto exato. O trecho real pode passar do alvo, e passar do teto seria um
        // upload perdido.
        let alvo = teto / 10 * 8;

        // Primeira tentativa pelo bitrate médio; se um trecho passar do teto (GOP longo, cena
        // pesada), corta na metade do tempo e tenta de novo. Duas tentativas bastam na prática, e
        // insistir mais sairia mais caro que cair nos volumes.
        let mut segundos = (duracao * alvo as f64 / tamanho as f64).max(5.0);
        for tentativa in 0..2 {
            let arquivos = segmenta(caminho, &dir, &base, &ext, segundos).await?;
            let maior = arquivos
                .iter()
                .filter_map(|p| p.metadata().ok().map(|m| m.len()))
                .max()
                .unwrap_or(0);
            if !arquivos.is_empty() && maior <= teto && arquivos.len() <= MAX_PARTES {
                let nome_original = caminho
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let nome_escapado = crate::frontend::formato::escapa(&nome_original);
                let como_juntar = format!(
                    "🧩 {} trechos de <b>{nome_escapado}</b>, na ordem. Cada um toca sozinho; \
                     para remontar o vídeo inteiro no PC, <code>ffmpeg -f concat -safe 0 -i \
                     lista.txt -c copy {nome_escapado}</code>, com os trechos listados em \
                     lista.txt.",
                    arquivos.len(),
                );
                return Ok(Partes::new(dir, arquivos, Midia::Video, como_juntar));
            }
            if tentativa == 0 {
                segundos /= 2.0;
            }
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
        bail!("os trechos continuaram passando do teto")
    }
}
