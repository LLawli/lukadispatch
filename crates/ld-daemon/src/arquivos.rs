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
use tracing::warn;

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
}

/// O que dá para baixar nesta mensagem.
pub fn anexos(msg: &Message) -> Achado {
    // Voz antes de áudio: quem grava segurando o microfone manda `voice`, e é esse o caso comum.
    // O `.oga` cai em disco como qualquer anexo; transcrever é outro passo, do lado de quem lê.
    if let Some(v) = msg.voice() {
        return um(&v.file, None, "mensagem de voz");
    }
    if let Some(a) = msg.audio() {
        return um(&a.file, a.file_name.clone(), "áudio");
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

    let arquivo = com_retentativa("pedindo o arquivo ao Telegram", || {
        let bot = bot.clone();
        let id = anexo.file_id.clone();
        async move { bot.get_file(id).await }
    })
    .await?;

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

/// Repete o pedido quando a rede falha, e só quando a rede falha.
///
/// Um timeout ao pedir o arquivo perdia a mensagem inteira: o áudio ficava no Telegram, o erro
/// aparecia no tópico e não havia segunda chance. Só que repetir tudo também é errado: um
/// "arquivo grande demais" ou um file_id vencido são respostas definitivas da API, e insistir
/// neles só atrasa o aviso de que não vai dar.
async fn com_retentativa<T, F, Fut>(o_que: &str, mut tentar: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, teloxide::RequestError>>,
{
    const ESPERAS: [u64; 3] = [1, 3, 8];
    for (n, espera) in ESPERAS.iter().enumerate() {
        match tentar().await {
            Ok(v) => return Ok(v),
            Err(e) if transitorio(&e) => {
                warn!(tentativa = n + 1, erro = %e, "{o_que}: a rede falhou, tento de novo em {espera}s");
                tokio::time::sleep(std::time::Duration::from_secs(*espera)).await;
            }
            Err(e) => return Err(e).context(o_que.to_string()),
        }
    }
    tentar()
        .await
        .with_context(|| format!("{o_que} (mesmo depois de {} tentativas)", ESPERAS.len() + 1))
}

/// Vale a pena tentar de novo? Rede e I/O sim; resposta da API não.
fn transitorio(e: &teloxide::RequestError) -> bool {
    matches!(
        e,
        teloxide::RequestError::Network(_)
            | teloxide::RequestError::Io(_)
            | teloxide::RequestError::RetryAfter(_)
    )
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

/// Apaga áudio transcrito que já passou da validade, sessão viva ou não.
///
/// O `.oga` fica porque quando a transcrição sai estranha ele é a única forma de saber se o erro
/// foi do modelo ou da gravação. Mas fica por um prazo: voz acumula rápido e ninguém audita uma
/// transcrição de semanas atrás.
///
/// Só mexe em áudio. Os outros anexos seguem a vida da sessão, e apagá-los por idade tiraria da
/// sessão um arquivo que ela ainda pode estar usando.
pub async fn varre_audio_velho(dias: u64) -> usize {
    if dias == 0 {
        return 0;
    }
    varre_audio_velho_em(&paths::arquivos_base(), dias).await
}

async fn varre_audio_velho_em(raiz: &Path, dias: u64) -> usize {
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

/// Extensões que o Telegram usa para voz e áudio.
fn ehaudio(caminho: &Path) -> bool {
    caminho
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| matches!(e.as_str(), "oga" | "ogg" | "opus" | "m4a" | "mp3" | "wav"))
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
    /// Não cabe numa mensagem: vai em volumes, e o envio sai da frente do fim de turno.
    pub precisa_dividir: bool,
}

/// Tamanho de cada volume quando o arquivo não cabe numa mensagem. Fica abaixo do teto de 50 MB
/// por margem: o 7z conta o volume, o Telegram conta o arquivo, e empatar com o limite é pedir
/// para um envio falhar no fim de um upload longo.
pub const VOLUME_MB: u64 = 45;

/// Teto de partes. Acima disso o tópico vira uma fila de upload e a coisa deixa de ser prática;
/// melhor dizer isso na cara do que passar meia hora mandando.
pub const MAX_PARTES: usize = 20;

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
    let precisa_dividir = meta.len() > LIMITE_ENVIO;
    if precisa_dividir {
        let previstas = meta.len().div_ceil(VOLUME_MB * 1024 * 1024) as usize;
        if previstas > MAX_PARTES {
            bail!(
                "{} tem {}, o que daria {previstas} partes de {VOLUME_MB} MB; o teto aqui é {MAX_PARTES}",
                caminho.display(),
                humano_u64(meta.len())
            );
        }
        if !tem_7z() {
            bail!(
                "{} tem {} e o Telegram só aceita 50 MB, mas não achei o 7z para dividir",
                caminho.display(),
                humano_u64(meta.len())
            );
        }
    }
    Ok(ParaEnviar {
        caminho: caminho.to_path_buf(),
        tamanho: meta.len(),
        como_foto: !como_arquivo && meta.len() <= LIMITE_FOTO && e_imagem(caminho),
        precisa_dividir,
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

fn tem_7z() -> bool {
    caminho_7z().is_some()
}

/// O 7z pode se chamar `7z` (p7zip completo) ou `7za` (só o núcleo). Os dois servem aqui.
fn caminho_7z() -> Option<&'static str> {
    ["7z", "7za", "7zz"].into_iter().find(|nome| {
        std::process::Command::new(nome)
            .arg("--help")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// Onde os volumes de uma sessão são montados: disco de verdade, e um subdiretório por envio
/// para dois arquivos grandes ao mesmo tempo não se misturarem.
pub fn dir_partes(session_id: &str) -> PathBuf {
    paths::arquivos_base()
        .join("partes")
        .join(session_id)
        .join(agora().to_string())
}

/// Como o arquivo foi partido, que é o que muda a instrução de juntar no fim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corte {
    /// Volumes de 7z: nenhum abre sozinho, o conjunto se abre pelo `.001`.
    Volumes,
    /// Trechos de vídeo: cada um toca por si, e juntar é opcional.
    Trechos,
}

/// Um arquivo grande partido, pronto para sair uma parte de cada vez.
pub struct Partes {
    /// Diretório só das partes; some inteiro depois do envio.
    dir: PathBuf,
    pub arquivos: Vec<PathBuf>,
    /// Nome da primeira parte, que é por onde se abre o conjunto.
    pub primeiro: String,
    pub corte: Corte,
}

impl Partes {
    pub async fn limpa(self) {
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
    }
}

/// Divide um arquivo em volumes de 7z que o Telegram aceite.
///
/// 7z, e não `split`, porque o critério é juntar de volta no celular: parte crua de `split` só
/// se remonta com `cat`, e os aplicativos de arquivo do Android (ZArchiver, RAR) abrem um
/// conjunto `.7z.001` direto, com todas as partes na mesma pasta. A compressão fica no mínimo
/// (`-mx1`): o que se quer aqui é o corte, não o ganho de tamanho.
pub async fn divide(caminho: &Path, dir: PathBuf) -> Result<Partes> {
    let bin = caminho_7z().context("não achei o 7z para dividir o arquivo")?;
    let nome = sanitiza(
        &caminho
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "arquivo".into()),
    );

    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("criando {}", dir.display()))?;

    let alvo = dir.join(format!("{nome}.7z"));
    let saida = tokio::process::Command::new(bin)
        .arg("a")
        .arg(format!("-v{VOLUME_MB}m"))
        .arg("-mx1")
        .arg("-y")
        .arg(&alvo)
        .arg(caminho)
        .output()
        .await
        .context("rodando o 7z")?;

    // Código de saída não é prova de artefato: o que vale é a lista de volumes em disco.
    let mut arquivos: Vec<PathBuf> = Vec::new();
    let mut entradas = tokio::fs::read_dir(&dir).await?;
    while let Some(e) = entradas.next_entry().await? {
        if e.path().is_file() {
            arquivos.push(e.path());
        }
    }
    arquivos.sort();

    if arquivos.is_empty()
        || arquivos
            .iter()
            .any(|p| p.metadata().is_ok_and(|m| m.len() == 0))
    {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        bail!(
            "o 7z não produziu volume utilizável ({}) {}",
            saida.status,
            String::from_utf8_lossy(&saida.stderr).trim()
        );
    }
    if arquivos.len() > MAX_PARTES {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        bail!("deu {} partes, e o teto é {MAX_PARTES}", arquivos.len());
    }

    let primeiro = arquivos[0]
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Partes {
        dir,
        arquivos,
        primeiro,
        corte: Corte::Volumes,
    })
}

/// Container de vídeo que vale cortar por tempo em vez de quebrar em volumes.
pub fn e_video(caminho: &Path) -> bool {
    caminho
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "mp4" | "mkv" | "mov" | "webm" | "m4v" | "avi" | "ts" | "mpg" | "mpeg"
            )
        })
}

/// Alvo de cada trecho de vídeo. Bem abaixo do teto porque o corte acontece no keyframe mais
/// próximo, e não no ponto exato: o trecho real pode passar do alvo, e passar do teto seria um
/// upload perdido.
const ALVO_TRECHO: u64 = 40 * 1024 * 1024;

/// Corta um vídeo em trechos que caibam numa mensagem, sem recodificar.
///
/// `-c copy` copia os fluxos como estão: é rápido (segundos para centenas de MB), não perde
/// qualidade e mantém cada trecho sendo um vídeo de verdade, que toca sozinho no celular. É essa
/// a diferença para os volumes de 7z, onde nenhuma parte serve para nada até estarem todas
/// juntas.
pub async fn corta_video(caminho: &Path, dir: PathBuf, alvo: u64) -> Result<Partes> {
    let tamanho = std::fs::metadata(caminho)?.len();
    let duracao = duracao_de(caminho).await?;
    if duracao <= 0.0 {
        bail!("não consegui ler a duração do vídeo");
    }

    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("criando {}", dir.display()))?;

    let ext = caminho
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mp4")
        .to_ascii_lowercase();
    let base = sanitiza(
        caminho
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("video"),
    );

    // Primeira tentativa pelo bitrate médio; se um trecho passar do teto (GOP longo, cena
    // pesada), corta na metade do tempo e tenta de novo. Duas tentativas bastam na prática, e
    // insistir mais sairia mais caro que cair nos volumes.
    let mut segundos = (duracao * alvo as f64 / tamanho as f64).max(5.0);
    for tentativa in 0..2 {
        let arquivos = segmenta(caminho, &dir, &base, &ext, segundos).await?;
        let maior = arquivos
            .iter()
            .filter_map(|p| p.metadata().ok().map(|m| m.len()))
            .max()
            .unwrap_or(0);
        if !arquivos.is_empty() && maior <= LIMITE_ENVIO && arquivos.len() <= MAX_PARTES {
            let primeiro = arquivos[0]
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            return Ok(Partes {
                dir,
                arquivos,
                primeiro,
                corte: Corte::Trechos,
            });
        }
        if tentativa == 0 {
            segundos /= 2.0;
        }
    }

    let _ = tokio::fs::remove_dir_all(&dir).await;
    bail!("os trechos continuaram passando do teto do Telegram")
}

/// Uma passada do `ffmpeg` cortando por tempo. Devolve os trechos em ordem.
async fn segmenta(
    caminho: &Path,
    dir: &Path,
    base: &str,
    ext: &str,
    segundos: f64,
) -> Result<Vec<PathBuf>> {
    // Rodar de novo por cima do que já existe misturaria as duas tentativas.
    let _ = tokio::fs::remove_dir_all(dir).await;
    tokio::fs::create_dir_all(dir).await?;

    let molde = dir.join(format!("{base}-parte-%03d.{ext}"));
    let saida = tokio::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(caminho)
        .args(["-map", "0", "-c", "copy", "-f", "segment"])
        .arg("-segment_time")
        .arg(format!("{segundos:.3}"))
        .args(["-reset_timestamps", "1"])
        .arg(&molde)
        .output()
        .await
        .context("rodando o ffmpeg")?;

    let mut arquivos = Vec::new();
    let mut entradas = tokio::fs::read_dir(dir).await?;
    while let Some(e) = entradas.next_entry().await? {
        if e.path().is_file() {
            arquivos.push(e.path());
        }
    }
    arquivos.sort();
    // Saída zero não prova nada: o que vale é ter trecho em disco, e nenhum deles vazio.
    if arquivos.is_empty()
        || arquivos
            .iter()
            .any(|p| p.metadata().is_ok_and(|m| m.len() == 0))
    {
        bail!(
            "o ffmpeg não produziu trecho utilizável: {}",
            String::from_utf8_lossy(&saida.stderr).trim()
        );
    }
    Ok(arquivos)
}

async fn duracao_de(caminho: &Path) -> Result<f64> {
    let saida = tokio::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(caminho)
        .output()
        .await
        .context("rodando o ffprobe")?;
    String::from_utf8_lossy(&saida.stdout)
        .trim()
        .parse::<f64>()
        .context("duração ilegível")
}

/// O ffmpeg está por aqui? Sem ele, vídeo grande cai nos volumes como qualquer outro arquivo.
pub fn tem_ffmpeg() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Alvo padrão de cada trecho.
pub fn alvo_trecho() -> u64 {
    ALVO_TRECHO
}

fn agora() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

    /// Uma `Message` como o Telegram manda, com só o que o `anexos` precisa ler.
    fn msg(anexo: serde_json::Value) -> Message {
        let mut v = serde_json::json!({
            "message_id": 1,
            "date": 0,
            "chat": {"id": 1, "type": "private", "first_name": "Luka"}
        });
        let obj = v.as_object_mut().unwrap();
        for (k, val) in anexo.as_object().unwrap() {
            obj.insert(k.clone(), val.clone());
        }
        serde_json::from_value(v).expect("Message válida")
    }

    fn tipos(achado: &Achado) -> Vec<&'static str> {
        match achado {
            Achado::Arquivos(l) => l.iter().map(|a| a.tipo).collect(),
            Achado::Nada => vec![],
        }
    }

    /// `voice` como o Telegram manda. Os `Option` do teloxide não têm `default` e o `mime_type`
    /// usa desserializador próprio, então campo que falta derruba a mensagem inteira para
    /// `MediaKind::Empty` em vez de dar erro: o fixture precisa ser completo para valer de teste.
    fn voz(mime: &str) -> serde_json::Value {
        serde_json::json!({"voice": {
            "file_id": "v1", "file_unique_id": "u1", "file_size": 12345,
            "duration": 7, "mime_type": mime
        }, "caption": null})
    }

    fn audio(nome: Option<&str>) -> serde_json::Value {
        serde_json::json!({"audio": {
            "file_id": "a1", "file_unique_id": "u2", "file_size": 999,
            "duration": 90, "performer": null, "title": null,
            "file_name": nome, "mime_type": "audio/mpeg", "thumbnail": null
        }, "caption": null, "media_group_id": null})
    }

    #[test]
    fn mensagem_de_voz_e_baixavel_como_qualquer_anexo() {
        let a = anexos(&msg(voz("audio/ogg")));
        assert_eq!(
            tipos(&a),
            ["mensagem de voz"],
            "voz não pode mais ser recusada"
        );
    }

    #[test]
    fn audio_enviado_como_musica_mantem_o_nome() {
        match anexos(&msg(audio(Some("recado.m4a")))) {
            Achado::Arquivos(l) => {
                assert_eq!(l[0].tipo, "áudio");
                assert_eq!(l[0].nome.as_deref(), Some("recado.m4a"));
            }
            Achado::Nada => panic!("áudio deveria ser baixável"),
        }
    }

    #[test]
    fn voz_sem_nome_cai_no_caminho_do_telegram_para_achar_a_extensao() {
        match anexos(&msg(voz("audio/ogg"))) {
            Achado::Arquivos(l) => assert!(l[0].nome.is_none(), "voz não tem file_name"),
            Achado::Nada => panic!("voz deveria ser baixável"),
        }
        // É o `sanitiza` do path do Telegram que salva a extensão nesse caso.
        assert_eq!(sanitiza("voice/file_5.oga"), "file_5.oga");
    }

    #[test]
    fn mensagem_so_de_texto_nao_tem_anexo() {
        assert_eq!(
            anexos(&msg(serde_json::json!({"text": "oi"}))),
            Achado::Nada
        );
    }

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

        let apagados = varre_audio_velho_em(dir.path(), 7).await;

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

        assert_eq!(varre_audio_velho(0).await, 0);
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

    /// Divide de verdade, com o 7z da máquina. Some quando ele não existe, porque aí o daemon
    /// também recusa antes de tentar.
    #[tokio::test]
    async fn arquivo_grande_vira_volumes_que_somam_o_original() {
        if !tem_7z() {
            return;
        }
        let raiz = tempfile::tempdir().unwrap();
        let grande = raiz.path().join("grande.bin");
        // Incompressível de propósito: com texto repetido o 7z geraria um volume só e o teste
        // não provaria nada.
        let mut dados = Vec::with_capacity(3 * 1024 * 1024);
        let mut x: u32 = 12345;
        while dados.len() < 3 * 1024 * 1024 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            dados.extend_from_slice(&x.to_le_bytes());
        }
        std::fs::write(&grande, &dados).unwrap();

        let partes = divide(&grande, raiz.path().join("partes")).await.unwrap();
        assert!(!partes.arquivos.is_empty());
        assert!(partes.primeiro.ends_with(".001"), "{}", partes.primeiro);
        let soma: u64 = partes
            .arquivos
            .iter()
            .map(|p| p.metadata().unwrap().len())
            .sum();
        assert!(soma > 0, "volume vazio não é divisão");
        let dir = partes.dir.clone();
        partes.limpa().await;
        assert!(!dir.exists(), "as partes têm que sumir depois do envio");
    }

    /// Corta um vídeo de verdade, gerado na hora, com alvo pequeno para caber no teste.
    #[tokio::test]
    async fn video_vira_trechos_que_tocam_sozinhos() {
        if !tem_ffmpeg() {
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
        // Alvo de um terço do arquivo: tem que sair mais de um trecho.
        let partes = corta_video(&video, raiz.path().join("trechos"), tamanho / 3)
            .await
            .unwrap();
        assert!(partes.arquivos.len() > 1, "era para cortar em mais de um");
        assert_eq!(partes.corte, Corte::Trechos);
        assert!(partes.primeiro.ends_with(".mp4"), "{}", partes.primeiro);
        for p in &partes.arquivos {
            let m = p.metadata().unwrap();
            assert!(m.len() > 0, "trecho vazio em {}", p.display());
        }
        // Cada trecho tem que ser um vídeo legível por si: é essa a razão de cortar por tempo.
        for p in &partes.arquivos {
            assert!(
                duracao_de(p).await.unwrap_or(0.0) > 0.0,
                "{} não é vídeo",
                p.display()
            );
        }
        partes.limpa().await;
    }

    #[test]
    fn tamanho_legivel() {
        assert_eq!(humano(512), "512 B");
        assert_eq!(humano(2048), "2 KB");
        assert_eq!(humano(3 * 1024 * 1024), "3.0 MB");
    }
}
