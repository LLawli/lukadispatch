//! Testes do contrato comum a todo frontend: os tipos que não dependem de nenhuma plataforma
//! (limites, nome de arquivo seguro, apresentação padrão ao agente) e como eles se comportam.

use super::*;

#[test]
fn tipo_de_anexo_fala_portugues_e_concorda() {
    assert_eq!(TipoAnexo::Voz.nome(), "mensagem de voz");
    assert_eq!(TipoAnexo::Foto.artigo(), "a");
    assert_eq!(TipoAnexo::Documento.artigo(), "o");
    assert!(TipoAnexo::Voz.e_audio() && TipoAnexo::Audio.e_audio());
    assert!(!TipoAnexo::Video.e_audio(), "vídeo não vira transcrição");
}

#[test]
fn limites_padrao_sao_os_do_bot_api() {
    // O divisor e o envio de foto dependem destes números. Mudá-los sem querer faria um upload
    // estourar o teto da API no fim de uma subida longa.
    let l = Limites::default();
    assert_eq!(l.enviar, 50 * 1024 * 1024);
    assert_eq!(l.baixar, 20 * 1024 * 1024);
    assert_eq!(l.foto, 10 * 1024 * 1024);
    assert_eq!(l.dado_botao, 64);
}

#[test]
fn nome_com_caminho_vira_nome_simples_dentro_do_diretorio() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    assert_eq!(
        caminho_livre(d, "../../.ssh/authorized_keys"),
        d.join("authorized_keys")
    );
    assert_eq!(caminho_livre(d, "/etc/passwd"), d.join("passwd"));
    assert_eq!(caminho_livre(d, r"C:\Users\x\nota.pdf"), d.join("nota.pdf"));
    assert_eq!(
        caminho_livre(d, "photos/file_42.jpg"),
        d.join("file_42.jpg")
    );
}

#[test]
fn nome_perigoso_nao_sobra() {
    let dir = tempfile::tempdir().unwrap();
    for bruto in ["..", ".", "...", "/", ""] {
        let c = caminho_livre(dir.path(), bruto);
        assert_eq!(c.parent(), Some(dir.path()), "{bruto:?} escapou para {c:?}");
        let nome = c.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !nome.is_empty() && nome != "." && nome != "..",
            "{bruto:?} virou {nome:?}"
        );
    }
}

#[test]
fn espaco_e_acento_viram_sublinhado() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        caminho_livre(dir.path(), "relatório final.pdf"),
        dir.path().join("relat_rio_final.pdf")
    );
}

#[test]
fn nome_gigante_e_cortado_mas_mantem_a_extensao() {
    let dir = tempfile::tempdir().unwrap();
    let c = caminho_livre(dir.path(), &format!("{}.pdf", "a".repeat(300)));
    let nome = c.file_name().unwrap().to_string_lossy().into_owned();
    assert!(nome.ends_with(".pdf"), "{nome}");
    assert!(nome.len() <= 80 + 4, "{} caracteres", nome.len());
}

#[test]
fn segundo_arquivo_de_mesmo_nome_nao_sobrescreve() {
    let dir = tempfile::tempdir().unwrap();
    let primeiro = caminho_livre(dir.path(), "nota.pdf");
    assert_eq!(primeiro, dir.path().join("nota.pdf"));
    std::fs::write(&primeiro, b"x").unwrap();
    assert_eq!(
        caminho_livre(dir.path(), "nota.pdf"),
        dir.path().join("nota-2.pdf"),
        "o primeiro tem que continuar lá"
    );
}

#[tokio::test(start_paused = true)]
async fn efemera_apaga_depois_do_prazo_e_nao_antes() {
    let mem = memoria::Memoria::new();
    let fe: Arc<dyn Frontend> = mem.clone();
    let id = fe.envia(None, "oi", &[], None).await.unwrap();

    efemera(fe.clone(), id.clone(), 25);
    tokio::time::sleep(Duration::from_secs(24)).await;
    assert!(
        !mem.chamadas()
            .contains(&memoria::Chamada::Apaga(id.clone())),
        "apagou antes do prazo"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        mem.chamadas().contains(&memoria::Chamada::Apaga(id)),
        "não apagou depois do prazo"
    );
}

#[test]
fn todo_frontend_sabe_se_apresentar_ao_agente() {
    // Métodos com implementação padrão: um adaptador novo não precisa escrevê-los para
    // compilar, mas o padrão tem de produzir uma frase que faça sentido no prompt.
    let m = memoria::Memoria::new();
    assert!(!m.plataforma().is_empty());
    let onde = m.onde("proj");
    assert!(onde.contains("\"proj\""), "{onde}");
    assert!(onde.contains(m.plataforma()), "{onde}");
    assert!(
        !m.renderiza_markdown(),
        "na dúvida, o agente manda tabela como imagem"
    );
}
