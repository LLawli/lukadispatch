//! A conversa do setup, separada de onde as linhas vêm: no uso real é o terminal, nos testes é
//! um roteiro de respostas.

use std::io::{BufRead, Write};

use anyhow::{Result, bail};

pub struct Tela<'a> {
    entrada: &'a mut dyn BufRead,
    saida: &'a mut dyn Write,
    /// Num terminal de verdade o segredo é lido sem eco; no teste, é só mais uma linha.
    terminal: bool,
}

impl<'a> Tela<'a> {
    pub fn nova(entrada: &'a mut dyn BufRead, saida: &'a mut dyn Write, terminal: bool) -> Self {
        Self {
            entrada,
            saida,
            terminal,
        }
    }

    pub fn diz(&mut self, texto: &str) {
        let _ = writeln!(self.saida, "{texto}");
    }

    /// Um título de passo, com uma linha em branco antes para a conversa respirar.
    pub fn passo(&mut self, titulo: &str) {
        let _ = writeln!(self.saida, "\n== {titulo}");
    }

    fn linha(&mut self) -> Result<String> {
        let _ = self.saida.flush();
        let mut s = String::new();
        if self.entrada.read_line(&mut s)? == 0 {
            bail!("a entrada acabou antes do fim do setup");
        }
        Ok(s.trim().to_string())
    }

    /// Pergunta com resposta livre. Enter sozinho fica com o `padrao`.
    pub fn pergunta(&mut self, texto: &str, padrao: &str) -> Result<String> {
        if padrao.is_empty() {
            let _ = write!(self.saida, "{texto}: ");
        } else {
            let _ = write!(self.saida, "{texto} [{padrao}]: ");
        }
        let r = self.linha()?;
        Ok(if r.is_empty() { padrao.to_string() } else { r })
    }

    /// Sim ou não, com sim como padrão.
    pub fn sim(&mut self, texto: &str) -> Result<bool> {
        loop {
            let _ = write!(self.saida, "{texto} [S/n] ");
            match self.linha()?.to_lowercase().as_str() {
                "" | "s" | "sim" | "y" | "yes" => return Ok(true),
                "n" | "nao" | "não" | "no" => return Ok(false),
                _ => self.diz("Responda s ou n."),
            }
        }
    }

    /// Uma das opções, pelo número. Enter fica com a primeira.
    pub fn escolhe(&mut self, texto: &str, opcoes: &[(&str, &str)]) -> Result<usize> {
        self.diz(texto);
        for (i, (nome, explica)) in opcoes.iter().enumerate() {
            self.diz(&format!("  {}. {nome}: {explica}", i + 1));
        }
        loop {
            let r = self.pergunta("Número", "1")?;
            match r.parse::<usize>() {
                Ok(n) if (1..=opcoes.len()).contains(&n) => return Ok(n - 1),
                _ => self.diz(&format!("Escolha de 1 a {}.", opcoes.len())),
            }
        }
    }

    /// Um segredo (o token do bot). Não aparece na tela nem fica no histórico do shell.
    pub fn segredo(&mut self, texto: &str) -> Result<String> {
        if self.terminal {
            let s = rpassword::prompt_password(format!("{texto}: "))?;
            return Ok(s.trim().to_string());
        }
        let _ = write!(self.saida, "{texto}: ");
        self.linha()
    }

    /// Espera o usuário fazer algo fora daqui (no celular) e voltar.
    pub fn espera(&mut self, texto: &str) -> Result<()> {
        let _ = write!(self.saida, "{texto} Depois aperte Enter. ");
        self.linha().map(|_| ())
    }
}
