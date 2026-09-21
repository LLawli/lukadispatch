//! Anexo que chega pelo Telegram: o que dá para baixar, com que nome e onde ele fica.
//!
//! O caminho do arquivo é o produto deste módulo: quem lê o arquivo é a sessão, com a ferramenta
//! Read, e ela só recebe um caminho absoluto na linha NDJSON. Por isso duas coisas importam mais
//! do que parecem:
//!
//! - **O nome é hostil até prova em contrário.** `file_name` vem do celular de quem mandou e o
//!   Telegram não promete nada sobre ele: `../../.ssh/authorized_keys` é um nome de arquivo
//!   válido do ponto de vista da API. Aqui ele é reduzido a um nome simples antes de virar
//!   caminho.
//! - **Nome que já existe não sobrescreve.** Duas fotos no mesmo segundo têm o mesmo nome
//!   derivado, e a segunda não pode apagar a primeira.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ld_core::paths;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{FileId, Message};

/// Teto do Bot API para baixar: acima disso o `getFile` recusa, então nem tentamos.
pub const LIMITE: u32 = 20 * 1024 * 1024;

/// Quanto do nome original sobrevive. O resto é cortado, mas a extensão fica: é ela que faz o
/// Read (e você, no celular) reconhecer o que é o arquivo.
const MAX_NOME: usize = 80;

/// Um anexo, já reduzido ao que interessa para baixar.
#[derive(Debug, Clone, PartialEq)]
pub struct Anexo {
    pub file_id: FileId,
    pub tamanho: u32,
    /// O nome que o Telegram mandou, quando mandou. Foto, sticker e nota de vídeo não têm.
    pub nome: Option<String>,
    /// Como a mensagem chamaria isto em português ("foto", "documento"). Vai para o texto que a
    /// sessão recebe quando não há legenda.
    pub tipo: &'static str,
}

/// O que veio junto da mensagem.
#[derive(Debug, Clone, PartialEq)]
pub enum Achado {
    Nada,
    Arquivos(Vec<Anexo>),
    /// Áudio ou mensagem de voz. Não baixamos de propósito: sem transcrição, um `.ogg` parado
    /// numa pasta não serve para nada à sessão, e entregá-lo caladamente seria pior do que
    /// dizer que ainda não dá.
    Audio,
}

/// O que dá para baixar nesta mensagem.
pub fn anexos(msg: &Message) -> Achado {
    if msg.audio().is_some() || msg.voice().is_some() {
        return Achado::Audio;
    }

    if let Some(d) = msg.document() {
        return um(&d.file, d.file_name.clone(), "documento");
    }
    if let Some(tamanhos) = msg.photo() {
        // `photo` é a mesma imagem em várias resoluções; a maior é a que presta. A ordem não é
        // garantida pela API, então escolhemos pelo tamanho em bytes em vez de pegar a última.
        let Some(maior) = tamanhos.iter().max_by_key(|p| p.file.size) else {
            return Achado::Nada;
        };
        return um(&maior.file, None, "foto");
    }
    if let Some(v) = msg.video() {
        return um(&v.file, v.file_name.clone(), "vídeo");
    }
    if let Some(a) = msg.animation() {
        return um(&a.file, a.file_name.clone(), "animação");
    }
    if let Some(v) = msg.video_note() {
        return um(&v.file, None, "nota de vídeo");
    }
    if let Some(s) = msg.sticker() {
        return um(&s.file, None, "figurinha");
    }
    Achado::Nada
}

fn um(meta: &teloxide::types::FileMeta, nome: Option<String>, tipo: &'static str) -> Achado {
    Achado::Arquivos(vec![Anexo {
        file_id: meta.id.clone(),
        tamanho: meta.size,
        nome,
        tipo,
    }])
}

/// Baixa o anexo para o diretório da sessão e devolve o caminho absoluto.
pub async fn baixa(bot: &Bot, session_id: &str, anexo: &Anexo) -> Result<PathBuf> {
    if anexo.tamanho > LIMITE {
        bail!(
            "{} tem {}, e o Bot API só entrega até 20 MB",
            anexo.tipo,
            humano(anexo.tamanho)
        );
    }

    let arquivo = bot
        .get_file(anexo.file_id.clone())
        .await
        .context("pedindo o arquivo ao Telegram")?;

    // Sem `file_name` (foto, figurinha), o caminho do lado do Telegram é a única fonte de
    // extensão: ele vem como "photos/file_42.jpg".
    let bruto = anexo.nome.clone().unwrap_or_else(|| arquivo.path.clone());
    let dir = paths::arquivos_dir(session_id);
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("criando {}", dir.display()))?;
    let destino = livre(&dir, &sanitiza(&bruto));

    let mut dst = tokio::fs::File::create(&destino)
        .await
        .with_context(|| format!("criando {}", destino.display()))?;
    if let Err(e) = bot.download_file(&arquivo.path, &mut dst).await {
        // Arquivo pela metade é pior que arquivo nenhum: a sessão abriria um PDF truncado sem
        // saber disso.
        let _ = tokio::fs::remove_file(&destino).await;
        return Err(e).context("baixando o arquivo");
    }

    // Código de saída zero não é prova de artefato: confere que chegou byte de verdade.
    let gravado = tokio::fs::metadata(&destino)
        .await
        .with_context(|| format!("conferindo {}", destino.display()))?
        .len();
    if gravado == 0 {
        let _ = tokio::fs::remove_file(&destino).await;
        bail!("o download veio vazio");
    }

    Ok(destino)
}

/// Apaga os arquivos de uma sessão que acabou. Best-effort: diretório que não existe (sessão que
/// nunca recebeu anexo) é o caso comum, e não é erro.
pub async fn limpa(session_id: &str) {
    let dir = paths::arquivos_dir(session_id);
    if dir.is_dir() {
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}

/// Apaga o que sobrou de sessões que já não existem.
///
/// O fim normal de uma sessão já leva os arquivos dela junto. Isto é para os outros fins: o
/// `/clear`, que troca o id da sessão sem passar pelo encerramento, o daemon morto no meio do
/// fechamento, e o reboot. Sem a varredura, esse resto fica em disco para sempre, e o dono dele
/// já não pode nem ler o tópico onde ele foi pedido.
pub async fn varre_orfaos(vivas: &std::collections::HashSet<String>) -> usize {
    varre_em(&paths::arquivos_base(), vivas).await
}

/// O corpo da varredura, com a raiz explícita: é assim que o teste roda num tempdir em vez de
/// mexer no `XDG_DATA_HOME` do processo inteiro.
async fn varre_em(raiz: &Path, vivas: &std::collections::HashSet<String>) -> usize {
    let Ok(mut entradas) = tokio::fs::read_dir(raiz).await else {
        return 0;
    };
    let mut apagados = 0;
    while let Ok(Some(e)) = entradas.next_entry().await {
        let nome = e.file_name();
        let Some(nome) = nome.to_str() else { continue };
        if vivas.contains(nome) || !e.path().is_dir() {
            continue;
        }
        if tokio::fs::remove_dir_all(e.path()).await.is_ok() {
            apagados += 1;
        }
    }
    apagados
}

/// Teto do Bot API para o bot mandar arquivo. É maior que o de baixar, e não é engano: a
/// assimetria está na documentação do Telegram.
pub const LIMITE_ENVIO: u64 = 50 * 1024 * 1024;

/// Teto de uma imagem enviada como foto. Acima disso, mesmo sendo imagem, vai como documento.
pub const LIMITE_FOTO: u64 = 10 * 1024 * 1024;

/// Um arquivo do disco pronto para sair pelo Telegram.
pub struct ParaEnviar {
    pub caminho: PathBuf,
    pub tamanho: u64,
    /// Se vale a pena tentar como foto (aparece na conversa em vez de virar download).
    pub como_foto: bool,
}

/// Confere o que o agente pediu para mandar, antes de qualquer chamada de rede.
///
/// Falha cedo e com o motivo escrito: quem lê o erro é o agente, dentro da sessão, e ele precisa
/// saber se o caminho está errado, se o arquivo está vazio ou se é grande demais.
pub fn para_enviar(caminho: &Path, como_arquivo: bool) -> Result<ParaEnviar> {
    let meta =
        std::fs::metadata(caminho).with_context(|| format!("não achei {}", caminho.display()))?;
    if meta.is_dir() {
        bail!(
            "{} é um diretório; o Telegram só recebe arquivo (compacte antes)",
            caminho.display()
        );
    }
    if !meta.is_file() {
        bail!("{} não é um arquivo comum", caminho.display());
    }
    if meta.len() == 0 {
        bail!("{} está vazio", caminho.display());
    }
    if meta.len() > LIMITE_ENVIO {
        bail!(
            "{} tem {}, e o Bot API só envia até 50 MB",
            caminho.display(),
            humano_u64(meta.len())
        );
    }
    Ok(ParaEnviar {
        caminho: caminho.to_path_buf(),
        tamanho: meta.len(),
        como_foto: !como_arquivo && meta.len() <= LIMITE_FOTO && e_imagem(caminho),
    })
}

/// Formato que o Telegram mostra inline como foto. GIF e SVG ficam de fora: o primeiro vira
/// animação (e perde a animação em `sendPhoto`), o segundo o Telegram nem renderiza.
fn e_imagem(caminho: &Path) -> bool {
    caminho
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "webp"
            )
        })
}

/// Reduz o que veio da API a um nome de arquivo simples: sem diretório, sem surpresa.
fn sanitiza(bruto: &str) -> String {
    // `file_name` do Telegram é texto livre. Ficar só com o último componente derruba de uma vez
    // `../`, caminho absoluto e barra invertida do Windows.
    let base = bruto
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    let limpo: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Ponto na borda faz "." e "..", e faz arquivo oculto sem ninguém pedir.
    let limpo = limpo.trim_matches('.');
    if limpo.is_empty() {
        return "arquivo".into();
    }

    // Nome longo demais estoura o limite do sistema de arquivos; cortar o miolo preserva a
    // extensão, que é o que faz o arquivo ser reconhecido depois.
    match limpo.rsplit_once('.') {
        Some((corpo, ext)) if !ext.is_empty() && ext.len() <= 16 => {
            let corpo: String = corpo.chars().take(MAX_NOME).collect();
            let corpo = if corpo.is_empty() { "arquivo" } else { &corpo };
            format!("{corpo}.{ext}")
        }
        _ => limpo.chars().take(MAX_NOME).collect(),
    }
}

/// Um caminho que ainda não existe: `nota.pdf`, depois `nota-2.pdf`, e assim por diante.
fn livre(dir: &Path, nome: &str) -> PathBuf {
    let candidato = dir.join(nome);
    if !candidato.exists() {
        return candidato;
    }
    let (corpo, ext) = match nome.rsplit_once('.') {
        Some((c, e)) if !c.is_empty() => (c.to_string(), format!(".{e}")),
        _ => (nome.to_string(), String::new()),
    };
    for n in 2..10_000 {
        let candidato = dir.join(format!("{corpo}-{n}{ext}"));
        if !candidato.exists() {
            return candidato;
        }
    }
    // Dez mil homônimos na mesma sessão é cenário de erro, não de uso: sobrescrever aqui é
    // melhor que devolver caminho impossível.
    dir.join(nome)
}

/// Tamanho para ler no celular.
pub fn humano(bytes: u32) -> String {
    humano_u64(bytes as u64)
}

pub fn humano_u64(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * KB;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caminho_no_nome_vira_nome_simples() {
        assert_eq!(sanitiza("../../.ssh/authorized_keys"), "authorized_keys");
        assert_eq!(sanitiza("/etc/passwd"), "passwd");
        assert_eq!(sanitiza(r"C:\Users\x\nota.pdf"), "nota.pdf");
        assert_eq!(sanitiza("photos/file_42.jpg"), "file_42.jpg");
    }

    #[test]
    fn nome_perigoso_nao_sobra() {
        for bruto in ["..", ".", "...", "/", ""] {
            let n = sanitiza(bruto);
            assert!(
                !n.is_empty() && n != "." && n != ".." && !n.contains('/'),
                "{bruto:?} virou {n:?}"
            );
        }
    }

    #[test]
    fn espaco_e_acento_viram_sublinhado() {
        assert_eq!(sanitiza("relatório final.pdf"), "relat_rio_final.pdf");
    }

    #[test]
    fn nome_gigante_e_cortado_mas_mantem_a_extensao() {
        let n = sanitiza(&format!("{}.pdf", "a".repeat(300)));
        assert!(n.ends_with(".pdf"), "{n}");
        assert!(n.len() <= MAX_NOME + 4, "{} caracteres", n.len());
    }

    #[test]
    fn segundo_arquivo_de_mesmo_nome_nao_sobrescreve() {
        let dir = tempfile::tempdir().unwrap();
        let primeiro = livre(dir.path(), "nota.pdf");
        assert_eq!(primeiro, dir.path().join("nota.pdf"));
        std::fs::write(&primeiro, b"x").unwrap();
        assert_eq!(
            livre(dir.path(), "nota.pdf"),
            dir.path().join("nota-2.pdf"),
            "o primeiro tem que continuar lá"
        );
    }

    #[tokio::test]
    async fn a_varredura_poupa_a_sessao_viva() {
        let raiz = tempfile::tempdir().unwrap();
        let viva = raiz.path().join("viva");
        let morta = raiz.path().join("morta");
        std::fs::create_dir_all(&viva).unwrap();
        std::fs::create_dir_all(&morta).unwrap();
        std::fs::write(morta.join("nota.pdf"), b"x").unwrap();

        let vivas = std::collections::HashSet::from(["viva".to_string()]);
        assert_eq!(varre_em(raiz.path(), &vivas).await, 1);
        assert!(viva.is_dir(), "a sessão viva não pode perder o que recebeu");
        assert!(!morta.exists());
        assert!(
            raiz.path().is_dir(),
            "a raiz de todas as sessões continua de pé"
        );
    }

    #[test]
    fn diretorio_e_vazio_nao_saem_daqui() {
        let dir = tempfile::tempdir().unwrap();
        assert!(para_enviar(dir.path(), false).is_err(), "diretório não vai");

        let vazio = dir.path().join("nada.txt");
        std::fs::write(&vazio, b"").unwrap();
        assert!(para_enviar(&vazio, false).is_err(), "arquivo vazio não vai");

        assert!(
            para_enviar(&dir.path().join("nao-existe"), false).is_err(),
            "caminho inexistente não vai"
        );
    }

    #[test]
    fn imagem_vai_como_foto_a_nao_ser_que_voce_peca_o_arquivo() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("grafico.PNG");
        std::fs::write(&png, b"x").unwrap();
        assert!(
            para_enviar(&png, false).unwrap().como_foto,
            "extensão não diferencia maiúscula"
        );
        assert!(
            !para_enviar(&png, true).unwrap().como_foto,
            "pedir o arquivo exato tem que valer mais que a conveniência"
        );

        let log = dir.path().join("saida.log");
        std::fs::write(&log, b"x").unwrap();
        assert!(!para_enviar(&log, false).unwrap().como_foto);
    }

    #[test]
    fn tamanho_legivel() {
        assert_eq!(humano(512), "512 B");
        assert_eq!(humano(2048), "2 KB");
        assert_eq!(humano(3 * 1024 * 1024), "3.0 MB");
    }
}
