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
//! - **Responder ao card**: a transcrição vai junto com o que você escreveu, marcado como
//!   correção. É o caso comum de "está quase certo, só essa palavra que ficou errada" —
//!   reescrever a mensagem inteira à mão anularia o ganho de ter falado.
//!
//! A correção exige responder ao card, e não qualquer texto. Na primeira versão bastava escrever,
//! e o efeito era que NENHUMA outra mensagem podia ser mandada enquanto uma transcrição esperava:
//! tudo que você digitasse seria anexado a ela.
//!
//! **Um card por vez, em fila.** Dois áudios seguidos são duas mensagens suas e merecem duas
//! decisões, mas mostrar os dois cards juntos tornaria ambíguo a qual deles uma correção escrita
//! se refere. Então o segundo espera: quando o primeiro é resolvido, o card dele vira registro e
//! o próximo sobe. É a mesma mecânica dos cards de pergunta e permissão.
//!
//! O pendente mora em memória, e não no banco: ele vale por um instante e só faz sentido com o
//! card à vista. Se o daemon reiniciar no meio, o áudio continua em disco e o card fica sem
//! efeito, que é melhor do que uma confirmação ressuscitando amanhã uma frase de hoje.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use teloxide::types::MessageId;

/// Uma transcrição pronta, à espera do que fazer com ela.
#[derive(Debug, Clone)]
pub struct Pendente {
    /// Identifica o card no callback do botão, para um toque não resolver o card de outro áudio.
    pub id: String,
    pub session_id: String,
    pub topic: i32,
    /// A mensagem do áudio que gerou isto. O card responde a ela, e é a seta do Telegram que
    /// diz de qual voz esta transcrição saiu quando há várias no tópico.
    pub origem: Option<MessageId>,
    /// A mensagem do card, enquanto ele está na tela. `None` enquanto espera a vez na fila.
    pub msg: Option<MessageId>,
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

/// A fila de transcrições esperando decisão.
#[derive(Default)]
pub struct Confirmacoes {
    /// Ordem de chegada importa: a fila anda na ordem em que você falou.
    fila: Mutex<Vec<Pendente>>,
    proximo_id: AtomicU64,
}

impl Confirmacoes {
    /// Um id curto para o callback do botão.
    ///
    /// O `callback_data` do Telegram só tem 64 bytes, e um contador em hexadecimal cabe com
    /// folga junto do prefixo, onde um uuid não caberia.
    pub fn novo_id(&self) -> String {
        format!("{:x}", self.proximo_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Põe na fila. O card só aparece quando chegar a vez (ver [`Self::proximo_sem_card`]).
    pub fn guarda(&self, p: Pendente) {
        self.fila.lock().unwrap().push(p);
    }

    /// Há card na tela neste tópico?
    pub fn tem_card_na_tela(&self, topic: i32) -> bool {
        self.fila
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.topic == topic && p.msg.is_some())
    }

    /// O próximo da fila que ainda não tem card, quando não há nenhum na tela.
    ///
    /// Devolve uma cópia: quem chamar manda o card e depois avisa com [`Self::marca_na_tela`].
    /// Fazer as duas coisas sob o mesmo lock exigiria segurá-lo durante uma chamada de rede, o
    /// que travaria o resto do daemon.
    pub fn proximo_sem_card(&self, topic: i32) -> Option<Pendente> {
        let f = self.fila.lock().unwrap();
        if f.iter().any(|p| p.topic == topic && p.msg.is_some()) {
            return None;
        }
        f.iter()
            .find(|p| p.topic == topic && p.msg.is_none())
            .cloned()
    }

    /// Registra que o card deste pendente está na tela.
    pub fn marca_na_tela(&self, id: &str, msg: MessageId) {
        if let Some(p) = self.fila.lock().unwrap().iter_mut().find(|p| p.id == id) {
            p.msg = Some(msg);
        }
    }

    /// Tira o card de id conhecido: é o caminho dos botões, que sabem qual resolver.
    pub fn tira_por_id(&self, id: &str) -> Option<Pendente> {
        let mut f = self.fila.lock().unwrap();
        let i = f.iter().position(|p| p.id == id)?;
        Some(f.remove(i))
    }

    /// Tira o que está na tela neste tópico.
    pub fn tira_da_tela(&self, topic: i32) -> Option<Pendente> {
        let mut f = self.fila.lock().unwrap();
        let i = f.iter().position(|p| p.topic == topic && p.msg.is_some())?;
        Some(f.remove(i))
    }

    /// Tira o card cuja mensagem é esta: é o caminho da correção, que agora exige responder
    /// ao card.
    ///
    /// Exigir o reply custa um toque a mais e paga por si: sem ele, QUALQUER texto digitado com
    /// um card aberto virava correção, e não havia como mandar uma mensagem nova e independente
    /// enquanto uma transcrição esperava confirmação.
    pub fn tira_por_msg(&self, msg: MessageId) -> Option<Pendente> {
        let mut f = self.fila.lock().unwrap();
        let i = f.iter().position(|p| p.msg == Some(msg))?;
        Some(f.remove(i))
    }

    /// Quantas ainda esperam a vez neste tópico, sem contar a que está na tela.
    pub fn na_fila(&self, topic: i32) -> usize {
        self.fila
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.topic == topic && p.msg.is_none())
            .count()
    }

    /// Esquece o que ficou para trás de uma sessão que acabou, devolvendo os cards para apagar.
    pub fn limpa_sessao(&self, session_id: &str) -> Vec<Pendente> {
        let mut f = self.fila.lock().unwrap();
        let (meus, resto): (Vec<_>, Vec<_>) = f.drain(..).partition(|p| p.session_id == session_id);
        *f = resto;
        meus
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pendente(c: &Confirmacoes, topic: i32, texto: &str) -> Pendente {
        Pendente {
            id: c.novo_id(),
            session_id: "s1".into(),
            topic,
            origem: Some(MessageId(10)),
            msg: None,
            texto: texto.into(),
            legenda: String::new(),
            de: "Luka".into(),
            arquivos: vec!["/tmp/voz.oga".into()],
        }
    }

    #[test]
    fn confirmado_sem_correcao_chega_como_texto_puro() {
        // Para a sessão não pode haver diferença entre falar e digitar.
        let c = Confirmacoes::default();
        let p = pendente(&c, 1, "  roda os testes  ");
        assert_eq!(p.para_sessao(None), "roda os testes");
    }

    #[test]
    fn correcao_vem_rotulada_e_manda_mais_que_a_transcricao() {
        let c = Confirmacoes::default();
        let p = pendente(&c, 1, "roda os testes do módulo arquivos");
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
        let c = Confirmacoes::default();
        let p = pendente(&c, 1, "o texto");
        assert_eq!(p.para_sessao(Some("   ")), p.para_sessao(None));
    }

    #[test]
    fn audio_com_legenda_mantem_a_legenda_nas_duas_saidas() {
        // Edge case: o áudio veio com texto escrito junto. A legenda é sua também, e some se
        // ninguém a carregar daqui até a sessão.
        let c = Confirmacoes::default();
        let mut p = pendente(&c, 1, "a transcrição");
        p.legenda = "olha isso".into();
        let so_confirmado = p.para_sessao(None);
        assert!(so_confirmado.starts_with("olha isso"), "{so_confirmado}");
        assert!(so_confirmado.contains("a transcrição"), "{so_confirmado}");
        let corrigido = p.para_sessao(Some("corrigindo"));
        assert!(corrigido.starts_with("olha isso"), "{corrigido}");
        assert!(corrigido.contains("corrigindo"), "{corrigido}");
    }

    #[test]
    fn so_um_card_na_tela_e_o_seguinte_espera_a_vez() {
        let c = Confirmacoes::default();
        let primeiro = pendente(&c, 7, "primeiro");
        let segundo = pendente(&c, 7, "segundo");
        let (id1, id2) = (primeiro.id.clone(), segundo.id.clone());
        c.guarda(primeiro);
        c.guarda(segundo);

        // O primeiro sobe; enquanto ele está na tela, o segundo não pode subir.
        let sobe = c.proximo_sem_card(7).expect("o primeiro tem que subir");
        assert_eq!(sobe.id, id1);
        c.marca_na_tela(&id1, MessageId(100));
        assert!(
            c.proximo_sem_card(7).is_none(),
            "dois cards juntos tornariam ambígua a correção escrita"
        );
        assert_eq!(c.na_fila(7), 1, "o segundo está esperando a vez");

        // Resolvido o primeiro, o segundo vira a vez.
        assert_eq!(c.tira_por_id(&id1).unwrap().texto, "primeiro");
        assert_eq!(
            c.proximo_sem_card(7).expect("agora é a vez do segundo").id,
            id2
        );
    }

    #[test]
    fn responder_ao_card_resolve_aquele_card() {
        let c = Confirmacoes::default();
        let p = pendente(&c, 3, "na tela");
        let id = p.id.clone();
        c.guarda(p);
        c.guarda(pendente(&c, 3, "esperando"));
        c.marca_na_tela(&id, MessageId(200));

        let resolvido = c
            .tira_por_msg(MessageId(200))
            .expect("respondi a este card");
        assert_eq!(resolvido.texto, "na tela");
        assert!(
            !c.tem_card_na_tela(3),
            "o que espera a vez não conta como na tela"
        );
    }

    #[test]
    fn responder_a_outra_mensagem_nao_corrige_transcricao_nenhuma() {
        // O ponto da regra: com um card aberto, escrever (ou responder a outra coisa) tem de
        // continuar sendo uma mensagem comum. Antes disso, qualquer texto virava correção e não
        // dava para falar de outro assunto enquanto uma transcrição esperava.
        let c = Confirmacoes::default();
        let p = pendente(&c, 3, "esperando confirmação");
        let id = p.id.clone();
        c.guarda(p);
        c.marca_na_tela(&id, MessageId(200));

        assert!(
            c.tira_por_msg(MessageId(999)).is_none(),
            "responder a outra mensagem não pode consumir o card"
        );
        assert!(c.tem_card_na_tela(3), "o card tem de continuar esperando");
    }

    #[test]
    fn responder_a_mensagem_antiga_nao_libera_o_topico() {
        // O furo que isto tranca: responder a QUALQUER mensagem passava pela guarda, porque ela
        // só perguntava "é reply?". Reply que não casa com o card na tela não resolve nada, e a
        // pendência continua de pé para a guarda segurar.
        let c = Confirmacoes::default();
        let p = pendente(&c, 4, "esperando");
        let id = p.id.clone();
        c.guarda(p);
        c.marca_na_tela(&id, MessageId(500));

        assert!(
            c.tira_por_msg(MessageId(499)).is_none(),
            "reply a outra mensagem casou"
        );
        assert!(
            c.tira_por_msg(MessageId(501)).is_none(),
            "reply a outra mensagem casou"
        );
        assert!(
            c.tem_card_na_tela(4),
            "a pendência tem de continuar de pé para a guarda segurar a mensagem"
        );
        assert!(
            c.tira_por_msg(MessageId(500)).is_some(),
            "o reply ao card tem de casar"
        );
    }

    #[test]
    fn responder_a_um_card_da_fila_que_ainda_nao_subiu_nao_resolve() {
        let c = Confirmacoes::default();
        c.guarda(pendente(&c, 5, "só na fila"));
        assert!(
            c.tira_por_msg(MessageId(1)).is_none(),
            "não há card na tela para responder"
        );
    }

    #[test]
    fn topicos_diferentes_tem_filas_independentes() {
        // Edge case: duas sessões falando ao mesmo tempo, em projetos diferentes.
        let c = Confirmacoes::default();
        let a = pendente(&c, 1, "de um");
        let b = pendente(&c, 2, "de outro");
        let (ida, idb) = (a.id.clone(), b.id.clone());
        c.guarda(a);
        c.guarda(b);
        c.marca_na_tela(&ida, MessageId(1));
        c.marca_na_tela(&idb, MessageId(2));

        // Um tópico ocupado não pode segurar a fila do outro.
        assert!(c.tem_card_na_tela(1) && c.tem_card_na_tela(2));
        assert_eq!(c.tira_por_msg(MessageId(1)).unwrap().texto, "de um");
        assert!(c.tem_card_na_tela(2), "mexer num tópico afetou o outro");
        assert_eq!(c.tira_por_id(&idb).unwrap().texto, "de outro");
    }

    #[test]
    fn botao_tocado_duas_vezes_nao_resolve_duas() {
        let c = Confirmacoes::default();
        let p = pendente(&c, 1, "a");
        let id = p.id.clone();
        c.guarda(p);
        assert!(c.tira_por_id(&id).is_some());
        assert!(
            c.tira_por_id(&id).is_none(),
            "o segundo toque não pode reenviar a mesma transcrição"
        );
    }

    #[test]
    fn ids_nao_se_repetem() {
        let c = Confirmacoes::default();
        let ids: std::collections::HashSet<_> = (0..500).map(|_| c.novo_id()).collect();
        assert_eq!(ids.len(), 500, "id repetido resolveria o card errado");
    }

    #[test]
    fn sessao_que_acabou_devolve_os_cards_para_apagar() {
        let c = Confirmacoes::default();
        let p = pendente(&c, 1, "a");
        let id = p.id.clone();
        c.guarda(p);
        c.marca_na_tela(&id, MessageId(9));
        let mut outra = pendente(&c, 2, "b");
        outra.session_id = "s2".into();
        c.guarda(outra);

        let apagar = c.limpa_sessao("s1");
        assert_eq!(apagar.len(), 1);
        assert_eq!(
            apagar[0].msg,
            Some(MessageId(9)),
            "sem devolver o card ele fica órfão na tela"
        );
        assert_eq!(c.na_fila(2), 1, "limpou a sessão errada");
    }
}
