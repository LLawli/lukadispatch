//! A porta do chat: tudo que o daemon precisa de um aplicativo de mensagens, e nada além disso.
//!
//! O resto do daemon (sessões, cards, painel, transcrição, envio de arquivo) fala só com a trait
//! [`Frontend`] e com os tipos deste módulo. Nenhum deles sabe o que é tópico de fórum,
//! `callback_data` ou `InlineKeyboardMarkup`. Trocar o Telegram pelo WhatsApp é escrever um
//! módulo novo aqui dentro que implemente a trait, e apontar `frontend = "..."` no config.
//!
//! O vocabulário é deliberadamente pequeno, e cada palavra tem um equivalente nos aplicativos
//! que se cogitou:
//!
//! | Aqui | Telegram | WhatsApp (Cloud API / multi-device) |
//! |---|---|---|
//! | canal principal (`None`) | tópico General do supergrupo | um grupo "painel" |
//! | [`Canal`] | tópico do fórum | um grupo por sessão, ou prefixo num chat só |
//! | [`MsgId`] | `message_id` | id da mensagem (junto do JID, se precisar) |
//! | [`Botao`] | botão inline com `callback_data` | botão de resposta ou item de lista |
//! | `responde_a` | `reply_to_message` | mensagem citada (`context.id`) |
//! | marcação ([`formato`]) | HTML (`<b>`, `<i>`, `<code>`, `<pre>`) | `*negrito*`, `_itálico_`, crase |
//!
//! Três regras valem para qualquer implementação, e o domínio conta com elas:
//!
//! - **Só pessoa autorizada vira [`Evento`].** A allowlist é do adaptador, porque só ele sabe o
//!   formato da identidade (id numérico no Telegram, telefone no WhatsApp). O domínio assume que
//!   todo evento que recebe já passou por ela.
//! - **`responde_a` é resposta de verdade.** Reply que a plataforma põe sozinha (o Telegram
//!   aponta toda mensagem de tópico para a raiz do tópico) tem de ser descartado no adaptador.
//!   Foi esse detalhe que desligou a guarda de pendência com o CI verde.
//! - **Ids são opacos.** [`Canal`] e [`MsgId`] são texto que só o adaptador interpreta, e o
//!   domínio só guarda, compara e devolve. É isso que permite um id de WhatsApp
//!   (`120363...@g.us`) morar na mesma coluna do banco que um id de tópico do Telegram.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

pub mod formato;
pub mod memoria;
pub mod nulo;
#[cfg(feature = "telegram")]
pub mod telegram;

#[cfg(test)]
mod testes;

/// Um canal de conversa com uma sessão (no Telegram, um tópico do fórum).
///
/// Opaco para o domínio: quem cria e interpreta é o adaptador.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Canal(pub String);

/// Uma mensagem já enviada, que pode ser editada, apagada ou respondida depois.
///
/// Opaco para o domínio. Se a plataforma exigir mais que um número para achar a mensagem de
/// novo (o WhatsApp precisa do chat junto), o adaptador codifica tudo aqui dentro.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MsgId(pub String);

impl Canal {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl MsgId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Canal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::fmt::Display for MsgId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Um botão tocável. Os botões de uma mensagem são mostrados em uma coluna: no celular, botão
/// largo é mais fácil de acertar que grade.
///
/// `dado` volta intacto no [`Evento::Toque`]. O domínio mantém ele curto (cabe em
/// [`Limites::dado_botao`]), então o adaptador não precisa encurtar nada.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Botao {
    pub rotulo: String,
    pub dado: String,
}

impl Botao {
    pub fn new(rotulo: impl Into<String>, dado: impl Into<String>) -> Self {
        Self {
            rotulo: rotulo.into(),
            dado: dado.into(),
        }
    }
}

/// Como um arquivo aparece do outro lado.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Midia {
    /// Byte a byte, como download. Sempre funciona; é o destino de todo fallback.
    Documento,
    /// Imagem mostrada na conversa. A plataforma costuma recomprimir.
    Foto,
    /// Vídeo com player, sem precisar baixar tudo antes.
    Video,
}

/// Os tetos da plataforma, que o domínio respeita antes de chamar a rede.
///
/// Mora aqui, e não em constante, porque é exatamente o que muda de um aplicativo para outro: o
/// Bot API do Telegram manda 50 MB e baixa 20 MB; o WhatsApp manda 100 MB de documento.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limites {
    /// Caracteres por mensagem de texto. Texto maior é quebrado pelo próprio adaptador em
    /// [`Frontend::envia_texto`]; o domínio usa isto para cortar o que ele mesmo compõe.
    pub mensagem: usize,
    /// Bytes que o adaptador consegue baixar de um anexo.
    pub baixar: u64,
    /// Bytes de um arquivo enviado. Acima disso o arquivo é partido por um `Divisor`.
    pub enviar: u64,
    /// Bytes de uma imagem mandada como [`Midia::Foto`]; acima disso vai como documento.
    pub foto: u64,
    /// Bytes do `dado` de um [`Botao`].
    pub dado_botao: usize,
}

impl Default for Limites {
    /// Os tetos do Bot API do Telegram, que são os mais apertados entre os cogitados. Um
    /// adaptador que não souber os seus pode começar daqui sem arriscar erro da API.
    fn default() -> Self {
        Self {
            mensagem: 3900,
            baixar: 20 * 1024 * 1024,
            enviar: 50 * 1024 * 1024,
            foto: 10 * 1024 * 1024,
            dado_botao: 64,
        }
    }
}

/// Resultado de apagar um canal.
///
/// A distinção importa para a varredura de canal vazado: canal que já não existe é trabalho
/// feito, mas queda de rede merece nova tentativa. Sem separar os dois, ou o daemon insiste para
/// sempre num canal que sumiu, ou desiste de um que ainda está lá.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolvido {
    Apagado,
    JaNaoExiste,
    TenteDepois,
}

/// De que tipo é um anexo recebido. O domínio decide por isto o que fazer (voz vira
/// transcrição, o resto vira caminho em disco).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TipoAnexo {
    /// Gravada segurando o microfone.
    Voz,
    /// Arquivo de áudio (música, recado encaminhado).
    Audio,
    Documento,
    Foto,
    Video,
    Animacao,
    NotaDeVideo,
    Figurinha,
}

impl TipoAnexo {
    /// Como a mensagem chamaria isto em português. Vai para o texto que a sessão e você leem.
    pub fn nome(self) -> &'static str {
        match self {
            Self::Voz => "mensagem de voz",
            Self::Audio => "áudio",
            Self::Documento => "documento",
            Self::Foto => "foto",
            Self::Video => "vídeo",
            Self::Animacao => "animação",
            Self::NotaDeVideo => "nota de vídeo",
            Self::Figurinha => "figurinha",
        }
    }

    /// "a foto", "o documento": só falta o artigo para a frase concordar.
    pub fn artigo(self) -> &'static str {
        match self {
            Self::Foto | Self::Animacao | Self::Figurinha | Self::NotaDeVideo | Self::Voz => "a",
            _ => "o",
        }
    }

    /// Fala que pode virar texto.
    pub fn e_audio(self) -> bool {
        matches!(self, Self::Voz | Self::Audio)
    }
}

/// Um anexo recebido, ainda do lado da plataforma.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anexo {
    /// Como o adaptador acha o arquivo de novo para baixar. Opaco para o domínio.
    pub id: String,
    pub tamanho: u64,
    /// O nome que veio de quem mandou, quando veio. É hostil até prova em contrário: só passa
    /// para o disco por [`caminho_livre`].
    pub nome: Option<String>,
    pub tipo: TipoAnexo,
}

/// Quem mandou.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Autor {
    /// Identidade na plataforma, em texto. Só o adaptador interpreta.
    pub id: String,
    /// Como a pessoa aparece. Vai para a sessão como remetente.
    pub nome: String,
}

/// O que chega do chat, já autorizado e sem nada da plataforma.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evento {
    /// Mensagem escrita, com ou sem anexo.
    Mensagem {
        autor: Autor,
        /// `None` é o canal principal (o painel); `Some` é o canal de uma sessão.
        canal: Option<Canal>,
        msg: MsgId,
        /// Texto ou legenda. Vazio quando só veio anexo.
        texto: String,
        /// A qual mensagem a pessoa respondeu de propósito. Nunca o reply automático da
        /// plataforma (ver as regras no topo deste módulo).
        responde_a: Option<MsgId>,
        anexos: Vec<Anexo>,
    },
    /// Toque num [`Botao`].
    Toque {
        autor: Autor,
        canal: Option<Canal>,
        /// A mensagem que carregava o botão, quando a plataforma diz.
        msg: Option<MsgId>,
        dado: String,
    },
}

/// Um aplicativo de mensagens, visto pelo daemon.
///
/// `canal: None` é sempre o canal principal. Texto "rico" é a marcação de [`formato`], que o
/// adaptador traduz para a da plataforma; texto "puro" é o que o agente escreveu e sai sem
/// interpretação nenhuma (a resposta do Claude tem crase, asterisco e sublinhado o tempo todo, e
/// interpretá-los faria a mensagem ser recusada ou sair deformada).
#[async_trait]
pub trait Frontend: Send + Sync + 'static {
    /// Nome curto para o log (`"telegram"`, `"nulo"`).
    fn nome(&self) -> &'static str;

    fn limites(&self) -> Limites;

    /// Como o aplicativo se chama, do jeito que gente fala: entra no prompt de partida do
    /// agente, que precisa saber por onde o Luka fala com ele. Padrão: o mesmo que [`nome`],
    /// que serve para o log mas não para a fala corrida; o Telegram sobrescreve com "Telegram".
    ///
    /// [`nome`]: Frontend::nome
    fn plataforma(&self) -> &'static str {
        self.nome()
    }

    /// Onde, dentro do aplicativo, a conversa desta sessão acontece: vai direto no prompt do
    /// agente ("o canal de conversa com o seu usuário é ..."). Padrão genérico o bastante para
    /// servir a um adaptador que não tenha o conceito de tópico ou de grupo; o Telegram
    /// sobrescreve para falar em tópico de um grupo.
    fn onde(&self, nome_do_canal: &str) -> String {
        format!("a conversa \"{nome_do_canal}\" no {}", self.plataforma())
    }

    /// O chat mostra Markdown formatado? Padrão `false`: um adaptador novo, sem saber ainda como
    /// a plataforma renderiza, é mais seguro assumir que não renderiza e pedir para o agente
    /// mandar tabela e gráfico como imagem do que assumir que renderiza e entregar sopa de pipes.
    fn renderiza_markdown(&self) -> bool {
        false
    }

    /// Confere que dá para trabalhar (credencial aceita, chat alcançável) e devolve como o bot
    /// se chama. Roda na partida, para o erro aparecer no log do serviço e não horas depois.
    async fn confere(&self) -> Result<String>;

    /// Entrega em `saida` tudo que chegar, para sempre. Falha de rede é problema do adaptador:
    /// ele espera e tenta de novo, porque derrubar o daemon mataria as sessões vivas.
    async fn escuta(&self, saida: mpsc::UnboundedSender<Evento>);

    /// Abre o canal de uma sessão nova.
    async fn cria_canal(&self, nome: &str) -> Result<Canal>;

    /// Fecha o canal de uma sessão que acabou.
    async fn apaga_canal(&self, canal: &Canal) -> Resolvido;

    /// Texto puro, sem marcação. O adaptador quebra no próprio limite e devolve o id do último
    /// pedaço, que é o que interessa para editar depois.
    async fn envia_texto(&self, canal: Option<&Canal>, texto: &str) -> Result<MsgId>;

    /// Texto com marcação, com botões opcionais (vazio = sem botões) e, se pedido, respondendo
    /// a uma mensagem. Mensagem respondida que já sumiu não impede o envio.
    async fn envia(
        &self,
        canal: Option<&Canal>,
        rico: &str,
        botoes: &[Botao],
        responde_a: Option<&MsgId>,
    ) -> Result<MsgId>;

    /// Troca o texto e os botões de uma mensagem (vazio = tira os botões). "Nada mudou" não é
    /// erro: é o caso comum do status que recalculou igual.
    async fn edita(&self, msg: &MsgId, rico: &str, botoes: &[Botao]) -> Result<()>;

    /// Apaga, best-effort: mensagem que já sumiu não é problema de ninguém.
    async fn apaga(&self, msg: &MsgId);

    /// Fixa no topo, best-effort. Plataforma sem fixar pode não fazer nada.
    async fn fixa(&self, msg: &MsgId);

    /// Manda um arquivo do disco. Recusa da plataforma (formato, dimensão) volta como erro, e
    /// quem chama decide cair para [`Midia::Documento`].
    async fn envia_arquivo(
        &self,
        canal: Option<&Canal>,
        caminho: &Path,
        legenda: Option<&str>,
        como: Midia,
    ) -> Result<MsgId>;

    /// Baixa o anexo para dentro de `dir`, que já existe, e devolve o caminho gravado.
    ///
    /// O nome do arquivo sai de [`caminho_livre`], nunca direto do que a plataforma mandou.
    /// Download pela metade não pode sobrar em disco. O domínio confere depois que o caminho
    /// está dentro de `dir` e que não está vazio.
    async fn baixa(&self, anexo: &Anexo, dir: &Path) -> Result<PathBuf>;
}

/// Apaga uma mensagem depois de um tempo, sem segurar quem chamou.
///
/// É o que mantém o canal principal sendo só o painel: comando, resposta e teclado são conversa
/// de um instante.
pub fn efemera(fe: Arc<dyn Frontend>, msg: MsgId, segundos: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(segundos)).await;
        fe.apaga(&msg).await;
    });
}

/// Manda uma resposta com marcação que some sozinha.
pub async fn responde_efemero(
    fe: &Arc<dyn Frontend>,
    canal: Option<&Canal>,
    rico: &str,
    segundos: u64,
) {
    if let Ok(id) = fe.envia(canal, rico, &[], None).await {
        efemera(fe.clone(), id, segundos);
    }
}

/// Um caminho seguro e ainda livre dentro de `dir` para um nome vindo de fora.
///
/// O nome é hostil até prova em contrário: `../../.ssh/authorized_keys` é um nome de arquivo
/// válido do ponto de vista de qualquer API de chat. Aqui ele vira um nome simples (sem
/// diretório, sem ponto na borda, sem caractere estranho, com a extensão preservada) e, se já
/// existir arquivo com esse nome, ganha um sufixo (`nota-2.pdf`) em vez de sobrescrever.
pub fn caminho_livre(dir: &Path, nome_bruto: &str) -> PathBuf {
    dir.join(livre(dir, &nome_simples(nome_bruto)))
}

/// Quanto do nome original sobrevive. O resto é cortado, mas a extensão fica: é ela que faz o
/// Read (e você, no celular) reconhecer o que é o arquivo.
const MAX_NOME: usize = 80;

/// Reduz um nome vindo de fora a um nome de arquivo simples: sem diretório, sem surpresa.
///
/// É a metade "nome" de [`caminho_livre`], exposta para quem gera arquivo a partir de um nome
/// que não controla (os divisores nomeiam as partes pelo arquivo original).
pub(crate) fn nome_simples(bruto: &str) -> String {
    // `nome` vem de fora e é texto livre. Ficar só com o último componente derruba de uma vez
    // `../`, caminho absoluto e barra invertida do Windows.
    let base = bruto
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .to_string();

    let limpo: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Ponto na borda faz "." e "..", e faz arquivo oculto sem ninguém pedir.
    let limpo = limpo.trim_matches('.');
    if limpo.is_empty() {
        return "arquivo".into();
    }

    // Nome longo demais estoura o limite do sistema de arquivos; cortar o miolo preserva a
    // extensão, que é o que faz o arquivo ser reconhecido depois.
    match limpo.rsplit_once('.') {
        Some((corpo, ext)) if !ext.is_empty() && ext.len() <= 16 => {
            let corpo: String = corpo.chars().take(MAX_NOME).collect();
            let corpo = if corpo.is_empty() { "arquivo" } else { &corpo };
            format!("{corpo}.{ext}")
        }
        _ => limpo.chars().take(MAX_NOME).collect(),
    }
}

/// Um nome que ainda não existe dentro de `dir`: `nota.pdf`, depois `nota-2.pdf`, e assim por
/// diante.
fn livre(dir: &Path, nome: &str) -> PathBuf {
    let candidato = dir.join(nome);
    if !candidato.exists() {
        return PathBuf::from(nome);
    }
    let (corpo, ext) = match nome.rsplit_once('.') {
        Some((c, e)) if !c.is_empty() => (c.to_string(), format!(".{e}")),
        _ => (nome.to_string(), String::new()),
    };
    for n in 2..10_000 {
        let candidato_nome = format!("{corpo}-{n}{ext}");
        if !dir.join(&candidato_nome).exists() {
            return PathBuf::from(candidato_nome);
        }
    }
    // Dez mil homônimos na mesma sessão é cenário de erro, não de uso: sobrescrever aqui é
    // melhor que devolver caminho impossível.
    PathBuf::from(nome)
}
