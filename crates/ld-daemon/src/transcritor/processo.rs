//! O motor `"processo"`: chama um transcritor de fora como processo filho.
//!
//! O motor NÃO está no código, e isso é de propósito. O que ganha hoje neste notebook (Ryzen
//! 5700U, Vega sem VRAM dedicada) não é o que ganharia noutra máquina, e medir a troca é barato
//! só enquanto trocar for editar uma linha de config. Então este módulo sabe montar um comando,
//! esperar por ele e ler o que ele produziu; qual binário é esse, com que modelo e em que
//! backend, é assunto do `config.toml` (`[transcricao]`).
//!
//! Três coisas que o desenho leva a sério:
//!
//! - **Código de saída zero não é prova de transcrição.** Tem transcritor que sai limpo e não
//!   escreve nada (o Parakeet devolveu string vazia num áudio ruim durante o benchmark). O
//!   sucesso aqui é "veio texto não vazio".
//! - **O Whisper quer PCM mono 16 kHz.** O chat manda Opus 48 kHz, então há sempre uma conversão
//!   com ffmpeg no caminho. Ela custa ~0,08 s por áudio, e o WAV temporário morre junto com o
//!   diretório da chamada.
//! - **Isto demora mais que o turno.** Nenhuma configuração medida transcreve um minuto de fala
//!   em menos de 39 s, e o hook `Stop` desiste em 60 s. Quem chama roda isto fora do caminho da
//!   resposta.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use ld_core::config::Transcricao;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tracing::{info, warn};

use super::{Transcrito, Transcritor};

/// Uma transcrição por vez, em todo o daemon.
///
/// Não é sobre CPU: no Vulkan o trabalho ocupa 0,3 dos 8 núcleos. É sobre memória. Cada
/// transcrição pede ~1 GB (quase todo em GTT) e esta máquina tem por volta de 1,2 GB livres,
/// então dois áudios que cheguem juntos somam ~2 GB e empurram a máquina para swap, e swap aqui
/// é zram, que não devolve a memória, comprime e segura.
///
/// Esperar é melhor que engasgar: a transcrição já acontece fora do turno, então a fila só
/// atrasa a mensagem, enquanto o swap atrasaria a máquina inteira.
pub(crate) static UMA_POR_VEZ: Semaphore = Semaphore::const_new(1);

/// Onde o comando deixa o texto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Saida {
    /// Escreve `<prefixo>.txt` (é o caso do `whisper-cli -otxt -of <prefixo>`).
    Arquivo,
    /// Imprime o texto na saída padrão (é o caso dos workers de sherpa-onnx e faster-whisper).
    Stdout,
}

impl Saida {
    pub(crate) fn de(bruto: &str) -> Result<Self> {
        match bruto {
            "arquivo" => Ok(Self::Arquivo),
            "stdout" => Ok(Self::Stdout),
            outro => {
                bail!("saida do transcritor deve ser \"arquivo\" ou \"stdout\", veio {outro:?}")
            }
        }
    }
}

pub struct ProcessoExterno {
    cfg: Transcricao,
    saida: Saida,
}

impl ProcessoExterno {
    /// Valida o config (a `saida` tem de ser conhecida e o `comando` não pode ser vazio) sem
    /// rodar nada. O modelo ausente NÃO é erro aqui: o disco de dados pode montar depois da
    /// partida, e a falta dele é dita no áudio que precisar dele.
    pub fn new(cfg: &Transcricao) -> Result<Self> {
        let saida = Saida::de(&cfg.saida)?;
        if cfg.comando.is_empty() {
            bail!("transcricao.comando está vazio");
        }
        Ok(Self {
            cfg: cfg.clone(),
            saida,
        })
    }
}

/// Troca os marcadores do comando pelos caminhos desta chamada.
///
/// Um marcador desconhecido é erro, e não texto literal: `{modleo}` num config passaria batido
/// como nome de arquivo e o transcritor falharia com uma mensagem que não ajuda ninguém.
pub(crate) fn monta(
    comando: &[String],
    wav: &Path,
    modelo: &Path,
    prefixo: &Path,
) -> Result<Vec<String>> {
    if comando.is_empty() {
        bail!("transcricao.comando está vazio");
    }
    let mut saida = Vec::with_capacity(comando.len());
    for parte in comando {
        // `~/` vale em qualquer parte do comando, e não só no modelo: o executável do
        // transcritor mora sob o home tanto quanto ele, e um til literal vira "arquivo não
        // encontrado" na hora de rodar.
        let trocado = expande(parte)
            .to_string_lossy()
            .replace("{audio}", &wav.to_string_lossy())
            .replace("{modelo}", &modelo.to_string_lossy())
            .replace("{saida}", &prefixo.to_string_lossy());
        if let Some(inicio) = trocado.find('{')
            && let Some(fim) = trocado[inicio..].find('}')
        {
            bail!(
                "marcador desconhecido {:?} em transcricao.comando (existem {{audio}}, {{modelo}} e {{saida}})",
                &trocado[inicio..inicio + fim + 1]
            );
        }
        saida.push(trocado);
    }
    Ok(saida)
}

/// O chat manda Opus 48 kHz; o Whisper (e o sherpa, e o faster-whisper) querem PCM mono
/// 16 kHz. Converter aqui, uma vez, evita que cada motor tenha a sua própria gambiarra.
async fn converte_para_wav16k(origem: &Path, destino: &Path) -> Result<()> {
    let saida = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-i"])
        .arg(origem)
        .args(["-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le"])
        .arg(destino)
        .stdin(Stdio::null())
        .output()
        .await
        .context("chamando o ffmpeg (ele está instalado?)")?;
    if !saida.status.success() {
        bail!(
            "ffmpeg não converteu o áudio: {}",
            String::from_utf8_lossy(&saida.stderr).trim()
        );
    }
    // Arquivo de zero byte passaria adiante e o transcritor diria algo inútil sobre ele.
    let bytes = tokio::fs::metadata(destino)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if bytes == 0 {
        bail!("a conversão para WAV saiu vazia");
    }
    Ok(())
}

async fn roda(
    argumentos: &[String],
    saida: Saida,
    prefixo: &Path,
    timeout_s: u64,
) -> Result<String> {
    let mut cmd = Command::new(&argumentos[0]);
    cmd.args(&argumentos[1..])
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let fim = tokio::time::timeout(Duration::from_secs(timeout_s), cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("o transcritor passou de {timeout_s}s e foi derrubado"))?
        .with_context(|| format!("rodando {:?}", argumentos[0]))?;

    if !fim.status.success() {
        let erro = String::from_utf8_lossy(&fim.stderr);
        bail!(
            "o transcritor saiu com {}: {}",
            fim.status,
            erro.trim().lines().last().unwrap_or("(sem mensagem)")
        );
    }
    // Queda silenciosa de backend não derruba a transcrição, e é justamente por isso que precisa
    // aparecer: foi assim que o sherpa-onnx rodou em CPU dizendo "webgpu" durante o benchmark.
    //
    // Casar só a palavra "fallback" não serve: o whisper.cpp imprime `fallbacks = 0 p / 0 h` nos
    // timings de toda execução normal, e o aviso passaria a gritar sempre. Quem grita sempre não
    // é lido no dia em que a queda for real.
    let erro = String::from_utf8_lossy(&fim.stderr);
    if let Some(linha) = erro.lines().find(|l| caiu_para_cpu(l)) {
        warn!(aviso = %linha.trim(), "o transcritor não usou o backend pedido");
    }

    match saida {
        Saida::Stdout => Ok(String::from_utf8_lossy(&fim.stdout).into_owned()),
        Saida::Arquivo => {
            let txt = prefixo.with_extension("txt");
            tokio::fs::read_to_string(&txt).await.with_context(|| {
                format!(
                    "o comando terminou bem mas não escreveu {} (confira o -of do config)",
                    txt.display()
                )
            })
        }
    }
}

/// A linha de stderr avisa que o backend pedido não subiu?
///
/// "fallback" precisa vir junto de um destino ("to cpu"), que é como os runtimes escrevem a
/// queda de verdade. Uma contagem de fallbacks de decodificação não é isso.
pub(crate) fn caiu_para_cpu(linha: &str) -> bool {
    let l = linha.to_lowercase();
    (l.contains("fallback") || l.contains("falling back")) && l.contains("cpu")
}

/// `~/` no começo vira o home.
///
/// Lê `$HOME` direto, como o resto do projeto (`paths::home`), em vez de trazer a crate `dirs`
/// só para isto.
pub(crate) fn expande(bruto: &str) -> PathBuf {
    if let Some(resto) = bruto.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(resto);
    }
    PathBuf::from(bruto)
}

#[async_trait]
impl Transcritor for ProcessoExterno {
    fn nome(&self) -> &str {
        "processo"
    }

    async fn transcreve(&self, audio: &Path) -> Result<Transcrito> {
        let modelo = expande(&self.cfg.modelo);
        if !self.cfg.modelo.is_empty() && !modelo.exists() {
            bail!(
                "modelo não está em {} (o disco de dados pode não ter montado)",
                modelo.display()
            );
        }

        // Diretório próprio por chamada: o WAV convertido e o .txt do transcritor somem juntos
        // no fim, mesmo se der erro no meio.
        let temp = tempfile::Builder::new()
            .prefix("ld-transcricao-")
            .tempdir()
            .context("criando diretório temporário da transcrição")?;
        let wav = temp.path().join("entrada.wav");
        converte_para_wav16k(audio, &wav).await?;

        let prefixo = temp.path().join("saida");
        let argumentos = monta(&self.cfg.comando, &wav, &modelo, &prefixo)?;

        // A vez chega antes do relógio começar: o tempo que interessa medir é o da transcrição,
        // não o da espera na fila, senão o log passa a acusar lentidão que é só concorrência.
        if UMA_POR_VEZ.available_permits() == 0 {
            info!(arquivo = %audio.display(), "outra transcrição está rodando; entro na fila");
        }
        let _vez = UMA_POR_VEZ
            .acquire()
            .await
            .expect("o semáforo da transcrição nunca é fechado");

        let inicio = Instant::now();
        let texto = roda(&argumentos, self.saida, &prefixo, self.cfg.timeout_s).await?;
        let duracao = inicio.elapsed();

        if texto.trim().is_empty() {
            bail!("o transcritor não devolveu texto nenhum");
        }
        info!(
            arquivo = %audio.display(),
            segundos = duracao.as_secs_f32(),
            caracteres = texto.trim().len(),
            "áudio transcrito"
        );
        Ok(Transcrito {
            texto: texto.trim().to_string(),
            duracao,
        })
    }
}
