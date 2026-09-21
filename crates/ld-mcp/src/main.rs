//! `lukadispatch-mcp`: o proxy que faz o diálogo de um servidor MCP caber no celular.
//!
//! ## Por que ele existe
//!
//! Um servidor MCP pode pedir confirmação ao usuário (mandar uma mensagem de WhatsApp, por
//! exemplo). Isso **não** passa pelo sistema de permissões do Claude Code: é uma requisição
//! `elicitation/create` que o servidor manda ao cliente, e quem desenha o diálogo é o cliente.
//! O hook `Elicitation` avisa que aconteceu, mas a documentação é explícita: a saída dele é
//! ignorada nesse evento. Ou seja, não há como responder por hook.
//!
//! O que sobra é o lugar onde a mensagem passa. Este binário entra no meio do cano:
//!
//! ```text
//! Claude Code  <->  lukadispatch-mcp  <->  servidor MCP de verdade
//! ```
//!
//! Tudo é repassado byte a byte, menos uma coisa: quando o servidor pede uma confirmação, o
//! proxy a responde ele mesmo, depois de abrir o MESMO card do Telegram e a MESMA janela GTK que
//! as perguntas do Claude já usam. O Claude Code nem chega a saber que houve diálogo.
//!
//! ## O que ele não faz
//!
//! Não inspeciona nem altera chamada de ferramenta, resultado ou qualquer outra mensagem: só a
//! requisição de elicitação sai do cano. E quando não consegue perguntar (daemon fora do ar, por
//! exemplo), ele **repassa a requisição para o Claude Code**, e o diálogo aparece no PC como
//! sempre apareceu. Ficar sem resposta travaria a chamada do servidor para sempre.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use ld_core::ask::{Answer, Ask, Opt, Question};
use ld_core::hooks::ASK_TIMEOUT_SECS;
use ld_core::proto::{Request, Response};
use ld_core::race::{Vencedor, disputar};
use serde_json::{Value, json};

const USO: &str = "uso: lukadispatch-mcp --session <uuid> -- <comando do servidor> [args...]";

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cfg) = Config::parse(&args) else {
        eprintln!("{USO}");
        return std::process::ExitCode::from(2);
    };

    match roda(cfg) {
        Ok(codigo) => std::process::ExitCode::from(codigo),
        Err(e) => {
            eprintln!("lukadispatch-mcp: {e}");
            std::process::ExitCode::from(1)
        }
    }
}

struct Config {
    session_id: String,
    comando: String,
    args: Vec<String>,
}

impl Config {
    fn parse(args: &[String]) -> Option<Self> {
        let sessao = args.iter().position(|a| a == "--session")?;
        let session_id = args.get(sessao + 1)?.clone();
        let separador = args.iter().position(|a| a == "--")?;
        let resto = args.get(separador + 1..)?;
        let (comando, args) = resto.split_first()?;
        Some(Self {
            session_id,
            comando: comando.clone(),
            args: args.to_vec(),
        })
    }
}

fn roda(cfg: Config) -> std::io::Result<u8> {
    let mut filho: Child = Command::new(&cfg.comando)
        .args(&cfg.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // stderr passa direto: erro do servidor tem que aparecer no log da sessão, não aqui.
        .stderr(Stdio::inherit())
        .spawn()?;

    let entrada_filho = Arc::new(Mutex::new(filho.stdin.take().expect("stdin pedido acima")));
    let saida_filho = filho.stdout.take().expect("stdout pedido acima");

    // Cliente -> servidor: cópia crua. Nada do que o Claude Code manda é da nossa conta.
    {
        let entrada_filho = entrada_filho.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for linha in stdin.lock().lines() {
                let Ok(linha) = linha else { break };
                let mut w = entrada_filho.lock().unwrap_or_else(|e| e.into_inner());
                if writeln!(w, "{linha}").is_err() || w.flush().is_err() {
                    break;
                }
            }
        });
    }

    // Servidor -> cliente: tudo passa, menos o pedido de confirmação.
    let mut saida = std::io::stdout();
    for linha in BufReader::new(saida_filho).lines() {
        let linha = linha?;
        match pedido_de_confirmacao(&linha) {
            Some((id, params)) => {
                let resposta = decide(&cfg.session_id, &params);
                match resposta {
                    Some(resultado) => {
                        let msg = json!({"jsonrpc": "2.0", "id": id, "result": resultado});
                        let mut w = entrada_filho.lock().unwrap_or_else(|e| e.into_inner());
                        writeln!(w, "{msg}")?;
                        w.flush()?;
                    }
                    // Não deu para perguntar: devolve a requisição ao caminho normal, e o
                    // diálogo aparece no PC. Engolir aqui travaria a chamada do servidor.
                    None => {
                        writeln!(saida, "{linha}")?;
                        saida.flush()?;
                    }
                }
            }
            None => {
                writeln!(saida, "{linha}")?;
                saida.flush()?;
            }
        }
    }

    let status = filho.wait()?;
    Ok(status.code().unwrap_or(0).clamp(0, 255) as u8)
}

/// Reconhece `elicitation/create` e devolve o id e os parâmetros.
///
/// É a única mensagem que o proxy abre. O resto passa sem ser sequer desserializado, porque
/// interpretar mais do que o necessário é como um proxy quebra protocolo.
fn pedido_de_confirmacao(linha: &str) -> Option<(Value, Value)> {
    let v: Value = serde_json::from_str(linha).ok()?;
    if v.get("method")?.as_str()? != "elicitation/create" {
        return None;
    }
    let id = v.get("id")?.clone();
    let params = v.get("params").cloned().unwrap_or(json!({}));
    Some((id, params))
}

/// Pergunta pelos dois canais e monta a resposta do protocolo.
///
/// `None` significa "não consegui perguntar": quem chama repassa a requisição adiante.
fn decide(session_id: &str, params: &Value) -> Option<Value> {
    // Sem daemon no ar não há card nem janela; melhor deixar o Claude Code desenhar o diálogo.
    if !daemon_responde() {
        return None;
    }

    let mensagem = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("O servidor MCP está pedindo uma confirmação.")
        .to_string();
    let esquema = params.get("requestedSchema").cloned().unwrap_or(json!({}));
    let pergunta = monta_pergunta(&mensagem, &esquema);

    let req = Request::Ask {
        session_id: session_id.to_string(),
        tool_use_id: None,
        questions: serde_json::to_value(&pergunta.questions).ok()?,
    };

    let vencedor = disputar(
        req,
        pergunta,
        std::time::Duration::from_secs(ASK_TIMEOUT_SECS),
    )?;
    let escolha = match vencedor {
        Vencedor::Janela(a) => primeira_resposta(&a),
        Vencedor::Telegram(Response::Answer {
            answered: true,
            text: Some(t),
            ..
        }) => Some(t),
        _ => None,
    }?;

    Some(resultado(&escolha, &esquema))
}

/// Uma pergunta com os botões que fazem sentido para este esquema.
///
/// A confirmação típica não pede dado nenhum: é sim ou não. Quando o esquema traz uma lista de
/// valores possíveis, ela vira os botões, que é melhor que obrigar você a digitar.
fn monta_pergunta(mensagem: &str, esquema: &Value) -> Ask {
    let mut options = vec![];
    if let Some(valores) = enum_do_esquema(esquema) {
        for v in valores {
            options.push(Opt {
                label: v,
                description: String::new(),
                preview: None,
            });
        }
    }
    options.push(Opt {
        label: "Aceitar".into(),
        description: "confirma e deixa o servidor seguir".into(),
        preview: None,
    });
    options.push(Opt {
        label: "Recusar".into(),
        description: "o servidor recebe uma recusa e não faz a ação".into(),
        preview: None,
    });

    Ask {
        questions: vec![Question {
            question: mensagem.to_string(),
            header: "MCP".into(),
            multi_select: false,
            options,
        }],
    }
}

/// Os valores de um `enum` do esquema, quando ele tem uma propriedade só.
fn enum_do_esquema(esquema: &Value) -> Option<Vec<String>> {
    let props = esquema.get("properties")?.as_object()?;
    if props.len() != 1 {
        return None;
    }
    let (_, prop) = props.iter().next()?;
    let valores = prop.get("enum")?.as_array()?;
    Some(
        valores
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
    )
}

/// Traduz a escolha para o formato que o protocolo espera.
fn resultado(escolha: &str, esquema: &Value) -> Value {
    let recusou = escolha.trim().eq_ignore_ascii_case("recusar")
        || escolha.trim().eq_ignore_ascii_case("negar")
        || escolha.trim().eq_ignore_ascii_case("não")
        || escolha.trim().eq_ignore_ascii_case("nao");
    if recusou {
        return json!({"action": "decline"});
    }

    // Aceitar pede o conteúdo no formato do esquema. Propriedade booleana vira `true` (aceitar é
    // dizer sim); as de texto recebem o que você escolheu ou escreveu.
    let mut conteudo = serde_json::Map::new();
    if let Some(props) = esquema.get("properties").and_then(Value::as_object) {
        for (nome, prop) in props {
            let tipo = prop.get("type").and_then(Value::as_str).unwrap_or("string");
            let valor = match tipo {
                "boolean" => json!(true),
                "number" | "integer" => escolha
                    .trim()
                    .parse::<f64>()
                    .map(|n| json!(n))
                    .unwrap_or(json!(0)),
                _ if escolha.eq_ignore_ascii_case("aceitar") => json!(""),
                _ => json!(escolha),
            };
            conteudo.insert(nome.clone(), valor);
        }
    }
    json!({"action": "accept", "content": Value::Object(conteudo)})
}

/// A escolha que a janela do PC devolveu.
fn primeira_resposta(a: &Answer) -> Option<String> {
    a.items.first()?.answers.first().cloned()
}

/// Sonda o daemon antes de prometer que consegue perguntar.
fn daemon_responde() -> bool {
    use std::io::Read;
    let Ok(mut s) = std::os::unix::net::UnixStream::connect(ld_core::paths::socket()) else {
        return false;
    };
    let prazo = std::time::Duration::from_millis(500);
    let _ = s.set_read_timeout(Some(prazo));
    let _ = s.set_write_timeout(Some(prazo));
    if s.write_all(ld_core::proto::line(&Request::Ping).as_bytes())
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 64];
    s.read(&mut buf).map(|n| n > 0).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn so_a_elicitacao_e_aberta() {
        let outra = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{}}"#;
        assert!(pedido_de_confirmacao(outra).is_none());
        let resposta = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        assert!(pedido_de_confirmacao(resposta).is_none());
        assert!(pedido_de_confirmacao("isto não é json").is_none());

        let pedido = r#"{"jsonrpc":"2.0","id":7,"method":"elicitation/create","params":{"message":"Enviar?"}}"#;
        let (id, params) = pedido_de_confirmacao(pedido).unwrap();
        assert_eq!(id, json!(7));
        assert_eq!(params["message"], "Enviar?");
    }

    #[test]
    fn recusar_vira_decline() {
        let r = resultado("Recusar", &json!({}));
        assert_eq!(r["action"], "decline");
        assert!(r.get("content").is_none());
    }

    #[test]
    fn aceitar_sem_esquema_manda_conteudo_vazio() {
        let r = resultado("Aceitar", &json!({}));
        assert_eq!(r["action"], "accept");
        assert_eq!(r["content"], json!({}));
    }

    #[test]
    fn propriedade_booleana_vira_sim() {
        let esquema = json!({"properties": {"confirm": {"type": "boolean"}}});
        let r = resultado("Aceitar", &esquema);
        assert_eq!(r["content"]["confirm"], json!(true));
    }

    #[test]
    fn texto_escrito_preenche_a_propriedade() {
        let esquema = json!({"properties": {"nome": {"type": "string"}}});
        let r = resultado("Maria", &esquema);
        assert_eq!(r["content"]["nome"], json!("Maria"));
    }

    #[test]
    fn enum_do_esquema_vira_botao() {
        let esquema =
            json!({"properties": {"conta": {"type": "string", "enum": ["pessoal", "trabalho"]}}});
        let a = monta_pergunta("Qual conta?", &esquema);
        let rotulos: Vec<&str> = a.questions[0]
            .options
            .iter()
            .map(|o| o.label.as_str())
            .collect();
        assert_eq!(rotulos, vec!["pessoal", "trabalho", "Aceitar", "Recusar"]);
    }

    #[test]
    fn esquema_com_duas_propriedades_nao_vira_botao() {
        // Com mais de uma propriedade não dá para saber qual o botão preencheria.
        let esquema = json!({"properties": {"a": {"enum": ["x"]}, "b": {"type": "string"}}});
        assert!(enum_do_esquema(&esquema).is_none());
    }

    #[test]
    fn config_exige_sessao_e_comando() {
        let bom: Vec<String> = ["--session", "s1", "--", "servidor", "-x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let c = Config::parse(&bom).unwrap();
        assert_eq!(c.session_id, "s1");
        assert_eq!(c.comando, "servidor");
        assert_eq!(c.args, vec!["-x"]);

        let sem_comando: Vec<String> = ["--session", "s1", "--"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(Config::parse(&sem_comando).is_none());
        assert!(Config::parse(&[]).is_none());
    }
}
