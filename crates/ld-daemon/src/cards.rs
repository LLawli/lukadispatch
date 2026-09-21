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
    /// O card aberto de uma sessão, se houver.
    pub fn aberto_da_sessao(&self, session_id: &str) -> Option<(String, Kind)> {
        self.abertos
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(_, c)| c.session_id == session_id)
            .map(|(id, c)| (id.clone(), c.kind))
    }

    pub fn tocar(&self, ask_id: &str, acao: Acao) -> Efeito {
        let mut abertos = self.abertos.lock().unwrap_or_else(|e| e.into_inner());
        let Some(card) = abertos.get_mut(ask_id) else {
            return Efeito::Ignorar;
        };
        card.tocar(acao)
    }
}

/// Teto de um preview dentro do card.
///
/// Uma mensagem do Telegram cabe em 4096 caracteres, e um card pode ter quatro opções com
/// preview. Cortar cada um é o que impede a pergunta inteira de ser recusada pela API por
/// tamanho, que seria pior: nenhum card, nenhuma pergunta.
const TETO_PREVIEW: usize = 600;

fn corta_preview(p: &str) -> String {
    let texto = p.trim_end();
    if texto.chars().count() <= TETO_PREVIEW {
        return texto.to_string();
    }
    format!(
        "{}\n[…]",
        texto.chars().take(TETO_PREVIEW).collect::<String>()
    )
}

/// O que o botão (ou a mensagem escrita) pediu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acao {
    /// Escolheu a opção `n` da pergunta atual.
    Opcao(usize),
    /// Confirmou uma pergunta de múltipla escolha.
    Confirmar,
    /// Escreveu a própria resposta, em vez de escolher.
    ///
    /// É o que acontece quando você responde ao card digitando, que é o gesto natural no
    /// Telegram. Sem isto a mensagem ia para a sessão, que está bloqueada justamente esperando a
    /// resposta do card: ela não chegava a lugar nenhum.
    Texto(String),
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
            Acao::Texto(escrito) => {
                let escrito = escrito.trim().to_string();
                if escrito.is_empty() {
                    return Efeito::Ignorar;
                }
                // Texto escrito substitui a escolha: se você escreveu, nenhuma opção servia.
                self.escolhas[self.atual] = vec![escrito];
                self.avancar()
            }
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

        // A descrição e o preview de cada opção vão no corpo: no botão não caberiam.
        for o in &q.options {
            if o.description.is_empty() && o.preview.is_none() {
                continue;
            }
            texto.push_str(&format!("\n\n<b>{}</b>", escape_html(&o.label)));
            if !o.description.is_empty() {
                texto.push_str(&format!("\n{}", escape_html(&o.description)));
            }
            if let Some(preview) = &o.preview {
                // `<pre>` é o que preserva o alinhamento por espaços; sem ele uma maquete em
                // ASCII vira sopa de letras na fonte proporcional do Telegram.
                texto.push_str(&format!(
                    "\n<pre>{}</pre>",
                    escape_html(&corta_preview(preview))
                ));
            }
        }
        // O mesmo convite que a janela do PC faz. Sem ele, escrever parece não ser opção, e
        // responder digitando é o gesto natural de quem está no Telegram.
        texto.push_str("\n\n<i>ou escreva a sua resposta</i>");

        Efeito::Redesenhar(texto, coluna(botoes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ld_core::ask::{Opt, Question};

    fn com_preview() -> Ask {
        Ask {
            questions: vec![Question {
                question: "Qual estilo?".into(),
                header: "Estilo".into(),
                multi_select: false,
                options: vec![Opt {
                    label: "K&R".into(),
                    description: "chaves na mesma linha".into(),
                    preview: Some("fn main() {\n    ok();\n}".into()),
                }],
            }],
        }
    }

    #[test]
    fn preview_vai_em_bloco_monoespacado() {
        let c = Card::nova_pergunta("a1".into(), "s1".into(), 7, MessageId(1), com_preview());
        match c.desenhar() {
            Efeito::Redesenhar(texto, _) => {
                assert!(texto.contains("<pre>"), "sem bloco o alinhamento se perde");
                assert!(texto.contains("fn main() {"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn preview_gigante_e_cortado() {
        let mut a = com_preview();
        a.questions[0].options[0].preview = Some("x".repeat(5000));
        let c = Card::nova_pergunta("a1".into(), "s1".into(), 7, MessageId(1), a);
        match c.desenhar() {
            Efeito::Redesenhar(texto, _) => {
                assert!(texto.chars().count() < 4096, "a API recusaria a mensagem");
                assert!(texto.contains("[…]"));
            }
            _ => panic!(),
        }
    }

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
                            preview: None,
                        },
                        Opt {
                            label: "Postgres".into(),
                            description: String::new(),
                            preview: None,
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
                        preview: None,
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
    fn texto_escrito_responde_a_pergunta_atual() {
        let mut c = card(false);
        match c.tocar(Acao::Texto("quero a opção C".into())) {
            Efeito::Redesenhar(texto, _) => assert!(texto.contains("Qual log?")),
            outro => panic!("devia avançar, veio {}", rotulo(&outro)),
        }
        match c.tocar(Acao::Texto("nenhum".into())) {
            Efeito::Pronto(r) => {
                assert_eq!(r.items[0].answers, vec!["quero a opção C"]);
                assert_eq!(r.items[1].answers, vec!["nenhum"]);
            }
            outro => panic!("devia terminar, veio {}", rotulo(&outro)),
        }
    }

    #[test]
    fn texto_vazio_nao_conta_como_resposta() {
        let mut c = card(false);
        assert!(matches!(
            c.tocar(Acao::Texto("   ".into())),
            Efeito::Ignorar
        ));
    }

    fn rotulo(e: &Efeito) -> &'static str {
        match e {
            Efeito::Redesenhar(_, _) => "redesenhar",
            Efeito::Pronto(_) => "pronto",
            Efeito::Ignorar => "ignorar",
        }
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
