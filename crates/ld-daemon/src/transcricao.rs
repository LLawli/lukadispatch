//! Áudio que chega vira texto, chamando um transcritor de fora como processo filho.
//!
//! O motor NÃO está no código, e isso é de propósito. O que ganha hoje neste notebook (Ryzen
//! 5700U, Vega sem VRAM dedicada) não é o que ganharia noutra máquina, e medir a troca é barato
//! só enquanto trocar for editar uma linha de config. Então o daemon sabe montar um comando,
//! esperar por ele e ler o que ele produziu; qual binário é esse, com que modelo e em que
//! backend, é assunto do `config.toml`.
//!
//! Três coisas que o desenho leva a sério:
//!
//! - **Código de saída zero não é prova de transcrição.** Tem transcritor que sai limpo e não
//!   escreve nada (o Parakeet devolveu string vazia num áudio ruim durante o benchmark). Por
//!   isso o sucesso aqui é "veio texto não vazio", e não "o processo saiu com 0".
//! - **O Whisper quer PCM mono 16 kHz.** O Telegram manda Opus 48 kHz, então há sempre uma
//!   conversão com ffmpeg no caminho. Ela custa ~0,08 s por áudio, o que é ruído perto da
//!   transcrição, e o WAV temporário morre junto com o diretório da chamada.
//! - **Isto demora mais que o turno.** Nenhuma configuração medida transcreve um minuto de fala
//!   em menos de 39 s, e o hook `Stop` desiste em 60 s. Quem chama tem que rodar isto fora do
//!   caminho da resposta; este módulo só não bloqueia, não resolve o problema sozinho.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ld_core::config::Transcricao as Cfg;
use tokio::process::Command;
use tracing::{info, warn};

/// O que o transcritor produziu, com o tempo que levou (vai para o log, e ajuda a perceber
/// quando uma troca de modelo saiu cara).
#[derive(Debug, Clone)]
pub struct Transcrito {
    pub texto: String,
    pub duracao: Duration,
}

/// Onde o comando deixa o texto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Saida {
    /// Escreve `<prefixo>.txt` (é o caso do `whisper-cli -otxt -of <prefixo>`).
    Arquivo,
    /// Imprime o texto na saída padrão (é o caso dos workers de sherpa-onnx e faster-whisper).
    Stdout,
}

impl Saida {
    fn de(bruto: &str) -> Result<Self> {
        match bruto {
            "arquivo" => Ok(Self::Arquivo),
            "stdout" => Ok(Self::Stdout),
            outro => {
                bail!("saida do transcritor deve ser \"arquivo\" ou \"stdout\", veio {outro:?}")
            }
        }
    }
}

/// Transcreve um áudio qualquer que o Telegram entregou.
///
/// Devolve `Ok(None)` quando a transcrição está desligada na config: assim quem chama trata
/// "não quero transcrever" e "não consegui transcrever" de formas diferentes, que é o que
/// interessa para decidir o que dizer no tópico.
pub async fn transcreve(cfg: &Cfg, audio: &Path) -> Result<Option<Transcrito>> {
    if !cfg.ativa {
        return Ok(None);
    }
    let saida = Saida::de(&cfg.saida)?;
    let modelo = expande(&cfg.modelo);
    if !cfg.modelo.is_empty() && !modelo.exists() {
        bail!(
            "modelo não está em {} (o disco de dados pode não ter montado)",
            modelo.display()
        );
    }

    // Diretório próprio por chamada: o WAV convertido e o .txt do transcritor somem juntos no
    // fim, mesmo se der erro no meio.
    let temp = tempfile::Builder::new()
        .prefix("ld-transcricao-")
        .tempdir()
        .context("criando diretório temporário da transcrição")?;
    let wav = temp.path().join("entrada.wav");
    converte_para_wav16k(audio, &wav).await?;

    let prefixo = temp.path().join("saida");
    let argumentos = monta(&cfg.comando, &wav, &modelo, &prefixo)?;
    let inicio = Instant::now();
    let texto = roda(&argumentos, saida, &prefixo, cfg.timeout_s).await?;
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
    Ok(Some(Transcrito {
        texto: texto.trim().to_string(),
        duracao,
    }))
}

/// Troca os marcadores do comando pelos caminhos desta chamada.
///
/// Um marcador desconhecido é erro, e não texto literal: `{modleo}` num config passaria batido
/// como nome de arquivo e o transcritor falharia com uma mensagem que não ajuda ninguém.
fn monta(comando: &[String], wav: &Path, modelo: &Path, prefixo: &Path) -> Result<Vec<String>> {
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

/// O Telegram manda Opus 48 kHz; o Whisper (e o sherpa, e o faster-whisper) querem PCM mono
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
    // Aviso do transcritor não derruba a transcrição, mas some do log se ninguém o registrar:
    // é assim que um "Fallback to cpu" passaria despercebido por semanas.
    let erro = String::from_utf8_lossy(&fim.stderr);
    if let Some(linha) = erro.lines().find(|l| l.to_lowercase().contains("fallback")) {
        warn!(aviso = %linha.trim(), "o transcritor avisou algo ao subir");
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

/// `~` no começo vira o home. O resto do caminho fica como está.
///
/// Lê `$HOME` direto, como o resto do projeto (`paths::home`), em vez de trazer a crate `dirs`
/// só para isto.
fn expande(bruto: &str) -> PathBuf {
    if let Some(resto) = bruto.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(resto);
    }
    PathBuf::from(bruto)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_base() -> Cfg {
        Cfg::default()
    }

    #[test]
    fn marcadores_viram_caminhos() {
        let a = monta(
            &[
                "bin".into(),
                "-m".into(),
                "{modelo}".into(),
                "-f".into(),
                "{audio}".into(),
            ],
            Path::new("/tmp/x.wav"),
            Path::new("/m/modelo.bin"),
            Path::new("/tmp/saida"),
        )
        .unwrap();
        assert_eq!(a, ["bin", "-m", "/m/modelo.bin", "-f", "/tmp/x.wav"]);
    }

    #[test]
    fn marcador_errado_e_erro_e_nao_nome_de_arquivo() {
        // `{modleo}` passaria como caminho literal e o erro apareceria lá na frente, longe daqui.
        let e = monta(
            &["bin".into(), "{modleo}".into()],
            Path::new("/tmp/x.wav"),
            Path::new("/m/m.bin"),
            Path::new("/tmp/s"),
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("{modleo}"), "{e:#}");
    }

    #[test]
    fn til_no_executavel_tambem_e_expandido() {
        // Só o modelo passava por `expande`, e o comando com "~/..." morria com ENOENT na hora
        // de rodar — o que os testes de marcador não pegam, porque nunca executam nada.
        let a = monta(
            &["~/bin/whisper".into(), "-f".into(), "{audio}".into()],
            Path::new("/tmp/x.wav"),
            Path::new("/m/m.bin"),
            Path::new("/tmp/s"),
        )
        .unwrap();
        assert!(
            !a[0].contains('~'),
            "executável ficou com til literal: {:?}",
            a[0]
        );
        assert!(a[0].ends_with("/bin/whisper"), "{:?}", a[0]);
    }

    #[test]
    fn comando_vazio_nao_passa() {
        assert!(monta(&[], Path::new("/a"), Path::new("/b"), Path::new("/c")).is_err());
    }

    #[test]
    fn saida_so_aceita_os_dois_modos() {
        assert_eq!(Saida::de("arquivo").unwrap(), Saida::Arquivo);
        assert_eq!(Saida::de("stdout").unwrap(), Saida::Stdout);
        assert!(Saida::de("Arquivo").is_err());
        assert!(Saida::de("").is_err());
    }

    #[tokio::test]
    async fn desligada_devolve_none_em_vez_de_erro() {
        let mut c = cfg_base();
        c.ativa = false;
        let r = transcreve(&c, Path::new("/nao/existe.oga")).await.unwrap();
        assert!(r.is_none(), "desligada não pode virar erro");
    }

    #[tokio::test]
    async fn modelo_que_nao_existe_diz_onde_procurou() {
        let mut c = cfg_base();
        c.ativa = true;
        c.modelo = "/nao/existe/modelo.bin".into();
        let e = transcreve(&c, Path::new("/tmp/x.oga")).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("/nao/existe/modelo.bin"), "{msg}");
    }

    #[test]
    fn til_vira_home() {
        let p = expande("~/x/y.bin");
        assert!(p.is_absolute(), "{p:?}");
        assert!(!p.to_string_lossy().contains('~'), "{p:?}");
        assert_eq!(expande("/ja/absoluto"), PathBuf::from("/ja/absoluto"));
    }
}
