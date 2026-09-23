//! Volumes de RAR (`rar a -v<tamanho>`). Mesmo papel do 7z, para quem prefere o formato ou abre melhor no celular.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::frontend::nome_simples;

use super::{Divisor, MAX_PARTES, Partes, volume_para};
use crate::frontend::Midia;

#[derive(Debug, Default, Clone, Copy)]
pub struct VolumesRar;

fn tem_rar() -> bool {
    std::process::Command::new("rar")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success() || s.code() == Some(1))
}

/// Ordena `nome.part1.rar`, `nome.part2.rar`, ... numericamente: comparação de texto puro
/// colocaria `part10` antes de `part9`.
fn numero_da_parte(caminho: &Path) -> u32 {
    let nome = caminho
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    nome.rsplit_once(".part")
        .and_then(|(_, resto)| resto.split('.').next())
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(0)
}

#[async_trait]
impl Divisor for VolumesRar {
    fn nome(&self) -> &str {
        "rar"
    }

    fn disponivel(&self) -> bool {
        tem_rar()
    }

    fn aceita(&self, _caminho: &Path) -> bool {
        // Divisor genérico: qualquer arquivo pode virar volumes de RAR.
        true
    }

    fn anuncio(&self, teto: u64) -> String {
        format!(
            "estou dividindo em volumes de {} MB",
            volume_para(teto) / (1024 * 1024)
        )
    }

    async fn divide(&self, caminho: &Path, dir: PathBuf, teto: u64) -> Result<Partes> {
        let nome_original = caminho
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "arquivo".into());
        let nome = nome_simples(&nome_original);

        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| anyhow::anyhow!("criando {}: {e}", dir.display()))?;

        let volume = volume_para(teto);
        let alvo = dir.join(format!("{nome}.rar"));
        // -m0: sem compressão (o que se quer é o corte, não o ganho de tamanho). -ep1: guarda só
        // o nome do arquivo, sem o caminho absoluto de onde ele veio. -idq: saída quieta, sem
        // progresso poluindo o log.
        let saida = tokio::process::Command::new("rar")
            .arg("a")
            .arg(format!("-v{volume}b"))
            .arg("-m0")
            .arg("-ep1")
            .arg("-y")
            .arg("-idq")
            .arg(&alvo)
            .arg(caminho)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("rodando o rar: {e}"))?;

        // Código de saída não é prova de artefato: o que vale é a lista de volumes em disco.
        let mut arquivos: Vec<PathBuf> = Vec::new();
        let mut entradas = tokio::fs::read_dir(&dir).await?;
        while let Some(e) = entradas.next_entry().await? {
            if e.path().is_file() {
                arquivos.push(e.path());
            }
        }
        arquivos.sort_by_key(|p| numero_da_parte(p));

        if arquivos.is_empty()
            || arquivos
                .iter()
                .any(|p| p.metadata().is_ok_and(|m| m.len() == 0))
        {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            bail!(
                "o rar não produziu volume utilizável ({}) {}",
                saida.status,
                String::from_utf8_lossy(&saida.stderr).trim()
            );
        }
        if arquivos
            .iter()
            .any(|p| p.metadata().is_ok_and(|m| m.len() > teto))
        {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            bail!("o rar produziu volume acima do teto de {teto} bytes");
        }
        if arquivos.len() > MAX_PARTES {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            bail!("deu {} partes, e o teto é {MAX_PARTES}", arquivos.len());
        }

        let primeiro = arquivos[0]
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let como_juntar = format!(
            "🧩 {} partes de <b>{}</b>. Baixe todas para a mesma pasta e abra a \
             <code>{}</code>: no celular o ZArchiver ou o RAR juntam sozinhos, e no PC é \
             <code>unrar x {}</code>.",
            arquivos.len(),
            crate::frontend::formato::escapa(&nome_original),
            crate::frontend::formato::escapa(&primeiro),
            crate::frontend::formato::escapa(&primeiro)
        );

        Ok(Partes::new(dir, arquivos, Midia::Documento, como_juntar))
    }
}
