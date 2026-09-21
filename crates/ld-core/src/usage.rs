//! Leitura read-only do banco do XClaudeUsage (`~/.claude/data/xclaude-usage.db`).
//!
//! Quem escreve esse banco é o hook `xclaudeusage record` que já existe no settings global do
//! usuário, a cada PostToolUse. Aqui nunca se escreve, e nunca se pode derrubar o daemon por
//! causa dele: banco ausente, travado ou com esquema diferente devolve `None`.
//!
//! Consequência que quem lê precisa ter em mente: os números envelhecem quando nenhuma sessão do
//! Claude Code está rodando. Por isso `updated_at` vai junto e o painel mostra a idade.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    /// 0 a 100.
    pub pct: f64,
    /// Epoch em segundos.
    pub resets_at: i64,
    /// Epoch em segundos de quando o statusline gravou este número.
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Windows {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionTokens {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

impl SessionTokens {
    /// Tokens que contam como trabalho da sessão. Leitura de cache entra: é o que o Claude
    /// realmente processou, e é o número que bate com a percepção de "essa sessão está pesada".
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_creation + self.cache_read
    }
}

fn abrir(db: &Path) -> Option<Connection> {
    if !db.exists() {
        return None;
    }
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    // O statusline escreve no mesmo banco o tempo todo; esperar um pouco é melhor que devolver
    // nada, e 2s é o mesmo teto que o sdispath usava.
    let _ = conn.busy_timeout(std::time::Duration::from_millis(2000));
    Some(conn)
}

fn ler_janela(conn: &Connection, tabela: &str) -> Option<Window> {
    // `tabela` nunca vem de fora: são as duas constantes abaixo.
    let sql = format!("SELECT used_percentage, resets_at, updated_at FROM {tabela} WHERE id = 1");
    conn.query_row(&sql, [], |row| {
        Ok(Window {
            pct: row.get::<_, f64>(0)?.clamp(0.0, 100.0),
            resets_at: row.get(1)?,
            updated_at: row.get(2)?,
        })
    })
    .ok()
}

/// As duas janelas de limite numa única abertura do banco.
pub fn windows(db: &Path) -> Windows {
    let Some(conn) = abrir(db) else {
        return Windows::default();
    };
    Windows {
        five_hour: ler_janela(&conn, "five_hour_window"),
        seven_day: ler_janela(&conn, "seven_day_window"),
    }
}

/// Tokens acumulados por uma sessão específica.
pub fn session_tokens(db: &Path, session_id: &str) -> Option<SessionTokens> {
    let conn = abrir(db)?;
    let mut stmt = conn
        .prepare("SELECT token_type, SUM(quantity) FROM token_usage WHERE session_id = ?1 GROUP BY token_type")
        .ok()?;
    let linhas = stmt
        .query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .ok()?;

    let mut t = SessionTokens::default();
    let mut achou = false;
    for linha in linhas.flatten() {
        achou = true;
        let (tipo, qtd) = linha;
        let qtd = qtd.max(0) as u64;
        match tipo.as_str() {
            "input" => t.input = qtd,
            "output" => t.output = qtd,
            "cache_creation" => t.cache_creation = qtd,
            "cache_read" => t.cache_read = qtd,
            _ => {}
        }
    }
    achou.then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn banco_de_teste() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let caminho = dir.path().join("usage.db");
        let conn = Connection::open(&caminho).unwrap();
        conn.execute_batch(
            "CREATE TABLE five_hour_window (id INTEGER PRIMARY KEY CHECK (id = 1), resets_at INTEGER NOT NULL, start_at INTEGER NOT NULL, used_percentage REAL NOT NULL, updated_at INTEGER NOT NULL);
             CREATE TABLE seven_day_window (id INTEGER PRIMARY KEY CHECK (id = 1), resets_at INTEGER NOT NULL, starts_at INTEGER NOT NULL, used_percentage REAL NOT NULL, updated_at INTEGER NOT NULL);
             CREATE TABLE token_usage (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, model TEXT NOT NULL, token_type TEXT NOT NULL, quantity INTEGER NOT NULL, executed_at INTEGER NOT NULL, device_id TEXT NOT NULL DEFAULT '', message_uuid TEXT);
             INSERT INTO five_hour_window VALUES (1, 1789995600, 1789977600, 13.0, 1789994218);
             INSERT INTO seven_day_window VALUES (1, 1790557200, 1789952400, 2.0, 1789994218);
             INSERT INTO token_usage (session_id, model, token_type, quantity, executed_at) VALUES
               ('s1','opus','input',10,0), ('s1','opus','input',5,0), ('s1','opus','output',7,0),
               ('s1','opus','cache_read',100,0), ('s2','opus','input',1,0);",
        )
        .unwrap();
        (dir, caminho)
    }

    #[test]
    fn le_as_duas_janelas() {
        let (_d, db) = banco_de_teste();
        let w = windows(&db);
        assert_eq!(w.five_hour.unwrap().pct, 13.0);
        assert_eq!(w.seven_day.unwrap().resets_at, 1790557200);
    }

    #[test]
    fn banco_ausente_nao_explode() {
        let w = windows(Path::new("/nao/existe/usage.db"));
        assert_eq!(w, Windows::default());
        assert!(session_tokens(Path::new("/nao/existe/usage.db"), "s1").is_none());
    }

    #[test]
    fn soma_tokens_da_sessao() {
        let (_d, db) = banco_de_teste();
        let t = session_tokens(&db, "s1").unwrap();
        assert_eq!(t.input, 15);
        assert_eq!(t.output, 7);
        assert_eq!(t.cache_read, 100);
        assert_eq!(t.total(), 122);
    }

    #[test]
    fn sessao_desconhecida_devolve_none() {
        let (_d, db) = banco_de_teste();
        assert!(session_tokens(&db, "inexistente").is_none());
    }
}
