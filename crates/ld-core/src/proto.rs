//! Protocolo do socket de controle: NDJSON nos dois sentidos, uma mensagem por linha.
//!
//! Escolha de formato: linha de JSON em vez de algo binário porque os dois lados são pequenos e
//! porque dá para depurar com `socat - UNIX-CONNECT:...` sem ferramenta nenhuma.
//!
//! A maioria das conexões é pergunta-e-resposta e morre em seguida. Duas são longas:
//! `Listen`, que vira um fluxo de mensagens do Telegram (é o que o Monitor da sessão lê), e
//! `Ask`/`Permission`, que ficam abertas enquanto a pergunta espera resposta.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Sonda de vida. O hook usa antes de decidir se sobe o daemon.
    Ping,

    /// Uma sessão do Claude Code começou (hook SessionStart). Vale tanto para sessão do bot
    /// quanto para sessão que o usuário abriu no terminal: as duas entram no painel.
    Register(RegisterSession),

    /// Telemetria de turno (ferramenta rodando, texto, erro). Nunca bloqueia.
    Event(SessionEvent),

    /// Fluxo de entrada da sessão. O daemon responde com um `Response::Message` por mensagem
    /// recebida do Telegram, para sempre, até o cliente sair.
    Listen {
        session_id: String,
    },

    /// O turno acabou (hook Stop). A resposta diz se o Monitor da sessão ainda está armado,
    /// porque é o hook Stop que força o re-arme quando ele expira.
    Stop(StopReport),

    /// Pergunta do Claude (AskUserQuestion) esperando resposta humana.
    Ask {
        session_id: String,
        tool_use_id: Option<String>,
        questions: serde_json::Value,
    },

    /// Pedido de permissão de ferramenta que escapou do modo auto.
    Permission {
        session_id: String,
        tool_name: String,
        tool_input: serde_json::Value,
        tool_use_id: Option<String>,
    },

    /// A janela nativa respondeu primeiro: cancela o card do Telegram.
    LocalAnswer {
        ask_id: String,
        answer: String,
    },

    /// A sessão terminou (hook SessionEnd): apaga o tópico e limpa o estado.
    SessionEnd {
        session_id: String,
        reason: String,
    },

    /// Entrega uma mensagem a uma sessão sem passar pelo Telegram.
    ///
    /// Existe para testar o canal de entrada inteiro (socket, fila, Monitor, hook Stop) numa
    /// máquina sem bot configurado, e para mandar recado a uma sessão do próprio PC.
    Inject {
        session_id: String,
        text: String,
    },

    /// A sessão quer devolver um arquivo pelo tópico dela.
    ///
    /// Sentido contrário do anexo que chega: aqui quem manda é o agente, com um caminho que ele
    /// acabou de produzir (um gráfico, um log, um build). O daemon é quem fala com o Telegram,
    /// então o token continua só do lado dele.
    SendFile {
        session_id: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        caption: Option<String>,
        /// Manda como documento mesmo sendo imagem: o Telegram recomprime foto, e às vezes o
        /// que importa é o arquivo exato.
        #[serde(default)]
        como_arquivo: bool,
    },

    /// Troca modelo ou esforço reiniciando a sessão com `--resume` (o contexto fica).
    Relaunch {
        session_id: String,
        model: Option<String>,
        effort: Option<String>,
    },

    /// Comandos administrativos, usados pelo CLI local (`lukadispatch ls|kill|new`).
    ListSessions,
    NewSession {
        project: String,
        /// Continuar a última conversa daquele projeto em vez de começar do zero.
        #[serde(default)]
        resume_last: bool,
        /// Abrir na worktree desta branch, criando a branch a partir da principal se ela não
        /// existir. Sem ela, a sessão abre na pasta do projeto.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
    },
    Kill {
        session_id: String,
    },
}

/// O que o hook SessionStart conta ao daemon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegisterSession {
    pub session_id: String,
    pub cwd: String,
    pub transcript_path: String,
    /// `startup`, `resume`, `clear`, `compact`, `fork`. O `clear` troca o id da sessão sem
    /// trocar o terminal, e é justamente aí que o mapa tópico -> sessão precisa ser remendado.
    pub reason: String,
    pub model: Option<String>,
}

/// Fim de turno. `last_assistant_message` é o que vai para o Telegram como resposta.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StopReport {
    pub session_id: String,
    pub transcript_path: Option<String>,
    pub last_assistant_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionEvent {
    pub session_id: String,
    pub event: EventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// Uma ferramenta vai rodar. `label` já vem pronto para leitura humana.
    ///
    /// `effort` vem junto porque o evento de ferramenta é o único que carrega o nível em vigor,
    /// e é assim que o painel percebe um `/effort` digitado no teclado do PC.
    ToolStart {
        tool: String,
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    ToolEnd {
        tool: String,
        ok: bool,
    },
    /// Texto do assistente em streaming (hook MessageDisplay). Usado só para o status
    /// "escrevendo": a resposta de verdade vem no Stop, que é autoritativo.
    Streaming,
    Notification {
        text: String,
    },
    Failure {
        text: String,
    },
    /// Você digitou direto no terminal da sessão (hook `UserPromptSubmit`).
    ///
    /// Sem isto, quem está no celular vê a resposta aparecer do nada, sem a pergunta. O daemon
    /// descarta o eco do que ele mesmo acabou de entregar pelo Telegram.
    UserPrompt {
        text: String,
    },
    /// Um servidor MCP pediu confirmação própria (hook `Elicitation`).
    ///
    /// É outro mecanismo, fora do sistema de permissões do Claude Code: quem desenha o diálogo é
    /// o próprio cliente, e o hook **não pode responder** (a documentação diz que a saída dele é
    /// ignorada nesse evento). Então isto aqui serve só para você saber que a sessão parou, e por
    /// quê, em vez de ela emudecer.
    Elicitation {
        servidor: String,
        pedido: String,
    },
    /// O pedido acima foi respondido no PC.
    ElicitationFim,
    /// O modelo da sessão mudou (hook `PostModelSwitch`). Acontece quando você usa `/model` no
    /// teclado do PC; a troca pedida pelo Telegram já passa pelo banco antes.
    ModelSwitch {
        model: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Pong,
    Ok,
    Error {
        message: String,
    },

    /// Uma mensagem vinda do Telegram para a sessão. É a linha que o Monitor transforma em
    /// evento dentro do Claude.
    ///
    /// `files` são caminhos absolutos de anexos já baixados em disco, para a sessão abrir com
    /// Read. Fica de fora quando está vazio: a linha é a mesma de antes para mensagem de texto,
    /// e um `listen` de versão anterior continua repassando o que não conhece.
    Message {
        text: String,
        from: String,
        at: i64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
    },

    /// Ação concluída que tem o que contar. `Ok` seco basta para quem só precisa saber que deu
    /// certo; isto é para quando o resultado em si importa (o que foi enviado, e como).
    Done {
        detail: String,
    },

    /// Encerra um `Listen` de vez: outro monitor assumiu o lugar deste.
    ///
    /// Existe porque o cliente reconecta sozinho quando a conexão cai (um restart do daemon não
    /// pode deixar a sessão surda). Sem uma despedida explícita, o monitor substituído voltaria
    /// e os dois ficariam se desbancando em laço.
    Bye {
        reason: String,
    },

    /// Resposta ao `Stop`: o daemon diz se ainda existe um `Listen` aberto para a sessão.
    /// Sem listener, o Monitor expirou e o hook precisa mandar o agente re-armar.
    Listener {
        alive: bool,
        /// Quantas vezes seguidas já pedimos re-arme. O hook desiste depois do teto para não
        /// prender a sessão num laço de "não consigo encerrar o turno".
        rearm_attempts: u32,
        rearm_command: Option<String>,
    },

    /// Resposta de uma pergunta. `answered=false` significa desistência (timeout, daemon caindo,
    /// sessão sem tópico): o hook então sai sem decidir e o Claude segue o fluxo normal.
    Answer {
        answered: bool,
        text: Option<String>,
        reason: Option<String>,
    },

    /// Decisão de permissão.
    Decision {
        decision: PermissionDecision,
        reason: Option<String>,
    },

    /// Id do card aberto, devolvido antes da espera para que a janela nativa possa cancelá-lo.
    AskOpened {
        ask_id: String,
    },

    /// Variante de campo nomeado, e não newtype: serde não consegue serializar uma variante
    /// newtype com lista dentro quando o enum tem tag interna (`#[serde(tag = "kind")]`), e o
    /// erro só aparece em runtime, na hora de responder.
    Sessions {
        sessions: Vec<SessionSummary>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDecision {
    Allow,
    Deny,
    /// Ninguém respondeu: deixa o Claude Code decidir como sempre decidiria.
    Undecided,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionSummary {
    pub session_id: String,
    pub project: String,
    pub cwd: String,
    /// Id opaco do canal no frontend em uso. `topic_id` é o nome antigo, de quando o único
    /// frontend era o Telegram; o alias mantém compatibilidade com quem ainda lê a linha assim.
    #[serde(alias = "topic_id")]
    pub canal_id: Option<String>,
    pub status: String,
    pub context_tokens: Option<u64>,
    pub context_limit: Option<u64>,
    pub owned_by_bot: bool,
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// Serializa uma mensagem do protocolo como uma linha NDJSON (com o `\n` no fim).
pub fn line<T: Serialize>(msg: &T) -> String {
    let mut s = serde_json::to_string(msg).expect("mensagem do protocolo sempre serializa");
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_de_cada_variante() {
        let casos = vec![
            Request::Ping,
            Request::Listen {
                session_id: "s1".into(),
            },
            Request::Stop(StopReport {
                session_id: "s1".into(),
                transcript_path: Some("/tmp/t.jsonl".into()),
                last_assistant_message: Some("pronto".into()),
            }),
            Request::Event(SessionEvent {
                session_id: "s1".into(),
                event: EventKind::ToolStart {
                    tool: "Bash".into(),
                    label: "cargo test".into(),
                    effort: Some("high".into()),
                },
            }),
        ];
        for caso in casos {
            let txt = line(&caso);
            assert!(txt.ends_with('\n'), "a linha precisa terminar em \\n");
            let volta: Request = serde_json::from_str(txt.trim()).unwrap();
            assert_eq!(caso, volta);
        }
    }

    /// Toda variante de `Response` precisa passar por aqui.
    ///
    /// Existe porque a `Sessions` já quebrou em produção: com tag interna, serde recusa variante
    /// newtype contendo lista, e o erro só aparece quando alguém pede a lista de sessões. Um
    /// round-trip por variante é o que impede isso de voltar.
    #[test]
    fn round_trip_de_cada_resposta() {
        let casos = vec![
            Response::Pong,
            Response::Ok,
            Response::Error {
                message: "x".into(),
            },
            Response::Done {
                detail: "grafico.png (12 KB) enviado como foto".into(),
            },
            Response::Message {
                text: "oi".into(),
                from: "luka".into(),
                at: 1,
                files: vec![],
            },
            Response::Message {
                text: "[arquivo recebido: nota.pdf]".into(),
                from: "luka".into(),
                at: 1,
                files: vec!["/home/luka/.local/share/lukadispatch/arquivos/s1/nota.pdf".into()],
            },
            Response::Listener {
                alive: false,
                rearm_attempts: 2,
                rearm_command: Some("lukadispatch listen".into()),
            },
            Response::Answer {
                answered: true,
                text: Some("sim".into()),
                reason: None,
            },
            Response::Decision {
                decision: PermissionDecision::Allow,
                reason: None,
            },
            Response::AskOpened {
                ask_id: "abc".into(),
            },
            Response::Bye {
                reason: "outro monitor assumiu".into(),
            },
            Response::Sessions {
                sessions: vec![SessionSummary {
                    session_id: "s1".into(),
                    project: "p".into(),
                    cwd: "/tmp".into(),
                    canal_id: Some("7".into()),
                    status: "ocioso".into(),
                    context_tokens: Some(10),
                    context_limit: Some(200_000),
                    owned_by_bot: true,
                    model: Some("opus".into()),
                    effort: None,
                }],
            },
        ];
        for caso in casos {
            let txt = serde_json::to_string(&caso)
                .unwrap_or_else(|e| panic!("{caso:?} não serializa: {e}"));
            let volta: Response = serde_json::from_str(&txt).unwrap();
            assert_eq!(caso, volta);
        }
    }

    #[test]
    fn resposta_de_mensagem_cabe_em_uma_linha() {
        // O Monitor quebra eventos por linha, então um texto com \n não pode virar duas linhas.
        let r = Response::Message {
            text: "linha1\nlinha2".into(),
            from: "luka".into(),
            at: 0,
            files: vec![],
        };
        let txt = line(&r);
        assert_eq!(txt.matches('\n').count(), 1);
    }
}
