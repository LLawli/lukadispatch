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
    let mut candidatos: Vec<(i64, PathBuf)> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .filter_map(|p| {
            let quando = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs() as i64;
            Some((quando, p))
        })
        .collect();
    candidatos.sort_by_key(|(quando, _)| std::cmp::Reverse(*quando));

    for (quando, caminho) in candidatos {
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
        let texto = texto_da_mensagem(msg);
        if texto.trim().is_empty() {
            continue;
        }
        falas.push(Fala { papel, texto });
    }
    if falas.len() > limite {
        falas.drain(..falas.len() - limite);
    }
    falas
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
        // Garante ordem de modificação previsível.
        let nova = transcript(&projeto, "nova.jsonl", &CONVERSA);
        filetime_recente(&nova);

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

    fn filetime_recente(p: &Path) {
        // Reescreve para garantir mtime maior que o dos outros arquivos do teste.
        let conteudo = std::fs::read(p).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(p, conteudo).unwrap();
    }
}
