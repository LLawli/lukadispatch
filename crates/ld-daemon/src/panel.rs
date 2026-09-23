//! O painel do canal principal: uma mensagem só, fixada, editada para sempre.
//!
//! O canal principal é o lugar do painel porque é o único que não some: no Telegram é o tópico
//! General, que a plataforma não deixa apagar, e tudo mais no grupo é transitório e vai embora
//! junto com a sessão.
//!
//! Mostra o que não dá para ver de dentro de um tópico: quanto de janela de contexto cada sessão
//! está gastando (inclusive as que você abriu no terminal, se a telemetria global estiver
//! instalada) e as duas janelas de limite da conta.
//!
//! Os tempos são relativos ("reseta em 2h13") de propósito. Hora absoluta exigiria descobrir o
//! fuso local, e `UtcOffset::current_local_offset` falha em programa com várias threads, que é
//! exatamente o caso aqui.

use std::sync::Arc;
use std::time::Duration;

use ld_core::state::Store;
use ld_core::usage::Window;
use tokio::sync::mpsc;
use tracing::warn;

use crate::agente::Agente;
use crate::frontend::formato::escapa as escape_html;
use crate::frontend::{Frontend, MsgId};

/// Piso entre duas edições. O painel reage a evento e também a um relógio, então sem isto ele
/// bateria no limite de edições da plataforma sozinho.
const DEBOUNCE: Duration = Duration::from_secs(3);

/// Chave do id da mensagem no banco: o painel precisa sobreviver a restart do daemon, senão cada
/// reinício deixaria um painel morto para trás e criaria outro.
const CHAVE: &str = "painel_message_id";

pub struct Panel {
    tx: mpsc::UnboundedSender<()>,
}

impl Panel {
    /// Sobe a tarefa do painel. Ela é a única dona da mensagem: ninguém mais edita.
    pub fn start(frontend: Arc<dyn Frontend>, store: Arc<Store>, agente: Arc<dyn Agente>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(tarefa(frontend, store, agente, rx));
        Self { tx }
    }

    /// Pede um redesenho. Não bloqueia e pode ser chamado à vontade: a tarefa junta os pedidos.
    pub fn refresh(&self) {
        let _ = self.tx.send(());
    }
}

async fn tarefa(
    frontend: Arc<dyn Frontend>,
    store: Arc<Store>,
    agente: Arc<dyn Agente>,
    mut rx: mpsc::UnboundedReceiver<()>,
) {
    let mut msg = store.kv_get(CHAVE).ok().flatten().map(MsgId::new);
    let mut ultimo = String::new();

    while rx.recv().await.is_some() {
        tokio::time::sleep(DEBOUNCE).await;
        while rx.try_recv().is_ok() {} // junta os pedidos que chegaram durante a espera

        let texto = match desenha(&store, agente.as_ref()) {
            Ok(t) => t,
            Err(e) => {
                warn!(erro = %e, "não consegui montar o painel");
                continue;
            }
        };
        if texto == ultimo {
            continue;
        }

        match &msg {
            Some(id) => {
                if frontend.edita(id, &texto, &[]).await.is_err() {
                    // Apagada na mão: manda outra e refixa.
                    msg = nova(&frontend, &store, &texto).await;
                }
            }
            None => msg = nova(&frontend, &store, &texto).await,
        }
        ultimo = texto;
    }
}

async fn nova(frontend: &Arc<dyn Frontend>, store: &Store, texto: &str) -> Option<MsgId> {
    let id = frontend.envia(None, texto, &[], None).await.ok()?;
    frontend.fixa(&id).await;
    let _ = store.kv_set(CHAVE, id.as_str());
    Some(id)
}

fn desenha(store: &Store, agente: &dyn Agente) -> anyhow::Result<String> {
    let mut s = String::from("📊 <b>lukadispatch</b>\n");

    let sessoes = store.live()?;
    if sessoes.is_empty() {
        s.push_str("\n<i>nenhuma sessão viva</i>\n");
    }
    for sessao in &sessoes {
        // Sessão que já estava aberta quando a telemetria foi instalada não passou pelo hook
        // SessionStart, então o modelo dela nunca foi gravado. O agente sabe: descobre uma vez
        // (lendo a conversa gravada) e guarda, para o painel não ficar incompleto para sempre.
        let modelo = match &sessao.model {
            None => {
                let achado = agente.modelo_da_sessao(sessao);
                if let Some(m) = &achado {
                    let _ = store.set_model(&sessao.session_id, Some(m), None);
                }
                achado
            }
            m => m.clone(),
        };
        let dono = if sessao.owned_by_bot() {
            "🤖"
        } else {
            "💻"
        };
        s.push_str(&format!(
            "\n{dono} <b>{}</b> · {}\n",
            escape_html(&sessao.project),
            escape_html(&sessao.status)
        ));

        let mut detalhe = Vec::new();
        if let Some(m) = &modelo {
            let esforco = sessao
                .effort
                .as_deref()
                .map(|e| format!("/{e}"))
                .unwrap_or_default();
            detalhe.push(format!("{}{esforco}", escape_html(m)));
        }
        // O modelo pode ter acabado de ser descoberto pela conversa gravada, acima: o que o
        // agente lê da sessão precisa refletir isso, senão o contexto some no primeiro desenho.
        let com_modelo = ld_core::state::Session {
            model: modelo.clone(),
            ..sessao.clone()
        };
        if let Some(ctx) = agente.contexto(&com_modelo) {
            detalhe.push(format!(
                "contexto {} / {} ({:.0}%)",
                milhares(ctx.tokens),
                milhares(ctx.limit),
                ctx.pct()
            ));
        }
        if let Some(t) = agente.tokens_da_sessao(&sessao.session_id) {
            detalhe.push(format!("{} tokens", milhares(t.total())));
        }
        if let Some(m) = &sessao.permission_mode {
            detalhe.push(escape_html(m));
        }
        if !detalhe.is_empty() {
            s.push_str(&format!("<code>{}</code>\n", detalhe.join(" · ")));
        }
    }

    let janelas = agente.uso();
    s.push_str("\n<b>Limites da conta</b>\n");
    s.push_str(&linha_janela("5h", janelas.five_hour));
    s.push_str(&linha_janela("7d", janelas.seven_day));
    if let Some(w) = janelas.five_hour.or(janelas.seven_day) {
        // Quem escreve esses números é o statusline do Claude Code, a cada ferramenta. Sem
        // sessão rodando, eles envelhecem, e o painel mentiria sem dizer desde quando.
        s.push_str(&format!(
            "\n<i>medido {}</i>",
            ha_quanto_tempo(agora() - w.updated_at)
        ));
    }
    Ok(s)
}

fn linha_janela(nome: &str, w: Option<Window>) -> String {
    match w {
        Some(w) => format!(
            "{nome}: {} {:.0}% · reseta em {}\n",
            barra(w.pct),
            w.pct,
            duracao(w.resets_at - agora())
        ),
        None => format!("{nome}: sem dado\n"),
    }
}

/// Barra de dez casas. No celular, ler "73%" num número é pior que ver a barra cheia.
fn barra(pct: f64) -> String {
    let cheias = ((pct / 10.0).round() as usize).min(10);
    format!("{}{}", "█".repeat(cheias), "░".repeat(10 - cheias))
}

fn milhares(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

fn duracao(segundos: i64) -> String {
    if segundos <= 0 {
        return "agora".into();
    }
    let h = segundos / 3600;
    let m = (segundos % 3600) / 60;
    if h > 24 {
        return format!("{}d{}h", h / 24, h % 24);
    }
    if h > 0 {
        return format!("{h}h{m:02}");
    }
    format!("{m}min")
}

fn ha_quanto_tempo(segundos: i64) -> String {
    if segundos < 120 {
        return "agora".into();
    }
    format!("há {}", duracao(segundos))
}

fn agora() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barra_reflete_a_porcentagem() {
        assert_eq!(barra(0.0), "░░░░░░░░░░");
        assert_eq!(barra(100.0), "██████████");
        assert_eq!(barra(51.0), "█████░░░░░");
        assert_eq!(barra(999.0), "██████████", "não pode transbordar");
    }

    #[test]
    fn milhares_encurta_sem_mentir() {
        assert_eq!(milhares(999), "999");
        assert_eq!(milhares(117_405), "117k");
        assert_eq!(milhares(1_200_000), "1.2M");
    }

    #[test]
    fn duracao_em_linguagem_de_gente() {
        assert_eq!(duracao(-5), "agora");
        assert_eq!(duracao(90), "1min");
        assert_eq!(duracao(3 * 3600 + 5 * 60), "3h05");
        assert_eq!(duracao(50 * 3600), "2d2h");
    }

    #[test]
    fn janela_sem_dado_nao_finge_zero() {
        // Banco ausente é diferente de 0% usado, e o painel não pode confundir os dois.
        assert_eq!(linha_janela("5h", None), "5h: sem dado\n");
    }

    #[test]
    fn painel_sem_sessao_ainda_desenha() {
        let store = Store::open_memory().unwrap();
        let raiz = tempfile::tempdir().unwrap();
        let agente = crate::agente::claude_code::ClaudeCode::new(
            crate::agente::claude_code::Locais {
                cli: "/opt/ld/lukadispatch".into(),
                mcp_proxy: "/opt/ld/lukadispatch-mcp".into(),
                settings: raiz.path().join("bot-settings.json"),
                claude_json: raiz.path().join("claude.json"),
                claude_dir: raiz.path().join("claude"),
                uso_db: raiz.path().join("uso.db"),
            },
            None,
        );
        let t = desenha(&store, &agente).unwrap();
        assert!(t.contains("nenhuma sessão viva"));
        assert!(t.contains("Limites da conta"));
    }
}
