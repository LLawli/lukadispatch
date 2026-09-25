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
//! - a **memória** ([`Memoria`]) embrulha essa chamada: [`AiMemory`] sobe o agente como
//!   `ai-memory run --new <workstream> claude ...`, para a sessão entrar na memória de longo
//!   prazo; [`SemMemoria`] roda o agente como está. Trocar o agente não mexe na memória, e
//!   vice-versa. Ela é opcional: nada do domínio pode depender de haver uma.
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
use ld_core::config::{Config, Project};
use ld_core::context::ContextUsage;
use ld_core::models::Modelo;
use ld_core::state::{Session, Worktree};
use ld_core::transcript::{Fala, SessaoAnterior};
use ld_core::usage::{SessionTokens, Windows};

pub mod ai_memory;
pub mod claude_code;

pub use ai_memory::{AiMemory, workstream};

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
    /// O projeto como a sessão o vê: o `path` é o cwd dela, que numa worktree é a pasta da
    /// worktree, e não a do repositório.
    pub projeto: &'a Project,
    /// A pasta do repositório. É a mesma do `projeto.path` fora de worktree; dentro de uma, é
    /// por ela que o agente acha o que guardou por projeto (os servidores MCP, no Claude Code).
    pub raiz: &'a str,
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
    /// O que a memória de longo prazo quer que o agente saiba ao começar ([`Memoria::instrucoes`]).
    pub instrucoes: Option<&'a str>,
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
    /// atualizar o lukadispatch atualizar os ganchos das próximas sessões. Os ganchos que o
    /// usuário instalou na máquina inteira são conferidos também, mas nunca instalados aqui.
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

/// O que a memória sabe de uma partida de sessão.
#[derive(Debug, Clone, Copy)]
pub struct PartidaDaMemoria<'a> {
    pub session_id: &'a str,
    /// O cwd da sessão.
    pub cwd: &'a Path,
    /// A worktree em que a sessão roda, quando roda numa.
    pub worktree: Option<&'a Worktree>,
    /// A memória da worktree estava ocupada por outra partida e não liberou a tempo: esta usa
    /// uma só dela, em vez de esperar mais.
    pub isolada: bool,
}

/// O que a memória tirou do caminho antes de uma partida, para devolver depois dela. Só a
/// implementação que tirou sabe o que tem dentro.
pub struct Guardado(pub Box<dyn std::any::Any + Send + Sync>);

/// A memória de longo prazo das sessões, que embrulha a chamada do agente antes de rodar.
///
/// Opcional ([`SemMemoria`] não faz nada): o que ela oferece a sessão ganha quando existe, e o
/// resto do daemon funciona igual sem ela. Por isso tudo aqui, menos o embrulho, tem um padrão
/// que não faz nada.
#[async_trait::async_trait]
pub trait Memoria: Send + Sync + 'static {
    /// Nome curto, o mesmo do config (`"ai-memory"`, `"nenhuma"`).
    fn nome(&self) -> &'static str;

    /// A linha de comando final, a partir da do agente.
    async fn embrulha(
        &self,
        partida: &PartidaDaMemoria<'_>,
        argv: Vec<String>,
    ) -> Result<Vec<String>>;

    /// O que o agente precisa saber da memória ao começar, e que vai no prompt de partida.
    fn instrucoes(&self, partida: &PartidaDaMemoria<'_>) -> Option<String> {
        let _ = partida;
        None
    }

    /// Antes de subir: deixa o disco pronto para a memória e tira do caminho o que a sessão não
    /// deve receber. O que foi tirado volta por [`Memoria::devolve`] depois que a sessão subiu.
    async fn antes_da_partida(&self, partida: &PartidaDaMemoria<'_>) -> Result<Option<Guardado>> {
        let _ = partida;
        Ok(None)
    }

    /// Devolve o que [`Memoria::antes_da_partida`] tirou.
    async fn devolve(&self, guardado: Guardado) -> Result<()> {
        let _ = guardado;
        Ok(())
    }

    /// A sessão morreu ao subir porque a memória dela estava presa a uma partida anterior (um
    /// processo que caiu sem soltá-la)? `saida` é o que o painel mostrou. Quem chama espera e
    /// tenta de novo.
    fn ocupada(&self, saida: &str) -> bool {
        let _ = saida;
        false
    }

    /// Para parar a sessão com calma, que processo recebe o sinal: o agente, e não o que o
    /// embrulha, para quem embrulha terminar o trabalho dele. `pid` é o processo do painel.
    fn a_parar(&self, pid: u32) -> Vec<u32> {
        vec![pid]
    }

    /// A worktree foi apagada: esquece o que a memória guardava só dela.
    async fn worktree_apagada(&self, worktree: &Worktree) -> Result<()> {
        let _ = worktree;
        Ok(())
    }
}

/// Sem memória de longo prazo: o agente roda como está.
#[derive(Debug, Default, Clone, Copy)]
pub struct SemMemoria;

#[async_trait::async_trait]
impl Memoria for SemMemoria {
    fn nome(&self) -> &'static str {
        "nenhuma"
    }

    async fn embrulha(
        &self,
        _partida: &PartidaDaMemoria<'_>,
        argv: Vec<String>,
    ) -> Result<Vec<String>> {
        Ok(argv)
    }
}

/// Os agentes que existem, pelo nome que o config (`[agente] tipo`) e o `setup --agent` usam.
pub const AGENTES: &[&str] = &["claude-code"];

/// As memórias que existem, pelo nome de `[agente] memoria` e do `setup --memoria`.
pub const MEMORIAS: &[&str] = &["ai-memory", "nenhuma"];

/// O agente e a memória que o config pede. Nome desconhecido é erro na partida do daemon.
pub fn da_config(config: &Config) -> Result<Pecas> {
    let cfg = &config.agente;
    let agente: Arc<dyn Agente> = match cfg.tipo.as_str() {
        "claude-code" => Arc::new(
            claude_code::ClaudeCode::new(
                claude_code::Locais::da_maquina(),
                config.claude_binary.clone(),
            )
            .com_usuario(config.usuario.clone()),
        ),
        outro => anyhow::bail!(
            "agente desconhecido: {outro:?} (disponíveis: {})",
            AGENTES.join(", ")
        ),
    };
    let memoria: Arc<dyn Memoria> = match cfg.memoria.as_str() {
        "ai-memory" => Arc::new(AiMemory),
        // `nenhum` é o nome de quando isto se chamava envelope, e está em configs instalados.
        "nenhuma" | "nenhum" => Arc::new(SemMemoria),
        outro => anyhow::bail!(
            "memória desconhecida: {outro:?} (disponíveis: {})",
            MEMORIAS.join(", ")
        ),
    };
    Ok(Pecas { agente, memoria })
}

/// O par escolhido pelo config.
pub struct Pecas {
    pub agente: Arc<dyn Agente>,
    pub memoria: Arc<dyn Memoria>,
}

/// Escreve o script de partida em `dir` e devolve o que o hospedeiro precisa para rodá-lo.
///
/// A linha de comando da `invocacao` já vem embrulhada pela memória. O prompt vai para
/// `dir/prompt.txt` e entra no script como `"$(cat <arquivo>)"`; o resto da linha de comando vai
/// com cada argumento entre aspas simples. O script é reescrito a cada
/// partida da sessão.
///
/// **O prompt aparece no `ps`**: o shell expande o `$(cat ...)` antes de o agente nascer, e o
/// texto vira argv. Um `pkill -f` cujo padrão apareça no prompt (e o prompt cita
/// `lukadispatch listen`) mata a sessão inteira.
pub fn escreve_partida(dir: &Path, session_id: &str, invocacao: Invocacao) -> Result<Partida> {
    std::fs::create_dir_all(dir).with_context(|| format!("criando {}", dir.display()))?;

    let prompt_arquivo = dir.join("prompt.txt");
    if let Some(texto) = &invocacao.prompt {
        std::fs::write(&prompt_arquivo, texto)
            .with_context(|| format!("escrevendo {}", prompt_arquivo.display()))?;
    }

    let argv = invocacao.argv;
    // A linha de comando em si (memória, agente e flags) fica junta, de um jeito legível de
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
