//! O passo de transcrição do setup: instalar um motor de voz, ou deixar a voz chegar como áudio.
//!
//! São dois motores, os dois medidos com áudio real ([decisão 0007]):
//!
//! - **whisper.cpp com `large-v3-turbo` em q5_0**: o que menos erra, inclusive em jargão e em
//!   palavra em inglês, ao custo de ~1 GB de memória e ~45 s por minuto de fala sem GPU dedicada.
//! - **FastConformer-pt no sherpa-onnx**: ~5x mais rápido e com uma fração da memória, mas erra
//!   quase o dobro em jargão, nome próprio e inglês. Serve bem para fala corrida.
//!
//! O programa vem pronto da release do lukadispatch (compilado uma vez no CI, conferido pelo
//! sha256 que a release publica); o modelo vem da fonte dele, com tamanho e sha256 fixados aqui.
//! Antes de gravar o config, o setup transcreve um áudio de teste pelo mesmo caminho que o daemon
//! usa: motor instalado que não transcreve não é motor instalado.
//!
//! [decisão 0007]: ../../../../docs/decisoes/0007-transcricao.md

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use ld_core::config::Transcricao;
use sha2::{Digest, Sha256};

use super::arquivos::{Rascunho, com_til};
use super::tela::Tela;
use crate::transcritor::Transcritor;
use crate::transcritor::processo::ProcessoExterno;

pub struct Modelo {
    /// O arquivo baixado, dentro de `<dir>/modelos`.
    pub arquivo: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub tamanho: u64,
    /// Para modelo que vem num `.tar.bz2`: o diretório que ele cria ao ser extraído, e que é o
    /// `{modelo}` do comando.
    pub extrai_em: Option<&'static str>,
}

pub struct Motor {
    pub rotulo: &'static str,
    pub explica: &'static str,
    /// O pacote da release com o programa: `lukadispatch-<pacote>-linux-<arq>.tar.gz`.
    pub pacote: &'static str,
    pub modelo: Modelo,
}

pub const WHISPER: Motor = Motor {
    rotulo: "whisper large-v3-turbo",
    explica: "o que menos erra, inclusive em jargão e palavra em inglês; baixa 574 MB e usa \
              ~1 GB de memória enquanto transcreve (~45 s por minuto de fala sem GPU dedicada)",
    pacote: "whisper",
    modelo: Modelo {
        arquivo: "ggml-large-v3-turbo-q5_0.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin",
        sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
        tamanho: 574_041_195,
        extrai_em: None,
    },
};

pub const FASTCONFORMER: Motor = Motor {
    rotulo: "FastConformer-pt",
    explica: "~5x mais rápido e leve (baixa 106 MB); ótimo em fala corrida, mas erra quase o \
              dobro em jargão, nome próprio e palavra em inglês",
    pacote: "sherpa",
    modelo: Modelo {
        arquivo: "sherpa-onnx-nemo-transducer-stt_pt_fastconformer_hybrid_large_pc-int8.tar.bz2",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-transducer-stt_pt_fastconformer_hybrid_large_pc-int8.tar.bz2",
        sha256: "870714949ac14c5a81a14e9f87f3e5e1bc81b127e23daff40c935de690a91bd2",
        tamanho: 106_541_116,
        extrai_em: Some("sherpa-onnx-nemo-transducer-stt_pt_fastconformer_hybrid_large_pc-int8"),
    },
};

pub const MOTORES: [&Motor; 2] = [&WHISPER, &FASTCONFORMER];

/// De onde vêm os pacotes com os programas: a release desta mesma versão do lukadispatch.
/// `LUKADISPATCH_ASR_URL` troca a base (um diretório local com `file://`, para testar um pacote
/// antes de publicá-lo).
pub fn base_dos_pacotes() -> String {
    std::env::var("LUKADISPATCH_ASR_URL").unwrap_or_else(|_| {
        format!(
            "https://github.com/LLawli/lukadispatch/releases/download/v{}",
            env!("CARGO_PKG_VERSION")
        )
    })
}

/// O `[transcricao]` de um motor instalado em `dir`, com o programa `binario` de lá.
pub fn config_do_motor(motor: &Motor, dir: &Path, binario: &str, threads: usize) -> Transcricao {
    let exe = dir.join(binario).to_string_lossy().into_owned();
    let modelos = dir.join("modelos");
    let t = threads.to_string();
    let (comando, modelo, saida): (Vec<String>, PathBuf, &str) = match motor.pacote {
        "whisper" => (
            [
                exe.as_str(),
                "-m",
                "{modelo}",
                "-f",
                "{audio}",
                "-l",
                "pt",
                "-t",
                &t,
                "-otxt",
                "-of",
                "{saida}",
                "-nt",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            modelos.join(motor.modelo.arquivo),
            "arquivo",
        ),
        _ => (
            vec![
                exe,
                "--encoder={modelo}/encoder.int8.onnx".into(),
                "--decoder={modelo}/decoder.int8.onnx".into(),
                "--joiner={modelo}/joiner.int8.onnx".into(),
                "--tokens={modelo}/tokens.txt".into(),
                "--model-type=nemo_transducer".into(),
                format!("--num-threads={t}"),
                "{audio}".into(),
            ],
            modelos.join(motor.modelo.extrai_em.unwrap_or(motor.modelo.arquivo)),
            "json",
        ),
    };
    Transcricao {
        ativa: true,
        motor: "processo".into(),
        comando,
        modelo: modelo.to_string_lossy().into_owned(),
        saida: saida.into(),
        ..Transcricao::default()
    }
}

/// O passo inteiro: oferece, instala, testa e escreve o config.
/// `base` é de onde vêm os pacotes com os programas (ver [`base_dos_pacotes`]).
pub async fn configura(
    tela: &mut Tela<'_>,
    r: &mut Rascunho,
    home: &Path,
    base: &str,
) -> Result<()> {
    tela.passo("Transcrição de voz");
    let atual = &r.atual.transcricao;
    let pronta = atual.ativa && instalado(atual, home);
    if r.havia_config && !r.refazer {
        if pronta {
            tela.diz(&format!("{}: ok.", nome_do_programa(atual)));
            return Ok(());
        }
        if !atual.ativa {
            tela.diz("Desligada. Para instalar um motor: lukadispatch setup --refazer");
            return Ok(());
        }
        // Ativa no config, mas o programa não está onde o comando aponta: é a pendência que o
        // setup existe para resolver.
        tela.diz(&format!(
            "Está ativa no config, mas o motor não está instalado ({} não existe).",
            atual
                .comando
                .first()
                .map(String::as_str)
                .unwrap_or("comando vazio")
        ));
    } else if pronta {
        tela.diz(&format!(
            "A transcrição já está instalada ({}).",
            nome_do_programa(atual)
        ));
        if tela.sim("Manter como está?")? {
            return Ok(());
        }
    }

    tela.diz(
        "Mensagem de voz vira texto aqui na sua máquina, sem o áudio sair dela, e só vai para a \
         sessão depois do seu aval.",
    );
    let mut opcoes: Vec<(&str, &str)> = MOTORES.iter().map(|m| (m.rotulo, m.explica)).collect();
    opcoes.push((
        "agora não",
        "a voz chega à sessão como arquivo de áudio, sem texto",
    ));
    let i = tela.escolhe("Qual motor instalar?", &opcoes)?;
    let Some(motor) = MOTORES.get(i) else {
        r.poe(Some("transcricao"), "ativa", false);
        tela.diz("Para instalar depois, rode lukadispatch setup de novo.");
        return Ok(());
    };

    let cfg = match instala(tela, r, home, base, motor).await {
        Ok(cfg) => cfg,
        Err(e) => {
            // O bot e o grupo já estão configurados a esta altura: um download que falhou não
            // pode levar o setup inteiro junto.
            tela.diz(&format!("A instalação da transcrição falhou: {e:#}"));
            if tela.sim("Seguir sem transcrição por enquanto?")? {
                r.poe(Some("transcricao"), "ativa", false);
                tela.diz("Para tentar de novo, rode lukadispatch setup.");
                return Ok(());
            }
            return Err(e);
        }
    };

    r.poe(Some("transcricao"), "ativa", true);
    r.poe(Some("transcricao"), "motor", "processo");
    r.poe(
        Some("transcricao"),
        "comando",
        cfg.comando
            .iter()
            .map(String::as_str)
            .collect::<toml_edit::Array>(),
    );
    r.poe(Some("transcricao"), "modelo", cfg.modelo.as_str());
    r.poe(Some("transcricao"), "saida", cfg.saida.as_str());
    tela.diz(&format!("Transcrição instalada: {}.", motor.rotulo));
    Ok(())
}

async fn instala(
    tela: &mut Tela<'_>,
    r: &Rascunho,
    home: &Path,
    base: &str,
    motor: &Motor,
) -> Result<Transcricao> {
    if !(r.tem_programa)("ffmpeg") {
        tela.diz(
            "Aviso: o ffmpeg não está no PATH, e é ele que converte a voz para o motor. Instale \
             pelo gerenciador de pacotes da sua distro.",
        );
    }
    let padrao = com_til(&home.join(".local/share/lukadispatch/asr"), home);
    let dir = expande(
        &tela.pergunta("Onde guardar o programa e o modelo", &padrao)?,
        home,
    );
    std::fs::create_dir_all(dir.join("modelos"))
        .with_context(|| format!("criando {}", dir.display()))?;

    let binario = instala_programa(tela, motor, &dir, base)?;
    instala_modelo(tela, motor, &dir)?;

    let threads = std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(4);
    let cfg = config_do_motor(motor, &dir, &binario, threads);
    if (r.tem_programa)("ffmpeg") {
        testa(tela, motor, &cfg, &dir).await?;
    }
    Ok(cfg)
}

fn instalado(t: &Transcricao, home: &Path) -> bool {
    let Some(exe) = t.comando.first() else {
        return false;
    };
    let exe = expande(exe, home);
    exe.is_file() && (t.modelo.is_empty() || expande(&t.modelo, home).exists())
}

fn nome_do_programa(t: &Transcricao) -> String {
    t.comando
        .first()
        .and_then(|c| Path::new(c).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn expande(caminho: &str, home: &Path) -> PathBuf {
    match caminho.strip_prefix("~/") {
        Some(resto) => home.join(resto),
        None => PathBuf::from(caminho),
    }
}

/// Baixa e extrai o pacote da release, e devolve qual programa dele usar.
fn instala_programa(tela: &mut Tela<'_>, motor: &Motor, dir: &Path, base: &str) -> Result<String> {
    let arq = match std::env::consts::ARCH {
        a @ ("x86_64" | "aarch64") => a,
        outra => bail!("não há pacote de transcrição pronto para {outra}"),
    };
    let nome = format!("lukadispatch-{}-linux-{arq}.tar.gz", motor.pacote);
    tela.diz(&format!("Baixando o programa ({nome})..."));
    let sha = baixa_texto(&format!("{base}/{nome}.sha256"))?;
    let sha = sha
        .split_whitespace()
        .next()
        .context("o .sha256 do pacote veio vazio")?
        .to_string();
    let pacote = dir.join(&nome);
    baixa(tela, &format!("{base}/{nome}"), &pacote, &sha, None)?;
    extrai(&pacote, dir, "-xzf")?;
    let _ = std::fs::remove_file(&pacote);

    Ok(match motor.pacote {
        // A variante Vulkan usa a GPU quando há uma e cai para a CPU sozinha quando não há; o
        // que ela não aguenta é faltar a libvulkan, e aí nem abre.
        "whisper" if abre(&dir.join("whisper-cli")) => "whisper-cli".into(),
        "whisper" => {
            tela.diz("Sem Vulkan nesta máquina: fica a variante só de CPU.");
            "whisper-cli-cpu".into()
        }
        _ => "sherpa-onnx-offline".into(),
    })
}

fn abre(exe: &Path) -> bool {
    Command::new(exe)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn instala_modelo(tela: &mut Tela<'_>, motor: &Motor, dir: &Path) -> Result<()> {
    let m = &motor.modelo;
    let modelos = dir.join("modelos");
    if let Some(sub) = m.extrai_em
        && modelos.join(sub).join("tokens.txt").is_file()
    {
        tela.diz("O modelo já está lá.");
        return Ok(());
    }
    let destino = modelos.join(m.arquivo);
    if destino.is_file() && sha256_de(&destino)? == m.sha256 {
        tela.diz("O modelo já está lá.");
        return Ok(());
    }
    tela.diz(&format!(
        "Baixando o modelo ({} MB)...",
        m.tamanho / 1_000_000
    ));
    baixa(tela, m.url, &destino, m.sha256, Some(m.tamanho))?;
    if m.extrai_em.is_some() {
        extrai(&destino, &modelos, "-xjf")?;
        let _ = std::fs::remove_file(&destino);
    }
    Ok(())
}

/// Transcreve um áudio de teste pelo mesmo motor que o daemon vai usar.
async fn testa(tela: &mut Tela<'_>, motor: &Motor, cfg: &Transcricao, dir: &Path) -> Result<()> {
    // O whisper traz o áudio no nosso pacote; o modelo do sherpa traz o dele em test_wavs/, e o
    // nome muda de modelo para modelo (o do FastConformer-pt é pt_br.wav).
    let audio = match motor.pacote {
        "whisper" => Some(dir.join("teste.wav")),
        _ => primeiro_wav(&PathBuf::from(&cfg.modelo).join("test_wavs")),
    };
    let Some(audio) = audio.filter(|a| a.is_file()) else {
        tela.diz("Sem áudio de teste no pacote; a transcrição fica sem conferência.");
        return Ok(());
    };
    tela.diz("Testando com um áudio curto...");
    let texto = ProcessoExterno::new(cfg)?
        .transcreve(&audio)
        .await
        .context("o motor instalado não conseguiu transcrever o áudio de teste")?
        .texto;
    tela.diz(&format!("Ouviu: \"{}\"", texto.trim()));
    Ok(())
}

fn primeiro_wav(dir: &Path) -> Option<PathBuf> {
    let mut wavs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "wav"))
        .collect();
    wavs.sort();
    wavs.into_iter().next()
}

/// Baixa `url` para `destino`, conferindo o sha256 (e o tamanho, quando conhecido). Arquivo que
/// já está lá com o sha certo não é baixado de novo; com o sha errado, é refeito.
pub fn baixa(
    tela: &Tela<'_>,
    url: &str,
    destino: &Path,
    sha256: &str,
    tamanho: Option<u64>,
) -> Result<()> {
    if destino.is_file() && sha256_de(destino)? == sha256 {
        return Ok(());
    }
    let parcial = PathBuf::from(format!("{}.parcial", destino.display()));
    let mut curl = Command::new("curl");
    curl.args(["-fL", "--retry", "3"]);
    if tela.terminal() {
        curl.arg("--progress-bar");
    } else {
        curl.arg("-sS");
    }
    let ok = curl
        .arg("-o")
        .arg(&parcial)
        .arg(url)
        .status()
        .context("chamando o curl (ele está instalado?)")?
        .success();
    let conferido = (|| -> Result<()> {
        if !ok {
            bail!("o download de {url} falhou");
        }
        let obtido = std::fs::metadata(&parcial)?.len();
        if let Some(t) = tamanho
            && obtido != t
        {
            bail!("{url} veio com {obtido} bytes, esperava {t}");
        }
        let sha = sha256_de(&parcial)?;
        if sha != sha256 {
            bail!("o sha256 de {url} não confere: veio {sha}, esperava {sha256}");
        }
        Ok(())
    })();
    if let Err(e) = conferido {
        let _ = std::fs::remove_file(&parcial);
        return Err(e);
    }
    std::fs::rename(&parcial, destino)
        .with_context(|| format!("trocando {}", destino.display()))?;
    Ok(())
}

fn baixa_texto(url: &str) -> Result<String> {
    let saida = Command::new("curl")
        .args(["-fsSL", "--retry", "3", url])
        .output()
        .context("chamando o curl (ele está instalado?)")?;
    if !saida.status.success() {
        bail!(
            "não consegui baixar {url}: {}",
            String::from_utf8_lossy(&saida.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&saida.stdout).into_owned())
}

pub fn sha256_de(caminho: &Path) -> Result<String> {
    let mut f =
        std::fs::File::open(caminho).with_context(|| format!("abrindo {}", caminho.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn extrai(pacote: &Path, dir: &Path, modo: &str) -> Result<()> {
    let ok = Command::new("tar")
        .arg(modo)
        .arg(pacote)
        .arg("-C")
        .arg(dir)
        .status()
        .context("chamando o tar")?
        .success();
    if !ok {
        bail!("não consegui extrair {}", pacote.display());
    }
    Ok(())
}
