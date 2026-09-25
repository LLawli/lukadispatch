//! O Claude Code como [`Agente`].
//!
//! Tudo que é Claude Code no daemon mora aqui: as flags da linha de comando (`--session-id`,
//! `--resume`, `--settings`, `--permission-mode`, `--model`, `--effort`, `--mcp-config`), o prompt
//! de partida que manda armar o `Monitor`, o `bot-settings.json` com os ganchos, a confiança de
//! pasta no `~/.claude.json`, o catálogo de modelos lido do binário, os modos de permissão e a
//! leitura do transcript `.jsonl`. As leituras em si continuam em `ld_core` (o CLI também as usa);
//! este módulo é o que as reúne atrás da trait.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use ld_core::context::ContextUsage;
use ld_core::hooks::Reconciliacao;
use ld_core::models::Modelo;
use ld_core::state::Session;
use ld_core::transcript::{Fala, SessaoAnterior};
use ld_core::usage::{SessionTokens, Windows};

use super::{Agente, DescricaoDoChat, Invocacao, Modo, PedidoDePartida};

/// Onde o Claude Code e o lukadispatch guardam cada coisa.
///
/// Explícito, e não lido de `ld_core::paths` lá dentro, para os testes rodarem num tempdir sem
/// tocar no `~/.claude.json` nem no `~/.local/state` de verdade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locais {
    /// O binário `lukadispatch` que os ganchos e o `listen` chamam.
    pub cli: String,
    /// O proxy de MCP (`lukadispatch-mcp`).
    pub mcp_proxy: String,
    /// O `bot-settings.json` que [`Agente::prepara`] escreve e `--settings` carrega.
    pub settings: PathBuf,
    /// O `~/.claude.json` (confiança de pasta, servidores MCP).
    pub claude_json: PathBuf,
    /// O `~/.claude` (transcripts em `projects/`).
    pub claude_dir: PathBuf,
    /// O banco do XClaudeUsage (janelas de limite, tokens por sessão).
    pub uso_db: PathBuf,
}

impl Locais {
    /// Os lugares de verdade desta máquina, de `ld_core::paths`.
    pub fn da_maquina() -> Self {
        Self {
            cli: ld_core::paths::cli(),
            mcp_proxy: ld_core::paths::mcp_proxy(),
            settings: ld_core::paths::bot_settings_file(),
            claude_json: ld_core::paths::claude_json(),
            claude_dir: ld_core::paths::claude_dir(),
            uso_db: ld_core::paths::usage_db(),
        }
    }
}

pub struct ClaudeCode {
    locais: Locais,
    /// Caminho do config, quando a descoberta automática não achar o binário certo.
    claude_binary: Option<PathBuf>,
    /// Catálogo de modelos, relido só quando o binário do Claude Code muda (a data dele é a
    /// chave). Sem isto, cada abertura do `/model` varreria de novo um binário de ~200 MB.
    catalogo: Mutex<Option<(SystemTime, Vec<Modelo>)>>,
    /// Nome com que o prompt de partida apresenta você à sessão; `None` fica "o seu usuário".
    usuario: Option<String>,
}

impl ClaudeCode {
    /// `claude_binary` é o caminho do config, quando a descoberta automática não servir para
    /// achar o catálogo de modelos.
    pub fn new(locais: Locais, claude_binary: Option<String>) -> Self {
        Self {
            locais,
            claude_binary: claude_binary.map(PathBuf::from),
            catalogo: Mutex::new(None),
            usuario: None,
        }
    }

    /// O nome do `usuario` do config, para a sessão saber com quem fala.
    pub fn com_usuario(mut self, usuario: Option<String>) -> Self {
        self.usuario = usuario;
        self
    }
}

/// A linha do `--permission-mode` para o modo pedido.
///
/// O modo `perguntar` é nosso, não do Claude Code, e ele vira `dontAsk` aqui. A escolha vem de
/// medição: `dontAsk` é o único modo em que o terminal **nunca** abre prompt (o que travaria a
/// sessão para quem está longe), e a decisão do nosso portão, que vive no `PreToolUse`, é
/// honrada mesmo dentro dele. O que o portão não cobrir é negado em vez de ficar esperando
/// teclado, e o agente relata a negativa em vez de emudecer.
fn modo_efetivo(modo: &str) -> Option<&str> {
    match modo {
        "perguntar" => Some("dontAsk"),
        "" | "padrao" => None,
        outro => Some(outro),
    }
}

/// O que a sessão lê antes de qualquer outra coisa.
///
/// Precisa ser explícito em quatro pontos, e cada um deles já foi motivo de bug em ponte de
/// agente: (1) o Monitor é ferramenta diferida, então sem `ToolSearch` antes o agente não
/// consegue chamá-lo; (2) a resposta vai sozinha pelo hook, senão o agente tenta "mandar" a
/// mensagem por conta própria e inventa um curl; (3) o monitor expira e precisa voltar; (4)
/// arquivo se manda escrevendo um marcador na resposta, não chamando ferramenta.
fn bootstrap_prompt(
    cli: &str,
    session_id: &str,
    chat: &DescricaoDoChat,
    usuario: Option<&str>,
) -> String {
    let marca = ld_core::transcript::MARCA_SISTEMA;
    // O nome aparece uma vez só, aqui; o resto do prompt diz "o seu usuário". Assim o texto não
    // precisa escolher entre "do" e "da" para um nome que ele não conhece.
    let quem = match usuario {
        Some(nome) => format!("o seu usuário ({nome})"),
        None => "o seu usuário".to_string(),
    };
    let teto_mb = chat.teto_envio / (1024 * 1024);
    // Sem renderização de Markdown, tabela e diagrama viram sopa de pipes: o agente precisa
    // saber que tem de gerar como imagem. Com renderização, o bloco todo é ruído.
    let aviso_markdown = if chat.renderiza_markdown {
        String::new()
    } else {
        format!(
            "\n- O {plataforma} NÃO renderiza Markdown: tabela vira um amontoado de pipes, e \
             cabeçalho de markdown aparece com os próprios sinais de cerquilha na tela. Então \
             TABELA, GRÁFICO, DIAGRAMA, comparação lado a lado e qualquer coisa que dependa de \
             alinhamento você GERA COMO IMAGEM e manda com \"@arquivo:\" (como foto, nunca \
             \"@documento:\", senão ele não aparece na conversa e vira um anexo para baixar). \
             Para texto corrido, negrito e itálico funcionam; lista simples com \"-\" também. \
             Prosa continua sendo prosa: não transforme duas frases num PNG.",
            plataforma = chat.plataforma
        )
    };
    format!(
        r#"{marca}
Você está rodando dentro do lukadispatch. O canal de conversa com {quem} é {onde}, e NÃO este terminal: ninguém está lendo esta tela.

Faça agora, nesta ordem, e nada além disso:

1. Chame ToolSearch com query "select:Monitor" para carregar o schema da ferramenta Monitor.
2. Chame Monitor com exatamente estes argumentos:
   command: {cli} listen --session {session_id}
   description: {descricao}{plataforma}
   timeout_ms: 1800000
3. Pare. Não escreva relatório, não explore o projeto, não chame mais nenhuma ferramenta. Fique em silêncio até chegar o primeiro evento do monitor.

Como funciona daqui em diante:

- Cada linha que o monitor emitir é uma mensagem do seu usuário, em JSON: {{"kind":"message","text":"...","from":"...","at":0}}. Trate o campo "text" exatamente como se ele tivesse acabado de digitar aquilo para você, e trabalhe normalmente. O campo "from" diz de ONDE a mensagem saiu (o nome de quem escreveu, ou "pc" quando foi injetada aqui da máquina), e não muda em nada o que você deve fazer.
- Quando ele manda um arquivo (foto, PDF, vídeo), a linha vem com um campo a mais: "files":["/caminho/absoluto"]. O arquivo JÁ ESTÁ em disco nesse caminho, e o mesmo caminho aparece no "text" como "[arquivo recebido: ...]". Abra com Read (ou a ferramenta que couber) antes de responder: ele mandou o arquivo porque quer que você olhe. Não tente baixar nada por conta própria.
- Para DEVOLVER um arquivo (um gráfico que você gerou, um log, um screenshot, um build), não rode comando nenhum: escreva na sua resposta final uma linha SOZINHA, contendo só isto, com caminho absoluto: "@arquivo: /caminho/do/arquivo.png". Pode ter legenda depois de " | ". O hook tira essa linha da mensagem e manda o arquivo NA POSIÇÃO EXATA em que ela apareceu, então você intercala texto e arquivo à vontade: parágrafo, imagem, parágrafo, log, parágrafo. Ponha cada marcador logo depois do trecho que fala dele. Use "@documento:" no lugar de "@arquivo:" quando os bytes EXATOS importarem (um .csv, um build, um PDF); "@arquivo:" manda imagem como foto, que aparece na conversa e é o que você quer em quase todo caso visual. A linha precisa ser a linha inteira: marcador no meio de uma frase, dentro de crase ou depois de hífen de lista é ignorado de propósito, para você poder FALAR do formato sem disparar envio. Tamanho não é problema seu: até {teto_mb} MB vai direto, e acima disso o daemon divide em partes e manda uma por vez, com a instrução de juntar. Nunca mande um arquivo que o seu usuário não pediu.{aviso_markdown}
- Chave, credencial, token e .env são caso à parte: NUNCA saem em claro por este canal. Eles só podem ser enviados criptografados, e só depois que o seu usuário tiver fornecido a chave pública dele nesta conversa: importe a chave e cifre para ela. A ferramenta se escolhe pelo FORMATO da chave que ele mandou, e não por preferência sua: se ela começa com "ssh-ed25519" ou "ssh-rsa", grave a linha inteira num arquivo e use "age -R chave.pub -o arquivo.age arquivo"; se começa com "age1", use "age -r age1... -o arquivo.age arquivo"; se vier um bloco "-----BEGIN PGP PUBLIC KEY BLOCK-----", use "gpg --import chave.asc" e depois "gpg --encrypt --recipient <id> --output arquivo.gpg arquivo". Nunca converta a chave de um formato para outro, e se não reconhecer o formato, pergunte em vez de tentar. Mande só o arquivo cifrado e nunca o original; apague o original em claro assim que cifrar, e o cifrado só NO TURNO SEGUINTE, porque o envio acontece depois da sua resposta (apagar antes faria o arquivo sumir antes de subir). Sem chave pública fornecida por ele, não mande: diga o que você tem e espere a chave.
- VOCÊ NÃO PRECISA ENVIAR NADA DE VOLTA em texto. Um hook pega a sua resposta final e entrega no canal sozinho. Nunca chame curl, nunca use API nenhuma, nunca tente "mandar mensagem": isso duplicaria tudo.
- Perguntas e pedidos de permissão também saem sozinhos: use AskUserQuestion normalmente, que ela aparece no celular e numa janela no PC ao mesmo tempo.
- O monitor expira a cada 30 minutos. Quando isso acontecer, arme-o de novo com a mesma chamada do passo 2, SEM ESCREVER NADA sobre isso: não diga "monitor rearmado", não avise, não comente. O re-arme é encanamento, e qualquer frase sua depois de uma resposta vira a mensagem que chega no celular no lugar da resposta. Se você terminar um turno sem monitor armado, um lembrete vai chegar: cumpra-o na hora, senão a sessão fica surda.
"#,
        plataforma = chat.plataforma,
        descricao = ld_core::transcript::DESCRICAO_DO_CANAL,
        onde = chat.onde,
    )
}

/// O que a sessão lê quando volta por `--resume` (troca de modelo ou de esforço).
///
/// Curto de propósito: o contexto todo já está de volta com ela, e a única coisa que se perdeu
/// no caminho foi o Monitor, que morre junto com o processo anterior.
fn rearm_prompt(cli: &str, session_id: &str, retomada: bool, chat: &DescricaoDoChat) -> String {
    let marca = ld_core::transcript::MARCA_SISTEMA;
    let abertura = if retomada {
        format!(
            "Esta conversa foi retomada pelo lukadispatch e agora está ligada a {onde}. Tudo o \
             que vocês já conversaram continua aqui; o seu usuário acabou de receber as últimas falas \
             no celular.",
            onde = chat.onde
        )
    } else {
        "A sua sessão foi reiniciada pelo lukadispatch (troca de modelo ou de esforço). O \
         contexto continua o mesmo; o que se perdeu foi o canal de conversa com o seu usuário."
            .to_string()
    };
    format!(
        r#"{marca}
{abertura}

Faça só isto, agora:

1. Chame ToolSearch com query "select:Monitor".
2. Chame Monitor com command "{cli} listen --session {session_id}", description "{descricao}{plataforma}" e timeout_ms 1800000.
3. Pare e fique em silêncio até chegar o próximo evento do monitor: nem "pronto", nem "monitor rearmado", nada. Não retome o que estava fazendo por conta própria, não resuma nada e não pergunte se pode continuar: se o seu usuário quiser seguir, ele manda.
"#,
        plataforma = chat.plataforma,
        descricao = ld_core::transcript::DESCRICAO_DO_CANAL,
    )
}

/// Modos de permissão oferecidos no menu, com rótulo legível.
///
/// `perguntar` é modo do lukadispatch, não do Claude Code: por baixo ele é `dontAsk` (o terminal
/// nunca abre prompt) mais o nosso portão no `PreToolUse`, que é quem pergunta no celular. Os
/// modos nativos que parecem servir para isso não servem: `manual` mostra o prompt e ignora a
/// decisão do hook, e `dontAsk` sozinho nega tudo sem perguntar a ninguém. Ver
/// `docs/decisoes/0006-permissoes.md`.
const MODOS: [Modo; 7] = [
    Modo {
        id: "auto",
        rotulo: "🤖 auto (classificador decide)",
        no_menu: true,
    },
    Modo {
        id: "perguntar",
        rotulo: "🙋 perguntar no celular",
        no_menu: true,
    },
    Modo {
        id: "plan",
        rotulo: "📋 plano (só propõe)",
        no_menu: true,
    },
    Modo {
        id: "bypassPermissions",
        rotulo: "⚠️ liberar tudo",
        no_menu: true,
    },
    Modo {
        id: "padrao",
        rotulo: "padrão",
        no_menu: false,
    },
    Modo {
        id: "acceptEdits",
        rotulo: "acceptEdits",
        no_menu: false,
    },
    Modo {
        id: "dontAsk",
        rotulo: "dontAsk",
        no_menu: false,
    },
];

const ESFORCOS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// Apelidos de modelo que o Claude Code aceita em `--model`.
const APELIDOS_MODELO: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

impl Agente for ClaudeCode {
    fn nome(&self) -> &'static str {
        "claude-code"
    }

    /// Grava o `bot-settings.json` que as sessões carregam com `claude --settings`.
    ///
    /// É reescrito a cada partida de propósito: assim, atualizar o lukadispatch atualiza os
    /// ganchos das próximas sessões sem precisar lembrar de nada.
    fn prepara(&self) -> Result<()> {
        if let Some(pai) = self.locais.settings.parent() {
            std::fs::create_dir_all(pai).with_context(|| format!("criando {}", pai.display()))?;
        }
        let json = serde_json::to_string_pretty(&ld_core::hooks::bot_settings(&self.locais.cli))?;
        std::fs::write(&self.locais.settings, json)
            .with_context(|| format!("escrevendo {}", self.locais.settings.display()))?;

        // A telemetria das suas sessões de terminal mora no settings global, que só o
        // `install --global` escreve. Conferir é trabalho da partida, porque toda atualização
        // passa por ela; falhar aqui é aviso, não motivo para o daemon não subir.
        let global = self.locais.claude_dir.join("settings.json");
        match ld_core::hooks::reconcilia_telemetria(&global, &self.locais.cli) {
            Ok(Reconciliacao::Regravada) => tracing::info!(
                settings = %global.display(),
                cli = %self.locais.cli,
                "telemetria do Claude Code regravada com o binário desta versão"
            ),
            Ok(Reconciliacao::OutraInstalacao(outro)) => tracing::info!(
                %outro,
                "a telemetria do Claude Code chama outra instalação do lukadispatch; fica como está"
            ),
            Ok(Reconciliacao::EmDia | Reconciliacao::NaoInstalada) => {}
            Err(e) => tracing::warn!("não consegui conferir a telemetria do Claude Code: {e:#}"),
        }
        Ok(())
    }

    fn confia(&self, pasta: &Path) -> Result<bool> {
        ld_core::trust::ensure_trusted(&self.locais.claude_json, pasta)
    }

    fn invocacao(
        &self,
        pedido: &PedidoDePartida<'_>,
        session_id: &str,
        dir: &Path,
    ) -> Result<Invocacao> {
        let mut argv = vec!["claude".to_string()];

        // `--session-id` cria; `--resume` continua. Os dois juntos o Claude Code recusa.
        match pedido.resume {
            Some(id) => {
                argv.push("--resume".into());
                argv.push(id.to_string());
            }
            None => {
                argv.push("--session-id".into());
                argv.push(session_id.to_string());
            }
        }

        argv.push("--settings".into());
        argv.push(self.locais.settings.to_string_lossy().into_owned());

        if let Some(efetivo) = modo_efetivo(pedido.permission_mode) {
            argv.push("--permission-mode".into());
            argv.push(efetivo.to_string());
        }
        if let Some(m) = pedido.model {
            argv.push("--model".into());
            argv.push(m.to_string());
        }
        if let Some(e) = pedido.effort {
            argv.push("--effort".into());
            argv.push(e.to_string());
        }

        // Configuração de MCP própria, com cada servidor de stdio passando pelo proxy. Precisa
        // vir com `--strict-mcp-config`, senão o original subiria junto com o embrulhado, e o
        // servidor apareceria duas vezes na sessão.
        if pedido.wrap_mcp {
            let servidores =
                ld_core::mcp::servidores_do_projeto(&self.locais.claude_json, &pedido.projeto.path);
            if !servidores.is_empty() {
                let arquivo = dir.join("mcp.json");
                let conteudo = ld_core::mcp::config_embrulhada(
                    &servidores,
                    session_id,
                    &self.locais.mcp_proxy,
                );
                std::fs::write(&arquivo, serde_json::to_string_pretty(&conteudo)?)
                    .with_context(|| format!("escrevendo {}", arquivo.display()))?;
                argv.push("--mcp-config".into());
                argv.push(arquivo.to_string_lossy().into_owned());
                argv.push("--strict-mcp-config".into());
            }
        }

        argv.push("-n".into());
        argv.push(pedido.projeto.name.clone());

        let prompt = match pedido.resume {
            Some(_) => rearm_prompt(&self.locais.cli, session_id, pedido.retomada, pedido.chat),
            None => bootstrap_prompt(
                &self.locais.cli,
                session_id,
                pedido.chat,
                self.usuario.as_deref(),
            ),
        };

        Ok(Invocacao {
            argv,
            prompt: Some(prompt),
        })
    }

    /// Catálogo de modelos, relido só quando o binário do Claude Code muda.
    fn modelos(&self) -> Vec<Modelo> {
        let preferido = self.claude_binary.as_deref();
        // A data do binário é a chave do cache, então atualizar o Claude Code derruba o cache
        // sozinho. Sem binário conhecido ainda, tenta de novo a cada chamada.
        let data_atual = preferido
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());

        {
            let cache = self.catalogo.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((quando, modelos)) = cache.as_ref()
                && !modelos.is_empty()
                && data_atual.is_none_or(|d| d == *quando)
            {
                return modelos.clone();
            }
        }

        let (achado, modelos) = ld_core::models::catalog_auto(preferido);
        // O binário lido vai para o log: quando o `/model` vier vazio, é a primeira coisa que se
        // quer saber, e sem isto só dá para adivinhar qual `claude` foi varrido.
        match &achado {
            Some(bin) => tracing::info!(
                quantos = modelos.len(),
                binario = %bin.display(),
                "catálogo de modelos lido"
            ),
            None => tracing::warn!(
                "não achei um binário do Claude Code com modelos dentro; aponte `claude_binary` \
                 no config.toml"
            ),
        }
        let data = achado
            .and_then(|b| std::fs::metadata(b).ok())
            .and_then(|m| m.modified().ok())
            .unwrap_or(std::time::UNIX_EPOCH);
        *self.catalogo.lock().unwrap_or_else(|e| e.into_inner()) = Some((data, modelos.clone()));
        modelos
    }

    fn e_nome_de_modelo(&self, palavra: &str) -> bool {
        let baixo = palavra.to_lowercase();
        APELIDOS_MODELO.contains(&baixo.as_str()) || baixo.starts_with("claude-")
    }

    fn esforcos(&self) -> &'static [&'static str] {
        &ESFORCOS
    }

    fn modos(&self) -> &'static [Modo] {
        &MODOS
    }

    fn valida_modo(&self, modo: &str) -> Result<()> {
        const VALIDOS: [&str; 8] = [
            "perguntar",
            "padrao",
            "auto",
            "manual",
            "plan",
            "acceptEdits",
            "bypassPermissions",
            "dontAsk",
        ];
        if !VALIDOS.contains(&modo) {
            bail!("modo desconhecido: {modo} (use {})", VALIDOS.join(", "));
        }
        if modo == "manual" {
            bail!(
                "o modo manual ignora a decisão do hook: o card aparece aqui, você responde, e \
                 o prompt continua esperando teclado no PC. Para perguntar pelo celular, use \
                 perguntar"
            );
        }
        Ok(())
    }

    fn historico(&self, sessao: &Session, limite: usize) -> Vec<Fala> {
        let caminho = match sessao.transcript_path.as_deref() {
            Some(p) => PathBuf::from(p),
            None => ld_core::transcript::dir_do_projeto(&self.locais.claude_dir, &sessao.cwd)
                .join(format!("{}.jsonl", sessao.session_id)),
        };
        ld_core::transcript::historico(&caminho, limite)
    }

    /// A fala que de fato responde ao turno.
    ///
    /// O hook entrega a ÚLTIMA mensagem do assistente, e o agente costuma continuar falando
    /// depois de responder: entrega o resultado e anuncia que re-armou o monitor. Quando a
    /// última é só esse anúncio, a resposta boa é a anterior do mesmo turno, que sai do
    /// transcript.
    fn resposta_do_turno(&self, ultima: Option<&str>, transcript: Option<&Path>) -> Option<String> {
        let ultima = ultima.map(str::trim).filter(|t| !t.is_empty());
        match ultima {
            Some(t) if !ld_core::transcript::e_recado_de_monitor(t) => Some(t.to_string()),
            _ => {
                let caminho = transcript?;
                ld_core::transcript::respostas_do_ultimo_turno(caminho)
                    .into_iter()
                    .rev()
                    .find(|f| !ld_core::transcript::e_recado_de_monitor(f))
            }
        }
    }

    fn e_fala_digitada(&self, texto: &str) -> bool {
        ld_core::transcript::e_fala_digitada(texto)
    }

    fn ultima_sessao(&self, cwd: &str) -> Option<SessaoAnterior> {
        ld_core::transcript::ultima_sessao(&self.locais.claude_dir, cwd)
    }

    fn contexto(&self, sessao: &Session) -> Option<ContextUsage> {
        let caminho = sessao.transcript_path.as_deref()?;
        ld_core::context::read_with_model(Path::new(caminho), sessao.model.as_deref())
    }

    fn modelo_da_sessao(&self, sessao: &Session) -> Option<String> {
        let caminho = sessao.transcript_path.as_deref()?;
        ld_core::context::model_from_transcript(Path::new(caminho))
    }

    fn uso(&self) -> Windows {
        ld_core::usage::windows(&self.locais.uso_db)
    }

    fn tokens_da_sessao(&self, session_id: &str) -> Option<SessionTokens> {
        ld_core::usage::session_tokens(&self.locais.uso_db, session_id)
    }
}
