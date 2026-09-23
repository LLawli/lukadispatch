//! Volumes de 7z: nenhum abre sozinho, o conjunto se abre pelo `.001`.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::frontend::nome_simples;

use super::{Divisor, MAX_PARTES, Partes, volume_para};
use crate::frontend::Midia;

#[derive(Debug, Default, Clone, Copy)]
pub struct Volumes7z;

/// O 7z pode se chamar `7z` (p7zip completo), `7za` (só o núcleo) ou `7zz` (7-Zip novo, sem o
/// wrapper `p7zip`). Os três servem aqui.
fn caminho_7z() -> Option<&'static str> {
    ["7z", "7za", "7zz"].into_iter().find(|nome| {
        std::process::Command::new(nome)
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

#[async_trait]
impl Divisor for Volumes7z {
    fn nome(&self) -> &str {
        "7z"
    }

    fn disponivel(&self) -> bool {
        caminho_7z().is_some()
    }

    fn aceita(&self, _caminho: &Path) -> bool {
        // Divisor genérico: qualquer arquivo pode virar volumes de 7z.
        true
    }

    fn anuncio(&self, teto: u64) -> String {
        format!(
            "estou dividindo em volumes de {} MB",
            volume_para(teto) / (1024 * 1024)
        )
    }

    /// Divide `caminho` em volumes de 7z que caibam em `teto`.
    ///
    /// 7z, e não `split`, porque o critério é juntar de volta no celular: parte crua de `split`
    /// só se remonta com `cat`, e os aplicativos de arquivo do Android (ZArchiver, RAR) abrem um
    /// conjunto `.7z.001` direto, com todas as partes na mesma pasta. Sem compressão (`-mx0`): o
    /// que se quer aqui é o corte, não o ganho de tamanho, e comprimir só arrisca um volume final
    /// menor que o esperado (ou, com dado já compactado, um único volume que não parte nada).
    async fn divide(&self, caminho: &Path, dir: PathBuf, teto: u64) -> Result<Partes> {
        let bin = caminho_7z().ok_or_else(|| anyhow::anyhow!("não achei o 7z"))?;
        let nome_original = caminho
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "arquivo".into());
        let nome = nome_simples(&nome_original);

        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| anyhow::anyhow!("criando {}: {e}", dir.display()))?;

        let volume = volume_para(teto);
        let alvo = dir.join(format!("{nome}.7z"));
        let saida = tokio::process::Command::new(bin)
            .arg("a")
            .arg(format!("-v{volume}b"))
            .arg("-mx0")
            .arg("-y")
            .arg(&alvo)
            .arg(caminho)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("rodando o 7z: {e}"))?;

        // Código de saída não é prova de artefato: o que vale é a lista de volumes em disco.
        let mut arquivos: Vec<PathBuf> = Vec::new();
        let mut entradas = tokio::fs::read_dir(&dir).await?;
        while let Some(e) = entradas.next_entry().await? {
            if e.path().is_file() {
                arquivos.push(e.path());
            }
        }
        arquivos.sort();

        if arquivos.is_empty()
            || arquivos
                .iter()
                .any(|p| p.metadata().is_ok_and(|m| m.len() == 0))
        {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            bail!(
                "o 7z não produziu volume utilizável ({}) {}",
                saida.status,
                String::from_utf8_lossy(&saida.stderr).trim()
            );
        }
        if arquivos
            .iter()
            .any(|p| p.metadata().is_ok_and(|m| m.len() > teto))
        {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            bail!("o 7z produziu volume acima do teto de {teto} bytes");
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
             <code>7z x {}</code>.",
            arquivos.len(),
            crate::frontend::formato::escapa(&nome_original),
            crate::frontend::formato::escapa(&primeiro),
            crate::frontend::formato::escapa(&primeiro)
        );

        Ok(Partes::new(dir, arquivos, Midia::Documento, como_juntar))
    }
}
