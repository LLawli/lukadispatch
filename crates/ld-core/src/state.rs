//! Estado persistente: sessões, fila de mensagens e um par chave-valor para o painel.
//!
//! É SQLite porque o daemon reinicia (atualização, reboot, `systemctl restart`) e o vínculo
//! sessão <-> tópico não pode morrer junto: um tópico órfão no Telegram é lixo que só dá para
//! limpar na mão.
//!
//! Sincronia: o daemon é assíncrono, o rusqlite não. As chamadas daqui são todas curtas (índice
//! por chave primária, uma linha), então rodam sob `Mutex` mesmo, sem `spawn_blocking`. Se algum
//! dia aparecer varredura pesada, ela é que deve ir para `spawn_blocking`, não isto.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::proto::SessionSummary;

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub session_id: String,
    pub project: String,
    pub cwd: String,
    pub transcript_path: Option<String>,
    /// Nome da sessão tmux. `None` em sessão que o usuário abriu no terminal: ela entra no
    /// painel, mas o bot não a controla.
    pub tmux: Option<String>,
    pub topic_id: Option<i32>,
    pub status: String,
    pub status_message_id: Option<i32>,
    pub created_at: i64,
    pub ended_at: Option<i64>,
}

impl Session {
    pub fn owned_by_bot(&self) -> bool {
        self.tmux.is_some()
    }
}

pub struct Store {
    conn: Mutex<Connection>,
}

const ESQUEMA: &str = r#"
PRAGMA journal_mode = WAL;
CREATE TABLE IF NOT EXISTS sessions (
    session_id        TEXT PRIMARY KEY,
    project           TEXT NOT NULL,
    cwd               TEXT NOT NULL,
    transcript_path   TEXT,
    tmux              TEXT,
    topic_id          INTEGER,
    status            TEXT NOT NULL DEFAULT 'idle',
    status_message_id INTEGER,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    ended_at          INTEGER
);
CREATE TABLE IF NOT EXISTS queue (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    text       TEXT NOT NULL,
    from_name  TEXT NOT NULL,
    at         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS queue_por_sessao ON queue(session_id, id);
CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#;

fn agora() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn open(caminho: &Path) -> Result<Self> {
        if let Some(pai) = caminho.parent() {
            std::fs::create_dir_all(pai)
                .with_context(|| format!("criando {}", pai.display()))?;
        }
        let conn = Connection::open(caminho)
            .with_context(|| format!("abrindo estado em {}", caminho.display()))?;
        conn.execute_batch(ESQUEMA).context("criando esquema")?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(ESQUEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // Envenenar o mutex só acontece se um painel entrar em pânico segurando a trava; seguir
        // com o dado é melhor que derrubar o daemon inteiro.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Cria ou atualiza a sessão. Só sobrescreve o que veio preenchido, para o `SessionStart` de
    /// uma sessão que o bot criou não apagar o tópico e o tmux que o daemon já tinha gravado.
    pub fn upsert(&self, s: &Session) -> Result<()> {
        let c = self.conn();
        c.execute(
            "INSERT INTO sessions (session_id, project, cwd, transcript_path, tmux, topic_id, status, status_message_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
             ON CONFLICT(session_id) DO UPDATE SET
                project         = excluded.project,
                cwd             = excluded.cwd,
                transcript_path = COALESCE(excluded.transcript_path, sessions.transcript_path),
                tmux            = COALESCE(excluded.tmux, sessions.tmux),
                topic_id        = COALESCE(excluded.topic_id, sessions.topic_id),
                updated_at      = excluded.updated_at",
            params![
                s.session_id,
                s.project,
                s.cwd,
                s.transcript_path,
                s.tmux,
                s.topic_id,
                s.status,
                s.status_message_id,
                agora(),
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, session_id: &str) -> Result<Option<Session>> {
        let c = self.conn();
        let s = c
            .query_row(
                "SELECT session_id, project, cwd, transcript_path, tmux, topic_id, status, status_message_id, created_at, ended_at
                 FROM sessions WHERE session_id = ?1",
                [session_id],
                linha_para_sessao,
            )
            .optional()?;
        Ok(s)
    }

    /// Sessão viva (não encerrada) de um tópico. É o caminho de volta: chegou mensagem no tópico
    /// X, para qual sessão ela vai?
    pub fn by_topic(&self, topic_id: i32) -> Result<Option<Session>> {
        let c = self.conn();
        let s = c
            .query_row(
                "SELECT session_id, project, cwd, transcript_path, tmux, topic_id, status, status_message_id, created_at, ended_at
                 FROM sessions WHERE topic_id = ?1 AND ended_at IS NULL
                 ORDER BY created_at DESC LIMIT 1",
                [topic_id],
                linha_para_sessao,
            )
            .optional()?;
        Ok(s)
    }

    /// Sessão viva de um diretório, sem contar a que já conhecemos pelo id.
    ///
    /// Existe por causa do `/clear`: ele troca o id da sessão sem trocar o terminal nem o cwd, e
    /// foi exatamente isso que quebrou o sdispath (card duplicado, mensagem indo para o id
    /// velho). É por aqui que o `SessionStart` com `reason=clear` acha a sessão anterior para
    /// herdar o tópico dela.
    pub fn live_by_cwd(&self, cwd: &str, exceto: &str) -> Result<Option<Session>> {
        let c = self.conn();
        let s = c
            .query_row(
                "SELECT session_id, project, cwd, transcript_path, tmux, topic_id, status, status_message_id, created_at, ended_at
                 FROM sessions WHERE cwd = ?1 AND session_id <> ?2 AND ended_at IS NULL
                 ORDER BY created_at DESC LIMIT 1",
                params![cwd, exceto],
                linha_para_sessao,
            )
            .optional()?;
        Ok(s)
    }

    /// Passa tópico, tmux e mensagem de status da sessão velha para a nova (o caso do `/clear`),
    /// e encerra a velha. A fila pendente vai junto: mensagem que chegou antes do `/clear` ainda
    /// é para a mesma pessoa, no mesmo tópico.
    pub fn rekey(&self, antigo: &str, novo: &str) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE sessions SET
                topic_id          = (SELECT topic_id FROM sessions WHERE session_id = ?1),
                tmux              = COALESCE(tmux, (SELECT tmux FROM sessions WHERE session_id = ?1)),
                status_message_id = (SELECT status_message_id FROM sessions WHERE session_id = ?1),
                updated_at        = ?3
             WHERE session_id = ?2",
            params![antigo, novo, agora()],
        )?;
        tx.execute(
            "UPDATE sessions SET topic_id = NULL, ended_at = ?2 WHERE session_id = ?1",
            params![antigo, agora()],
        )?;
        tx.execute(
            "UPDATE queue SET session_id = ?2 WHERE session_id = ?1",
            params![antigo, novo],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_status(&self, session_id: &str, status: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET status = ?2, updated_at = ?3 WHERE session_id = ?1",
            params![session_id, status, agora()],
        )?;
        Ok(())
    }

    pub fn set_status_message(&self, session_id: &str, message_id: Option<i32>) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET status_message_id = ?2, updated_at = ?3 WHERE session_id = ?1",
            params![session_id, message_id, agora()],
        )?;
        Ok(())
    }

    pub fn set_transcript(&self, session_id: &str, transcript: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET transcript_path = ?2, updated_at = ?3 WHERE session_id = ?1",
            params![session_id, transcript, agora()],
        )?;
        Ok(())
    }

    pub fn end(&self, session_id: &str) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE sessions SET status = 'ended', ended_at = ?2, updated_at = ?2 WHERE session_id = ?1",
            params![session_id, agora()],
        )?;
        tx.execute("DELETE FROM queue WHERE session_id = ?1", [session_id])?;
        tx.commit()?;
        Ok(())
    }

    /// Sessões vivas, mais recente primeiro.
    pub fn live(&self) -> Result<Vec<Session>> {
        let c = self.conn();
        let mut stmt = c.prepare(
            "SELECT session_id, project, cwd, transcript_path, tmux, topic_id, status, status_message_id, created_at, ended_at
             FROM sessions WHERE ended_at IS NULL ORDER BY created_at DESC",
        )?;
        let linhas = stmt.query_map([], linha_para_sessao)?;
        Ok(linhas.flatten().collect())
    }

    pub fn enqueue(&self, session_id: &str, texto: &str, de: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO queue (session_id, text, from_name, at) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, texto, de, agora()],
        )?;
        Ok(())
    }

    /// Tira da fila tudo que estava guardado para a sessão. Usado quando um `Listen` abre: as
    /// mensagens que chegaram sem ninguém ouvindo são entregues antes de a espera começar.
    pub fn drain(&self, session_id: &str) -> Result<Vec<(String, String, i64)>> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        let itens: Vec<(String, String, i64)> = {
            let mut stmt = tx.prepare(
                "SELECT text, from_name, at FROM queue WHERE session_id = ?1 ORDER BY id",
            )?;
            let linhas = stmt.query_map([session_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            linhas.flatten().collect()
        };
        tx.execute("DELETE FROM queue WHERE session_id = ?1", [session_id])?;
        tx.commit()?;
        Ok(itens)
    }

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let c = self.conn();
        Ok(c.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

fn linha_para_sessao(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        session_id: row.get(0)?,
        project: row.get(1)?,
        cwd: row.get(2)?,
        transcript_path: row.get(3)?,
        tmux: row.get(4)?,
        topic_id: row.get(5)?,
        status: row.get(6)?,
        status_message_id: row.get(7)?,
        created_at: row.get(8)?,
        ended_at: row.get(9)?,
    })
}

impl Session {
    pub fn summary(&self, context: Option<crate::context::ContextUsage>) -> SessionSummary {
        SessionSummary {
            session_id: self.session_id.clone(),
            project: self.project.clone(),
            cwd: self.cwd.clone(),
            topic_id: self.topic_id,
            status: self.status.clone(),
            context_tokens: context.map(|c| c.tokens),
            context_limit: context.map(|c| c.limit),
            owned_by_bot: self.owned_by_bot(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sessao(id: &str) -> Session {
        Session {
            session_id: id.into(),
            project: "proj".into(),
            cwd: "/tmp/proj".into(),
            transcript_path: Some("/tmp/t.jsonl".into()),
            tmux: Some("ld-proj".into()),
            topic_id: Some(7),
            status: "idle".into(),
            status_message_id: None,
            created_at: 0,
            ended_at: None,
        }
    }

    #[test]
    fn upsert_e_get() {
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("s1")).unwrap();
        let s = st.get("s1").unwrap().unwrap();
        assert_eq!(s.topic_id, Some(7));
        assert!(s.owned_by_bot());
    }

    #[test]
    fn upsert_nao_apaga_topico_com_valor_nulo() {
        // O hook SessionStart não sabe o tópico; se ele sobrescrevesse com NULL, a sessão criada
        // pelo bot perderia o vínculo no primeiro evento.
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("s1")).unwrap();
        let mut sem_topico = sessao("s1");
        sem_topico.topic_id = None;
        sem_topico.tmux = None;
        st.upsert(&sem_topico).unwrap();
        let s = st.get("s1").unwrap().unwrap();
        assert_eq!(s.topic_id, Some(7));
        assert_eq!(s.tmux.as_deref(), Some("ld-proj"));
    }

    #[test]
    fn busca_por_topico_ignora_encerrada() {
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("s1")).unwrap();
        assert_eq!(st.by_topic(7).unwrap().unwrap().session_id, "s1");
        st.end("s1").unwrap();
        assert!(st.by_topic(7).unwrap().is_none());
    }

    #[test]
    fn rekey_do_clear_move_topico_e_fila() {
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("velha")).unwrap();
        st.enqueue("velha", "oi", "luka").unwrap();

        let mut nova = sessao("nova");
        nova.topic_id = None;
        nova.tmux = None;
        st.upsert(&nova).unwrap();
        st.rekey("velha", "nova").unwrap();

        let n = st.get("nova").unwrap().unwrap();
        assert_eq!(n.topic_id, Some(7), "o tópico foi para a sessão nova");
        assert_eq!(n.tmux.as_deref(), Some("ld-proj"));
        let v = st.get("velha").unwrap().unwrap();
        assert!(v.ended_at.is_some() && v.topic_id.is_none());
        assert_eq!(st.by_topic(7).unwrap().unwrap().session_id, "nova");
        assert_eq!(st.drain("nova").unwrap().len(), 1, "a fila seguiu junto");
    }

    #[test]
    fn fila_entrega_em_ordem_e_esvazia() {
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("s1")).unwrap();
        st.enqueue("s1", "um", "luka").unwrap();
        st.enqueue("s1", "dois", "luka").unwrap();
        let itens = st.drain("s1").unwrap();
        assert_eq!(
            itens.iter().map(|i| i.0.as_str()).collect::<Vec<_>>(),
            vec!["um", "dois"]
        );
        assert!(st.drain("s1").unwrap().is_empty());
    }

    #[test]
    fn live_by_cwd_acha_a_anterior() {
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("velha")).unwrap();
        let achada = st.live_by_cwd("/tmp/proj", "nova").unwrap().unwrap();
        assert_eq!(achada.session_id, "velha");
    }

    #[test]
    fn kv_guarda_o_id_do_painel() {
        let st = Store::open_memory().unwrap();
        assert!(st.kv_get("painel").unwrap().is_none());
        st.kv_set("painel", "42").unwrap();
        st.kv_set("painel", "43").unwrap();
        assert_eq!(st.kv_get("painel").unwrap().as_deref(), Some("43"));
    }
}
