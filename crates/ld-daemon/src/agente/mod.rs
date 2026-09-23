//! A porta do agente de código: o programa que roda dentro de cada sessão.
//!
//! O domínio (criar, relançar e encerrar sessão, fim de turno, painel, `/model`) não sabe que o
//! agente é o Claude Code. Tudo que depende disso passa pela trait [`Agente`]: a linha de
//! comando que sobe uma sessão, o prompt de partida, os hooks instalados na máquina, a confiança
//! de pasta, o catálogo de modelos, os modos de permissão e a leitura da conversa gravada.
//!
//! Há duas camadas na partida, e elas são independentes:
//!
//! - o **agente** diz como ele mesmo é chamado ([`Agente::invocacao`]): `claude --session-id ...
//!   --settings ... "<prompt>"`;
//! - o **envelope** ([`Envelope`]) embrulha essa chamada: [`AiMemory`] sobe o agente como
//!   `ai-memory run --new <workstream> claude ...`, para a sessão entrar na memória de longo
//!   prazo; [`Direto`] roda o agente como está. Trocar o agente não mexe no envelope, e vice-versa.
//!
//! [`escreve_partida`] junta os dois num script, que o `Hospedeiro` (o tmux) roda. O script existe
//! para dar para ler depois, exatamente, o que foi lançado quando algo der errado.
//!
//! A implementação que existe é [`claude_code::ClaudeCode`]. Um agente novo (Codex, por exemplo)
//! precisa oferecer o que o lukadispatch usa do Claude Code: um jeito de receber mensagem no meio
//! do turno (hoje a ferramenta `Monitor` lendo `lukadispatch listen`), ganchos de fim de turno e
//! de ferramenta que chamem `lukadispatch hook`, e uma conversa gravada que dê para ler. O
//! que ele não tiver, o domínio perde (sem transcript, não há histórico ao retomar). Ver
//! `docs/portas.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use ld_core::config::{Agente as CfgAgente, Project};
use ld_core::context::ContextUsage;
use ld_core::models::Modelo;
use ld_core::state::Session;
use ld_core::transcript::{Fala, SessaoAnterior};
use ld_core::usage::{SessionTokens, Windows};

pub mod claude_code;

#[cfg(test)]
mod testes;

/// Como o chat se apresenta para o agente, no prompt de partida.
///
/// O prompt precisa dizer por onde o usuário fala e o que funciona ali (arquivo até que
/// tamanho, se Markdown aparece formatado). Isso é do frontend, e não do agente: quem monta esta
/// descrição é o domínio, a partir do `Frontend` em uso.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescricaoDoChat {
    /// Nome do aplicativo, como gente fala: `"Telegram"`, `"WhatsApp"`.
    pub plataforma: String,
    /// Onde, dentro dele, a conversa desta sessão acontece. Ex.: `o tópico "proj" de um grupo
    /// do Telegram`.
    pub onde: String,
    /// Bytes que um arquivo pode ter para ir inteiro (acima disso o daemon parte).
    pub teto_envio: u64,
    /// O chat mostra Markdown formatado? Se não, o agente precisa saber que tabela vira sopa de
    /// pipes e deve ir como imagem.
    pub renderiza_markdown: bool,
}

/// O que o domínio pede ao abrir (ou reabrir) uma sessão.
#[derive(Debug, Clone)]
pub struct PedidoDePartida<'a> {
    pub projeto: &'a Project,
    /// Modo de permissão do lukadispatch (`"perguntar"`, `"auto"`...). O agente traduz para o
    /// que ele entende.
    pub permission_mode: &'a str,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    /// Continuar esta sessão (troca de modelo ou "continuar de onde parou") em vez de criar.
    pub resume: Option<&'a str>,
    /// Com `resume`: `true` é "continuar a conversa anterior", `false` é "fui reiniciada por
    /// troca de modelo". Muda só a abertura do prompt.
    pub retomada: bool,
    /// Passar os servidores MCP pelo proxy do lukadispatch.
    pub wrap_mcp: bool,
    pub chat: &'a DescricaoDoChat,
}

/// Como chamar o agente: programa, argumentos e o prompt inicial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocacao {
    /// Programa e argumentos, um item por argumento, sem shell no meio.
    pub argv: Vec<String>,
    /// Prompt inicial, que entra como último argumento. Vai num arquivo ao lado do script,
    /// porque é longo e cheio de aspas.
    pub prompt: Option<String>,
}

/// O script pronto para o hospedeiro rodar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partida {
    pub session_id: String,
    /// O script de partida (`bash <script>`).
    pub script: PathBuf,
    /// Onde o hospedeiro espelha a saída da sessão: sem isso, uma sessão que morre ao subir não
    /// deixa pista nenhuma.
    pub log: PathBuf,
}

/// Um modo de permissão que o agente aceita.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Modo {
    pub id: &'static str,
    /// Como aparece no botão.
    pub rotulo: &'static str,
    /// Oferecido no teclado do `/mode`. Os outros são aceitos por nome, mas não oferecidos.
    pub no_menu: bool,
}

/// O agente de código que roda em cada sessão.
///
/// As leituras (`historico`, `contexto`, `uso`...) são síncronas e baratas o bastante para o
/// painel, com uma exceção documentada em [`Agente::modelos`]. Nenhuma delas pode derrubar o
/// daemon: dado ausente ou ilegível é `None` ou lista vazia.
pub trait Agente: Send + Sync + 'static {
    /// Nome curto, o mesmo do config (`"claude-code"`).
    fn nome(&self) -> &'static str;

    /// Deixa a máquina pronta na partida do daemon: os ganchos que o agente carrega nas
    /// sessões do bot, apontando para o `lukadispatch`. Reescrito a cada partida, para
    /// atualizar o lukadispatch atualizar os ganchos das próximas sessões.
    fn prepara(&self) -> Result<()>;

    /// Antes de abrir sessão numa pasta: tira do caminho o que travaria a partida esperando uma
    /// tecla (o diálogo de confiança do Claude Code). Devolve `true` quando precisou mudar algo.
    fn confia(&self, pasta: &Path) -> Result<bool>;

    /// Um id para sessão nova. O Claude Code aceita o id que se escolhe, então é um uuid.
    fn novo_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// Como chamar o agente para esta sessão. `dir` é o diretório da sessão, que já existe, para
    /// arquivos auxiliares (a configuração de MCP, por exemplo).
    fn invocacao(
        &self,
        pedido: &PedidoDePartida<'_>,
        session_id: &str,
        dir: &Path,
    ) -> Result<Invocacao>;

    /// O catálogo de modelos que dá para escolher no `/model`. Pode ser caro (o do Claude Code
    /// varre o binário): a implementação guarda em cache.
    fn modelos(&self) -> Vec<Modelo>;

    /// A palavra é um modelo? Serve para separar `/new tintim opus` em projeto e modelo.
    fn e_nome_de_modelo(&self, palavra: &str) -> bool;

    /// Os níveis de esforço aceitos, na ordem em que aparecem no teclado.
    fn esforcos(&self) -> &'static [&'static str];

    /// Todos os modos de permissão aceitos, na ordem do teclado.
    fn modos(&self) -> &'static [Modo];

    /// `Ok` se o modo pode ser pedido pelo chat; erro explicando por que não, senão.
    fn valida_modo(&self, modo: &str) -> Result<()>;

    /// As últimas `limite` falas da conversa desta sessão, para o canal não retomar às cegas.
    fn historico(&self, sessao: &Session, limite: usize) -> Vec<Fala>;

    /// A fala que responde ao turno. `ultima` é o que o gancho de fim de turno entregou; quando
    /// ela é só encanamento (o anúncio de que o canal foi re-armado), a resposta vem da conversa
    /// gravada.
    fn resposta_do_turno(&self, ultima: Option<&str>, transcript: Option<&Path>) -> Option<String>;

    /// O texto é algo que a pessoa digitou, e não encanamento (prompt injetado, lembrete)?
    fn e_fala_digitada(&self, texto: &str) -> bool;

    /// A conversa anterior neste projeto, para oferecer "continuar de onde parou".
    fn ultima_sessao(&self, cwd: &str) -> Option<SessaoAnterior>;

    /// Quanto da janela de contexto a sessão ocupa.
    fn contexto(&self, sessao: &Session) -> Option<ContextUsage>;

    /// O modelo da sessão, lido da conversa gravada, para sessão que não o informou ao subir.
    fn modelo_da_sessao(&self, sessao: &Session) -> Option<String>;

    /// As janelas de limite da conta (5 h, 7 dias).
    fn uso(&self) -> Windows;

    /// Tokens gastos pela sessão.
    fn tokens_da_sessao(&self, session_id: &str) -> Option<SessionTokens>;
}

/// O que embrulha a chamada do agente antes de rodar.
pub trait Envelope: Send + Sync + 'static {
    /// Nome curto, o mesmo do config (`"ai-memory"`, `"nenhum"`).
    fn nome(&self) -> &'static str;

    /// A linha de comando final, a partir da do agente.
    fn embrulha(&self, session_id: &str, argv: Vec<String>) -> Vec<String>;
}

/// `ai-memory run --new <workstream> <agente...>`: a sessão entra na memória de longo prazo.
#[derive(Debug, Default, Clone, Copy)]
pub struct AiMemory;

/// O agente roda como está.
#[derive(Debug, Default, Clone, Copy)]
pub struct Direto;

impl Envelope for AiMemory {
    fn nome(&self) -> &'static str {
        "ai-memory"
    }

    fn embrulha(&self, session_id: &str, argv: Vec<String>) -> Vec<String> {
        let mut linha = vec![
            "ai-memory".to_string(),
            "run".to_string(),
            "--new".to_string(),
            workstream(session_id),
        ];
        linha.extend(argv);
        linha
    }
}

impl Envelope for Direto {
    fn nome(&self) -> &'static str {
        "nenhum"
    }

    fn embrulha(&self, session_id: &str, argv: Vec<String>) -> Vec<String> {
        let _ = session_id;
        argv
    }
}

/// Nome do workstream do ai-memory para esta partida da sessão.
///
/// Único por partida: o `ai-memory run` recusa com 409 um workstream já ativo e recusa de novo
/// um nome de `--new` que já existe, e a mesma sessão sobe mais de uma vez (troca de modelo por
/// `--resume`). O prefixo `lukadispatch-<8 do id>` é o que você procura no `ai-memory`; o
/// carimbo de tempo no fim é o que o torna inédito.
pub fn workstream(session_id: &str) -> String {
    let agora = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("lukadispatch-{}-{agora}", &session_id[..8])
}

/// O agente e o envelope que o config pede. Nome desconhecido é erro na partida do daemon.
pub fn da_config(cfg: &CfgAgente, claude_binary: Option<String>) -> Result<Pecas> {
    let agente: Arc<dyn Agente> = match cfg.tipo.as_str() {
        "claude-code" => Arc::new(claude_code::ClaudeCode::new(
            claude_code::Locais::da_maquina(),
            claude_binary,
        )),
        outro => anyhow::bail!("agente desconhecido: {outro:?} (disponíveis: claude-code)"),
    };
    let envelope: Arc<dyn Envelope> = match cfg.envelope.as_str() {
        "ai-memory" => Arc::new(AiMemory),
        "nenhum" => Arc::new(Direto),
        outro => anyhow::bail!("envelope desconhecido: {outro:?} (disponíveis: ai-memory, nenhum)"),
    };
    Ok(Pecas { agente, envelope })
}

/// O par escolhido pelo config.
pub struct Pecas {
    pub agente: Arc<dyn Agente>,
    pub envelope: Arc<dyn Envelope>,
}

/// Escreve o script de partida em `dir` e devolve o que o hospedeiro precisa para rodá-lo.
///
/// O prompt vai para `dir/prompt.txt` e entra no script como `"$(cat <arquivo>)"`; o resto da
/// linha de comando vai com cada argumento entre aspas simples. O script é reescrito a cada
/// partida da sessão.
///
/// **O prompt aparece no `ps`**: o shell expande o `$(cat ...)` antes de o agente nascer, e o
/// texto vira argv. Um `pkill -f` cujo padrão apareça no prompt (e o prompt cita
/// `lukadispatch listen`) mata a sessão inteira.
pub fn escreve_partida(
    dir: &Path,
    session_id: &str,
    envelope: &dyn Envelope,
    invocacao: Invocacao,
) -> Result<Partida> {
    std::fs::create_dir_all(dir).with_context(|| format!("criando {}", dir.display()))?;

    let prompt_arquivo = dir.join("prompt.txt");
    if let Some(texto) = &invocacao.prompt {
        std::fs::write(&prompt_arquivo, texto)
            .with_context(|| format!("escrevendo {}", prompt_arquivo.display()))?;
    }

    let argv = envelope.embrulha(session_id, invocacao.argv);
    // A linha de comando em si (envelope, agente e flags) fica junta, de um jeito legível de
    // ler inteira; só o prompt, que é sempre o argumento mais longo, ganha linha própria.
    let mut comando: String = argv
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    if invocacao.prompt.is_some() {
        comando.push_str(&format!(" \\\n  \"$(cat {})\"", prompt_arquivo.display()));
    }

    let script = dir.join("launch.sh");
    std::fs::write(
        &script,
        format!(
            r#"#!/usr/bin/env bash
# Gerado pelo lukadispatch para a sessão {session_id}. Editar aqui não muda nada:
# o arquivo é reescrito a cada partida da sessão.
set -u
exec {comando}
"#
        ),
    )
    .with_context(|| format!("escrevendo {}", script.display()))?;

    Ok(Partida {
        session_id: session_id.to_string(),
        script,
        log: dir.join("pane.log"),
    })
}

/// Aspas simples para um argumento de shell, com o truque padrão para a própria aspa.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
