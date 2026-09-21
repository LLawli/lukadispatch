//! Os cards do Telegram: pergunta do Claude e pedido de permissão.
//!
//! Uma pergunta pode ter várias sub-perguntas. O card é um só e vai avançando: responde a
//! primeira, ele vira a segunda, e só quando a última é respondida a pendência inteira resolve.
//! Assim a resposta que volta para o Claude é sempre completa, nunca pela metade.
//!
//! O outro canal (a janela GTK4 no PC) mostra tudo de uma vez e responde de uma vez. Os dois
//! terminam no mesmo lugar: `Hub::answer`, que só aceita o primeiro.

use std::collections::HashMap;
use std::sync::Mutex;

use ld_core::ask::{Answer, AnswerItem, Ask};
use teloxide::types::{InlineKeyboardMarkup, MessageId};

use crate::telegram::{coluna, escape_html};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Pergunta,
    Permissao,
}

pub struct Card {
    pub ask_id: String,
    pub session_id: String,
    pub topic: i32,
    pub msg: MessageId,
    pub kind: Kind,
    ask: Ask,
    escolhas: Vec<Vec<String>>,
    atual: usize,
}

#[derive(Default)]
pub struct Cards {
    abertos: Mutex<HashMap<String, Card>>,
}

/// O que fazer depois de um toque no botão.
pub enum Efeito {
    /// Redesenhar o card com este conteúdo.
    Redesenhar(String, InlineKeyboardMarkup),
    /// Acabou: esta é a resposta final.
    Pronto(Answer),
    /// Botão de um card que não existe mais (respondido pelo PC, sessão morta).
    Ignorar,
}

impl Cards {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn abrir(&self, ask_id: &str, card: Card) {
        self.abertos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ask_id.to_string(), card);
    }

    pub fn fechar(&self, ask_id: &str) -> Option<Card> {
        self.abertos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(ask_id)
    }

    pub fn da_sessao(&self, session_id: &str) -> Vec<String> {
        self.abertos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, c)| c.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn msg(&self, ask_id: &str) -> Option<MessageId> {
        self.abertos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(ask_id)
            .map(|c| c.msg)
    }

    /// Aplica um toque de botão. `Ignorar` quando o card já morreu, que é o caso normal de quem
    /// tocou no celular depois de responder pelo PC.
    pub fn tocar(&self, ask_id: &str, acao: Acao) -> Efeito {
        let mut abertos = self.abertos.lock().unwrap_or_else(|e| e.into_inner());
        let Some(card) = abertos.get_mut(ask_id) else {
            return Efeito::Ignorar;
        };
        card.tocar(acao)
    }
}

/// O que o botão pediu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acao {
    /// Escolheu a opção `n` da pergunta atual.
    Opcao(usize),
    /// Confirmou uma pergunta de múltipla escolha.
    Confirmar,
}

impl Card {
    pub fn nova_pergunta(
        ask_id: String,
        session_id: String,
        topic: i32,
        msg: MessageId,
        ask: Ask,
    ) -> Self {
        let n = ask.questions.len();
        Self {
            ask_id,
            session_id,
            topic,
            msg,
            kind: Kind::Pergunta,
            ask,
            escolhas: vec![Vec::new(); n],
            atual: 0,
        }
    }

    /// Card de permissão: guardado só para poder ser apagado quando a sessão morre ou quando a
    /// janela do PC responde primeiro. O toque nele é resolvido direto, sem máquina de estados.
    pub fn nova_permissao(ask_id: String, session_id: String, topic: i32, msg: MessageId) -> Self {
        Self {
            ask_id,
            session_id,
            topic,
            msg,
            kind: Kind::Permissao,
            ask: Ask::default(),
            escolhas: Vec::new(),
            atual: 0,
        }
    }

    fn tocar(&mut self, acao: Acao) -> Efeito {
        let Some(q) = self.ask.questions.get(self.atual) else {
            return Efeito::Ignorar;
        };

        match acao {
            Acao::Opcao(i) => {
                let Some(opt) = q.options.get(i) else {
                    return Efeito::Ignorar;
                };
                if q.multi_select {
                    // Alterna: tocar de novo tira a opção, senão não haveria como desmarcar.
                    let escolhas = &mut self.escolhas[self.atual];
                    match escolhas.iter().position(|e| e == &opt.label) {
                        Some(p) => {
                            escolhas.remove(p);
                        }
                        None => escolhas.push(opt.label.clone()),
                    }
                    return self.desenhar();
                }
                self.escolhas[self.atual] = vec![opt.label.clone()];
                self.avancar()
            }
            Acao::Confirmar => {
                if self.escolhas[self.atual].is_empty() {
                    // Confirmar sem escolher nada não avança: a resposta vazia não diz nada ao
                    // Claude, e ele voltaria a perguntar.
                    return self.desenhar();
                }
                self.avancar()
            }
        }
    }

    fn avancar(&mut self) -> Efeito {
        self.atual += 1;
        if self.atual >= self.ask.questions.len() {
            return Efeito::Pronto(self.resposta());
        }
        self.desenhar()
    }

    fn resposta(&self) -> Answer {
        Answer {
            items: self
                .ask
                .questions
                .iter()
                .zip(&self.escolhas)
                .map(|(q, escolhas)| AnswerItem {
                    header: q.header.clone(),
                    question: q.question.clone(),
                    answers: escolhas.clone(),
                })
                .collect(),
        }
    }

    pub fn desenhar(&self) -> Efeito {
        let Some(q) = self.ask.questions.get(self.atual) else {
            return Efeito::Ignorar;
        };
        let total = self.ask.questions.len();
        let mut texto = String::new();
        if total > 1 {
            texto.push_str(&format!("<i>{} de {}</i>\n", self.atual + 1, total));
        }
        texto.push_str(&format!("❓ <b>{}</b>", escape_html(&q.question)));

        let escolhidas = &self.escolhas[self.atual];
        let mut botoes: Vec<(String, String)> = q
            .options
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let marca = if escolhidas.contains(&o.label) {
                    "☑ "
                } else if q.multi_select {
                    "☐ "
                } else {
                    ""
                };
                (
                    format!("{marca}{}", o.label),
                    format!("a:{}:{i}", self.ask_id),
                )
            })
            .collect();
        if q.multi_select {
            botoes.push(("✅ Confirmar".into(), format!("a:{}:c", self.ask_id)));
        }

        // A descrição de cada opção vai no corpo: no botão ela não caberia.
        for o in &q.options {
            if !o.description.is_empty() {
                texto.push_str(&format!(
                    "\n\n<b>{}</b>\n{}",
                    escape_html(&o.label),
                    escape_html(&o.description)
                ));
            }
        }

        Efeito::Redesenhar(texto, coluna(botoes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ld_core::ask::{Opt, Question};

    fn ask(multi: bool) -> Ask {
        Ask {
            questions: vec![
                Question {
                    question: "Qual banco?".into(),
                    header: "Banco".into(),
                    multi_select: multi,
                    options: vec![
                        Opt {
                            label: "SQLite".into(),
                            description: "arquivo local".into(),
                        },
                        Opt {
                            label: "Postgres".into(),
                            description: String::new(),
                        },
                    ],
                },
                Question {
                    question: "Qual log?".into(),
                    header: "Log".into(),
                    multi_select: false,
                    options: vec![Opt {
                        label: "json".into(),
                        description: String::new(),
                    }],
                },
            ],
        }
    }

    fn card(multi: bool) -> Card {
        Card::nova_pergunta("a1".into(), "s1".into(), 7, MessageId(1), ask(multi))
    }

    #[test]
    fn escolha_unica_avanca_para_a_proxima_pergunta() {
        let mut c = card(false);
        match c.tocar(Acao::Opcao(0)) {
            Efeito::Redesenhar(texto, _) => {
                assert!(texto.contains("Qual log?"), "devia ter avançado: {texto}");
                assert!(texto.contains("2 de 2"));
            }
            _ => panic!("esperava avançar para a segunda pergunta"),
        }
    }

    #[test]
    fn responder_todas_fecha_com_a_resposta_completa() {
        let mut c = card(false);
        let _ = c.tocar(Acao::Opcao(1));
        match c.tocar(Acao::Opcao(0)) {
            Efeito::Pronto(r) => {
                assert_eq!(r.items.len(), 2);
                assert_eq!(r.items[0].answers, vec!["Postgres"]);
                assert_eq!(r.items[1].answers, vec!["json"]);
            }
            _ => panic!("esperava terminar"),
        }
    }

    #[test]
    fn multipla_escolha_alterna_e_so_confirma_no_botao() {
        let mut c = card(true);
        assert!(matches!(c.tocar(Acao::Opcao(0)), Efeito::Redesenhar(_, _)));
        assert!(matches!(c.tocar(Acao::Opcao(1)), Efeito::Redesenhar(_, _)));
        // Tocar de novo desmarca.
        assert!(matches!(c.tocar(Acao::Opcao(0)), Efeito::Redesenhar(_, _)));
        match c.tocar(Acao::Confirmar) {
            Efeito::Redesenhar(texto, _) => assert!(texto.contains("Qual log?")),
            _ => panic!("confirmar devia avançar"),
        }
        assert_eq!(c.escolhas[0], vec!["Postgres"]);
    }

    #[test]
    fn confirmar_sem_escolher_nao_avanca() {
        let mut c = card(true);
        match c.tocar(Acao::Confirmar) {
            Efeito::Redesenhar(texto, _) => assert!(texto.contains("Qual banco?")),
            _ => panic!("não podia avançar sem escolha"),
        }
    }

    #[test]
    fn botao_fora_do_intervalo_e_ignorado() {
        let mut c = card(false);
        assert!(matches!(c.tocar(Acao::Opcao(99)), Efeito::Ignorar));
    }

    #[test]
    fn descricao_da_opcao_vai_no_corpo() {
        let c = card(false);
        match c.desenhar() {
            Efeito::Redesenhar(texto, _) => {
                assert!(texto.contains("arquivo local"));
                assert!(
                    !texto.contains("<b>Postgres</b>"),
                    "opção sem descrição não vira seção"
                );
            }
            _ => panic!(),
        }
    }

    #[test]
    fn card_de_sessao_morta_e_ignorado() {
        let cards = Cards::new();
        cards.abrir("a1", card(false));
        assert_eq!(cards.da_sessao("s1"), vec!["a1"]);
        cards.fechar("a1");
        assert!(matches!(cards.tocar("a1", Acao::Opcao(0)), Efeito::Ignorar));
    }
}
