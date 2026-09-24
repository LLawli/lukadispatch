//! Testes do passo de transcrição do setup: o config que cada motor gera, o download conferido e
//! a instalação inteira contra uma "release" local, com um motor de mentira que imprime o JSON
//! do sherpa-onnx.

use std::io::Cursor;
use std::path::Path;

use ld_core::config::Config;

use super::arquivos::Rascunho;
use super::tela::Tela;
use super::transcricao::{self, FASTCONFORMER, WHISPER, baixa, config_do_motor, sha256_de};
use crate::transcritor::ProcessoExterno;

fn tela_muda<'a>(entrada: &'a mut Cursor<Vec<u8>>, saida: &'a mut Vec<u8>) -> Tela<'a> {
    Tela::nova(entrada, saida, false)
}

#[test]
fn whisper_escreve_em_arquivo_e_sherpa_em_json() {
    let dir = Path::new("/opt/asr");
    let w = config_do_motor(&WHISPER, dir, "whisper-cli", 8);
    assert_eq!(w.comando[0], "/opt/asr/whisper-cli");
    assert_eq!(w.modelo, "/opt/asr/modelos/ggml-large-v3-turbo-q5_0.bin");
    assert_eq!(w.saida, "arquivo");
    assert!(w.comando.windows(2).any(|p| p == ["-t", "8"]));

    let s = config_do_motor(&FASTCONFORMER, dir, "sherpa-onnx-offline", 4);
    assert_eq!(s.saida, "json");
    assert!(
        s.modelo
            .ends_with("stt_pt_fastconformer_hybrid_large_pc-int8")
    );
    assert!(s.comando.contains(&"--num-threads=4".to_string()));
    assert_eq!(s.comando.last().unwrap(), "{audio}");

    // O daemon aceita os dois na partida (saida conhecida, comando não vazio).
    ProcessoExterno::new(&w).unwrap();
    ProcessoExterno::new(&s).unwrap();
}

#[test]
fn download_conferido_entra_e_sha_errado_nao_deixa_nada() {
    let dir = tempfile::tempdir().unwrap();
    let origem = dir.path().join("origem.bin");
    std::fs::write(&origem, b"conteudo de verdade").unwrap();
    let sha = sha256_de(&origem).unwrap();
    let url = format!("file://{}", origem.display());
    let (mut e, mut s) = (Cursor::new(Vec::new()), Vec::new());
    let tela = tela_muda(&mut e, &mut s);

    let destino = dir.path().join("baixado.bin");
    baixa(&tela, &url, &destino, &sha, Some(19)).unwrap();
    assert_eq!(std::fs::read(&destino).unwrap(), b"conteudo de verdade");

    let outro = dir.path().join("outro.bin");
    let erro = baixa(&tela, &url, &outro, &"0".repeat(64), None).unwrap_err();
    assert!(format!("{erro:#}").contains("não confere"), "{erro:#}");
    assert!(!outro.exists());
    assert!(
        !dir.path().join("outro.bin.parcial").exists(),
        "sobrou o parcial"
    );

    let errado = baixa(&tela, &url, &outro, &sha, Some(3)).unwrap_err();
    assert!(format!("{errado:#}").contains("19 bytes"), "{errado:#}");
}

#[test]
fn arquivo_que_ja_esta_la_com_o_sha_certo_nao_e_baixado_de_novo() {
    let dir = tempfile::tempdir().unwrap();
    let destino = dir.path().join("modelo.bin");
    std::fs::write(&destino, b"modelo").unwrap();
    let sha = sha256_de(&destino).unwrap();
    let (mut e, mut s) = (Cursor::new(Vec::new()), Vec::new());
    let tela = tela_muda(&mut e, &mut s);
    // A URL não existe: se ele tentasse baixar, falharia.
    baixa(&tela, "file:///nao/existe", &destino, &sha, None).unwrap();
}

#[tokio::test]
async fn agora_nao_desliga_a_transcricao() {
    let home = tempfile::tempdir().unwrap();
    let mut r = Rascunho::de(None, None).unwrap();
    let (mut e, mut s) = (Cursor::new(b"3\n".to_vec()), Vec::new());
    let mut tela = tela_muda(&mut e, &mut s);
    transcricao::configura(&mut tela, &mut r, home.path(), "file:///nada")
        .await
        .unwrap();
    let cfg: Config = toml::from_str(&r.config_texto()).unwrap();
    assert!(!cfg.transcricao.ativa);
}

#[tokio::test]
async fn ativa_no_config_sem_motor_instalado_e_pendencia() {
    // O caso que motivou a idempotência: o config diz que a voz vira texto, mas o programa não
    // está onde o comando aponta. Rodar o setup de novo tem de resolver isso, e só isso.
    let home = tempfile::tempdir().unwrap();
    let config = "[transcricao]\nativa = true\ncomando = [\"/nao/existe/whisper-cli\"]\n";
    let mut r = Rascunho::de(Some(config), None).unwrap();
    let (mut e, mut s) = (Cursor::new(b"3\n".to_vec()), Vec::new());
    let mut tela = tela_muda(&mut e, &mut s);
    transcricao::configura(&mut tela, &mut r, home.path(), "file:///nada")
        .await
        .unwrap();
    let conversa = String::from_utf8(s).unwrap();
    assert!(conversa.contains("não está instalado"), "{conversa}");
    assert!(conversa.contains("Qual motor instalar?"), "{conversa}");
}

#[tokio::test]
async fn desligada_por_escolha_nao_pergunta() {
    let home = tempfile::tempdir().unwrap();
    let mut r = Rascunho::de(Some("[transcricao]\nativa = false\n"), None).unwrap();
    let (mut e, mut s) = (Cursor::new(Vec::new()), Vec::new());
    let mut tela = tela_muda(&mut e, &mut s);
    transcricao::configura(&mut tela, &mut r, home.path(), "file:///nada")
        .await
        .unwrap();
    assert!(String::from_utf8(s).unwrap().contains("--refazer"));
}

#[tokio::test]
async fn download_que_falha_oferece_seguir_sem_derrubar_o_setup() {
    let home = tempfile::tempdir().unwrap();
    let mut r = Rascunho::de(None, None).unwrap();
    r.tem_programa = Box::new(|_| true);
    // FastConformer, diretório padrão, e "sim" para seguir sem.
    let (mut e, mut s) = (Cursor::new(b"2\n\ns\n".to_vec()), Vec::new());
    let mut tela = tela_muda(&mut e, &mut s);
    transcricao::configura(
        &mut tela,
        &mut r,
        home.path(),
        "file:///release/que/nao/existe",
    )
    .await
    .unwrap();
    let conversa = String::from_utf8(s).unwrap();
    assert!(
        conversa.contains("A instalação da transcrição falhou"),
        "{conversa}"
    );
    let cfg: Config = toml::from_str(&r.config_texto()).unwrap();
    assert!(!cfg.transcricao.ativa);
}

/// Uma "release" local com um `lukadispatch-sherpa-linux-<arq>.tar.gz` cujo programa imprime a
/// linha JSON do sherpa-onnx, e o modelo já extraído na home (o download do modelo de verdade
/// não cabe num teste). O que se testa é o caminho inteiro: baixar e conferir o pacote, extrair,
/// transcrever o áudio de teste pelo motor do daemon e escrever o config.
#[tokio::test]
async fn fastconformer_instala_testa_e_escreve_o_config() {
    if std::process::Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_err()
    {
        return;
    }
    let base = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let asr = home.path().join(".local/share/lukadispatch/asr");

    let conteudo = base.path().join("conteudo");
    std::fs::create_dir(&conteudo).unwrap();
    let programa = conteudo.join("sherpa-onnx-offline");
    std::fs::write(
        &programa,
        "#!/bin/sh\necho '{\"lang\": \"\", \"text\": \"oi do motor de mentira\", \"words\": []}'\n",
    )
    .unwrap();
    std::fs::set_permissions(
        &programa,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let arq = std::env::consts::ARCH;
    let pacote = base
        .path()
        .join(format!("lukadispatch-sherpa-linux-{arq}.tar.gz"));
    assert!(
        std::process::Command::new("tar")
            .arg("-czf")
            .arg(&pacote)
            .arg("-C")
            .arg(&conteudo)
            .arg(".")
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(
        format!("{}.sha256", pacote.display()),
        format!("{}  x\n", sha256_de(&pacote).unwrap()),
    )
    .unwrap();

    let modelo = asr
        .join("modelos")
        .join(FASTCONFORMER.modelo.extrai_em.unwrap());
    std::fs::create_dir_all(modelo.join("test_wavs")).unwrap();
    std::fs::write(modelo.join("tokens.txt"), "a 0\n").unwrap();
    assert!(
        std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"])
            .arg("sine=frequency=440:duration=0.3")
            .arg(modelo.join("test_wavs/pt_br.wav"))
            .status()
            .unwrap()
            .success()
    );

    let mut r = Rascunho::de(None, None).unwrap();
    r.tem_programa = Box::new(|_| true);
    let (mut e, mut s) = (Cursor::new(b"2\n\n".to_vec()), Vec::new());
    let mut tela = tela_muda(&mut e, &mut s);
    let url = format!("file://{}", base.path().display());
    transcricao::configura(&mut tela, &mut r, home.path(), &url)
        .await
        .unwrap();
    let conversa = String::from_utf8(s).unwrap();
    assert!(
        conversa.contains("Ouviu: \"oi do motor de mentira\""),
        "{conversa}"
    );
    assert!(conversa.contains("O modelo já está lá"), "{conversa}");
    assert!(
        !asr.join(pacote.file_name().unwrap()).exists(),
        "o pacote baixado fica para trás"
    );

    let cfg: Config = toml::from_str(&r.config_texto()).unwrap();
    assert!(cfg.transcricao.ativa);
    assert_eq!(cfg.transcricao.saida, "json");
    assert_eq!(
        cfg.transcricao.comando[0],
        asr.join("sherpa-onnx-offline").to_string_lossy()
    );
    assert_eq!(cfg.transcricao.modelo, modelo.to_string_lossy());
    crate::transcritor::da_config(&cfg.transcricao).unwrap();
}
