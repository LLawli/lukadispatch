//! Leitura dos transcripts do Claude Code: qual foi a última sessão de um projeto, e o que foi
//! dito nela.
//!
//! Serve a duas coisas: oferecer "continuar de onde parou" ao abrir uma sessão, e mostrar no
//! Telegram o que já tinha sido conversado, para você não retomar às cegas.
//!
//! O formato é um `.jsonl` por sessão, dentro de `~/.claude/projects/<caminho-codificado>/`. A
//! codificação do diretório troca **todo** caractere não alfanumérico por `-`, então
//! `/home/luka/Personal/cnpj_validator` vira `-home-luka-Personal-cnpj-validator`.

use std::path::{Path, PathBuf};

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Papel {
    Usuario,
    Assistente,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fala {
    pub papel: Papel,
    pub texto: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessaoAnterior {
    pub session_id: String,
    pub transcript: PathBuf,
    /// Epoch em segundos da última escrita.
    pub quando: i64,
    /// Primeira linha da última fala do usuário, para você reconhecer a conversa.
    pub resumo: String,
}

/// Nome do diretório de transcripts de um projeto.
pub fn dir_do_projeto(claude_dir: &Path, cwd: &str) -> PathBuf {
    let codificado: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    claude_dir.join("projects").join(codificado)
}

/// A sessão mais recente daquele diretório, ou `None` quando ainda não houve nenhuma.
///
/// Arquivos de subagente (`isSidechain`) ficam de fora: retomar um deles abriria uma conversa
/// que nunca foi sua.
pub fn ultima_sessao(claude_dir: &Path, cwd: &str) -> Option<SessaoAnterior> {
    let dir = dir_do_projeto(claude_dir, cwd);
    // Ordena pelo mtime com a precisão inteira: em segundos, duas conversas do mesmo segundo
    // empatam, e o empate fica na ordem do read_dir, que muda de um sistema de arquivos para outro.
    let mut candidatos: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .filter_map(|p| Some((std::fs::metadata(&p).and_then(|m| m.modified()).ok()?, p)))
        .collect();
    candidatos.sort_by_key(|(quando, _)| std::cmp::Reverse(*quando));

    for (modificado, caminho) in candidatos {
        let quando = modificado
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs() as i64;
        if e_sidechain(&caminho) {
            continue;
        }
        let falas = historico(&caminho, 1);
        let session_id = caminho.file_stem()?.to_string_lossy().into_owned();
        return Some(SessaoAnterior {
            session_id,
            transcript: caminho,
            quando,
            resumo: falas
                .last()
                .map(|f| primeira_linha(&f.texto, 60))
                .unwrap_or_else(|| "(sem texto)".into()),
        });
    }
    None
}

fn e_sidechain(caminho: &Path) -> bool {
    let Ok(conteudo) = std::fs::read_to_string(caminho) else {
        return false;
    };
    conteudo
        .lines()
        .take(50)
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .any(|v| v.get("isSidechain").and_then(Value::as_bool) == Some(true))
}

/// As últimas `limite` falas de verdade da conversa, da mais antiga para a mais nova.
///
/// "De verdade" exclui o que não é conversa: resultado de ferramenta (que também chega como
/// `user`), bloco de raciocínio, chamada de ferramenta e mensagem de sistema. O que sobra é o
/// que você reconheceria como diálogo.
pub fn historico(caminho: &Path, limite: usize) -> Vec<Fala> {
    let Ok(conteudo) = std::fs::read_to_string(caminho) else {
        return Vec::new();
    };
    let mut falas: Vec<Fala> = Vec::new();
    // A resposta a um prompt do sistema também não é diálogo: é o agente respondendo a algo que
    // você nunca escreveu ("Monitor rearmado. Aguardando."). Some junto com o prompt que a
    // provocou, senão o replay mostra resposta sem pergunta.
    let mut pular_proxima_resposta = false;
    for linha in conteudo.lines() {
        let Ok(v) = serde_json::from_str::<Value>(linha) else {
            continue;
        };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let papel = match v.get("type").and_then(Value::as_str) {
            Some("user") => Papel::Usuario,
            Some("assistant") => Papel::Assistente,
            _ => continue,
        };
        let Some(msg) = v.get("message") else {
            continue;
        };
        let bruto = texto_da_mensagem(msg);
        let Some(texto) = limpa(&bruto) else {
            // Só prompt do sistema silencia a resposta seguinte; entrada vazia ou ruído interno
            // não, porque eles não provocam resposta nenhuma.
            pular_proxima_resposta = papel == Papel::Usuario && e_do_sistema(&bruto);
            continue;
        };
        if papel == Papel::Assistente && pular_proxima_resposta {
            pular_proxima_resposta = false;
            continue;
        }
        pular_proxima_resposta = false;
        falas.push(Fala { papel, texto });
    }
    if falas.len() > limite {
        falas.drain(..falas.len() - limite);
    }
    falas
}

/// As falas do assistente no último turno, da mais antiga para a mais nova.
///
/// "Último turno" é o que veio depois da última entrada do usuário (mensagem, evento do Monitor
/// ou prompt injetado). Serve para o caso em que o agente responde e **continua falando**: ele
/// entrega o resultado e depois anuncia que re-armou o monitor, e é esse anúncio que o hook
/// `Stop` entrega como "última mensagem".
pub fn respostas_do_ultimo_turno(caminho: &Path) -> Vec<String> {
    let Ok(conteudo) = std::fs::read_to_string(caminho) else {
        return Vec::new();
    };
    let mut falas: Vec<String> = Vec::new();
    for linha in conteudo.lines() {
        let Ok(v) = serde_json::from_str::<Value>(linha) else {
            continue;
        };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        match v.get("type").and_then(Value::as_str) {
            // Entrada de usuário fecha o turno anterior, inclusive resultado de ferramenta: é o
            // único marcador de fronteira que o transcript oferece.
            Some("user") => falas.clear(),
            Some("assistant") => {
                if let Some(msg) = v.get("message") {
                    let texto = texto_da_mensagem(msg);
                    if !texto.trim().is_empty() {
                        falas.push(texto.trim().to_string());
                    }
                }
            }
            _ => {}
        }
    }
    falas
}

/// `true` quando a fala é o agente comentando o próprio encanamento ("Monitor rearmado").
///
/// Curta e falando de monitor: é o formato do anúncio. O teto de tamanho evita engolir uma
/// resposta de verdade que por acaso fale de monitor.
pub fn e_recado_de_monitor(texto: &str) -> bool {
    let t = texto.trim();
    t.chars().count() < 120 && t.to_lowercase().contains("monitor")
}

/// Primeira linha de todo prompt que o daemon injeta na sessão.
///
/// Mora aqui, e não no daemon, porque quem filtra por ela é este módulo: se as duas pontas
/// tivessem cópias separadas do texto, uma mudança de redação faria os prompts voltarem a
/// aparecer no replay como se fossem fala sua, e em silêncio.
pub const MARCA_SISTEMA: &str =
    "«lukadispatch: mensagem automática do sistema, não é o usuário falando»";

/// O começo da descrição do `Monitor` que escuta o canal (`mensagens do Telegram`). O prompt que
/// arma o monitor e o classificador de [`origem_do_turno`] usam esta mesma constante: se o texto
/// divergisse, o canal expirando passaria por trabalho da sessão, e vice-versa.
pub const DESCRICAO_DO_CANAL: &str = "mensagens do ";

/// O que abriu o turno que acabou de terminar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origem {
    /// Uma mensagem que chegou pelo canal (o daemon já marcou o pedido ao entregá-la).
    MensagemDoCanal,
    /// O monitor do canal expirou: o turno é o re-arme, e o que ele diz é encanamento.
    CanalExpirou,
    /// Trabalho da própria sessão terminou (um comando em segundo plano, um monitor dela): o
    /// turno continua o que foi pedido antes, e a resposta é para quem pediu.
    TarefaDeFundo,
    /// Um prompt do daemon (partida, re-arme depois de troca de modelo).
    Sistema,
    /// Outra coisa: texto digitado no terminal, aviso de hook.
    Outra,
}

/// Classifica o turno que acabou de terminar pela entrada que o abriu.
///
/// O `Stop` do Claude Code não diz o que originou o turno, e é daí que depende se a resposta vai
/// para o canal: o re-arme do monitor não deve ir, o fim de um CI que a sessão deixou rodando
/// deve. A entrada que abre o turno é a última fala `user` em texto: resultado de ferramenta é
/// meio de turno, e o empurrão de "saída vazia" do Claude Code também.
pub fn origem_do_turno(caminho: &Path) -> Option<Origem> {
    let conteudo = std::fs::read_to_string(caminho).ok()?;
    // De trás para frente: a entrada que interessa é a mais recente, e o transcript cresce.
    let gatilho = conteudo
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("type").and_then(Value::as_str) == Some("user"))
        .filter_map(|v| texto_de_entrada(v.get("message")?))
        .find(|t| !t.starts_with("[Your previous response had no visible output"))?;
    Some(classifica(&gatilho))
}

/// O texto de uma entrada do usuário, ou `None` quando ela é resultado de ferramenta.
fn texto_de_entrada(msg: &Value) -> Option<String> {
    match msg.get("content")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocos) => {
            if blocos
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            {
                return None;
            }
            let t: Vec<&str> = blocos
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect();
            (!t.is_empty()).then(|| t.join("\n"))
        }
        _ => None,
    }
}

fn classifica(texto: &str) -> Origem {
    let t = texto.trim_start();
    if t.starts_with(PREFIXO_MARCA) {
        return Origem::Sistema;
    }
    if !t.starts_with("<task-notification>") {
        return Origem::Outra;
    }
    let resumo = t
        .split("<summary>")
        .nth(1)
        .and_then(|r| r.split("</summary>").next())
        .unwrap_or_default();
    if resumo.starts_with(&format!("Monitor event: \"{DESCRICAO_DO_CANAL}")) {
        Origem::MensagemDoCanal
    } else if resumo.starts_with(&format!("Monitor \"{DESCRICAO_DO_CANAL}")) {
        Origem::CanalExpirou
    } else {
        Origem::TarefaDeFundo
    }
}

/// O prefixo basta para reconhecer a marca, mesmo que o resto da frase mude.
const PREFIXO_MARCA: &str = "«lukadispatch:";

/// Aberturas dos prompts injetados antes de a marca existir.
///
/// Transcript é histórico: uma conversa gravada semana passada não ganha marca retroativa. Sem
/// esta lista, retomar uma sessão antiga mostraria os prompts do daemon como se fossem fala sua.
const ABERTURAS_ANTIGAS: [&str; 3] = [
    "Você está rodando dentro do lukadispatch",
    "A sua sessão foi reiniciada pelo lukadispatch",
    "Esta conversa foi retomada pelo lukadispatch",
];

/// `true` quando o texto é algo que uma pessoa digitou, e não encanamento.
///
/// Usada em dois lugares que precisam concordar: o replay de uma conversa retomada e o espelho
/// ao vivo do que você digita no `tmux attach`. O `UserPromptSubmit` dispara para tudo que entra
/// como prompt, e isso inclui os prompts que o próprio daemon injeta e as notificações de evento
/// do Monitor. Sem este filtro, o tópico recebe o bootstrap inteiro e cada expiração de monitor
/// como se fossem mensagens suas.
pub fn e_fala_digitada(bruto: &str) -> bool {
    let texto = bruto.trim();
    !texto.is_empty()
        && !e_do_sistema(texto)
        && !texto.starts_with("<task-notification>")
        && !texto.starts_with("<system-reminder>")
        && !texto.starts_with("<command-name>")
}

/// Texto que o próprio lukadispatch injetou como prompt.
fn e_do_sistema(bruto: &str) -> bool {
    let texto = bruto.trim();
    texto.starts_with(PREFIXO_MARCA) || ABERTURAS_ANTIGAS.iter().any(|a| texto.starts_with(a))
}

/// Transforma o texto bruto de uma entrada em fala de gente, ou descarta.
///
/// Três coisas entram no transcript como se fossem fala do usuário e não são:
///
/// 1. Os prompts que o próprio lukadispatch injeta (bootstrap, re-arme). Saem pela marca.
/// 2. As notificações de evento do Monitor. Essas, na verdade, CARREGAM a mensagem que você
///    mandou pelo Telegram, dentro de um `<event>` em JSON: o texto é extraído de lá, senão o
///    replay de uma sessão do bot não teria nenhuma fala sua.
/// 3. Avisos internos do harness (`<system-reminder>`, outras `<task-notification>`), que são
///    ruído para quem lê no celular.
fn limpa(bruto: &str) -> Option<String> {
    let texto = bruto.trim();
    if texto.is_empty() || texto.starts_with(PREFIXO_MARCA) {
        return None;
    }
    if ABERTURAS_ANTIGAS.iter().any(|a| texto.starts_with(a)) {
        return None;
    }
    if texto.starts_with("<task-notification>") {
        return mensagem_do_evento(texto);
    }
    if texto.starts_with("<system-reminder>") || texto.starts_with("<command-name>") {
        return None;
    }
    Some(texto.to_string())
}

/// Tira a mensagem de dentro de uma notificação de evento do Monitor.
fn mensagem_do_evento(texto: &str) -> Option<String> {
    let inicio = texto.find("<event>")? + "<event>".len();
    let fim = texto[inicio..].find("</event>")? + inicio;
    let v: Value = serde_json::from_str(texto[inicio..fim].trim()).ok()?;
    if v.get("kind").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let t = v.get("text").and_then(Value::as_str)?.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn texto_da_mensagem(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocos)) => blocos
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn primeira_linha(texto: &str, teto: usize) -> String {
    let linha = texto.lines().next().unwrap_or("").trim();
    if linha.chars().count() <= teto {
        return linha.to_string();
    }
    format!("{}…", linha.chars().take(teto).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    // As notificações abaixo são cópias literais de um transcript real (24/09/2026).
    const MSG_DO_CANAL: &str = "<task-notification>\n<task-id>b11znyhr7</task-id>\n<summary>Monitor event: \"mensagens do Telegram\"</summary>\n<event>{\"kind\":\"message\",\"text\":\"Pode começar\",\"from\":\"Luka\",\"at\":1790251993}</event>\nIf this event is something the user would act on now, send a PushNotification. Routine or benign output doesn't need one.\n</task-notification>";
    const CI_DE_FUNDO: &str = "<task-notification>\n<task-id>bm64xovu8</task-id>\n<tool-use-id>toolu_01EGhFg7i7n5h99xisWnQYfb</tool-use-id>\n<output-file>/tmp/x/tasks/bm64xovu8.output</output-file>\n<status>completed</status>\n<summary>Background command \"Run full CI pipeline in background\" completed (exit code 0)</summary>\n</task-notification>";
    const MONITOR_DA_SESSAO: &str = "<task-notification>\n<task-id>bq2hp7nlu</task-id>\n<tool-use-id>toolu_01Bj9qg47vjgLLYWLaYbuwMR</tool-use-id>\n<output-file>/tmp/x/tasks/bq2hp7nlu.output</output-file>\n<status>completed</status>\n<summary>Monitor \"aguarda validação da fase 4\" stream ended</summary>\n</task-notification>";
    const CANAL_EXPIROU: &str = "<task-notification>\n<task-id>b0000000</task-id>\n<tool-use-id>toolu_x</tool-use-id>\n<output-file>/tmp/x/tasks/b0000000.output</output-file>\n<status>completed</status>\n<summary>Monitor \"mensagens do Telegram\" stream ended</summary>\n</task-notification>";

    fn linha_de(tipo: &str, conteudo: serde_json::Value) -> String {
        serde_json::json!({"type": tipo, "message": {"role": tipo, "content": conteudo}})
            .to_string()
    }

    /// Um transcript em que o último turno começa com `gatilho` e tem uma ferramenta no meio.
    fn turno_aberto_por(gatilho: &str) -> tempfile::NamedTempFile {
        let f = tempfile::NamedTempFile::new().unwrap();
        let linhas = [
            linha_de("user", serde_json::json!("pedido antigo")),
            linha_de(
                "assistant",
                serde_json::json!([{"type": "text", "text": "feito"}]),
            ),
            linha_de("user", serde_json::json!(gatilho)),
            linha_de(
                "assistant",
                serde_json::json!([{"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}]),
            ),
            linha_de(
                "user",
                serde_json::json!([{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]),
            ),
            linha_de(
                "assistant",
                serde_json::json!([{"type": "text", "text": "O deploy não saiu"}]),
            ),
        ];
        std::fs::write(f.path(), linhas.join("\n")).unwrap();
        f
    }

    #[test]
    fn o_que_abriu_o_turno_se_le_do_transcript() {
        for (gatilho, esperado) in [
            (MSG_DO_CANAL, Origem::MensagemDoCanal),
            (CI_DE_FUNDO, Origem::TarefaDeFundo),
            (MONITOR_DA_SESSAO, Origem::TarefaDeFundo),
            (CANAL_EXPIROU, Origem::CanalExpirou),
            (
                &format!("{MARCA_SISTEMA}\nArme o monitor de novo."),
                Origem::Sistema,
            ),
            ("roda os testes", Origem::Outra),
        ] {
            let f = turno_aberto_por(gatilho);
            assert_eq!(origem_do_turno(f.path()), Some(esperado), "{gatilho}");
        }
    }

    #[test]
    fn o_empurrao_de_saida_vazia_nao_abre_turno() {
        // O Claude Code injeta este texto no meio de um turno que terminou sem fala; ele não é
        // o que abriu o turno.
        let f = tempfile::NamedTempFile::new().unwrap();
        let linhas = [
            linha_de("user", serde_json::json!(CI_DE_FUNDO)),
            linha_de(
                "assistant",
                serde_json::json!([{"type": "text", "text": ""}]),
            ),
            linha_de(
                "user",
                serde_json::json!(
                    "[Your previous response had no visible output. Please continue and produce a user-visible response.]"
                ),
            ),
            linha_de(
                "assistant",
                serde_json::json!([{"type": "text", "text": "CI verde"}]),
            ),
        ];
        std::fs::write(f.path(), linhas.join("\n")).unwrap();
        assert_eq!(origem_do_turno(f.path()), Some(Origem::TarefaDeFundo));
    }

    #[test]
    fn o_prompt_do_monitor_usa_a_descricao_que_o_classificador_conhece() {
        assert!(CANAL_EXPIROU.contains(&format!("Monitor \"{DESCRICAO_DO_CANAL}")));
        assert!(MSG_DO_CANAL.contains(&format!("Monitor event: \"{DESCRICAO_DO_CANAL}")));
    }
    use std::io::Write;

    fn transcript(dir: &Path, nome: &str, linhas: &[&str]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(nome);
        let mut f = std::fs::File::create(&p).unwrap();
        for l in linhas {
            writeln!(f, "{l}").unwrap();
        }
        p
    }

    const CONVERSA: [&str; 6] = [
        r#"{"type":"user","message":{"role":"user","content":"oi, tudo bem?"}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"deixa eu pensar"},{"type":"text","text":"tudo, e você?"}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"saída do comando"}]}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{}}]}}"#,
        r#"{"type":"user","message":{"role":"user","content":"roda os testes"}}"#,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"rodei, passaram"}]}}"#,
    ];

    #[test]
    fn a_marca_casa_com_o_prefixo_que_o_filtro_usa() {
        // As duas constantes precisam andar juntas: se a frase mudar e o prefixo não, os prompts
        // do daemon voltam a aparecer como fala do usuário sem ninguém perceber.
        assert!(MARCA_SISTEMA.starts_with(PREFIXO_MARCA));
    }

    #[test]
    fn historico_traz_so_o_dialogo() {
        let dir = tempfile::tempdir().unwrap();
        let p = transcript(dir.path(), "a.jsonl", &CONVERSA);
        let falas = historico(&p, 50);
        assert_eq!(falas.len(), 4, "ferramenta e raciocínio não são diálogo");
        assert_eq!(falas[0].papel, Papel::Usuario);
        assert_eq!(falas[0].texto, "oi, tudo bem?");
        assert_eq!(falas[1].texto, "tudo, e você?");
        assert_eq!(falas[3].texto, "rodei, passaram");
    }

    #[test]
    fn prompt_injetado_pelo_daemon_nao_e_fala_do_usuario() {
        let dir = tempfile::tempdir().unwrap();
        let bootstrap = serde_json::json!({
            "type": "user",
            "message": {"content": format!("{MARCA_SISTEMA}\nVocê está rodando dentro do lukadispatch...")}
        });
        let p = transcript(
            dir.path(),
            "a.jsonl",
            &[
                &bootstrap.to_string(),
                // A resposta a ele ("Monitor armado.") também não é diálogo e some junto.
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Monitor armado."}]}}"#,
                r#"{"type":"user","message":{"content":"agora sim, roda os testes"}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"rodei"}]}}"#,
            ],
        );
        let falas = historico(&p, 50);
        assert_eq!(falas.len(), 2, "só a conversa de verdade: {falas:?}");
        assert_eq!(falas[0].texto, "agora sim, roda os testes");
        assert_eq!(falas[1].texto, "rodei");
    }

    #[test]
    fn ultimo_turno_ignora_o_que_veio_antes_da_ultima_entrada() {
        let dir = tempfile::tempdir().unwrap();
        let p = transcript(
            dir.path(),
            "a.jsonl",
            &[
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"de um turno velho"}]}}"#,
                r#"{"type":"user","message":{"content":"liste os arquivos"}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"tem só o README."}]}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Monitor rearmado."}]}}"#,
            ],
        );
        let falas = respostas_do_ultimo_turno(&p);
        assert_eq!(falas, vec!["tem só o README.", "Monitor rearmado."]);
    }

    #[test]
    fn recado_de_monitor_e_reconhecido_sem_engolir_resposta_longa() {
        assert!(e_recado_de_monitor("Monitor rearmado."));
        assert!(e_recado_de_monitor("Monitor armado. Aguardando mensagens."));
        assert!(!e_recado_de_monitor("tem só o README."));
        // Resposta de verdade que fala de monitor, mas é longa demais para ser o anúncio.
        let longa = format!(
            "O monitor do projeto {} está configurado assim: ",
            "x".repeat(120)
        );
        assert!(!e_recado_de_monitor(&longa));
    }

    #[test]
    fn so_e_fala_digitada_o_que_veio_de_gente() {
        assert!(e_fala_digitada("roda os testes"));
        assert!(!e_fala_digitada(&format!(
            "{MARCA_SISTEMA}\nArme o monitor"
        )));
        assert!(!e_fala_digitada(
            "Você está rodando dentro do lukadispatch..."
        ));
        assert!(!e_fala_digitada(
            "<task-notification>\n<event>{}</event>\n</task-notification>"
        ));
        assert!(!e_fala_digitada("   "));
    }

    #[test]
    fn prompt_antigo_sem_marca_tambem_fica_de_fora() {
        let dir = tempfile::tempdir().unwrap();
        let linha = serde_json::json!({
            "type": "user",
            "message": {"content": "Você está rodando dentro do lukadispatch. O seu usuário (Luka) fala com você pelo tópico..."}
        });
        let p = transcript(dir.path(), "a.jsonl", &[&linha.to_string()]);
        assert!(historico(&p, 50).is_empty());
    }

    #[test]
    fn resposta_a_prompt_do_sistema_some_junto_com_ele() {
        let dir = tempfile::tempdir().unwrap();
        let sistema = serde_json::json!({
            "type": "user",
            "message": {"content": format!("{MARCA_SISTEMA}\nArme o monitor de novo.")}
        });
        let resposta = serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "Monitor rearmado. Aguardando."}]}
        });
        let depois = serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "rodei, passaram"}]}
        });
        let p = transcript(
            dir.path(),
            "a.jsonl",
            &[
                &sistema.to_string(),
                &resposta.to_string(),
                &depois.to_string(),
            ],
        );
        let falas = historico(&p, 50);
        assert_eq!(falas.len(), 1, "só a segunda resposta é diálogo: {falas:?}");
        assert_eq!(falas[0].texto, "rodei, passaram");
    }

    #[test]
    fn evento_do_monitor_vira_a_mensagem_que_estava_dentro() {
        let dir = tempfile::tempdir().unwrap();
        // O conteúdo é montado com `json!` de propósito: escrever este aninhamento à mão, com
        // JSON dentro de XML dentro de JSON, é como o teste anterior nasceu quebrado.
        let conteudo = "<task-notification>\n<task-id>x</task-id>\n<summary>Monitor event</summary>\n<event>{\"kind\":\"message\",\"text\":\"roda os testes\",\"from\":\"Luka\",\"at\":0}</event>\n</task-notification>";
        let linha = serde_json::json!({"type": "user", "message": {"content": conteudo}});
        let p = transcript(dir.path(), "a.jsonl", &[&linha.to_string()]);

        let falas = historico(&p, 50);
        assert_eq!(falas.len(), 1, "a mensagem estava dentro do evento");
        assert_eq!(falas[0].papel, Papel::Usuario);
        assert_eq!(falas[0].texto, "roda os testes");
    }

    #[test]
    fn notificacao_que_nao_e_mensagem_fica_de_fora() {
        let dir = tempfile::tempdir().unwrap();
        let evento = serde_json::json!({
            "type": "user",
            "message": {"content": "<task-notification>\n<event>{\"kind\":\"exit\",\"code\":0}</event>\n</task-notification>"}
        });
        let lembrete = serde_json::json!({
            "type": "user",
            "message": {"content": "<system-reminder>lembrete interno</system-reminder>"}
        });
        let p = transcript(
            dir.path(),
            "a.jsonl",
            &[&evento.to_string(), &lembrete.to_string()],
        );
        assert!(historico(&p, 50).is_empty());
    }

    #[test]
    fn limite_mantem_as_ultimas() {
        let dir = tempfile::tempdir().unwrap();
        let p = transcript(dir.path(), "a.jsonl", &CONVERSA);
        let falas = historico(&p, 2);
        assert_eq!(falas.len(), 2);
        assert_eq!(falas[0].texto, "roda os testes");
        assert_eq!(falas[1].texto, "rodei, passaram");
    }

    #[test]
    fn codifica_o_caminho_como_o_claude_code() {
        let d = dir_do_projeto(
            Path::new("/home/luka/.claude"),
            "/home/luka/Personal/cnpj_validator",
        );
        assert!(d.ends_with("-home-luka-Personal-cnpj-validator"));
    }

    #[test]
    fn acha_a_sessao_mais_recente_e_pula_subagente() {
        let dir = tempfile::tempdir().unwrap();
        let projeto = dir_do_projeto(dir.path(), "/tmp/x");
        transcript(&projeto, "velha.jsonl", &CONVERSA);
        transcript(
            &projeto,
            "subagente.jsonl",
            &[r#"{"type":"user","isSidechain":true,"message":{"content":"tarefa interna"}}"#],
        );
        let nova = transcript(&projeto, "nova.jsonl", &CONVERSA);
        // Os três no mesmo segundo, só milissegundos entre eles, e a nova no meio da ordem
        // alfabética: é o empate que, ordenado em segundos, dependia do sistema de arquivos.
        let agora = std::time::SystemTime::now();
        let ms = std::time::Duration::from_millis;
        muda_mtime(&projeto.join("velha.jsonl"), agora);
        muda_mtime(&projeto.join("subagente.jsonl"), agora + ms(10));
        muda_mtime(&nova, agora + ms(5));

        let achada = ultima_sessao(dir.path(), "/tmp/x").unwrap();
        assert_eq!(achada.session_id, "nova");
        assert_eq!(achada.resumo, "rodei, passaram");
    }

    #[test]
    fn projeto_sem_historico_devolve_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ultima_sessao(dir.path(), "/tmp/nunca-usado").is_none());
    }

    #[test]
    fn resumo_corta_linha_longa() {
        assert_eq!(primeira_linha("abc\ndef", 10), "abc");
        let longo = "x".repeat(80);
        assert!(primeira_linha(&longo, 10).ends_with('…'));
    }

    fn muda_mtime(p: &Path, quando: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(quando)
            .unwrap();
    }
}
