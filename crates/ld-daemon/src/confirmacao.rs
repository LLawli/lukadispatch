//! A transcrição espera o seu aval antes de virar mensagem para a sessão.
//!
//! Transcrever erra, e errar aqui é caro de um jeito particular: a sessão recebe algo que você
//! não disse, age em cima disso, e desfazer custa mais que o tempo que a voz economizou. Um
//! "daemon" virando "Demon Heroes" é engraçado; um caminho de arquivo trocado no meio de uma
//! frase, não.
//!
//! São três saídas, e a terceira é a que justifica o resto:
//!
//! - **Confirmar**: o texto vai para a sessão como se você o tivesse digitado.
//! - **Descartar**: some sem deixar rastro. A sessão nunca soube que houve áudio, e é isso que
//!   se quer quando a transcrição saiu irreconhecível ou o áudio foi sem querer.
//! - **Escrever algo**: a transcrição vai junto com o que você escreveu, marcado como correção.
//!   É o caso comum de "está quase certo, só essa palavra que ficou errada" — reescrever a
//!   mensagem inteira à mão anularia o ganho de ter falado.
//!
//! O pendente mora em memória, e não no banco: ele vale por um instante e só faz sentido com o
//! card à vista. Se o daemon reiniciar no meio, o áudio continua em disco e o card fica sem
//! efeito, que é melhor do que uma confirmação ressuscitando amanhã uma frase de hoje.

use std::collections::HashMap;
use std::sync::Mutex;

use teloxide::types::MessageId;

/// Uma transcrição pronta, à espera do que fazer com ela.
#[derive(Debug, Clone)]
pub struct Pendente {
    pub session_id: String,
    pub topic: i32,
    /// A mensagem do card, que é apagada nas três saídas.
    pub msg: MessageId,
    pub texto: String,
    /// A legenda que veio junto do áudio, quando veio.
    pub legenda: String,
    /// Quem mandou, para a mensagem chegar à sessão com o mesmo remetente de sempre.
    pub de: String,
    /// O `.oga` original, que segue junto para a sessão poder reouvir.
    pub arquivos: Vec<String>,
}

impl Pendente {
    /// O texto que a sessão recebe.
    ///
    /// Sem correção, é a transcrição pura: para a sessão não há diferença entre falar e digitar.
    /// Com correção, os dois vão juntos e rotulados, porque a sessão precisa saber qual das duas
    /// versões manda — e a resposta é sempre a escrita.
    pub fn para_sessao(&self, correcao: Option<&str>) -> String {
        let mut partes = Vec::new();
        if !self.legenda.trim().is_empty() {
            partes.push(self.legenda.trim().to_string());
        }
        match correcao.map(str::trim).filter(|c| !c.is_empty()) {
            Some(c) => {
                partes.push(format!("[transcrição do áudio] {}", self.texto.trim()));
                partes.push(format!(
                    "[correção, escrita depois de ouvir: vale mais que a transcrição] {c}"
                ));
            }
            None => partes.push(self.texto.trim().to_string()),
        }
        partes.join("\n\n")
    }
}

/// As transcrições esperando decisão, uma por tópico.
///
/// Uma só por tópico de propósito: dois cards abertos no mesmo tópico tornariam ambíguo a qual
/// deles uma correção escrita se refere, e o jeito de desfazer essa ambiguidade seria pedir mais
/// um toque ao Luka, que é exatamente o que este caminho existe para evitar.
#[derive(Default)]
pub struct Confirmacoes {
    por_topico: Mutex<HashMap<i32, Pendente>>,
}

impl Confirmacoes {
    /// Guarda a transcrição e devolve a anterior daquele tópico, se havia uma.
    ///
    /// Quem chamar precisa resolver a anterior (apagar o card), senão fica um teclado órfão que
    /// não responde mais a nada.
    pub fn guarda(&self, p: Pendente) -> Option<Pendente> {
        let mut m = self.por_topico.lock().unwrap();
        m.insert(p.topic, p)
    }

    /// Tira o pendente do tópico, se houver. Usado nas três saídas.
    pub fn tira(&self, topic: i32) -> Option<Pendente> {
        self.por_topico.lock().unwrap().remove(&topic)
    }

    /// Há transcrição esperando neste tópico?
    pub fn tem(&self, topic: i32) -> bool {
        self.por_topico.lock().unwrap().contains_key(&topic)
    }

    /// Esquece o que ficou para trás de uma sessão que acabou.
    pub fn limpa_sessao(&self, session_id: &str) {
        self.por_topico
            .lock()
            .unwrap()
            .retain(|_, p| p.session_id != session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pendente(topic: i32, texto: &str) -> Pendente {
        Pendente {
            session_id: "s1".into(),
            topic,
            msg: MessageId(1),
            texto: texto.into(),
            legenda: String::new(),
            de: "Luka".into(),
            arquivos: vec!["/tmp/voz.oga".into()],
        }
    }

    #[test]
    fn confirmado_sem_correcao_chega_como_texto_puro() {
        // Para a sessão não pode haver diferença entre falar e digitar.
        let p = pendente(1, "  roda os testes  ");
        assert_eq!(p.para_sessao(None), "roda os testes");
    }

    #[test]
    fn correcao_vem_rotulada_e_manda_mais_que_a_transcricao() {
        let p = pendente(1, "roda os testes do módulo arquivos");
        let t = p.para_sessao(Some("na verdade é o módulo transcricao"));
        assert!(
            t.contains("[transcrição do áudio] roda os testes do módulo arquivos"),
            "{t}"
        );
        assert!(t.contains("vale mais que a transcrição"), "{t}");
        assert!(t.contains("na verdade é o módulo transcricao"), "{t}");
    }

    #[test]
    fn correcao_em_branco_e_o_mesmo_que_confirmar() {
        let p = pendente(1, "o texto");
        assert_eq!(p.para_sessao(Some("   ")), p.para_sessao(None));
    }

    #[test]
    fn legenda_do_audio_sobrevive_nas_duas_saidas() {
        let mut p = pendente(1, "a transcrição");
        p.legenda = "olha isso".into();
        assert!(p.para_sessao(None).starts_with("olha isso"));
        assert!(p.para_sessao(Some("corrigindo")).starts_with("olha isso"));
    }

    #[test]
    fn um_pendente_por_topico_e_o_anterior_volta_para_quem_chamou() {
        let c = Confirmacoes::default();
        assert!(c.guarda(pendente(7, "primeira")).is_none());
        let anterior = c
            .guarda(pendente(7, "segunda"))
            .expect("a primeira tem que voltar");
        assert_eq!(
            anterior.texto, "primeira",
            "o card antigo ficaria órfão sem isto"
        );
        assert_eq!(c.tira(7).unwrap().texto, "segunda");
        assert!(!c.tem(7));
    }

    #[test]
    fn topicos_diferentes_nao_se_atrapalham() {
        let c = Confirmacoes::default();
        c.guarda(pendente(1, "de um"));
        c.guarda(pendente(2, "de outro"));
        assert_eq!(c.tira(1).unwrap().texto, "de um");
        assert!(c.tem(2), "tirar de um tópico não pode mexer no outro");
    }

    #[test]
    fn sessao_que_acabou_nao_deixa_pendente_para_tras() {
        let c = Confirmacoes::default();
        c.guarda(pendente(1, "a"));
        let mut outra = pendente(2, "b");
        outra.session_id = "s2".into();
        c.guarda(outra);
        c.limpa_sessao("s1");
        assert!(!c.tem(1));
        assert!(c.tem(2), "limpou a sessão errada");
    }

    #[test]
    fn tirar_de_topico_vazio_nao_explode() {
        let c = Confirmacoes::default();
        assert!(c.tira(42).is_none());
        assert!(!c.tem(42));
    }
}
