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

/// Uma mensagem que esperou na fila porque a sessão estava sem monitor armado.
#[derive(Debug, Clone, PartialEq)]
pub struct Guardada {
    pub text: String,
    pub from: String,
    pub at: i64,
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub session_id: String,
    pub project: String,
    pub cwd: String,
    pub transcript_path: Option<String>,
    /// Id opaco da sessão no hospedeiro (o nome da sessão tmux, o terminal do herdr): só o
    /// hospedeiro que a subiu sabe o que fazer com o valor. `None` em sessão que o usuário abriu
    /// no terminal: ela entra no painel, mas o bot não a controla.
    pub hospedagem: Option<String>,
    /// Id opaco do canal no frontend em uso: um tópico do Telegram (`"630"`) ou um grupo do
    /// WhatsApp (`"120363...@g.us"`). Só o adaptador do frontend sabe o que fazer com o valor;
    /// aqui é só texto.
    pub canal_id: Option<String>,
    pub status: String,
    /// Id opaco da mensagem de status no frontend em uso. Mesmo motivo do `canal_id`: cada
    /// frontend tem seu próprio formato de id de mensagem.
    pub status_msg_id: Option<String>,
    /// Modelo e esforço em vigor. Vêm do hook `SessionStart`, da troca pedida no Telegram ou do
    /// `PostModelSwitch` (quando você troca pelo `/model` no teclado do PC).
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Modo de permissão em vigor. Vem da config na criação e muda por `/mode` no Telegram.
    pub permission_mode: Option<String>,
    pub created_at: i64,
    pub ended_at: Option<i64>,
}

impl Session {
    pub fn owned_by_bot(&self) -> bool {
        self.hospedagem.is_some()
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
    hospedagem        TEXT,
    -- Nomes de coluna herdados de quando o único frontend era o Telegram e os ids eram inteiros
    -- dele. Ficam TEXT desde que o frontend virou trocável (id opaco de qualquer adaptador), mas
    -- o NOME não muda: renomear pediria recriar a tabela, e isso não compra nada. Um banco criado
    -- antes desta mudança ainda tem a coluna como INTEGER; a leitura trata os dois casos (veja
    -- `coluna_como_texto`).
    topic_id          TEXT,
    status            TEXT NOT NULL DEFAULT 'idle',
    status_message_id TEXT,
    model             TEXT,
    effort            TEXT,
    permission_mode   TEXT,
    -- 1 enquanto há um pedido seu esperando resposta. Fica no banco, e não na memória do
    -- daemon, porque um restart no meio de um turno faria a resposta ser descartada em silêncio.
    pedido            INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    ended_at          INTEGER
);
CREATE TABLE IF NOT EXISTS queue (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    text       TEXT NOT NULL,
    from_name  TEXT NOT NULL,
    at         INTEGER NOT NULL,
    -- Caminhos dos anexos já baixados, em JSON. NULL é o caso comum (mensagem de texto).
    files      TEXT
);
CREATE INDEX IF NOT EXISTS queue_por_sessao ON queue(session_id, id);
CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#;

/// Colunas acrescentadas depois que bancos já existiam por aí.
///
/// `CREATE TABLE IF NOT EXISTS` não altera tabela que já existe, então um banco criado antes
/// destas colunas ficaria sem elas. O erro de coluna duplicada é o caminho normal aqui (banco
/// novo já nasce com tudo), por isso ele é ignorado em silêncio.
fn migra(conn: &Connection) {
    for coluna in ["model", "effort", "permission_mode"] {
        let _ = conn.execute(
            &format!("ALTER TABLE sessions ADD COLUMN {coluna} TEXT"),
            [],
        );
    }
    let _ = conn.execute("ALTER TABLE queue ADD COLUMN files TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE sessions ADD COLUMN pedido INTEGER NOT NULL DEFAULT 0",
        [],
    );
    // A coluna nasceu `tmux`, quando o tmux era o único hospedeiro. O `RENAME COLUMN` não recria
    // a tabela, e o SQLite embutido (`bundled`) sempre o tem; em banco novo a coluna velha não
    // existe e o erro é o caminho normal, como acima. Veja a decisão 0014.
    let _ = conn.execute("ALTER TABLE sessions RENAME COLUMN tmux TO hospedagem", []);
}

fn agora() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Store {
    pub fn open(caminho: &Path) -> Result<Self> {
        if let Some(pai) = caminho.parent() {
            std::fs::create_dir_all(pai).with_context(|| format!("criando {}", pai.display()))?;
        }
        let conn = Connection::open(caminho)
            .with_context(|| format!("abrindo estado em {}", caminho.display()))?;
        conn.execute_batch(ESQUEMA).context("criando esquema")?;
        migra(&conn);
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
    /// uma sessão que o bot criou não apagar o tópico e a hospedagem que o daemon já tinha gravado.
    pub fn upsert(&self, s: &Session) -> Result<()> {
        let c = self.conn();
        c.execute(
            "INSERT INTO sessions (session_id, project, cwd, transcript_path, hospedagem, topic_id, status, status_message_id, model, effort, permission_mode, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)
             ON CONFLICT(session_id) DO UPDATE SET
                project         = excluded.project,
                cwd             = excluded.cwd,
                transcript_path = COALESCE(excluded.transcript_path, sessions.transcript_path),
                hospedagem      = COALESCE(excluded.hospedagem, sessions.hospedagem),
                topic_id        = COALESCE(excluded.topic_id, sessions.topic_id),
                -- (nome de coluna herdado; guarda o canal_id, veja o comentário no esquema)
                model           = COALESCE(excluded.model, sessions.model),
                effort          = COALESCE(excluded.effort, sessions.effort),
                permission_mode = COALESCE(excluded.permission_mode, sessions.permission_mode),
                status          = excluded.status,
                -- Retomar uma conversa reusa o id da sessão anterior, que estava encerrada. Sem
                -- limpar isto aqui, ela voltaria viva no hospedeiro e morta no banco: sem tópico, sem
                -- painel e sem ninguém para entregar mensagem.
                ended_at        = NULL,
                updated_at      = excluded.updated_at",
            params![
                s.session_id,
                s.project,
                s.cwd,
                s.transcript_path,
                s.hospedagem,
                s.canal_id,
                s.status,
                s.status_msg_id,
                s.model,
                s.effort,
                s.permission_mode,
                agora(),
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, session_id: &str) -> Result<Option<Session>> {
        let c = self.conn();
        let s = c
            .query_row(
                "SELECT session_id, project, cwd, transcript_path, hospedagem, topic_id, status, status_message_id, model, effort, permission_mode, created_at, ended_at
                 FROM sessions WHERE session_id = ?1",
                [session_id],
                linha_para_sessao,
            )
            .optional()?;
        Ok(s)
    }

    /// Sessão viva (não encerrada) de um canal. É o caminho de volta: chegou mensagem no canal
    /// X, para qual sessão ela vai?
    ///
    /// O `CAST` é por causa do banco antigo: a coluna nasceu INTEGER (id do Telegram), e a
    /// afinidade dela pode converter o parâmetro texto de volta para número na comparação. O
    /// `CAST` torna a busca por texto confiável nos dois casos, sem depender de como a
    /// afinidade decide converter.
    pub fn by_canal(&self, canal_id: &str) -> Result<Option<Session>> {
        let c = self.conn();
        let s = c
            .query_row(
                "SELECT session_id, project, cwd, transcript_path, hospedagem, topic_id, status, status_message_id, model, effort, permission_mode, created_at, ended_at
                 FROM sessions WHERE CAST(topic_id AS TEXT) = ?1 AND ended_at IS NULL
                 ORDER BY created_at DESC LIMIT 1",
                [canal_id],
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
                "SELECT session_id, project, cwd, transcript_path, hospedagem, topic_id, status, status_message_id, model, effort, permission_mode, created_at, ended_at
                 FROM sessions WHERE cwd = ?1 AND session_id <> ?2 AND ended_at IS NULL
                 ORDER BY created_at DESC LIMIT 1",
                params![cwd, exceto],
                linha_para_sessao,
            )
            .optional()?;
        Ok(s)
    }

    /// Passa tópico, hospedagem e mensagem de status da sessão velha para a nova (o caso do `/clear`),
    /// e encerra a velha. A fila pendente vai junto: mensagem que chegou antes do `/clear` ainda
    /// é para a mesma pessoa, no mesmo tópico.
    pub fn rekey(&self, antigo: &str, novo: &str) -> Result<()> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE sessions SET
                topic_id          = (SELECT topic_id FROM sessions WHERE session_id = ?1),
                hospedagem        = COALESCE(hospedagem, (SELECT hospedagem FROM sessions WHERE session_id = ?1)),
                status_message_id = (SELECT status_message_id FROM sessions WHERE session_id = ?1),
                -- O pedido em aberto é da conversa, não do id: um /clear no meio dele não pode
                -- fazer a resposta ser descartada.
                pedido            = (SELECT pedido FROM sessions WHERE session_id = ?1),
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

    /// Marca que alguém pediu alguma coisa a esta sessão, e que a resposta do próximo turno é
    /// para ir ao Telegram.
    pub fn marca_pedido(&self, session_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET pedido = 1, updated_at = ?2 WHERE session_id = ?1",
            params![session_id, agora()],
        )?;
        Ok(())
    }

    /// Consome a marca: devolve `true` uma vez só, no `Stop` daquele turno.
    pub fn tira_pedido(&self, session_id: &str) -> Result<bool> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        let tinha: i64 = tx
            .query_row(
                "SELECT pedido FROM sessions WHERE session_id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        tx.execute(
            "UPDATE sessions SET pedido = 0 WHERE session_id = ?1",
            [session_id],
        )?;
        tx.commit()?;
        Ok(tinha != 0)
    }

    pub fn set_status(&self, session_id: &str, status: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET status = ?2, updated_at = ?3 WHERE session_id = ?1",
            params![session_id, status, agora()],
        )?;
        Ok(())
    }

    pub fn set_status_msg(&self, session_id: &str, msg_id: Option<&str>) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET status_message_id = ?2, updated_at = ?3 WHERE session_id = ?1",
            params![session_id, msg_id, agora()],
        )?;
        Ok(())
    }

    /// Grava o modelo e o esforço em vigor. `None` não apaga o que já estava.
    pub fn set_model(
        &self,
        session_id: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET
                model      = COALESCE(?2, model),
                effort     = COALESCE(?3, effort),
                updated_at = ?4
             WHERE session_id = ?1",
            params![session_id, model, effort, agora()],
        )?;
        Ok(())
    }

    pub fn set_permission_mode(&self, session_id: &str, modo: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET permission_mode = ?2, updated_at = ?3 WHERE session_id = ?1",
            params![session_id, modo, agora()],
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
            "SELECT session_id, project, cwd, transcript_path, hospedagem, topic_id, status, status_message_id, model, effort, permission_mode, created_at, ended_at
             FROM sessions WHERE ended_at IS NULL ORDER BY created_at DESC",
        )?;
        let linhas = stmt.query_map([], linha_para_sessao)?;
        Ok(linhas.flatten().collect())
    }

    /// As sessões que um id digitado pode querer dizer.
    ///
    /// O `lukadispatch ls` mostra só o começo do id, e é esse começo que você digita depois no
    /// `send` e no `kill`. O id exato vale sempre, encerrada ou não. Um prefixo só casa com
    /// sessão viva: mandar mensagem ou matar uma sessão morta pelo começo do id nunca é o que
    /// se quis. Mais de um resultado é ambiguidade, e quem chama decide o que dizer.
    pub fn ids_por_prefixo(&self, id: &str) -> Result<Vec<String>> {
        let c = self.conn();
        let exato: Option<String> = c
            .query_row(
                "SELECT session_id FROM sessions WHERE session_id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(e) = exato {
            return Ok(vec![e]);
        }
        // `substr` e não `LIKE`: o id vem do usuário, e `%` ou `_` nele virariam curinga.
        let mut stmt = c.prepare(
            "SELECT session_id FROM sessions
             WHERE ended_at IS NULL AND substr(session_id, 1, length(?1)) = ?1
             ORDER BY created_at DESC",
        )?;
        let linhas = stmt.query_map([id], |r| r.get::<_, String>(0))?;
        Ok(linhas.flatten().collect())
    }

    pub fn enqueue(&self, session_id: &str, texto: &str, de: &str, files: &[String]) -> Result<()> {
        // Lista vazia vira NULL em vez de "[]": o caso comum é mensagem sem anexo, e assim a
        // coluna nova não muda nada para quem só manda texto.
        let files = if files.is_empty() {
            None
        } else {
            Some(serde_json::to_string(files)?)
        };
        self.conn().execute(
            "INSERT INTO queue (session_id, text, from_name, at, files) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, texto, de, agora(), files],
        )?;
        Ok(())
    }

    /// Tira da fila tudo que estava guardado para a sessão. Usado quando um `Listen` abre: as
    /// mensagens que chegaram sem ninguém ouvindo são entregues antes de a espera começar.
    pub fn drain(&self, session_id: &str) -> Result<Vec<Guardada>> {
        let mut c = self.conn();
        let tx = c.transaction()?;
        let itens: Vec<Guardada> = {
            let mut stmt = tx.prepare(
                "SELECT text, from_name, at, files FROM queue WHERE session_id = ?1 ORDER BY id",
            )?;
            let linhas = stmt.query_map([session_id], |r| {
                let files: Option<String> = r.get(3)?;
                Ok(Guardada {
                    text: r.get(0)?,
                    from: r.get(1)?,
                    at: r.get(2)?,
                    // JSON corrompido na coluna não pode derrubar a entrega da mensagem: o
                    // texto ainda vale, e o anexo perdido aparece como ausência de caminho.
                    files: files
                        .and_then(|j| serde_json::from_str(&j).ok())
                        .unwrap_or_default(),
                })
            })?;
            linhas.flatten().collect()
        };
        tx.execute("DELETE FROM queue WHERE session_id = ?1", [session_id])?;
        tx.commit()?;
        Ok(itens)
    }

    /// Sessões encerradas que ainda carregam tópico: o tópico não foi apagado (daemon caiu no
    /// meio, API fora do ar) e virou um canal morto no grupo.
    pub fn canais_vazados(&self) -> Result<Vec<(String, String)>> {
        let c = self.conn();
        let mut stmt = c.prepare(
            "SELECT session_id, topic_id FROM sessions
             WHERE ended_at IS NOT NULL AND topic_id IS NOT NULL",
        )?;
        let linhas = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, coluna_como_texto(r, 1)?))
        })?;
        Ok(linhas
            .flatten()
            .filter_map(|(id, canal)| canal.map(|c| (id, c)))
            .collect())
    }

    /// Esquece o canal de uma sessão. Chamado depois de apagá-lo de verdade, para a varredura
    /// de canal vazado saber o que já foi resolvido.
    pub fn clear_canal(&self, session_id: &str) -> Result<()> {
        self.conn().execute(
            "UPDATE sessions SET topic_id = NULL, updated_at = ?2 WHERE session_id = ?1",
            params![session_id, agora()],
        )?;
        Ok(())
    }

    /// `true` quando esta hospedagem é de uma sessão que já foi encerrada.
    ///
    /// Serve para varrer sessão órfã no hospedeiro: um relançamento interrompido no meio pode
    /// deixar o processo vivo com a sessão já morta no banco, e aí ele é lixo que ninguém mais
    /// alcança.
    pub fn hospedagem_de_sessao_morta(&self, hospedagem: &str) -> Result<bool> {
        let c = self.conn();
        let achou: Option<i64> = c
            .query_row(
                "SELECT 1 FROM sessions WHERE hospedagem = ?1 AND ended_at IS NOT NULL LIMIT 1",
                [hospedagem],
                |r| r.get(0),
            )
            .optional()?;
        Ok(achou.is_some())
    }

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        let c = self.conn();
        Ok(
            c.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
                .optional()?,
        )
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

/// Lê `topic_id`/`status_message_id` como texto seja qual for o tipo real gravado na coluna.
///
/// Num banco criado antes desta mudança as duas colunas são INTEGER (id do Telegram). Num banco
/// novo elas nascem TEXT. `rusqlite::types::ValueRef` cobre os dois casos sem exigir migração de
/// tabela: inteiro vira `to_string()`, texto vem como está, nulo vira `None`.
fn coluna_como_texto(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Option<String>> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(idx)? {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        // Não esperado nestas colunas, mas um tipo estranho não pode travar a leitura da sessão.
        ValueRef::Real(_) | ValueRef::Blob(_) => None,
    })
}

fn linha_para_sessao(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        session_id: row.get(0)?,
        project: row.get(1)?,
        cwd: row.get(2)?,
        transcript_path: row.get(3)?,
        hospedagem: row.get(4)?,
        canal_id: coluna_como_texto(row, 5)?,
        status: row.get(6)?,
        status_msg_id: coluna_como_texto(row, 7)?,
        model: row.get(8)?,
        effort: row.get(9)?,
        permission_mode: row.get(10)?,
        created_at: row.get(11)?,
        ended_at: row.get(12)?,
    })
}

impl Session {
    pub fn summary(&self, context: Option<crate::context::ContextUsage>) -> SessionSummary {
        SessionSummary {
            session_id: self.session_id.clone(),
            project: self.project.clone(),
            cwd: self.cwd.clone(),
            canal_id: self.canal_id.clone(),
            status: self.status.clone(),
            context_tokens: context.map(|c| c.tokens),
            context_limit: context.map(|c| c.limit),
            owned_by_bot: self.owned_by_bot(),
            model: self.model.clone(),
            effort: self.effort.clone(),
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
            hospedagem: Some("ld-proj".into()),
            canal_id: Some("7".into()),
            status: "idle".into(),
            status_msg_id: None,
            model: Some("opus".into()),
            effort: None,
            permission_mode: Some("auto".into()),
            created_at: 0,
            ended_at: None,
        }
    }

    #[test]
    fn prefixo_do_id_acha_so_a_sessao_viva() {
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("379a86dc-afb4-viva")).unwrap();
        st.upsert(&sessao("d428b688-5-morta")).unwrap();
        st.end("d428b688-5-morta").unwrap();
        st.upsert(&sessao("abcd0000-um")).unwrap();
        st.upsert(&sessao("abcd1111-dois")).unwrap();

        // O que o `ls` mostra resolve para o id inteiro.
        assert_eq!(
            st.ids_por_prefixo("379a86dc").unwrap(),
            ["379a86dc-afb4-viva"]
        );
        // O id exato vale mesmo encerrado; o prefixo, não.
        assert_eq!(
            st.ids_por_prefixo("d428b688-5-morta").unwrap(),
            ["d428b688-5-morta"]
        );
        assert!(st.ids_por_prefixo("d428b688").unwrap().is_empty());
        // Ambíguo volta os dois, e quem chama decide.
        assert_eq!(st.ids_por_prefixo("abcd").unwrap().len(), 2);
        assert!(st.ids_por_prefixo("ffff").unwrap().is_empty());
        // O id vem do usuário: curinga de LIKE não pode casar com tudo.
        assert!(st.ids_por_prefixo("%").unwrap().is_empty());
        assert!(st.ids_por_prefixo("____").unwrap().is_empty());
    }

    #[test]
    fn upsert_e_get() {
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("s1")).unwrap();
        let s = st.get("s1").unwrap().unwrap();
        assert_eq!(s.canal_id.as_deref(), Some("7"));
        assert!(s.owned_by_bot());
    }

    #[test]
    fn upsert_nao_apaga_topico_com_valor_nulo() {
        // O hook SessionStart não sabe o canal; se ele sobrescrevesse com NULL, a sessão criada
        // pelo bot perderia o vínculo no primeiro evento.
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("s1")).unwrap();
        let mut sem_topico = sessao("s1");
        sem_topico.canal_id = None;
        sem_topico.hospedagem = None;
        st.upsert(&sem_topico).unwrap();
        let s = st.get("s1").unwrap().unwrap();
        assert_eq!(s.canal_id.as_deref(), Some("7"));
        assert_eq!(s.hospedagem.as_deref(), Some("ld-proj"));
    }

    #[test]
    fn retomar_uma_sessao_encerrada_a_traz_de_volta() {
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("s1")).unwrap();
        st.end("s1").unwrap();
        assert!(st.live().unwrap().is_empty());

        st.upsert(&sessao("s1")).unwrap();
        let viva = st.get("s1").unwrap().unwrap();
        assert!(viva.ended_at.is_none(), "a sessão precisa voltar viva");
        assert_eq!(st.live().unwrap().len(), 1);
    }

    #[test]
    fn busca_por_topico_ignora_encerrada() {
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("s1")).unwrap();
        assert_eq!(st.by_canal("7").unwrap().unwrap().session_id, "s1");
        st.end("s1").unwrap();
        assert!(st.by_canal("7").unwrap().is_none());
    }

    #[test]
    fn rekey_do_clear_move_topico_e_fila() {
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("velha")).unwrap();
        st.enqueue("velha", "oi", "luka", &[]).unwrap();

        let mut nova = sessao("nova");
        nova.canal_id = None;
        nova.hospedagem = None;
        st.upsert(&nova).unwrap();
        st.rekey("velha", "nova").unwrap();

        let n = st.get("nova").unwrap().unwrap();
        assert_eq!(
            n.canal_id.as_deref(),
            Some("7"),
            "o tópico foi para a sessão nova"
        );
        assert_eq!(n.hospedagem.as_deref(), Some("ld-proj"));
        let v = st.get("velha").unwrap().unwrap();
        assert!(v.ended_at.is_some() && v.canal_id.is_none());
        assert_eq!(st.by_canal("7").unwrap().unwrap().session_id, "nova");
        assert_eq!(st.drain("nova").unwrap().len(), 1, "a fila seguiu junto");
    }

    #[test]
    fn topico_vazado_aparece_ate_ser_limpo() {
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("s1")).unwrap();
        assert!(
            st.canais_vazados().unwrap().is_empty(),
            "sessão viva não vaza"
        );
        st.end("s1").unwrap();
        assert_eq!(
            st.canais_vazados().unwrap(),
            vec![("s1".to_string(), "7".to_string())]
        );
        st.clear_canal("s1").unwrap();
        assert!(st.canais_vazados().unwrap().is_empty());
    }

    #[test]
    fn pedido_sobrevive_e_e_consumido_uma_vez_so() {
        // O que este teste trava: com a marca só na memória do daemon, um `systemctl restart`
        // no meio de um turno fazia a resposta ser descartada em silêncio.
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("s1")).unwrap();
        assert!(!st.tira_pedido("s1").unwrap(), "sessão nova não tem pedido");

        st.marca_pedido("s1").unwrap();
        assert!(st.tira_pedido("s1").unwrap());
        assert!(!st.tira_pedido("s1").unwrap(), "consumir é uma vez só");
    }

    #[test]
    fn fila_entrega_em_ordem_e_esvazia() {
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("s1")).unwrap();
        st.enqueue("s1", "um", "luka", &[]).unwrap();
        st.enqueue("s1", "dois", "luka", &[]).unwrap();
        let itens = st.drain("s1").unwrap();
        assert_eq!(
            itens.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
            vec!["um", "dois"]
        );
        assert!(st.drain("s1").unwrap().is_empty());
    }

    #[test]
    fn fila_guarda_o_caminho_do_anexo() {
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("s1")).unwrap();
        st.enqueue(
            "s1",
            "[arquivo recebido: nota.pdf]",
            "luka",
            &["/data/nota.pdf".to_string()],
        )
        .unwrap();
        let itens = st.drain("s1").unwrap();
        assert_eq!(itens[0].files, vec!["/data/nota.pdf".to_string()]);
    }

    #[test]
    fn fila_sem_anexo_volta_com_lista_vazia() {
        // A coluna nasceu depois, então a linha antiga tem NULL ali: isso não pode virar erro.
        let st = Store::open_memory().unwrap();
        st.upsert(&sessao("s1")).unwrap();
        st.enqueue("s1", "oi", "luka", &[]).unwrap();
        assert!(st.drain("s1").unwrap()[0].files.is_empty());
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

#[cfg(test)]
#[path = "state_canal_testes.rs"]
mod canal_testes;
