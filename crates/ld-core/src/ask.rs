//! A pergunta do Claude, do jeito que os três lados precisam dela.
//!
//! O mesmo objeto atravessa o hook (que recebe o `tool_input` do AskUserQuestion), o daemon (que
//! desenha os cards no Telegram) e a janela GTK4 do PC. Ter o tipo num lugar só é o que garante
//! que os dois canais mostram exatamente a mesma pergunta e devolvem a mesma resposta.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Ask {
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Question {
    pub question: String,
    /// Rótulo curto (até 12 caracteres, é o contrato do AskUserQuestion). Vira o título do card.
    pub header: String,
    pub options: Vec<Opt>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Opt {
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// Bloco de exemplo que a opção carrega (maquete em ASCII, trecho de código, diagrama).
    ///
    /// É monoespaçado e alinhado por espaços: sem fonte de largura fixa ele vira sopa de letras,
    /// então os dois canais o mostram em bloco de código.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// A resposta: uma entrada por pergunta, na mesma ordem.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Answer {
    pub items: Vec<AnswerItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnswerItem {
    pub header: String,
    pub question: String,
    /// Rótulos escolhidos, ou o texto livre que a pessoa digitou.
    pub answers: Vec<String>,
}

impl Ask {
    /// Lê o `tool_input` do AskUserQuestion. Campo que falta vira valor vazio em vez de erro: um
    /// hook que explode por causa de um formato novo seria pior que um card feio.
    pub fn from_tool_input(input: &Value) -> Self {
        let questions = input
            .get("questions")
            .and_then(Value::as_array)
            .map(|qs| qs.iter().map(Question::from_value).collect())
            .unwrap_or_default();
        Self { questions }
    }

    pub fn is_empty(&self) -> bool {
        self.questions.is_empty()
    }
}

impl Question {
    fn from_value(v: &Value) -> Self {
        let texto = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let options = v
            .get("options")
            .and_then(Value::as_array)
            .map(|os| {
                os.iter()
                    .map(|o| Opt {
                        label: o
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        description: o
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        preview: o
                            .get("preview")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .filter(|s| !s.trim().is_empty()),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            question: texto("question"),
            header: texto("header"),
            options,
            multi_select: v
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

impl Answer {
    /// O texto que volta para o Claude.
    ///
    /// Ele diz em letras claras que isto é RESPOSTA e não recusa, porque o canal de volta é o
    /// `permissionDecisionReason` de um `deny`: sem essa frase, o Claude leria um bloqueio e
    /// sairia pedindo desculpa em vez de seguir com a escolha feita.
    pub fn to_claude(&self) -> String {
        let mut s = String::from(
            "O usuário respondeu pelo lukadispatch (Telegram ou janela do PC), então a pergunta \
             já está respondida e a ferramenta AskUserQuestion não precisa rodar. Siga com estas \
             escolhas:\n",
        );
        for item in &self.items {
            s.push_str(&format!(
                "\n- {}: {}",
                if item.header.is_empty() {
                    &item.question
                } else {
                    &item.header
                },
                item.answers.join(", ")
            ));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entrada() -> Value {
        json!({
            "questions": [
                {
                    "question": "Qual banco usar?",
                    "header": "Banco",
                    "multiSelect": false,
                    "options": [
                        {"label": "SQLite", "description": "arquivo local"},
                        {"label": "Postgres", "description": "servidor", "preview": "┌─────┐\n│ srv │\n└─────┘"}
                    ]
                },
                {
                    "question": "Quais extras?",
                    "header": "Extras",
                    "multiSelect": true,
                    "options": [{"label": "logs"}, {"label": "métricas"}]
                }
            ]
        })
    }

    #[test]
    fn le_o_tool_input_do_askuserquestion() {
        let a = Ask::from_tool_input(&entrada());
        assert_eq!(a.questions.len(), 2);
        assert_eq!(a.questions[0].header, "Banco");
        assert_eq!(a.questions[0].options[1].label, "Postgres");
        assert!(!a.questions[0].multi_select);
        assert!(a.questions[1].multi_select);
        assert_eq!(
            a.questions[1].options[0].description, "",
            "opção sem descrição não pode quebrar a leitura"
        );
        assert!(a.questions[0].options[0].preview.is_none());
        assert!(
            a.questions[0].options[1]
                .preview
                .as_deref()
                .is_some_and(|p| p.contains("srv")),
            "o preview precisa sobreviver à leitura"
        );
    }

    #[test]
    fn entrada_estranha_vira_pergunta_vazia() {
        assert!(Ask::from_tool_input(&json!({})).is_empty());
        assert!(Ask::from_tool_input(&json!({"questions": "isso não é lista"})).is_empty());
    }

    #[test]
    fn resposta_deixa_claro_que_nao_e_recusa() {
        let a = Answer {
            items: vec![AnswerItem {
                header: "Banco".into(),
                question: "Qual banco usar?".into(),
                answers: vec!["SQLite".into()],
            }],
        };
        let t = a.to_claude();
        assert!(t.contains("respondeu"));
        assert!(t.contains("não precisa rodar"));
        assert!(t.contains("- Banco: SQLite"));
    }

    #[test]
    fn multipla_escolha_vira_lista_separada_por_virgula() {
        let a = Answer {
            items: vec![AnswerItem {
                header: "Extras".into(),
                question: "Quais extras?".into(),
                answers: vec!["logs".into(), "métricas".into()],
            }],
        };
        assert!(a.to_claude().contains("- Extras: logs, métricas"));
    }

    #[test]
    fn sem_header_usa_a_pergunta() {
        let a = Answer {
            items: vec![AnswerItem {
                header: String::new(),
                question: "Qual banco?".into(),
                answers: vec!["SQLite".into()],
            }],
        };
        assert!(a.to_claude().contains("- Qual banco?: SQLite"));
    }

    #[test]
    fn resposta_sobrevive_a_ida_e_volta_em_json() {
        let a = Answer {
            items: vec![AnswerItem {
                header: "Banco".into(),
                question: "q".into(),
                answers: vec!["SQLite".into()],
            }],
        };
        let txt = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<Answer>(&txt).unwrap(), a);
    }
}
