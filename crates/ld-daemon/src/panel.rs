//! O painel do tópico General: uma mensagem só, fixada, editada para sempre.
//!
//! O General é o único tópico que o Telegram não deixa apagar, e é por isso que ele é o lugar do
//! painel: tudo mais nesse grupo é transitório e some junto com a sessão.
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
use ld_core::usage::{self, Window};
use ld_core::{context, paths};
use teloxide::types::MessageId;
use tokio::sync::mpsc;
use tracing::warn;

use crate::telegram::{Tg, escape_html};

/// Piso entre duas edições. O painel reage a evento e também a um relógio, então sem isto ele
/// bateria no rate limit do Telegram sozinho.
const DEBOUNCE: Duration = Duration::from_secs(3);

/// Chave do id da mensagem no banco: o painel precisa sobreviver a restart do daemon, senão cada
/// reinício deixaria um painel morto para trás e criaria outro.
const CHAVE: &str = "painel_message_id";

pub struct Panel {
    tx: mpsc::UnboundedSender<()>,
}

impl Panel {
    /// Sobe a tarefa do painel. Ela é a única dona da mensagem: ninguém mais edita.
    pub fn start(tg: Tg, store: Arc<Store>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(tarefa(tg, store, rx));
        Self { tx }
    }

    /// Pede um redesenho. Não bloqueia e pode ser chamado à vontade: a tarefa junta os pedidos.
    pub fn refresh(&self) {
        let _ = self.tx.send(());
    }
}

async fn tarefa(tg: Tg, store: Arc<Store>, mut rx: mpsc::UnboundedReceiver<()>) {
    let mut msg = store
        .kv_get(CHAVE)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i32>().ok())
        .map(MessageId);
    let mut ultimo = String::new();

    while rx.recv().await.is_some() {
        tokio::time::sleep(DEBOUNCE).await;
        while rx.try_recv().is_ok() {} // junta os pedidos que chegaram durante a espera

        let texto = match desenha(&store) {
            Ok(t) => t,
            Err(e) => {
                warn!(erro = %e, "não consegui montar o painel");
                continue;
            }
        };
        if texto == ultimo {
            continue;
        }

        match msg {
            Some(id) => {
                if tg.edit_html(id, &texto).await.is_err() {
                    // Apagada na mão: manda outra e refixa.
                    msg = nova(&tg, &store, &texto).await;
                }
            }
            None => msg = nova(&tg, &store, &texto).await,
        }
        ultimo = texto;
    }
}

async fn nova(tg: &Tg, store: &Store, texto: &str) -> Option<MessageId> {
    let id = tg.send_html(None, texto).await.ok()?;
    tg.pin(id).await;
    let _ = store.kv_set(CHAVE, &id.0.to_string());
    Some(id)
}

fn desenha(store: &Store) -> anyhow::Result<String> {
    let db = paths::usage_db();
    let mut s = String::from("📊 <b>lukadispatch</b>\n");

    let sessoes = store.live()?;
    if sessoes.is_empty() {
        s.push_str("\n<i>nenhuma sessão viva</i>\n");
    }
    for sessao in &sessoes {
        // Sessão que já estava aberta quando a telemetria foi instalada não passou pelo hook
        // SessionStart, então o modelo dela nunca foi gravado. O transcript sabe: descobre uma
        // vez e guarda, para o painel não ficar incompleto para sempre.
        let modelo = match (&sessao.model, &sessao.transcript_path) {
            (None, Some(caminho)) => {
                let achado = context::model_from_transcript(std::path::Path::new(caminho));
                if let Some(m) = &achado {
                    let _ = store.set_model(&sessao.session_id, Some(m), None);
                }
                achado
            }
            (m, _) => m.clone(),
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
        if let Some(ctx) = sessao
            .transcript_path
            .as_deref()
            .map(std::path::Path::new)
            .and_then(|p| context::read_with_model(p, modelo.as_deref()))
        {
            detalhe.push(format!(
                "contexto {} / {} ({:.0}%)",
                milhares(ctx.tokens),
                milhares(ctx.limit),
                ctx.pct()
            ));
        }
        if let Some(t) = usage::session_tokens(&db, &sessao.session_id) {
            detalhe.push(format!("{} tokens", milhares(t.total())));
        }
        if let Some(m) = &sessao.permission_mode {
            detalhe.push(escape_html(m));
        }
        if !detalhe.is_empty() {
            s.push_str(&format!("<code>{}</code>\n", detalhe.join(" · ")));
        }
    }

    let janelas = usage::windows(&db);
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
        let t = desenha(&store).unwrap();
        assert!(t.contains("nenhuma sessão viva"));
        assert!(t.contains("Limites da conta"));
    }
}
