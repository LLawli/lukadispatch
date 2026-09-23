//! Um frontend que guarda tudo em memória, para testar o domínio sem rede.
//!
//! Serve a dois propósitos. O óbvio: os testes de fluxo (card de transcrição, guarda de
//! pendência, envio de arquivo) rodam contra ele, e conferem o que o daemon mandou, editou e
//! apagou. O menos óbvio: ele é a prova de que a trait [`Frontend`] basta. Se um fluxo precisar
//! de algo que não passa por ela, este módulo não consegue imitá-lo, e o teste não compila.
//!
//! Os ids são previsíveis (`c1`, `c2` para canal; `m1`, `m2` para mensagem) para as asserções
//! poderem nomeá-los.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{
    Anexo, Botao, Canal, Evento, Frontend, Limites, Midia, MsgId, Resolvido, caminho_livre,
};

/// Uma chamada que o domínio fez, na ordem em que fez.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chamada {
    CriaCanal {
        nome: String,
        canal: Canal,
    },
    ApagaCanal(Canal),
    /// [`Frontend::envia_texto`]: texto puro, inteiro (a quebra por limite não é registrada).
    Texto {
        canal: Option<Canal>,
        texto: String,
        msg: MsgId,
    },
    /// [`Frontend::envia`].
    Envia {
        canal: Option<Canal>,
        rico: String,
        botoes: Vec<Botao>,
        responde_a: Option<MsgId>,
        msg: MsgId,
    },
    Edita {
        msg: MsgId,
        rico: String,
        botoes: Vec<Botao>,
    },
    Apaga(MsgId),
    Fixa(MsgId),
    /// [`Frontend::envia_arquivo`] que deu certo. Uma recusa (ver [`Memoria::recusa`]) não
    /// entra no registro.
    Arquivo {
        canal: Option<Canal>,
        caminho: PathBuf,
        legenda: Option<String>,
        como: Midia,
        msg: MsgId,
    },
}

/// Estado interno, atrás de um `Mutex`: os testes chamam a mesma `Memoria` de vários pontos (o
/// domínio e a asserção), e um `Frontend` precisa ser `Send + Sync`.
#[derive(Default)]
struct Interno {
    chamadas: Vec<Chamada>,
    /// Contador de canal. Cresce mesmo depois de [`Memoria::limpa_registro`]: um id repetido
    /// confundiria uma asserção sobre um canal antigo.
    canal_n: u64,
    /// Contador de mensagem, compartilhado por `envia_texto`, `envia` e `envia_arquivo`. Mesma
    /// razão: sobrevive a `limpa_registro`.
    msg_n: u64,
    /// Canais que existem agora. `apaga_canal` tira daqui; se já não estava, é `JaNaoExiste`.
    canais_vivos: HashSet<Canal>,
    /// Anexos que [`Frontend::baixa`] consegue entregar: id -> (nome bruto, bytes).
    anexos: HashMap<String, (String, Vec<u8>)>,
    /// Mídias que `envia_arquivo` deve recusar.
    recusas: HashSet<Midia>,
    limites: Limites,
}

pub struct Memoria {
    interno: Mutex<Interno>,
    tx: mpsc::UnboundedSender<Evento>,
    // Tirado do `Option` por `escuta`, que precisa ser dono do receptor para dar `.recv().await`
    // sem segurar o lock do mutex através de um `await`.
    rx: Mutex<Option<mpsc::UnboundedReceiver<Evento>>>,
}

impl Memoria {
    /// Com os limites padrão ([`Limites::default`]).
    pub fn new() -> Arc<Self> {
        Self::com_limites(Limites::default())
    }

    pub fn com_limites(limites: Limites) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            interno: Mutex::new(Interno {
                limites,
                ..Interno::default()
            }),
            tx,
            rx: Mutex::new(Some(rx)),
        })
    }

    /// Tudo que foi chamado até agora, em ordem.
    pub fn chamadas(&self) -> Vec<Chamada> {
        self.interno.lock().unwrap().chamadas.clone()
    }

    /// Esquece o registro (os canais e anexos guardados continuam).
    pub fn limpa_registro(&self) {
        self.interno.lock().unwrap().chamadas.clear();
    }

    /// Todo texto que saiu, na ordem: o `texto` de [`Chamada::Texto`] e o `rico` de
    /// [`Chamada::Envia`] e de [`Chamada::Edita`]. Atalho para asserção do tipo "algum aviso
    /// falou disto".
    pub fn textos(&self) -> Vec<String> {
        self.interno
            .lock()
            .unwrap()
            .chamadas
            .iter()
            .filter_map(|c| match c {
                Chamada::Texto { texto, .. } => Some(texto.clone()),
                Chamada::Envia { rico, .. } | Chamada::Edita { rico, .. } => Some(rico.clone()),
                _ => None,
            })
            .collect()
    }

    /// Registra um anexo que [`Frontend::baixa`] vai conseguir entregar. `nome_bruto` faz o
    /// papel do nome que a plataforma mandaria (e passa por `caminho_livre` como passaria de
    /// verdade).
    pub fn guarda_anexo(&self, id: &str, nome_bruto: &str, conteudo: &[u8]) {
        self.interno
            .lock()
            .unwrap()
            .anexos
            .insert(id.to_string(), (nome_bruto.to_string(), conteudo.to_vec()));
    }

    /// Faz [`Frontend::envia_arquivo`] falhar para esta mídia, como a plataforma recusando uma
    /// foto por dimensão. As outras mídias continuam funcionando.
    pub fn recusa(&self, como: Midia) {
        self.interno.lock().unwrap().recusas.insert(como);
    }

    /// Põe um evento na fila que [`Frontend::escuta`] entrega. Funciona mesmo antes de `escuta`
    /// ter sido chamada, porque o canal já existe desde o construtor.
    pub fn injeta(&self, evento: Evento) {
        // Ninguém está ouvindo ainda ou o receptor já sumiu: não é erro do chamador, é só um
        // evento que não tem para onde ir.
        let _ = self.tx.send(evento);
    }

    /// Próximo id de canal (`c1`, `c2`, ...).
    fn proximo_canal(interno: &mut Interno) -> Canal {
        interno.canal_n += 1;
        Canal::new(format!("c{}", interno.canal_n))
    }

    /// Próximo id de mensagem (`m1`, `m2`, ...).
    fn proxima_msg(interno: &mut Interno) -> MsgId {
        interno.msg_n += 1;
        MsgId::new(format!("m{}", interno.msg_n))
    }
}

#[async_trait]
impl Frontend for Memoria {
    fn nome(&self) -> &'static str {
        "memória"
    }

    fn limites(&self) -> Limites {
        self.interno.lock().unwrap().limites
    }

    async fn confere(&self) -> Result<String> {
        Ok("memória".to_string())
    }

    async fn escuta(&self, saida: mpsc::UnboundedSender<Evento>) {
        // Tira o receptor do `Option` com o lock só por um instante, para não segurá-lo através
        // do `.recv().await` que vem a seguir.
        let mut rx = match self.rx.lock().unwrap().take() {
            Some(rx) => rx,
            None => return, // `escuta` já foi chamada antes; não há um segundo receptor.
        };
        while let Some(evento) = rx.recv().await {
            if saida.send(evento).is_err() {
                break;
            }
        }
    }

    async fn cria_canal(&self, nome: &str) -> Result<Canal> {
        let mut interno = self.interno.lock().unwrap();
        let canal = Self::proximo_canal(&mut interno);
        interno.canais_vivos.insert(canal.clone());
        interno.chamadas.push(Chamada::CriaCanal {
            nome: nome.to_string(),
            canal: canal.clone(),
        });
        Ok(canal)
    }

    async fn apaga_canal(&self, canal: &Canal) -> Resolvido {
        let mut interno = self.interno.lock().unwrap();
        interno.chamadas.push(Chamada::ApagaCanal(canal.clone()));
        if interno.canais_vivos.remove(canal) {
            Resolvido::Apagado
        } else {
            Resolvido::JaNaoExiste
        }
    }

    async fn envia_texto(&self, canal: Option<&Canal>, texto: &str) -> Result<MsgId> {
        let mut interno = self.interno.lock().unwrap();
        let msg = Self::proxima_msg(&mut interno);
        interno.chamadas.push(Chamada::Texto {
            canal: canal.cloned(),
            texto: texto.to_string(),
            msg: msg.clone(),
        });
        Ok(msg)
    }

    async fn envia(
        &self,
        canal: Option<&Canal>,
        rico: &str,
        botoes: &[Botao],
        responde_a: Option<&MsgId>,
    ) -> Result<MsgId> {
        let mut interno = self.interno.lock().unwrap();
        let msg = Self::proxima_msg(&mut interno);
        interno.chamadas.push(Chamada::Envia {
            canal: canal.cloned(),
            rico: rico.to_string(),
            botoes: botoes.to_vec(),
            responde_a: responde_a.cloned(),
            msg: msg.clone(),
        });
        Ok(msg)
    }

    async fn edita(&self, msg: &MsgId, rico: &str, botoes: &[Botao]) -> Result<()> {
        self.interno.lock().unwrap().chamadas.push(Chamada::Edita {
            msg: msg.clone(),
            rico: rico.to_string(),
            botoes: botoes.to_vec(),
        });
        Ok(())
    }

    async fn apaga(&self, msg: &MsgId) {
        self.interno
            .lock()
            .unwrap()
            .chamadas
            .push(Chamada::Apaga(msg.clone()));
    }

    async fn fixa(&self, msg: &MsgId) {
        self.interno
            .lock()
            .unwrap()
            .chamadas
            .push(Chamada::Fixa(msg.clone()));
    }

    async fn envia_arquivo(
        &self,
        canal: Option<&Canal>,
        caminho: &Path,
        legenda: Option<&str>,
        como: Midia,
    ) -> Result<MsgId> {
        let mut interno = self.interno.lock().unwrap();
        if interno.recusas.contains(&como) {
            bail!("{} recusado (simulado pela memória)", caminho.display());
        }
        let msg = Self::proxima_msg(&mut interno);
        interno.chamadas.push(Chamada::Arquivo {
            canal: canal.cloned(),
            caminho: caminho.to_path_buf(),
            legenda: legenda.map(str::to_string),
            como,
            msg: msg.clone(),
        });
        Ok(msg)
    }

    async fn baixa(&self, anexo: &Anexo, dir: &Path) -> Result<PathBuf> {
        let (nome_bruto, conteudo) = {
            let interno = self.interno.lock().unwrap();
            let Some(par) = interno.anexos.get(&anexo.id) else {
                bail!("anexo {} não foi guardado nesta memória", anexo.id);
            };
            par.clone()
        };
        let nome = anexo.nome.clone().unwrap_or(nome_bruto);
        let destino = caminho_livre(dir, &nome);
        tokio::fs::write(&destino, &conteudo).await?;
        Ok(destino)
    }
}

#[cfg(test)]
mod testes {
    //! Testes da `Memoria`: ids previsíveis, registro de cada chamada, e o que ela devolve para
    //! os testes de fluxo do domínio conferirem.

    use super::*;
    use crate::frontend::{Autor, TipoAnexo};

    fn fe(m: &Arc<Memoria>) -> Arc<dyn Frontend> {
        m.clone()
    }

    #[tokio::test]
    async fn canais_e_mensagens_tem_ids_previsiveis_e_distintos() {
        let m = Memoria::new();
        let f = fe(&m);
        let c1 = f.cria_canal("proj").await.unwrap();
        let c2 = f.cria_canal("proj").await.unwrap();
        assert_eq!((c1.as_str(), c2.as_str()), ("c1", "c2"));
        let a = f.envia_texto(Some(&c1), "oi").await.unwrap();
        let b = f.envia(None, "<b>x</b>", &[], None).await.unwrap();
        assert_eq!((a.as_str(), b.as_str()), ("m1", "m2"));
    }

    #[tokio::test]
    async fn registra_cada_chamada_com_o_que_veio() {
        let m = Memoria::new();
        let f = fe(&m);
        let c = f.cria_canal("proj").await.unwrap();
        let botoes = vec![Botao::new("✅ Enviar", "t:ok:0")];
        let origem = MsgId::new("m99");
        let id = f
            .envia(Some(&c), "card", &botoes, Some(&origem))
            .await
            .unwrap();
        f.edita(&id, "registro", &[]).await.unwrap();
        f.fixa(&id).await;
        f.apaga(&id).await;
        assert_eq!(f.apaga_canal(&c).await, Resolvido::Apagado);

        assert_eq!(
            m.chamadas(),
            vec![
                Chamada::CriaCanal {
                    nome: "proj".into(),
                    canal: c.clone()
                },
                Chamada::Envia {
                    canal: Some(c.clone()),
                    rico: "card".into(),
                    botoes,
                    responde_a: Some(origem),
                    msg: id.clone(),
                },
                Chamada::Edita {
                    msg: id.clone(),
                    rico: "registro".into(),
                    botoes: vec![],
                },
                Chamada::Fixa(id.clone()),
                Chamada::Apaga(id),
                Chamada::ApagaCanal(c),
            ]
        );
        assert_eq!(m.textos(), vec!["card".to_string(), "registro".to_string()]);
    }

    #[tokio::test]
    async fn apagar_canal_duas_vezes_diz_que_ja_nao_existe() {
        let m = Memoria::new();
        let f = fe(&m);
        let c = f.cria_canal("x").await.unwrap();
        assert_eq!(f.apaga_canal(&c).await, Resolvido::Apagado);
        assert_eq!(f.apaga_canal(&c).await, Resolvido::JaNaoExiste);
        assert_eq!(
            f.apaga_canal(&Canal::new("nunca-existiu")).await,
            Resolvido::JaNaoExiste
        );
    }

    #[tokio::test]
    async fn limpa_registro_esquece_as_chamadas() {
        let m = Memoria::new();
        let f = fe(&m);
        f.envia_texto(None, "a").await.unwrap();
        m.limpa_registro();
        assert!(m.chamadas().is_empty());
        // O contador de ids não volta: id repetido confundiria asserção sobre mensagem velha.
        assert_eq!(f.envia_texto(None, "b").await.unwrap().as_str(), "m2");
    }

    #[tokio::test]
    async fn recusa_derruba_so_a_midia_pedida() {
        let m = Memoria::new();
        let f = fe(&m);
        m.recusa(Midia::Foto);
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("g.png");
        std::fs::write(&png, b"x").unwrap();
        assert!(
            f.envia_arquivo(None, &png, None, Midia::Foto)
                .await
                .is_err()
        );
        let id = f
            .envia_arquivo(None, &png, Some("legenda"), Midia::Documento)
            .await
            .unwrap();
        assert_eq!(
            m.chamadas(),
            vec![Chamada::Arquivo {
                canal: None,
                caminho: png,
                legenda: Some("legenda".into()),
                como: Midia::Documento,
                msg: id,
            }],
            "a recusa não entra no registro"
        );
    }

    #[tokio::test]
    async fn baixa_grava_o_anexo_guardado_com_nome_seguro() {
        let m = Memoria::new();
        let f = fe(&m);
        m.guarda_anexo("v1", "../voz.oga", b"OggS");
        let dir = tempfile::tempdir().unwrap();
        let anexo = Anexo {
            id: "v1".into(),
            tamanho: 4,
            nome: None,
            tipo: TipoAnexo::Voz,
        };
        let caminho = f.baixa(&anexo, dir.path()).await.unwrap();
        assert_eq!(caminho, dir.path().join("voz.oga"));
        assert_eq!(std::fs::read(&caminho).unwrap(), b"OggS");

        let desconhecido = Anexo {
            id: "nao-guardado".into(),
            ..anexo
        };
        assert!(f.baixa(&desconhecido, dir.path()).await.is_err());
    }

    #[tokio::test]
    async fn escuta_entrega_o_que_foi_injetado_em_ordem() {
        let m = Memoria::new();
        let f = fe(&m);
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::spawn(async move { f.escuta(tx).await });

        let ev = |dado: &str| Evento::Toque {
            autor: Autor {
                id: "42".into(),
                nome: "Luka".into(),
            },
            canal: None,
            msg: None,
            dado: dado.into(),
        };
        m.injeta(ev("a"));
        m.injeta(ev("b"));
        let espera = std::time::Duration::from_secs(2);
        assert_eq!(
            tokio::time::timeout(espera, rx.recv()).await.unwrap(),
            Some(ev("a"))
        );
        assert_eq!(
            tokio::time::timeout(espera, rx.recv()).await.unwrap(),
            Some(ev("b"))
        );
    }

    #[test]
    fn limites_sao_os_pedidos() {
        let l = Limites {
            enviar: 1234,
            ..Limites::default()
        };
        assert_eq!(Memoria::com_limites(l).limites(), l);
        assert_eq!(Memoria::new().limites(), Limites::default());
    }
}
