//! Testes da porta de transcrição e do motor `"processo"`.
//!
//! Os casos de borda rodam um comando de shell no lugar do transcritor: são sobre o que o daemon
//! faz quando o motor se comporta mal, e para isso não é preciso (nem honesto) carregar um
//! modelo de 548 MB.

use std::path::{Path, PathBuf};

use ld_core::config::Transcricao;

use super::processo::{self, Saida, UMA_POR_VEZ, caiu_para_cpu, expande, monta};
use super::*;

fn cfg_com(comando: &[&str], saida: &str, timeout_s: u64) -> Transcricao {
    Transcricao {
        ativa: true,
        motor: "processo".into(),
        comando: comando.iter().map(|s| s.to_string()).collect(),
        modelo: String::new(),
        saida: saida.into(),
        timeout_s,
        guardar_audio_dias: 7,
    }
}

fn motor(cfg: &Transcricao) -> ProcessoExterno {
    ProcessoExterno::new(cfg).expect("config válido")
}

/// Um .oga de verdade, pequeno, gerado pelo ffmpeg. Sem ele o teste mediria a conversão
/// falhando, e não o caso que se quer.
fn audio_curto(dir: &Path) -> Option<PathBuf> {
    let destino = dir.join("t.oga");
    let ok = std::process::Command::new("ffmpeg")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=0.3",
            "-c:a",
            "libopus",
        ])
        .arg(&destino)
        .status()
        .ok()
        .is_some_and(|s| s.success());
    ok.then_some(destino)
}

// ------------------------------------------------------------------ escolha do motor

#[test]
fn desligada_nao_tem_transcritor() {
    let c = Transcricao {
        ativa: false,
        ..Transcricao::default()
    };
    assert!(
        da_config(&c).unwrap().is_none(),
        "desligada não pode virar erro"
    );
}

#[test]
fn o_padrao_e_o_motor_processo() {
    let t = da_config(&Transcricao::default())
        .unwrap()
        .expect("ligada por padrão");
    assert_eq!(t.nome(), "processo");
}

#[test]
fn motor_desconhecido_falha_na_partida_dizendo_qual() {
    let c = Transcricao {
        motor: "nuvem-magica".into(),
        ..Transcricao::default()
    };
    let e = da_config(&c)
        .err()
        .expect("motor desconhecido tem de ser erro");
    assert!(format!("{e:#}").contains("nuvem-magica"), "{e:#}");
}

#[test]
fn saida_invalida_falha_na_partida_e_nao_no_primeiro_audio() {
    let e = ProcessoExterno::new(&cfg_com(&["bin"], "Arquivo", 60))
        .err()
        .expect("saida inválida");
    assert!(format!("{e:#}").contains("Arquivo"), "{e:#}");
    assert!(
        ProcessoExterno::new(&cfg_com(&[], "stdout", 60)).is_err(),
        "comando vazio"
    );
}

#[test]
fn modelo_ausente_nao_impede_a_partida() {
    // O disco de dados pode montar depois do daemon; recusar aqui desligaria a transcrição
    // até o próximo restart.
    let mut c = cfg_com(&["bin", "{modelo}"], "stdout", 60);
    c.modelo = "/nao/existe/modelo.bin".into();
    assert!(ProcessoExterno::new(&c).is_ok());
}

// ------------------------------------------------------------------ peças puras

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
    assert!(!caiu_para_cpu(
        "whisper_print_timings:     fallbacks =   0 p /   0 h"
    ));
    assert!(!caiu_para_cpu("total fallbacks = 3"));
    assert!(caiu_para_cpu(
        "provider.cc:StringToProvider:37 Unsupported string: webgpu. Fallback to cpu"
    ));
    assert!(caiu_para_cpu(
        "Available providers: CPUExecutionProvider. Fallback to cpu!"
    ));
    assert!(caiu_para_cpu("WARNING: falling back to CPU"));
}

#[test]
fn saida_so_aceita_os_dois_modos() {
    assert_eq!(Saida::de("arquivo").unwrap(), Saida::Arquivo);
    assert_eq!(Saida::de("stdout").unwrap(), Saida::Stdout);
    assert!(Saida::de("Arquivo").is_err());
    assert!(Saida::de("").is_err());
}

#[test]
fn til_vira_home() {
    let p = expande("~/x/y.bin");
    assert!(p.is_absolute(), "{p:?}");
    assert!(!p.to_string_lossy().contains('~'), "{p:?}");
    assert_eq!(expande("/ja/absoluto"), PathBuf::from("/ja/absoluto"));
}

// ------------------------------------------------------------------ comportamento do processo

#[tokio::test]
async fn modelo_que_nao_existe_diz_onde_procurou() {
    let mut c = cfg_com(&["bin", "{modelo}"], "stdout", 60);
    c.modelo = "/nao/existe/modelo.bin".into();
    let e = motor(&c)
        .transcreve(Path::new("/tmp/x.oga"))
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("/nao/existe/modelo.bin"), "{e:#}");
}

#[tokio::test]
async fn transcritor_que_nao_devolve_texto_e_erro_e_nao_mensagem_vazia() {
    let dir = tempfile::tempdir().unwrap();
    let Some(audio) = audio_curto(dir.path()) else {
        return;
    };
    let e = motor(&cfg_com(&["true"], "stdout", 60))
        .transcreve(&audio)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("não devolveu texto"), "{e:#}");
}

#[tokio::test]
async fn audio_longo_que_estoura_o_prazo_e_derrubado() {
    let dir = tempfile::tempdir().unwrap();
    let Some(audio) = audio_curto(dir.path()) else {
        return;
    };
    let inicio = std::time::Instant::now();
    let e = motor(&cfg_com(&["sleep", "30"], "stdout", 1))
        .transcreve(&audio)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("passou de 1s"), "{e:#}");
    assert!(
        inicio.elapsed() < std::time::Duration::from_secs(10),
        "o prazo não interrompeu de verdade: levou {:?}",
        inicio.elapsed()
    );
}

#[tokio::test]
async fn transcritor_que_falha_diz_o_que_ele_reclamou() {
    let dir = tempfile::tempdir().unwrap();
    let Some(audio) = audio_curto(dir.path()) else {
        return;
    };
    let e = motor(&cfg_com(
        &["sh", "-c", "echo deu ruim no modelo >&2; exit 3"],
        "stdout",
        60,
    ))
    .transcreve(&audio)
    .await
    .unwrap_err();
    assert!(format!("{e:#}").contains("deu ruim no modelo"), "{e:#}");
}

#[tokio::test]
async fn arquivo_que_nao_e_audio_falha_na_conversao_e_nao_no_modelo() {
    let dir = tempfile::tempdir().unwrap();
    let falso = dir.path().join("nao-e-audio.oga");
    std::fs::write(&falso, b"isto nao e um ogg").unwrap();
    let e = motor(&cfg_com(&["true"], "stdout", 60))
        .transcreve(&falso)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("ffmpeg"), "{e:#}");
}

#[tokio::test]
async fn saida_por_arquivo_le_o_txt_que_o_comando_escreveu() {
    let dir = tempfile::tempdir().unwrap();
    let Some(audio) = audio_curto(dir.path()) else {
        return;
    };
    let cfg = cfg_com(
        &["sh", "-c", "printf 'oi do arquivo' > \"$0.txt\"", "{saida}"],
        "arquivo",
        60,
    );
    let t = motor(&cfg).transcreve(&audio).await.unwrap();
    assert_eq!(t.texto, "oi do arquivo");
}

#[tokio::test]
async fn saida_por_stdout_vem_aparada() {
    let dir = tempfile::tempdir().unwrap();
    let Some(audio) = audio_curto(dir.path()) else {
        return;
    };
    let t = motor(&cfg_com(&["echo", "  oi do stdout  "], "stdout", 60))
        .transcreve(&audio)
        .await
        .unwrap();
    assert_eq!(t.texto, "oi do stdout");
}

#[tokio::test]
async fn comando_que_nao_existe_nao_derruba_o_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let Some(audio) = audio_curto(dir.path()) else {
        return;
    };
    let e = motor(&cfg_com(&["/nao/existe/transcritor"], "stdout", 60))
        .transcreve(&audio)
        .await
        .unwrap_err();
    assert!(
        format!("{e:#}").contains("/nao/existe/transcritor"),
        "{e:#}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duas_transcricoes_nao_rodam_juntas() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Cada transcrição pede ~1 GB e a máquina tem ~1,2 GB livres: duas juntas vão para o swap.
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
    assert_eq!(maximo.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn o_processo_de_verdade_respeita_a_vez() {
    // O teste acima prova o semáforo; este prova que `transcreve` o usa. Dois comandos que
    // marcam entrada e saída num arquivo não podem se sobrepor.
    let dir = tempfile::tempdir().unwrap();
    let Some(audio) = audio_curto(dir.path()) else {
        return;
    };
    let log = dir.path().join("log");
    let script = format!(
        "echo entra >> {l}; sleep 0.3; echo sai >> {l}; echo feito",
        l = log.display()
    );
    let m = Arc::new(motor(&cfg_com(&["sh", "-c", &script], "stdout", 60)));
    let (a, b) = (m.clone(), m.clone());
    let (audio_a, audio_b) = (audio.clone(), audio.clone());
    let (ra, rb) = tokio::join!(
        tokio::spawn(async move { a.transcreve(&audio_a).await }),
        tokio::spawn(async move { b.transcreve(&audio_b).await })
    );
    ra.unwrap().unwrap();
    rb.unwrap().unwrap();
    let linhas = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        linhas, "entra\nsai\nentra\nsai\n",
        "sobrepuseram: {linhas:?}"
    );
}

#[test]
fn processo_e_um_transcritor() {
    // Garante em tempo de compilação que o motor cabe atrás da trait como objeto.
    fn aceita(_: Arc<dyn Transcritor>) {}
    aceita(Arc::new(motor(&cfg_com(&["bin"], "stdout", 60))));
    let _ = processo::ProcessoExterno::new;
}
