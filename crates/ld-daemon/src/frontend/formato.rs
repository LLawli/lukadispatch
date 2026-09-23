//! A marcação que o domínio usa para texto com formatação, e como um adaptador a lê.
//!
//! O domínio escreve num subconjunto mínimo de HTML: `<b>`, `<i>`, `<code>` e `<pre>`, com `&`,
//! `<` e `>` escapados por [`escapa`]. Foi a escolha porque é a marcação que o Telegram já aceita
//! como está (o adaptador dele repassa sem traduzir), e porque tem fechamento explícito: ao
//! contrário do Markdown, não há ambiguidade sobre onde um negrito termina.
//!
//! Um adaptador cuja plataforma fala outra coisa (o WhatsApp usa `*negrito*`, `_itálico_` e
//! crase) não precisa entender HTML: [`trechos`] devolve a árvore, e ele só percorre e escreve.
//! [`texto_puro`] é o caso extremo, para plataforma sem formatação nenhuma.

/// Um pedaço de texto com a formatação dele.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trecho {
    /// Texto comum, já sem escape (`&lt;` voltou a ser `<`).
    Texto(String),
    Negrito(Vec<Trecho>),
    Italico(Vec<Trecho>),
    /// Código em linha. Não tem formatação dentro.
    Codigo(String),
    /// Bloco monoespaçado, que preserva alinhamento por espaço.
    Bloco(String),
}

/// Escapa o que a marcação trata como sintaxe. O `&` vem primeiro, senão `&lt;` viraria
/// `&amp;lt;` e o escape apareceria cru do outro lado.
pub fn escapa(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Lê a marcação e devolve a árvore.
///
/// É tolerante de propósito, porque quem escreve a marcação é o próprio daemon e errar aqui só
/// pode custar formatação, nunca a mensagem: etiqueta desconhecida vira texto literal, e
/// etiqueta que não fecha fecha sozinha no fim.
pub fn trechos(rico: &str) -> Vec<Trecho> {
    let mut pilha: Vec<(&'static str, Vec<Trecho>)> = vec![("", Vec::new())];
    let mut chars = rico.char_indices().peekable();

    // Acrescenta texto ao topo da pilha, juntando com o último `Trecho::Texto` se houver: sem
    // isso, "a" seguido de "b" viraria dois trechos em vez de um "ab".
    fn empurra_texto(alvo: &mut Vec<Trecho>, s: &str) {
        if s.is_empty() {
            return;
        }
        if let Some(Trecho::Texto(ultimo)) = alvo.last_mut() {
            ultimo.push_str(s);
        } else {
            alvo.push(Trecho::Texto(s.to_string()));
        }
    }

    while let Some(&(i, c)) = chars.peek() {
        if c != '<' {
            // Um trecho de texto corrido até o próximo '<' (ou o fim).
            let ini = i;
            while let Some(&(_, c)) = chars.peek() {
                if c == '<' {
                    break;
                }
                chars.next();
            }
            let fim = chars.peek().map(|&(i, _)| i).unwrap_or(rico.len());
            let s = desescapa(&rico[ini..fim]);
            let topo = &mut pilha.last_mut().unwrap().1;
            empurra_texto(topo, &s);
            continue;
        }

        // Tenta ler uma etiqueta de abertura ou fechamento a partir de '<'.
        let resto = &rico[i..];
        if let Some((etiqueta, fecha, consumidos)) = le_etiqueta(resto) {
            if fecha {
                // Só fecha se a etiqueta bate com alguma etiqueta aberta; senão é texto literal
                // (um `</b>` sem `<b>` correspondente não pode sumir a mensagem).
                if let Some(pos) = pilha.iter().rposition(|(e, _)| *e == etiqueta) {
                    // Fecha tudo que ficou aberto por cima, na ordem: mais interno primeiro.
                    while pilha.len() > pos + 1 {
                        let (e, filhos) = pilha.pop().unwrap();
                        let trecho = embrulha(e, filhos);
                        empurra_texto_ou_trecho(&mut pilha.last_mut().unwrap().1, trecho);
                    }
                    let (_, filhos) = pilha.pop().unwrap();
                    let trecho = embrulha(etiqueta, filhos);
                    empurra_texto_ou_trecho(&mut pilha.last_mut().unwrap().1, trecho);
                    for _ in 0..consumidos {
                        chars.next();
                    }
                    continue;
                }
            } else if matches!(etiqueta, "code" | "pre") {
                // Não aninha: o conteúdo é texto cru até a etiqueta de fechamento correspondente
                // (ou o fim da mensagem).
                let fecha_com = format!("</{etiqueta}>");
                let dentro_ini = i + consumidos;
                let dentro = &rico[dentro_ini..];
                let (bruto, avanco) = match dentro.find(&fecha_com) {
                    Some(pos) => (&dentro[..pos], pos + fecha_com.len()),
                    None => (dentro, dentro.len()),
                };
                let trecho = if etiqueta == "code" {
                    Trecho::Codigo(desescapa(bruto))
                } else {
                    Trecho::Bloco(desescapa(bruto))
                };
                empurra_texto_ou_trecho(&mut pilha.last_mut().unwrap().1, trecho);
                let total = consumidos + avanco;
                for _ in 0..total {
                    chars.next();
                }
                continue;
            } else {
                pilha.push((etiqueta, Vec::new()));
                for _ in 0..consumidos {
                    chars.next();
                }
                continue;
            }
        }

        // '<' solto, ou etiqueta desconhecida, ou fechamento sem abertura correspondente: vira
        // texto literal, caractere a caractere, sem interpretar nada.
        chars.next();
        empurra_texto(&mut pilha.last_mut().unwrap().1, "<");
    }

    // Etiqueta que não fecha: fecha sozinha no fim, na ordem em que foi aberta.
    while pilha.len() > 1 {
        let (e, filhos) = pilha.pop().unwrap();
        let trecho = embrulha(e, filhos);
        empurra_texto_ou_trecho(&mut pilha.last_mut().unwrap().1, trecho);
    }
    pilha.pop().unwrap().1
}

/// Igual a `empurra_texto`, mas para um `Trecho` qualquer: só junta se os dois forem `Texto`.
fn empurra_texto_ou_trecho(alvo: &mut Vec<Trecho>, trecho: Trecho) {
    if let Trecho::Texto(s) = &trecho
        && let Some(Trecho::Texto(ultimo)) = alvo.last_mut()
    {
        ultimo.push_str(s);
        return;
    }
    alvo.push(trecho);
}

fn embrulha(etiqueta: &str, filhos: Vec<Trecho>) -> Trecho {
    match etiqueta {
        "b" => Trecho::Negrito(filhos),
        "i" => Trecho::Italico(filhos),
        _ => unreachable!("só b e i chegam aqui; code e pre não empilham"),
    }
}

/// Lê uma etiqueta (`<b>`, `</b>`, `<i>`, ...) a partir do começo de `s`, que começa com '<'.
/// Devolve o nome da etiqueta, se é fechamento, e quantos caracteres ela ocupa. `None` quando não
/// é uma das etiquetas conhecidas (vira '<' literal em quem chama).
fn le_etiqueta(s: &str) -> Option<(&'static str, bool, usize)> {
    const CONHECIDAS: [&str; 4] = ["b", "i", "code", "pre"];
    let sem_abre = &s[1..];
    let (fecha, sem_barra) = match sem_abre.strip_prefix('/') {
        Some(r) => (true, r),
        None => (false, sem_abre),
    };
    let nome_bruto: String = sem_barra.chars().take_while(|c| *c != '>').collect();
    if !sem_barra[nome_bruto.len()..].starts_with('>') {
        return None; // não fechou com '>' antes do fim da mensagem.
    }
    let etiqueta = CONHECIDAS.iter().find(|e| **e == nome_bruto)?;
    let consumidos = 1 + usize::from(fecha) + nome_bruto.chars().count() + 1;
    Some((etiqueta, fecha, consumidos))
}

/// Desfaz o escape de [`escapa`]: `&lt;`, `&gt;` e `&amp;` voltam a ser `<`, `>` e `&`.
///
/// A ordem importa e é o inverso de `escapa`: `&amp;` por último, senão `&amp;lt;` (que é como
/// `escapa` grava um `&lt;` literal) voltaria a `<` em vez de `&lt;`.
fn desescapa(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// A mesma mensagem sem formatação nenhuma, com o escape desfeito.
pub fn texto_puro(rico: &str) -> String {
    fn concatena(trechos: &[Trecho], saida: &mut String) {
        for t in trechos {
            match t {
                Trecho::Texto(s) | Trecho::Codigo(s) | Trecho::Bloco(s) => saida.push_str(s),
                Trecho::Negrito(filhos) | Trecho::Italico(filhos) => concatena(filhos, saida),
            }
        }
    }
    let mut saida = String::new();
    concatena(&trechos(rico), &mut saida);
    saida
}

/// Quebra o texto em pedaços de no máximo `limite` caracteres, preferindo cortar em quebra de
/// linha para não picar código no meio. Linha única maior que o limite é cortada no braço.
/// Concatenar os pedaços devolve o texto original, sem perder nem inventar caractere.
pub fn quebra(texto: &str, limite: usize) -> Vec<String> {
    if texto.chars().count() <= limite {
        return vec![texto.to_string()];
    }
    let mut pedacos = Vec::new();
    let mut atual = String::new();
    for linha in texto.split_inclusive('\n') {
        if atual.chars().count() + linha.chars().count() > limite {
            if !atual.is_empty() {
                pedacos.push(std::mem::take(&mut atual));
            }
            // Linha única maior que o limite (log gigante, base64): corta no braço.
            let mut resto: Vec<char> = linha.chars().collect();
            while resto.len() > limite {
                let cabeca: String = resto.drain(..limite).collect();
                pedacos.push(cabeca);
            }
            atual = resto.into_iter().collect();
        } else {
            atual.push_str(linha);
        }
    }
    if !atual.is_empty() {
        pedacos.push(atual);
    }
    pedacos
}

#[cfg(test)]
mod testes {
    //! A marcação vai e volta sem perder caractere, e a quebra respeita o limite.

    use super::*;
    use Trecho::*;

    fn t(s: &str) -> Trecho {
        Texto(s.into())
    }

    #[test]
    fn escape_cobre_os_tres_e_o_e_comercial_vem_primeiro() {
        assert_eq!(escapa("a<b>&c"), "a&lt;b&gt;&amp;c");
        assert_eq!(escapa("<"), "&lt;");
    }

    #[test]
    fn as_quatro_etiquetas_viram_arvore() {
        assert_eq!(
            trechos("a <b>b</b> <i>c</i> <code>d</code>\n<pre>e  f</pre>"),
            vec![
                t("a "),
                Negrito(vec![t("b")]),
                t(" "),
                Italico(vec![t("c")]),
                t(" "),
                Codigo("d".into()),
                t("\n"),
                Bloco("e  f".into()),
            ]
        );
    }

    #[test]
    fn escape_e_desfeito_no_texto_e_no_codigo() {
        assert_eq!(trechos("a &lt;b&gt; &amp;"), vec![t("a <b> &")]);
        assert_eq!(
            trechos("<code>x &amp;&amp; y</code>"),
            vec![Codigo("x && y".into())]
        );
    }

    #[test]
    fn negrito_pode_conter_italico() {
        assert_eq!(
            trechos("<b>x <i>y</i></b>"),
            vec![Negrito(vec![t("x "), Italico(vec![t("y")])])]
        );
    }

    #[test]
    fn etiqueta_desconhecida_e_texto_literal() {
        assert_eq!(trechos("<u>x</u>"), vec![t("<u>x</u>")]);
    }

    #[test]
    fn etiqueta_que_nao_fecha_fecha_no_fim() {
        assert_eq!(trechos("<b>x"), vec![Negrito(vec![t("x")])]);
    }

    #[test]
    fn texto_sem_marcacao_e_um_trecho_so() {
        assert_eq!(trechos("só texto"), vec![t("só texto")]);
        assert!(trechos("").is_empty(), "texto vazio não tem trecho");
    }

    #[test]
    fn texto_puro_tira_a_marcacao_e_desfaz_o_escape() {
        assert_eq!(
            texto_puro("🎤 <b>Transcrição</b>\n&lt;ok&gt; <code>a&amp;b</code>"),
            "🎤 Transcrição\n<ok> a&b"
        );
    }

    #[test]
    fn escapar_e_ler_devolve_o_original() {
        // O que o domínio faz com texto seu: escapa e embrulha. Nenhum caractere pode mudar.
        for original in [
            "a<b>&c",
            "&lt; literal",
            "<b>não é negrito</b>",
            "x && y > z",
        ] {
            assert_eq!(texto_puro(&escapa(original)), original);
            assert_eq!(trechos(&escapa(original)), vec![t(original)]);
        }
    }

    #[test]
    fn texto_curto_nao_e_quebrado() {
        assert_eq!(quebra("oi", 10), vec!["oi"]);
    }

    #[test]
    fn quebra_em_linha_quando_da() {
        let texto = "aaaa\nbbbb\ncccc\n";
        let p = quebra(texto, 10);
        assert!(p.len() > 1);
        assert!(p.iter().all(|x| x.chars().count() <= 10));
        assert_eq!(
            p.concat(),
            texto,
            "quebrar não pode perder nem inventar caractere"
        );
    }

    #[test]
    fn linha_gigante_e_cortada_no_braco() {
        let texto = "x".repeat(25);
        let p = quebra(&texto, 10);
        assert_eq!(p.len(), 3);
        assert_eq!(p.concat(), texto);
    }

    #[test]
    fn limite_conta_caractere_e_nao_byte() {
        // "ç" tem dois bytes; cortar por byte partiria o caractere e o texto viraria lixo.
        let texto = "ç".repeat(15);
        let p = quebra(&texto, 10);
        assert_eq!(p.concat(), texto);
        assert!(p.iter().all(|x| x.chars().count() <= 10));
    }
}
