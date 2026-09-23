//! A porta do divisor: arquivo grande demais para o chat vira várias partes que cabem.
//!
//! Quem manda arquivo (o fim de turno com `@arquivo:`, o `lukadispatch send-file`) só conhece
//! [`Divisores`], que é uma cadeia ordenada de implementações da trait [`Divisor`]. A cadeia
//! tenta cada divisor que aceita o arquivo, em ordem, e cai para o próximo se um falhar. A
//! ordem padrão, montada por [`Divisores::da_config`], é:
//!
//! 1. [`video::TrechosDeVideo`], se `cortar_video` estiver ligado: vídeo cortado por tempo, sem
//!    recodificar. Cada trecho toca sozinho no celular.
//! 2. O divisor genérico por volumes, escolhido por `[arquivos] divisor`:
//!    [`sete_z::Volumes7z`] (`"7z"`) ou [`rar::VolumesRar`] (`"rar"`). Nenhum volume abre
//!    sozinho; o conjunto se abre pelo primeiro.
//!
//! Trocar 7z por RAR é mudar uma linha do config. Um formato novo (zip dividido, `split` cru)
//! é uma implementação nova desta trait, registrada com um nome em [`Divisores::da_config`].
//!
//! O contrato que toda implementação cumpre:
//!
//! - **Nenhuma parte passa de `teto`.** O teto vem do frontend (`Limites::enviar`), e uma parte
//!   acima dele é um upload que falha no fim.
//! - **Código de saída não é prova.** O que vale é haver parte em disco, nenhuma vazia.
//! - **Falhou, limpou.** Em erro, `dir` fica ausente ou vazio, para o próximo da cadeia começar
//!   do zero.
//! - **No máximo [`MAX_PARTES`].** Acima disso o canal vira uma fila de upload, e é melhor dizer
//!   isso na cara do que passar meia hora mandando.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use ld_core::config::Arquivos;

use crate::frontend::Midia;

pub mod rar;
pub mod sete_z;
pub mod video;

#[cfg(test)]
mod testes;

/// Teto de partes de um envio.
pub const MAX_PARTES: usize = 20;

/// Tamanho de volume para um teto de envio: 90% dele.
///
/// A margem existe porque o compactador conta o volume e a plataforma conta o arquivo, e
/// empatar com o limite é pedir para um envio falhar no fim de um upload longo. Para os 50 MB
/// do Telegram dá os 45 MB que o projeto sempre usou.
pub fn volume_para(teto: u64) -> u64 {
    teto / 10 * 9
}

/// Um arquivo partido, pronto para sair uma parte de cada vez.
#[derive(Debug)]
pub struct Partes {
    /// Diretório só das partes; some inteiro em [`Partes::limpa`].
    pub(crate) dir: PathBuf,
    /// As partes, em ordem.
    pub arquivos: Vec<PathBuf>,
    /// Como cada parte deve sair (volume vai como documento; trecho de vídeo, como vídeo).
    pub midia: Midia,
    /// A instrução de juntar, em marcação (ver `frontend::formato`), para mandar depois da
    /// última parte. Sem ela, um punhado de `.001`, `.002` no celular é só lixo.
    pub como_juntar: String,
}

impl Partes {
    /// Para implementações de fora deste módulo. `arquivos` tem de estar dentro de `dir`, que é
    /// apagado inteiro em [`Partes::limpa`].
    pub fn new(dir: PathBuf, arquivos: Vec<PathBuf>, midia: Midia, como_juntar: String) -> Self {
        Self {
            dir,
            arquivos,
            midia,
            como_juntar,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Apaga as partes. Chamado depois do envio, deu certo ou não.
    pub async fn limpa(self) {
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
    }
}

#[async_trait]
pub trait Divisor: Send + Sync + 'static {
    /// Nome curto, o mesmo do config quando houver (`"7z"`, `"rar"`, `"video"`).
    fn nome(&self) -> &str;

    /// A ferramenta de que ele depende está instalada?
    fn disponivel(&self) -> bool;

    /// Ele sabe partir este arquivo? (O de vídeo só aceita vídeo; os de volume aceitam tudo.)
    fn aceita(&self, caminho: &Path) -> bool;

    /// O aviso que sai no canal antes de começar, em marcação: dividir e subir leva minutos, e
    /// sem aviso parece que o arquivo sumiu.
    fn anuncio(&self, teto: u64) -> String;

    /// Parte `caminho` em pedaços de no máximo `teto` bytes, dentro de `dir` (que ele cria).
    async fn divide(&self, caminho: &Path, dir: PathBuf, teto: u64) -> Result<Partes>;
}

/// A cadeia de divisores, na ordem em que são tentados.
#[derive(Clone)]
pub struct Divisores {
    cadeia: Vec<Arc<dyn Divisor>>,
}

impl Divisores {
    pub fn new(cadeia: Vec<Arc<dyn Divisor>>) -> Self {
        Self { cadeia }
    }

    /// A cadeia que o config pede. Divisor desconhecido é erro na partida.
    pub fn da_config(cfg: &Arquivos) -> Result<Self> {
        let mut cadeia: Vec<Arc<dyn Divisor>> = Vec::new();
        if cfg.cortar_video {
            cadeia.push(Arc::new(video::TrechosDeVideo));
        }
        match cfg.divisor.as_str() {
            "7z" => cadeia.push(Arc::new(sete_z::Volumes7z)),
            "rar" => cadeia.push(Arc::new(rar::VolumesRar)),
            outro => anyhow::bail!("divisor desconhecido no config: \"{outro}\""),
        }
        Ok(Self::new(cadeia))
    }

    /// Os nomes, na ordem. Vai para o log da partida.
    pub fn nomes(&self) -> Vec<String> {
        self.cadeia.iter().map(|d| d.nome().to_string()).collect()
    }

    /// Os que estão disponíveis e aceitam este arquivo, na ordem da cadeia.
    pub fn candidatos(&self, caminho: &Path) -> Vec<Arc<dyn Divisor>> {
        self.cadeia
            .iter()
            .filter(|d| d.disponivel() && d.aceita(caminho))
            .cloned()
            .collect()
    }

    /// Tenta cada candidato em ordem e devolve as partes do primeiro que der certo. Sem
    /// candidato, ou com todos falhando, o erro diz o que foi tentado e por que falhou.
    pub async fn divide(&self, caminho: &Path, dir: PathBuf, teto: u64) -> Result<Partes> {
        // Por que cada divisor da cadeia ficou de fora, para quando nenhum candidato sobrar.
        let mut fora_de_jogo: Vec<String> = Vec::new();
        let mut candidatos: Vec<&Arc<dyn Divisor>> = Vec::new();
        for d in &self.cadeia {
            if !d.disponivel() {
                fora_de_jogo.push(format!("{} (não instalado)", d.nome()));
            } else if !d.aceita(caminho) {
                fora_de_jogo.push(format!("{} (não aceita este arquivo)", d.nome()));
            } else {
                candidatos.push(d);
            }
        }

        if candidatos.is_empty() {
            anyhow::bail!(
                "nenhum divisor disponível para {}: {}",
                caminho.display(),
                fora_de_jogo.join("; ")
            );
        }

        let mut motivos: Vec<String> = Vec::new();
        for d in candidatos {
            // Antes de cada tentativa, garante que o diretório não sobrou de uma tentativa
            // anterior: o próximo divisor da cadeia começa do zero.
            let _ = tokio::fs::remove_dir_all(&dir).await;
            match d.divide(caminho, dir.clone(), teto).await {
                Ok(partes) => return Ok(partes),
                Err(e) => {
                    tracing::warn!(divisor = d.nome(), erro = %e, "divisor falhou; tentando o próximo");
                    motivos.push(format!("{}: {e:#}", d.nome()));
                }
            }
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
        anyhow::bail!(
            "nenhum divisor conseguiu partir {}: {}",
            caminho.display(),
            motivos.join("; ")
        );
    }
}
