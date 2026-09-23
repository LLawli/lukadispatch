//! Testes da porta do divisor.
//!
//! Os que dependem de ferramenta externa (7z, rar, ffmpeg) pulam quando ela falta, porque aí o
//! daemon também recusa antes de tentar. A cadeia é testada com divisores de mentira.

use std::sync::Mutex;

use super::rar::VolumesRar;
use super::sete_z::Volumes7z;
use super::video::TrechosDeVideo;
use super::*;

/// Bytes pseudoaleatórios: com texto repetido o compactador geraria um volume só e o teste não
/// provaria nada.
fn incompressivel(caminho: &Path, bytes: usize) {
    let mut dados = Vec::with_capacity(bytes);
    let mut x: u32 = 12345;
    while dados.len() < bytes {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223);
        dados.extend_from_slice(&x.to_le_bytes());
    }
    dados.truncate(bytes);
    std::fs::write(caminho, &dados).unwrap();
}

fn tamanhos(p: &Partes) -> Vec<u64> {
    p.arquivos
        .iter()
        .map(|a| a.metadata().unwrap().len())
        .collect()
}

// ------------------------------------------------------------------ regra de volume

#[test]
fn volume_tem_margem_abaixo_do_teto() {
    assert_eq!(volume_para(50 * 1024 * 1024), 45 * 1024 * 1024);
    assert!(volume_para(100) < 100);
}

// ------------------------------------------------------------------ implementações reais

/// O mesmo roteiro para os dois divisores por volume: 3 MB com teto de 1 MB.
async fn volumes_respeitam_o_teto(d: &dyn Divisor, sufixo_primeiro: &str) {
    let raiz = tempfile::tempdir().unwrap();
    let grande = raiz.path().join("grande.bin");
    incompressivel(&grande, 3 * 1024 * 1024);
    let teto = 1024 * 1024;

    assert!(
        d.aceita(&grande),
        "divisor por volume aceita qualquer arquivo"
    );
    let partes = d
        .divide(&grande, raiz.path().join("partes"), teto)
        .await
        .unwrap();

    assert!(
        partes.arquivos.len() >= 3,
        "{} partes",
        partes.arquivos.len()
    );
    let t = tamanhos(&partes);
    assert!(t.iter().all(|&b| b > 0), "parte vazia: {t:?}");
    assert!(t.iter().all(|&b| b <= teto), "parte acima do teto: {t:?}");
    assert!(
        t.iter().sum::<u64>() >= 3 * 1024 * 1024,
        "as partes somam menos que o original: {t:?}"
    );
    let primeiro = partes.arquivos[0]
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(primeiro.ends_with(sufixo_primeiro), "{primeiro}");
    assert_eq!(partes.midia, Midia::Documento);
    assert!(
        partes.como_juntar.contains(&primeiro),
        "a instrução tem de dizer por onde abrir: {}",
        partes.como_juntar
    );
    assert!(partes.arquivos.iter().all(|a| a.starts_with(partes.dir())));

    let dir = partes.dir().to_path_buf();
    partes.limpa().await;
    assert!(!dir.exists(), "as partes têm que sumir depois do envio");
}

#[tokio::test]
async fn sete_z_parte_em_volumes_que_cabem() {
    let d = Volumes7z;
    assert_eq!(d.nome(), "7z");
    if !d.disponivel() {
        return;
    }
    volumes_respeitam_o_teto(&d, ".7z.001").await;
}

#[tokio::test]
async fn rar_parte_em_volumes_que_cabem() {
    let d = VolumesRar;
    assert_eq!(d.nome(), "rar");
    if !d.disponivel() {
        return;
    }
    volumes_respeitam_o_teto(&d, ".part1.rar").await;
}

#[test]
fn anuncio_dos_volumes_diz_o_tamanho() {
    let teto = 50 * 1024 * 1024;
    assert!(
        Volumes7z.anuncio(teto).contains("45 MB"),
        "{}",
        Volumes7z.anuncio(teto)
    );
    assert!(
        VolumesRar.anuncio(teto).contains("45 MB"),
        "{}",
        VolumesRar.anuncio(teto)
    );
}

#[test]
fn video_so_aceita_video() {
    let v = TrechosDeVideo;
    assert_eq!(v.nome(), "video");
    for bom in ["a.mp4", "a.MKV", "a.mov", "a.webm"] {
        assert!(v.aceita(Path::new(bom)), "{bom}");
    }
    for ruim in ["a.pdf", "a.7z", "a", "a.mp4.txt"] {
        assert!(!v.aceita(Path::new(ruim)), "{ruim}");
    }
}

#[tokio::test]
async fn video_vira_trechos_que_tocam_sozinhos() {
    let v = TrechosDeVideo;
    if !v.disponivel() {
        return;
    }
    let raiz = tempfile::tempdir().unwrap();
    let video = raiz.path().join("fonte.mp4");
    let feito = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=15:duration=12",
            "-c:v",
            "libx264",
            "-g",
            "15",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&video)
        .status()
        .await;
    if !feito.is_ok_and(|s| s.success()) {
        return; // sem codificador de vídeo nesta máquina
    }

    let tamanho = std::fs::metadata(&video).unwrap().len();
    // Teto de metade do arquivo: tem de sair mais de um trecho, todos abaixo dele.
    let teto = tamanho / 2;
    let partes = v
        .divide(&video, raiz.path().join("trechos"), teto)
        .await
        .unwrap();
    assert!(partes.arquivos.len() > 1, "era para cortar em mais de um");
    assert_eq!(partes.midia, Midia::Video);
    assert!(tamanhos(&partes).iter().all(|&b| b > 0 && b <= teto));
    assert!(
        partes.arquivos[0].to_string_lossy().ends_with(".mp4"),
        "trecho tem de continuar sendo mp4"
    );
    assert!(
        partes.como_juntar.contains("ffmpeg"),
        "{}",
        partes.como_juntar
    );
    partes.limpa().await;
}

// ------------------------------------------------------------------ a cadeia

/// Um divisor de mentira que registra quando foi chamado.
struct Falso {
    nome: &'static str,
    disponivel: bool,
    so_aceita: Option<&'static str>,
    falha: bool,
    chamado: Mutex<u32>,
}

impl Falso {
    fn new(nome: &'static str) -> Self {
        Self {
            nome,
            disponivel: true,
            so_aceita: None,
            falha: false,
            chamado: Mutex::new(0),
        }
    }
    fn chamadas(&self) -> u32 {
        *self.chamado.lock().unwrap()
    }
}

#[async_trait]
impl Divisor for Falso {
    fn nome(&self) -> &str {
        self.nome
    }
    fn disponivel(&self) -> bool {
        self.disponivel
    }
    fn aceita(&self, caminho: &Path) -> bool {
        self.so_aceita
            .is_none_or(|ext| caminho.extension().is_some_and(|e| e == ext))
    }
    fn anuncio(&self, _teto: u64) -> String {
        format!("anúncio de {}", self.nome)
    }
    async fn divide(&self, _caminho: &Path, dir: PathBuf, _teto: u64) -> Result<Partes> {
        *self.chamado.lock().unwrap() += 1;
        if self.falha {
            anyhow::bail!("{} quebrou", self.nome);
        }
        tokio::fs::create_dir_all(&dir).await?;
        let parte = dir.join(format!("{}.001", self.nome));
        tokio::fs::write(&parte, b"x").await?;
        Ok(Partes {
            dir,
            arquivos: vec![parte],
            midia: Midia::Documento,
            como_juntar: format!("junte com {}", self.nome),
        })
    }
}

#[tokio::test]
async fn cadeia_cai_para_o_proximo_quando_um_falha() {
    let raiz = tempfile::tempdir().unwrap();
    let mut quebrado = Falso::new("quebrado");
    quebrado.falha = true;
    let quebrado = Arc::new(quebrado);
    let bom = Arc::new(Falso::new("bom"));
    let c = Divisores::new(vec![quebrado.clone(), bom.clone()]);

    let partes = c
        .divide(Path::new("/x/a.bin"), raiz.path().join("p"), 100)
        .await
        .unwrap();
    assert_eq!(partes.como_juntar, "junte com bom");
    assert_eq!((quebrado.chamadas(), bom.chamadas()), (1, 1));
}

#[tokio::test]
async fn cadeia_pula_quem_nao_aceita_ou_nao_esta_instalado() {
    let raiz = tempfile::tempdir().unwrap();
    let mut so_video = Falso::new("video");
    so_video.so_aceita = Some("mp4");
    let so_video = Arc::new(so_video);
    let mut ausente = Falso::new("ausente");
    ausente.disponivel = false;
    let ausente = Arc::new(ausente);
    let volumes = Arc::new(Falso::new("volumes"));
    let c = Divisores::new(vec![so_video.clone(), ausente.clone(), volumes.clone()]);

    let nomes =
        |v: Vec<Arc<dyn Divisor>>| v.iter().map(|d| d.nome().to_string()).collect::<Vec<_>>();
    assert_eq!(nomes(c.candidatos(Path::new("/x/a.pdf"))), ["volumes"]);
    assert_eq!(
        nomes(c.candidatos(Path::new("/x/a.mp4"))),
        ["video", "volumes"]
    );

    c.divide(Path::new("/x/a.pdf"), raiz.path().join("p"), 100)
        .await
        .unwrap();
    assert_eq!(so_video.chamadas(), 0, "não aceita pdf");
    assert_eq!(ausente.chamadas(), 0, "não está instalado");
    assert_eq!(volumes.chamadas(), 1);
}

#[tokio::test]
async fn sem_candidato_o_erro_diz_o_que_faltou() {
    let mut ausente = Falso::new("7z");
    ausente.disponivel = false;
    let c = Divisores::new(vec![Arc::new(ausente)]);
    let raiz = tempfile::tempdir().unwrap();
    let e = c
        .divide(Path::new("/x/a.bin"), raiz.path().join("p"), 100)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("7z"), "{e:#}");
    assert!(c.candidatos(Path::new("/x/a.bin")).is_empty());
}

#[tokio::test]
async fn todos_falhando_o_erro_traz_cada_motivo() {
    let mut a = Falso::new("primeiro");
    a.falha = true;
    let mut b = Falso::new("segundo");
    b.falha = true;
    let c = Divisores::new(vec![Arc::new(a), Arc::new(b)]);
    let raiz = tempfile::tempdir().unwrap();
    let e = format!(
        "{:#}",
        c.divide(Path::new("/x/a.bin"), raiz.path().join("p"), 100)
            .await
            .unwrap_err()
    );
    assert!(
        e.contains("primeiro quebrou") && e.contains("segundo quebrou"),
        "{e}"
    );
}

// ------------------------------------------------------------------ escolha pelo config

#[test]
fn config_padrao_e_video_e_depois_7z() {
    let c = Divisores::da_config(&Arquivos::default()).unwrap();
    assert_eq!(c.nomes(), ["video", "7z"]);
}

#[test]
fn config_troca_7z_por_rar_e_desliga_o_video() {
    let c = Divisores::da_config(&Arquivos {
        divisor: "rar".into(),
        cortar_video: false,
    })
    .unwrap();
    assert_eq!(c.nomes(), ["rar"]);
}

#[test]
fn divisor_desconhecido_falha_na_partida_dizendo_qual() {
    let e = Divisores::da_config(&Arquivos {
        divisor: "zip-magico".into(),
        cortar_video: true,
    })
    .err()
    .expect("desconhecido tem de ser erro");
    assert!(format!("{e:#}").contains("zip-magico"), "{e:#}");
}
