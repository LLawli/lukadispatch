//! `lukadispatch hook [agente] <evento>`: a porta de entrada dos ganchos de cada agente.
//!
//! Cada agente de código tem o seu formato de gancho (nome dos campos, eventos que existem,
//! como uma decisão volta). Um tradutor por agente converte o que ele emite para o protocolo
//! neutro do socket, e o daemon nunca vê o formato de agente nenhum.
//!
//! A linha de comando tem duas formas, e a contagem de argumentos é o que separa uma da outra,
//! sem ambiguidade possível entre nome de agente e nome de evento:
//!
//! - `hook <evento>`: o agente é o [`PADRAO`], o Claude Code. É a forma que os settings já
//!   instalados por versões anteriores usam, e continua valendo.
//! - `hook <agente> <evento>`: o agente é dito. É a forma que os ganchos gerados agora escrevem.
//!
//! Um agente novo (Codex, por exemplo) é um módulo aqui dentro que implemente [`Ganchos`] e uma
//! linha em [`agentes`]. Os ganchos que esse agente instala chamam `lukadispatch hook codex
//! <evento>`.
//!
//! Regra que vale para todos: **um gancho nunca trava a sessão**. Agente desconhecido, stdin
//! vazio ou JSON quebrado viram um aviso no stderr e saída 0.

use std::io::Read;

use serde_json::Value;

pub mod claude;

/// O agente de quando a linha de comando não diz qual.
pub const PADRAO: &str = "claude";

/// Um agente cujos ganchos o lukadispatch sabe ler.
pub trait Ganchos {
    /// Nome na linha de comando (`hook <nome> <evento>`).
    fn nome(&self) -> &'static str;

    /// Trata um evento, com o JSON que o agente mandou no stdin já lido, e devolve o código de
    /// saída do gancho.
    fn trata(&self, evento: &str, entrada: Value) -> i32;
}

/// Todos os agentes cujos ganchos o CLI sabe ler. É a única lista: a busca por nome e a
/// mensagem de agente desconhecido saem daqui.
pub fn agentes() -> Vec<Box<dyn Ganchos>> {
    vec![Box::new(claude::Claude)]
}

/// O tradutor de ganchos de um agente, pelo nome.
pub fn agente(nome: &str) -> Option<Box<dyn Ganchos>> {
    agentes().into_iter().find(|a| a.nome() == nome)
}

/// Separa os argumentos de `hook` em (agente, evento).
pub fn separa(args: &[String]) -> (&str, &str) {
    match args {
        [agente, evento, ..] => (agente.as_str(), evento.as_str()),
        [evento] => (PADRAO, evento.as_str()),
        [] => (PADRAO, ""),
    }
}

/// Roda um evento para um agente. `entrada` é o JSON do stdin, `None` quando não veio nada
/// legível.
pub fn executa(nome: &str, evento: &str, entrada: Option<Value>) -> i32 {
    let Some(tradutor) = agente(nome) else {
        // stderr e 0: o gancho de um agente mal configurado não pode derrubar a sessão dele.
        let conhecidos: Vec<&str> = agentes().iter().map(|a| a.nome()).collect();
        eprintln!(
            "lukadispatch: não sei ler ganchos do agente {nome:?} (conheço: {})",
            conhecidos.join(", ")
        );
        return 0;
    };
    let Some(entrada) = entrada else {
        return 0; // stdin vazio ou JSON quebrado: não é problema do agente.
    };
    tradutor.trata(evento, entrada)
}

/// O subcomando inteiro: argumentos depois de `hook`, com o evento no stdin.
pub fn run(args: &[String]) -> i32 {
    let (nome, evento) = separa(args);
    executa(nome, evento, ler_evento())
}

fn ler_evento() -> Option<Value> {
    let mut bruto = String::new();
    std::io::stdin().read_to_string(&mut bruto).ok()?;
    serde_json::from_str(&bruto).ok()
}

#[cfg(test)]
mod testes {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sem_agente_e_o_claude_code() {
        // É a forma que os settings instalados por versões anteriores usam.
        assert_eq!(separa(&args(&["stop"])), ("claude", "stop"));
        assert_eq!(separa(&args(&[])), ("claude", ""));
    }

    #[test]
    fn com_agente_ele_e_quem_manda() {
        assert_eq!(separa(&args(&["claude", "stop"])), ("claude", "stop"));
        assert_eq!(
            separa(&args(&["codex", "session-start"])),
            ("codex", "session-start")
        );
    }

    #[test]
    fn o_padrao_esta_entre_os_conhecidos() {
        assert!(
            agente(PADRAO).is_some(),
            "hook <evento> sem agente cairia no vazio"
        );
    }

    #[test]
    fn so_o_claude_code_e_conhecido_hoje() {
        assert_eq!(agente("claude").map(|a| a.nome()), Some("claude"));
        assert!(agente("codex").is_none());
    }

    #[test]
    fn agente_desconhecido_nao_trava_a_sessao() {
        let json = serde_json::json!({"session_id": "s1"});
        assert_eq!(executa("codex", "stop", Some(json)), 0);
    }

    #[test]
    fn entrada_ilegivel_nao_trava_a_sessao() {
        assert_eq!(executa("claude", "stop", None), 0);
    }
}
