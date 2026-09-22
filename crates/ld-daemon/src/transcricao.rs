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
use tokio::sync::Semaphore;
use tracing::{info, warn};

/// Uma transcrição por vez, sempre.
///
/// Não é sobre CPU: no Vulkan o trabalho ocupa 0,3 dos 8 núcleos. É sobre memória. Cada
/// transcrição pede ~1 GB (quase todo em GTT) e esta máquina tem por volta de 1,2 GB livres, então
/// dois áudios que cheguem juntos somam ~2 GB e empurram a máquina para swap — e swap aqui é zram,
/// que não devolve a memória, comprime e segura.
///
/// Esperar é melhor que engasgar: a transcrição já acontece fora do turno, então a fila só atrasa
/// a mensagem, enquanto o swap atrasaria a máquina inteira.
static UMA_POR_VEZ: Semaphore = Semaphore::const_new(1);

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

    // A vez chega antes do relógio começar: o tempo que interessa medir é o da transcrição, não
    // o da espera na fila, senão o log passa a acusar lentidão que é só concorrência.
    if UMA_POR_VEZ.available_permits() == 0 {
        info!(arquivo = %audio.display(), "outra transcrição está rodando; entro na fila");
    }
    let _vez = UMA_POR_VEZ
        .acquire()
        .await
        .expect("o semáforo da transcrição nunca é fechado");

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

/// A linha avisa que o backend pedido não subiu?
///
/// "fallback" precisa vir junto de um destino ("to cpu"), que é como os runtimes escrevem a
/// queda de verdade. Uma contagem de fallbacks de decodificação não é isso.
fn caiu_para_cpu(linha: &str) -> bool {
    let l = linha.to_lowercase();
    (l.contains("fallback") || l.contains("falling back")) && l.contains("cpu")
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
    fn contagem_de_fallback_nao_e_queda_de_backend() {
        // O whisper.cpp imprime isto em TODA execução normal; tratar como aviso faria o log
        // gritar sempre e o alerta real passar batido.
        assert!(!caiu_para_cpu(
            "whisper_print_timings:     fallbacks =   0 p /   0 h"
        ));
        assert!(!caiu_para_cpu("total fallbacks = 3"));
        // Estas são as quedas de verdade, escritas como os runtimes escrevem.
        assert!(caiu_para_cpu(
            "provider.cc:StringToProvider:37 Unsupported string: webgpu. Fallback to cpu"
        ));
        assert!(caiu_para_cpu(
            "Available providers: CPUExecutionProvider. Fallback to cpu!"
        ));
        assert!(caiu_para_cpu("WARNING: falling back to CPU"));
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn duas_transcricoes_nao_rodam_juntas() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Um "transcritor" que só marca quantos estão dentro ao mesmo tempo. O que importa provar
        // não é que o comando roda, é que nunca há dois ocupando memória de uma vez.
        let dentro = Arc::new(AtomicUsize::new(0));
        let maximo = Arc::new(AtomicUsize::new(0));

        let mut tarefas = Vec::new();
        for _ in 0..4 {
            let (dentro, maximo) = (Arc::clone(&dentro), Arc::clone(&maximo));
            tarefas.push(tokio::spawn(async move {
                let _vez = UMA_POR_VEZ.acquire().await.unwrap();
                let agora = dentro.fetch_add(1, Ordering::SeqCst) + 1;
                maximo.fetch_max(agora, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                dentro.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for t in tarefas {
            t.await.unwrap();
        }

        assert_eq!(
            maximo.load(Ordering::SeqCst),
            1,
            "duas transcrições rodaram juntas: seriam ~2 GB de pico numa máquina com ~1,2 GB livres"
        );
        assert_eq!(
            UMA_POR_VEZ.available_permits(),
            1,
            "a vez não foi devolvida no fim"
        );
    }

    #[test]
    fn til_vira_home() {
        let p = expande("~/x/y.bin");
        assert!(p.is_absolute(), "{p:?}");
        assert!(!p.to_string_lossy().contains('~'), "{p:?}");
        assert_eq!(expande("/ja/absoluto"), PathBuf::from("/ja/absoluto"));
    }
}
