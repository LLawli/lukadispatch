//! O lado do domínio no envio e recebimento de arquivo: o marcador `@arquivo:` que o agente
//! escreve na resposta, o que dá para mandar (com que mídia, se precisa dividir) e a limpeza dos
//! anexos de uma sessão.
//!
//! O que é específico de plataforma (baixar do Telegram, dividir em volumes de 7z, cortar vídeo)
//! mora nas portas: `frontend::telegram` e `divisor`. Este módulo só conhece a trait `Frontend`
//! e a trait `Divisor`, nunca uma API de chat.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::app::App;
use crate::divisor::{Divisores, MAX_PARTES, volume_para};
use crate::frontend::{Anexo, Limites};

/// Baixa um anexo recebido para dentro da pasta da sessão.
///
/// Recusa acima do teto de download do frontend antes de tentar (não adianta gastar banda para
/// depois jogar fora), e confere depois que o caminho devolvido pelo adaptador está mesmo dentro
/// da pasta da sessão e não veio vazio: um adaptador com bug não pode entregar um caminho que
/// escape da sandbox de arquivos, nem um download pela metade que a sessão leria como se fosse o
/// arquivo inteiro.
/// `caminho` é um arquivo diretamente dentro de `dir`?
///
/// A conferência é por componente, e não por prefixo: `Path::starts_with` é lexical, e
/// `<dir>/../../.ssh/authorized_keys` começa com os componentes de `dir`. `caminho_livre` sempre
/// devolve `dir` mais um nome simples, então qualquer outra forma é de um adaptador errado.
fn direto_em(caminho: &Path, dir: &Path) -> bool {
    use std::path::Component;
    caminho.parent() == Some(dir)
        && matches!(caminho.components().next_back(), Some(Component::Normal(_)))
        && !caminho
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
}

pub async fn recebe(app: &App, session_id: &str, anexo: &Anexo) -> Result<PathBuf> {
    let limites = app.frontend.limites();
    if anexo.tamanho > limites.baixar {
        bail!(
            "{} tem {}, e o frontend só entrega até {}",
            anexo.tipo.nome(),
            humano_u64(anexo.tamanho),
            humano_u64(limites.baixar)
        );
    }

    let dir = app.raiz_arquivos.join(session_id);
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("criando {}", dir.display()))?;

    let caminho = app.frontend.baixa(anexo, &dir).await?;

    if !direto_em(&caminho, &dir) {
        // Não apaga: o caminho está fora da pasta da sessão, e apagar ali seria o daemon mexer
        // num arquivo que não é dele por causa de um adaptador com bug.
        bail!("o adaptador devolveu um caminho fora da pasta da sessão: {caminho:?}");
    }
    let tamanho = tokio::fs::metadata(&caminho)
        .await
        .with_context(|| format!("conferindo {}", caminho.display()))?
        .len();
    if tamanho == 0 {
        let _ = tokio::fs::remove_file(&caminho).await;
        bail!("o download veio vazio");
    }

    Ok(caminho)
}

/// Apaga os arquivos de uma sessão que acabou. Best-effort: diretório que não existe (sessão que
/// nunca recebeu anexo) é o caso comum, e não é erro.
pub async fn limpa(raiz: &Path, session_id: &str) {
    let dir = raiz.join(session_id);
    if dir.is_dir() {
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}

/// Apaga o que sobrou de sessões que já não existem.
///
/// O fim normal de uma sessão já leva os arquivos dela junto. Isto é para os outros fins: o
/// `/clear`, que troca o id da sessão sem passar pelo encerramento, o daemon morto no meio do
/// fechamento, e o reboot. Sem a varredura, esse resto fica em disco para sempre, e o dono dele
/// já não pode nem ler o canal onde ele foi pedido.
pub async fn varre_orfaos(raiz: &Path, vivas: &std::collections::HashSet<String>) -> usize {
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

/// Apaga áudio transcrito que já passou da validade, sessão viva ou não.
///
/// O áudio original fica porque quando a transcrição sai estranha ele é a única forma de saber
/// se o erro foi do modelo ou da gravação. Mas fica por um prazo: voz acumula rápido e ninguém
/// audita uma transcrição de semanas atrás.
///
/// Só mexe em áudio. Os outros anexos seguem a vida da sessão, e apagá-los por idade tiraria da
/// sessão um arquivo que ela ainda pode estar usando.
pub async fn varre_audio_velho(raiz: &Path, dias: u64) -> usize {
    if dias == 0 {
        return 0;
    }
    let limite = std::time::Duration::from_secs(dias * 24 * 60 * 60);
    let agora = std::time::SystemTime::now();
    let Ok(mut sessoes) = tokio::fs::read_dir(raiz).await else {
        return 0;
    };
    let mut apagados = 0;
    while let Ok(Some(sessao)) = sessoes.next_entry().await {
        let Ok(mut entradas) = tokio::fs::read_dir(sessao.path()).await else {
            continue;
        };
        while let Ok(Some(e)) = entradas.next_entry().await {
            let caminho = e.path();
            if !ehaudio(&caminho) {
                continue;
            }
            // Sem data legível, não apaga: melhor guardar demais que apagar o que não devia.
            let Ok(idade) = e
                .metadata()
                .await
                .and_then(|m| m.modified())
                .map(|t| agora.duration_since(t).unwrap_or_default())
            else {
                continue;
            };
            if idade > limite && tokio::fs::remove_file(&caminho).await.is_ok() {
                apagados += 1;
            }
        }
    }
    apagados
}

/// Extensões que áudio de voz costuma usar.
fn ehaudio(caminho: &Path) -> bool {
    caminho
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| matches!(e.as_str(), "oga" | "ogg" | "opus" | "m4a" | "mp3" | "wav"))
}

/// Onde os volumes de uma sessão são montados: um subdiretório por envio, para dois arquivos
/// grandes ao mesmo tempo não se misturarem.
pub fn dir_partes(raiz: &Path, session_id: &str) -> PathBuf {
    raiz.join("partes")
        .join(session_id)
        .join(agora().to_string())
}

fn agora() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Um arquivo do disco pronto para sair pelo frontend.
#[derive(Debug)]
pub struct ParaEnviar {
    pub caminho: PathBuf,
    pub tamanho: u64,
    /// Se vale a pena tentar como foto (aparece na conversa em vez de virar download).
    pub como_foto: bool,
    /// Não cabe numa mensagem: vai em volumes, e o envio sai da frente do fim de turno.
    pub precisa_dividir: bool,
}

/// Confere o que o agente pediu para mandar, antes de qualquer chamada de rede.
///
/// Falha cedo e com o motivo escrito: quem lê o erro é o agente, dentro da sessão, e ele precisa
/// saber se o caminho está errado, se o arquivo está vazio, grande demais ou sem divisor que dê
/// conta dele.
pub fn para_enviar(
    caminho: &Path,
    como_arquivo: bool,
    limites: &Limites,
    divisores: &Divisores,
) -> Result<ParaEnviar> {
    let meta =
        std::fs::metadata(caminho).with_context(|| format!("não achei {}", caminho.display()))?;
    if meta.is_dir() {
        bail!(
            "{} é um diretório; o frontend só recebe arquivo (compacte antes)",
            caminho.display()
        );
    }
    if !meta.is_file() {
        bail!("{} não é um arquivo comum", caminho.display());
    }
    if meta.len() == 0 {
        bail!("{} está vazio", caminho.display());
    }
    let precisa_dividir = meta.len() > limites.enviar;
    if precisa_dividir {
        let previstas = meta.len().div_ceil(volume_para(limites.enviar)) as usize;
        if previstas > MAX_PARTES {
            bail!(
                "{} tem {}, o que daria {previstas} partes; o teto aqui é {MAX_PARTES}",
                caminho.display(),
                humano_u64(meta.len())
            );
        }
        if divisores.candidatos(caminho).is_empty() {
            bail!(
                "{} tem {} e passa do teto do frontend, mas nenhum divisor está disponível para ele",
                caminho.display(),
                humano_u64(meta.len())
            );
        }
    }
    Ok(ParaEnviar {
        caminho: caminho.to_path_buf(),
        tamanho: meta.len(),
        como_foto: !como_arquivo && meta.len() <= limites.foto && e_imagem(caminho),
        precisa_dividir,
    })
}

/// Formato que costuma ser mostrado inline como foto. GIF e SVG ficam de fora: o primeiro vira
/// animação (e perde a animação como foto), o segundo a maioria dos chats nem renderiza.
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

/// O que o agente escreve na resposta para mandar um arquivo junto.
///
/// Duas formas, e a segunda existe porque imagem sai recomprimida quando vai como foto:
///
/// ```text
/// @arquivo: /caminho/grafico.png | o gasto por dia
/// @documento: /caminho/grafico.png
/// ```
pub const MARCA_ARQUIVO: &str = "@arquivo:";
pub const MARCA_DOCUMENTO: &str = "@documento:";

/// Um envio pedido dentro da resposta do agente.
#[derive(Debug, Clone, PartialEq)]
pub struct Marcado {
    pub caminho: String,
    pub legenda: Option<String>,
    pub como_arquivo: bool,
}

/// Um pedaço da resposta, na ordem em que o agente escreveu.
#[derive(Debug, Clone, PartialEq)]
pub enum Pedaco {
    Texto(String),
    Envio(Marcado),
}

/// Quebra a resposta em pedaços, preservando a ordem entre texto e arquivo.
///
/// A ordem é o ponto: uma resposta que explica, mostra o gráfico, explica de novo e mostra o
/// log só faz sentido se chegar nessa sequência. Mandar tudo que é arquivo antes de tudo que é
/// texto embaralha o raciocínio, e no celular a legenda de uma imagem fica a três mensagens de
/// distância dela.
///
/// O reconhecimento do marcador é deliberadamente estreito, porque o custo de errar é alto nos
/// dois sentidos: um falso positivo manda um arquivo que ninguém pediu, e um falso negativo
/// deixa uma linha de sintaxe crua aparecendo no celular. Por isso a linha precisa ser **só** o
/// marcador, do começo ao fim: um `@arquivo:` no meio de uma frase, dentro de crase ou depois
/// de um hífen de lista é o agente FALANDO do formato, não usando ele.
pub fn divide_resposta(texto: &str) -> Vec<Pedaco> {
    // Resposta sem marcador nenhum sai byte a byte como o agente escreveu. É a esmagadora
    // maioria delas, e não há por que esta função tocar no que não veio mexer.
    if !texto.lines().any(|l| marcador(l).is_some()) {
        return if texto.trim().is_empty() {
            Vec::new()
        } else {
            vec![Pedaco::Texto(texto.to_string())]
        };
    }

    let mut pedacos = Vec::new();
    let mut acumulado: Vec<&str> = Vec::new();

    // Texto acumulado vira um pedaço só quando algo o interrompe: assim parágrafos seguidos
    // continuam numa mensagem única, em vez de virar uma mensagem por linha.
    let fecha = |acumulado: &mut Vec<&str>, pedacos: &mut Vec<Pedaco>| {
        let junto = acumulado.join("\n");
        acumulado.clear();
        if !junto.trim().is_empty() {
            pedacos.push(Pedaco::Texto(junto.trim().to_string()));
        }
    };

    for linha in texto.lines() {
        match marcador(linha) {
            Some(m) => {
                fecha(&mut acumulado, &mut pedacos);
                pedacos.push(Pedaco::Envio(m));
            }
            None => acumulado.push(linha),
        }
    }
    fecha(&mut acumulado, &mut pedacos);
    pedacos
}

fn marcador(linha: &str) -> Option<Marcado> {
    let t = linha.trim();
    let (resto, como_arquivo) = match (
        t.strip_prefix(MARCA_ARQUIVO),
        t.strip_prefix(MARCA_DOCUMENTO),
    ) {
        (Some(r), _) => (r, false),
        (_, Some(r)) => (r, true),
        _ => return None,
    };

    // " | " separa caminho e legenda. Caminho de verdade não tem essa sequência, e exigir os
    // espaços evita quebrar um nome que por acaso contenha barra vertical.
    let (caminho, legenda) = match resto.split_once(" | ") {
        Some((c, l)) => (
            c.trim(),
            Some(l.trim().to_string()).filter(|l| !l.is_empty()),
        ),
        None => (resto.trim(), None),
    };

    // Marcador sem caminho não é marcador: é uma linha de texto que por acaso começa assim, e
    // engoli-la esconderia do Luka o que o agente escreveu.
    if caminho.is_empty() {
        return None;
    }

    let caminho = expande_til(caminho);
    // Só caminho absoluto. O daemon roda com outro diretório atual, então relativo aqui não
    // significa nada, e adivinhar a base seria pior que recusar.
    if !caminho.starts_with('/') {
        return None;
    }

    Some(Marcado {
        caminho,
        legenda,
        como_arquivo,
    })
}

/// `~/x` vira `$HOME/x`. O til é do shell, e aqui não passa shell nenhum.
fn expande_til(caminho: &str) -> String {
    match caminho.strip_prefix("~/") {
        Some(resto) => match std::env::var_os("HOME") {
            Some(h) => Path::new(&h).join(resto).to_string_lossy().into_owned(),
            None => caminho.to_string(),
        },
        None => caminho.to_string(),
    }
}

/// Tamanho para ler no celular.
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
    fn so_arquivo_direto_na_pasta_da_sessao_conta_como_dentro() {
        let dir = Path::new("/home/luka/.local/share/lukadispatch/arquivos/s1");
        assert!(direto_em(&dir.join("nota.pdf"), dir));
        for fora in [
            dir.join("../../.ssh/authorized_keys"),
            dir.join(".."),
            dir.join("sub/nota.pdf"),
            Path::new("/etc/passwd").to_path_buf(),
            dir.to_path_buf(),
        ] {
            assert!(!direto_em(&fora, dir), "{fora:?} passou por dentro");
        }
    }
    use crate::divisor::Divisor;
    use async_trait::async_trait;

    #[tokio::test]
    async fn audio_velho_sai_e_o_resto_fica() {
        let dir = tempfile::tempdir().unwrap();
        let sessao = dir.path().join("sessao-1");
        tokio::fs::create_dir_all(&sessao).await.unwrap();

        let antigo = std::time::SystemTime::now() - std::time::Duration::from_secs(30 * 24 * 3600);
        for (nome, velho) in [
            ("voz_velha.oga", true),
            ("voz_nova.oga", false),
            ("relatorio_velho.pdf", true),
        ] {
            let f = sessao.join(nome);
            tokio::fs::write(&f, b"x").await.unwrap();
            if velho {
                let ft = filetime::FileTime::from_system_time(antigo);
                filetime::set_file_mtime(&f, ft).unwrap();
            }
        }

        let apagados = varre_audio_velho(dir.path(), 7).await;

        // A contagem sozinha passaria com 0 == 0: o que prende o teste é QUAL arquivo sobrou.
        assert_eq!(apagados, 1, "só o áudio velho devia sair");
        assert!(!sessao.join("voz_velha.oga").exists(), "áudio velho ficou");
        assert!(
            sessao.join("voz_nova.oga").exists(),
            "áudio novo foi apagado"
        );
        assert!(
            sessao.join("relatorio_velho.pdf").exists(),
            "a varredura mexeu num anexo que não é áudio"
        );
    }

    #[tokio::test]
    async fn prazo_zero_desliga_a_varredura() {
        let dir = tempfile::tempdir().unwrap();
        let sessao = dir.path().join("s");
        tokio::fs::create_dir_all(&sessao).await.unwrap();
        let f = sessao.join("voz.oga");
        tokio::fs::write(&f, b"x").await.unwrap();
        let antigo = std::time::SystemTime::now() - std::time::Duration::from_secs(400 * 24 * 3600);
        filetime::set_file_mtime(&f, filetime::FileTime::from_system_time(antigo)).unwrap();

        assert_eq!(varre_audio_velho(dir.path(), 0).await, 0);
        assert!(f.exists(), "prazo zero não pode apagar nada");
    }

    #[test]
    fn so_extensao_de_audio_conta() {
        for bom in ["a.oga", "a.OGG", "a.opus", "a.m4a", "a.mp3", "a.wav"] {
            assert!(ehaudio(Path::new(bom)), "{bom}");
        }
        for ruim in ["a.pdf", "a.png", "a.ogv", "a", "a.ogg.pdf"] {
            assert!(!ehaudio(Path::new(ruim)), "{ruim}");
        }
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
        assert_eq!(varre_orfaos(raiz.path(), &vivas).await, 1);
        assert!(viva.is_dir(), "a sessão viva não pode perder o que recebeu");
        assert!(!morta.exists());
        assert!(
            raiz.path().is_dir(),
            "a raiz de todas as sessões continua de pé"
        );
    }

    struct DivisorFalso(bool);
    #[async_trait]
    impl Divisor for DivisorFalso {
        fn nome(&self) -> &str {
            "falso"
        }
        fn disponivel(&self) -> bool {
            self.0
        }
        fn aceita(&self, _c: &Path) -> bool {
            true
        }
        fn anuncio(&self, _teto: u64) -> String {
            String::new()
        }
        async fn divide(
            &self,
            _c: &Path,
            dir: PathBuf,
            _teto: u64,
        ) -> Result<crate::divisor::Partes> {
            Ok(crate::divisor::Partes::new(
                dir,
                vec![],
                crate::frontend::Midia::Documento,
                String::new(),
            ))
        }
    }

    #[test]
    fn diretorio_e_vazio_nao_saem_daqui() {
        let dir = tempfile::tempdir().unwrap();
        let limites = Limites::default();
        let divisores = Divisores::new(vec![]);
        assert!(
            para_enviar(dir.path(), false, &limites, &divisores).is_err(),
            "diretório não vai"
        );

        let vazio = dir.path().join("nada.txt");
        std::fs::write(&vazio, b"").unwrap();
        assert!(
            para_enviar(&vazio, false, &limites, &divisores).is_err(),
            "arquivo vazio não vai"
        );

        assert!(
            para_enviar(&dir.path().join("nao-existe"), false, &limites, &divisores).is_err(),
            "caminho inexistente não vai"
        );
    }

    #[test]
    fn imagem_vai_como_foto_a_nao_ser_que_voce_peca_o_arquivo() {
        let dir = tempfile::tempdir().unwrap();
        let limites = Limites::default();
        let divisores = Divisores::new(vec![]);
        let png = dir.path().join("grafico.PNG");
        std::fs::write(&png, b"x").unwrap();
        assert!(
            para_enviar(&png, false, &limites, &divisores)
                .unwrap()
                .como_foto,
            "extensão não diferencia maiúscula"
        );
        assert!(
            !para_enviar(&png, true, &limites, &divisores)
                .unwrap()
                .como_foto,
            "pedir o arquivo exato tem que valer mais que a conveniência"
        );

        let log = dir.path().join("saida.log");
        std::fs::write(&log, b"x").unwrap();
        assert!(
            !para_enviar(&log, false, &limites, &divisores)
                .unwrap()
                .como_foto
        );
    }

    #[test]
    fn precisa_dividir_sem_candidato_e_erro() {
        let dir = tempfile::tempdir().unwrap();
        let grande = dir.path().join("grande.bin");
        std::fs::write(&grande, vec![7u8; 200]).unwrap();
        let limites = Limites {
            enviar: 100,
            ..Limites::default()
        };
        // Sem divisor disponível na cadeia.
        let sem_candidato = Divisores::new(vec![std::sync::Arc::new(DivisorFalso(false))]);
        let e = para_enviar(&grande, false, &limites, &sem_candidato).unwrap_err();
        assert!(format!("{e:#}").contains("nenhum divisor"), "{e:#}");

        let com_candidato = Divisores::new(vec![std::sync::Arc::new(DivisorFalso(true))]);
        assert!(
            para_enviar(&grande, false, &limites, &com_candidato)
                .unwrap()
                .precisa_dividir
        );
    }

    #[test]
    fn previsao_de_partes_acima_do_teto_e_erro() {
        let dir = tempfile::tempdir().unwrap();
        let grande = dir.path().join("grande.bin");
        std::fs::write(&grande, vec![7u8; 3000]).unwrap();
        let limites = Limites {
            enviar: 10,
            ..Limites::default()
        };
        let divisores = Divisores::new(vec![std::sync::Arc::new(DivisorFalso(true))]);
        let e = para_enviar(&grande, false, &limites, &divisores).unwrap_err();
        assert!(format!("{e:#}").contains("partes"), "{e:#}");
    }

    /// Achata os pedaços de volta em (texto, envios). Os testes que não são sobre ORDEM
    /// continuam mais legíveis assim.
    fn separa_para_teste(texto: &str) -> (String, Vec<Marcado>) {
        let mut t = Vec::new();
        let mut e = Vec::new();
        for p in divide_resposta(texto) {
            match p {
                Pedaco::Texto(s) => t.push(s),
                Pedaco::Envio(m) => e.push(m),
            }
        }
        (t.join("\n"), e)
    }

    #[test]
    fn marcador_sozinho_na_linha_e_um_envio() {
        assert_eq!(
            divide_resposta("olha o gráfico\n\n@arquivo: /tmp/g.png\n"),
            vec![
                Pedaco::Texto("olha o gráfico".into()),
                Pedaco::Envio(Marcado {
                    caminho: "/tmp/g.png".into(),
                    legenda: None,
                    como_arquivo: false,
                }),
            ]
        );
    }

    #[test]
    fn falar_do_formato_nao_manda_arquivo() {
        // Isto é o que trava o pior bug possível daqui: o agente explicando o marcador e, com
        // isso, disparando um envio.
        for linha in [
            "use @arquivo: /tmp/g.png no fim da resposta",
            "- `@arquivo: /tmp/g.png`",
            "escreva **@arquivo:** e o caminho",
            "@arquivo:",
            "@arquivo: relativo/g.png",
            "@arquivos: /tmp/g.png",
        ] {
            let (texto, envios) = separa_para_teste(linha);
            assert!(envios.is_empty(), "{linha:?} não podia virar envio");
            assert_eq!(texto, linha, "e o texto tem que sair intacto");
        }
    }

    #[test]
    fn legenda_e_documento_forcado() {
        let (texto, envios) = separa_para_teste("  @documento: /tmp/dados.csv | a planilha crua  ");
        assert!(texto.is_empty(), "sobrou {texto:?}");
        assert_eq!(envios[0].caminho, "/tmp/dados.csv");
        assert_eq!(envios[0].legenda.as_deref(), Some("a planilha crua"));
        assert!(envios[0].como_arquivo);
    }

    #[test]
    fn varios_arquivos_na_mesma_resposta() {
        // O que esta mudança comprou: antes os dois arquivos saíam juntos, antes de "antes",
        // e a legenda de cada um ficava longe do parágrafo que falava dele.
        let p = divide_resposta(
            "antes\n@arquivo: /tmp/a.png\nmeio\n@arquivo: /tmp/b.log | o log\ndepois",
        );
        assert_eq!(
            p,
            vec![
                Pedaco::Texto("antes".into()),
                Pedaco::Envio(Marcado {
                    caminho: "/tmp/a.png".into(),
                    legenda: None,
                    como_arquivo: false,
                }),
                Pedaco::Texto("meio".into()),
                Pedaco::Envio(Marcado {
                    caminho: "/tmp/b.log".into(),
                    legenda: Some("o log".into()),
                    como_arquivo: false,
                }),
                Pedaco::Texto("depois".into()),
            ]
        );
    }

    #[test]
    fn paragrafos_seguidos_continuam_numa_mensagem_so() {
        // Sem isto, cada linha em branco viraria uma notificação separada no celular.
        let p = divide_resposta("primeira\n\nsegunda\n\nterceira");
        assert_eq!(p.len(), 1, "{p:?}");
        assert_eq!(
            p[0],
            Pedaco::Texto("primeira\n\nsegunda\n\nterceira".into())
        );
    }

    #[test]
    fn resposta_que_e_so_marcador_nao_gera_mensagem_vazia() {
        let p = divide_resposta("@arquivo: /tmp/g.png");
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(matches!(p[0], Pedaco::Envio(_)));
    }

    #[test]
    fn arquivo_no_comeco_sai_antes_do_texto() {
        let p = divide_resposta("@arquivo: /tmp/g.png\ncomentário depois");
        assert!(matches!(p[0], Pedaco::Envio(_)), "{p:?}");
        assert_eq!(p[1], Pedaco::Texto("comentário depois".into()));
    }

    #[test]
    fn til_vira_home() {
        // Sem mexer no HOME do processo: outros testes leem o mesmo env, e trocá-lo aqui
        // quebraria quem estivesse rodando ao lado.
        let home = std::env::var("HOME").expect("HOME");
        let (_, envios) = separa_para_teste("@arquivo: ~/nota.pdf");
        assert_eq!(envios[0].caminho, format!("{home}/nota.pdf"));
    }

    #[test]
    fn resposta_sem_marcador_nao_e_tocada() {
        let original = "uma resposta normal\n\ncom duas linhas\n";
        let (texto, envios) = separa_para_teste(original);
        assert!(envios.is_empty());
        assert_eq!(
            texto, original,
            "sem envio, o texto não pode nem perder o \\n"
        );
    }

    #[test]
    fn tamanho_legivel() {
        assert_eq!(humano_u64(512), "512 B");
        assert_eq!(humano_u64(2048), "2 KB");
        assert_eq!(humano_u64(3 * 1024 * 1024), "3.0 MB");
    }
}
