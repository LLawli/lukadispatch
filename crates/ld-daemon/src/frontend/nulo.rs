//! O frontend do modo offline (`LUKADISPATCH_OFFLINE=1`): aceita tudo e não manda nada.
//!
//! Serve para exercitar o sistema inteiro (tmux, hooks, Monitor, fila) numa máquina sem bot.
//! Antes ele era um `bool` espalhado pelo daemon ("se offline, não cria tópico"); como
//! implementação da trait, o resto do código nem sabe que está offline.
//!
//! Os canais que ele cria são de mentira mas únicos (`nulo-<uuid>`), para a sessão ter com o
//! que se identificar. Se o daemon voltar a subir com um frontend de verdade, esses ids aparecem
//! como canal vazado e o adaptador real responde que não existem, o que os limpa do banco.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, bail};
use async_trait::async_trait;
use tokio::sync::mpsc;
use uuid::Uuid;

use super::{Anexo, Botao, Canal, Evento, Frontend, Limites, Midia, MsgId, Resolvido};

#[derive(Default)]
pub struct Nulo {
    /// Contador de mensagem; canal usa um uuid (ver [`Frontend::cria_canal`]) porque duas
    /// sessões nunca podem dividir um canal, nem por coincidência de contagem.
    msg_n: AtomicU64,
}

#[async_trait]
impl Frontend for Nulo {
    fn nome(&self) -> &'static str {
        "nulo"
    }

    fn limites(&self) -> Limites {
        Limites::default()
    }

    async fn confere(&self) -> Result<String> {
        Ok("offline".to_string())
    }

    async fn escuta(&self, _saida: mpsc::UnboundedSender<Evento>) {
        // Sem plataforma, não há o que ouvir. Voltar daqui faria o daemon achar que o laço de
        // entrada morreu; ficar parado para sempre é o comportamento certo.
        std::future::pending().await
    }

    async fn cria_canal(&self, nome: &str) -> Result<Canal> {
        tracing::debug!(nome, "nulo: criando canal de mentira");
        Ok(Canal::new(format!("nulo-{}", Uuid::new_v4())))
    }

    async fn apaga_canal(&self, canal: &Canal) -> Resolvido {
        tracing::debug!(%canal, "nulo: apagando canal de mentira");
        Resolvido::Apagado
    }

    async fn envia_texto(&self, canal: Option<&Canal>, texto: &str) -> Result<MsgId> {
        tracing::debug!(?canal, texto, "nulo: envia_texto");
        Ok(self.proxima_msg())
    }

    async fn envia(
        &self,
        canal: Option<&Canal>,
        rico: &str,
        botoes: &[Botao],
        responde_a: Option<&MsgId>,
    ) -> Result<MsgId> {
        tracing::debug!(?canal, rico, ?botoes, ?responde_a, "nulo: envia");
        Ok(self.proxima_msg())
    }

    async fn edita(&self, msg: &MsgId, rico: &str, botoes: &[Botao]) -> Result<()> {
        tracing::debug!(%msg, rico, ?botoes, "nulo: edita");
        Ok(())
    }

    async fn apaga(&self, msg: &MsgId) {
        tracing::debug!(%msg, "nulo: apaga");
    }

    async fn fixa(&self, msg: &MsgId) {
        tracing::debug!(%msg, "nulo: fixa");
    }

    async fn envia_arquivo(
        &self,
        canal: Option<&Canal>,
        caminho: &Path,
        legenda: Option<&str>,
        como: Midia,
    ) -> Result<MsgId> {
        tracing::debug!(?canal, ?caminho, legenda, ?como, "nulo: envia_arquivo");
        Ok(self.proxima_msg())
    }

    async fn baixa(&self, anexo: &Anexo, _dir: &Path) -> Result<PathBuf> {
        let _ = anexo;
        bail!("o frontend nulo não recebe anexo: não há nada para baixar")
    }
}

impl Nulo {
    /// Próximo id de mensagem (`nulo-1`, `nulo-2`, ...).
    fn proxima_msg(&self) -> MsgId {
        let n = self.msg_n.fetch_add(1, Ordering::Relaxed) + 1;
        MsgId::new(format!("nulo-{n}"))
    }
}

#[cfg(test)]
mod testes {
    //! O contrato do modo offline: tudo dá certo e nada sai daqui.

    use super::*;
    use crate::frontend::TipoAnexo;

    #[tokio::test]
    async fn canais_sao_unicos_e_marcados_como_nulos() {
        let n = Nulo::default();
        let a = n.cria_canal("proj").await.unwrap();
        let b = n.cria_canal("proj").await.unwrap();
        assert_ne!(a, b, "duas sessões não podem dividir um canal");
        assert!(a.as_str().starts_with("nulo-"), "{a}");
    }

    #[tokio::test]
    async fn envio_da_certo_e_nao_vai_a_lugar_nenhum() {
        let n = Nulo::default();
        let c = n.cria_canal("proj").await.unwrap();
        let a = n.envia_texto(Some(&c), "oi").await.unwrap();
        let b = n
            .envia(None, "<b>x</b>", &[Botao::new("a", "b")], None)
            .await
            .unwrap();
        assert_ne!(a, b);
        n.edita(&a, "y", &[]).await.unwrap();
        n.apaga(&a).await;
        n.fixa(&b).await;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x.txt");
        std::fs::write(&f, b"x").unwrap();
        n.envia_arquivo(Some(&c), &f, None, Midia::Documento)
            .await
            .unwrap();
        assert_eq!(n.apaga_canal(&c).await, Resolvido::Apagado);
        assert!(n.confere().await.is_ok());
    }

    #[tokio::test]
    async fn nao_ha_anexo_para_baixar() {
        let n = Nulo::default();
        let dir = tempfile::tempdir().unwrap();
        let anexo = Anexo {
            id: "x".into(),
            tamanho: 1,
            nome: None,
            tipo: TipoAnexo::Documento,
        };
        assert!(n.baixa(&anexo, dir.path()).await.is_err());
    }

    #[tokio::test]
    async fn escuta_fica_parada_sem_derrubar_ninguem() {
        // Sem plataforma não há o que ouvir, e sair daqui faria o daemon achar que o laço
        // de entrada morreu.
        let n = Nulo::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        let r = tokio::time::timeout(std::time::Duration::from_millis(100), n.escuta(tx)).await;
        assert!(r.is_err(), "escuta voltou");
    }
}
