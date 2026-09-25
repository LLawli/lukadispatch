//! O `/new` e o `/kill` de sessão em worktree: os menus, as escolhas e as perguntas.
//!
//! O `/new` sem argumento é uma conversa em etapas: a pasta que junta projetos (as raízes do
//! `[scan]`, mais os fixados no config), o projeto, a branch, e por fim continuar a conversa
//! anterior ou começar do zero. Cada branch abre na worktree dela ([`crate::worktree`]); a
//! branch principal nunca: escolhê-la pede o nome de uma branch nova a partir dela.
//!
//! Com argumento: `/new <projeto> <branch>` pula direto para a branch (criando se não existir),
//! e `/new new-project` cria um projeto numa das pastas.
//!
//! Os botões carregam só um número (`e:<n>`), e a escolha fica aqui, em memória: o dado de um
//! botão tem teto pequeno (64 bytes no Telegram) e não cabe caminho de projeto nem nome de
//! branch. Um teclado de antes de um restart do daemon perde o sentido e diz isso.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ld_core::config::{Project, expand_tilde};
use ld_core::state::{Session, Worktree};
use tracing::{info, warn};

use crate::app::App;
use crate::frontend::formato::escapa;
use crate::frontend::{Botao, Canal, MsgId};
use crate::roteador::{TTL_RESPOSTA, TTL_TECLADO};
use crate::worktree;

/// Quanto uma escolha ou uma pergunta vale. É o tempo de vida do teclado que a mostrou.
const VALIDADE: Duration = Duration::from_secs(TTL_TECLADO);

/// Quantas branches o seletor mostra. Mais que isso não se lê num celular, e as que ficam de
/// fora são as sem commit há mais tempo: `/new <projeto> <branch>` chega nelas.
const TETO_DE_BRANCHES: usize = 24;

/// O que um botão do `/new` ou do `/kill` quer dizer.
#[derive(Debug, Clone)]
enum Escolha {
    /// Uma pasta de projetos: mostra os projetos dela.
    Pasta(Pasta),
    /// Volta à lista de pastas.
    Pastas,
    /// Um projeto: mostra as branches (ou abre, se não for repositório com commit).
    Projeto(Project),
    /// Uma branch que já existe: abre na worktree dela.
    Branch { projeto: Project, branch: String },
    /// Uma branch nova a partir de `base`: pergunta o nome.
    NovaBranch { projeto: Project, base: String },
    /// Gera o nome da branch que está sendo perguntada.
    GeraNome,
    /// Desiste da pergunta em aberto.
    Cancela,
    /// Continua a conversa anterior no projeto (ou worktree) `projeto.path`.
    Continuar { projeto: Project, sessao: String },
    /// Começa uma conversa nova ali.
    DoZero(Project),
    /// Cria um projeto numa pasta: pergunta o nome.
    NovoProjetoEm(PathBuf),
    /// Mostra as pastas onde dá para criar projeto.
    NovoProjeto,
    /// Fecha a sessão e mantém a worktree para continuar depois.
    FechaMantendo(String),
    /// Fecha a sessão e apaga a worktree e a branch, perguntando antes se houver o que perder.
    FechaApagando(String),
    /// Já perguntou: apaga mesmo.
    ApagaMesmo(String),
}

/// Uma pergunta que espera texto no canal principal.
#[derive(Debug, Clone)]
enum Espera {
    NomeDaBranch { projeto: Project, base: String },
    NomeDoProjeto(PathBuf),
}

/// As escolhas e a pergunta em aberto do `/new` e do `/kill`.
#[derive(Default)]
pub struct Estado {
    escolhas: Mutex<HashMap<u64, (Escolha, Instant)>>,
    proximo: AtomicU64,
    espera: Mutex<Option<(Espera, Instant, Option<MsgId>)>>,
}

impl Estado {
    fn guarda(&self, e: Escolha) -> String {
        let n = self.proximo.fetch_add(1, Ordering::Relaxed);
        let mut mapa = self.escolhas.lock().unwrap_or_else(|e| e.into_inner());
        mapa.retain(|_, (_, quando)| quando.elapsed() < VALIDADE);
        mapa.insert(n, (e, Instant::now()));
        format!("e:{n}")
    }

    fn tira(&self, n: u64) -> Option<Escolha> {
        let mut mapa = self.escolhas.lock().unwrap_or_else(|e| e.into_inner());
        mapa.remove(&n)
            .filter(|(_, quando)| quando.elapsed() < VALIDADE)
            .map(|(e, _)| e)
    }

    fn espera(&self, e: Espera, pergunta: Option<MsgId>) {
        *self.espera.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((e, Instant::now(), pergunta));
    }

    fn tira_espera(&self) -> Option<(Espera, Option<MsgId>)> {
        self.espera
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .filter(|(_, quando, _)| quando.elapsed() < VALIDADE)
            .map(|(e, _, m)| (e, m))
    }

    fn ve_espera(&self) -> Option<Espera> {
        self.espera
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|(_, quando, _)| quando.elapsed() < VALIDADE)
            .map(|(e, _, _)| e.clone())
    }
}

/// Uma pasta que junta projetos: uma raiz do `[scan]`, ou o grupo dos fixados no config que
/// moram fora delas.
#[derive(Debug, Clone)]
struct Pasta {
    nome: String,
    projetos: Vec<Project>,
}

/// As pastas com os projetos de cada uma, na ordem das raízes do config.
fn pastas(app: &App) -> Vec<Pasta> {
    let raizes: Vec<PathBuf> = app.cfg.scan.roots.iter().map(|r| expand_tilde(r)).collect();
    agrupa(app.cfg.projects_available(), &raizes)
}

/// Põe cada projeto na primeira raiz que o contém; o que não cabe em nenhuma vai para
/// "Fixados". Pasta sem projeto não aparece.
fn agrupa(projetos: Vec<Project>, raizes: &[PathBuf]) -> Vec<Pasta> {
    let mut saida: Vec<Pasta> = raizes
        .iter()
        .map(|r| Pasta {
            nome: r
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| r.to_string_lossy().into_owned()),
            projetos: projetos
                .iter()
                .filter(|p| raizes.iter().find(|r| Path::new(&p.path).starts_with(r)) == Some(r))
                .cloned()
                .collect(),
        })
        .collect();
    let fora: Vec<Project> = projetos
        .into_iter()
        .filter(|p| !raizes.iter().any(|r| Path::new(&p.path).starts_with(r)))
        .collect();
    if !fora.is_empty() {
        saida.push(Pasta {
            nome: "Fixados".into(),
            projetos: fora,
        });
    }
    saida.retain(|p| !p.projetos.is_empty());
    saida
}

// ------------------------------------------------------------------ entradas

/// `/new` sem argumento.
pub async fn inicio(app: &Arc<App>) -> anyhow::Result<()> {
    let pastas = pastas(app);
    match pastas.as_slice() {
        [] => {
            avisa(
                app,
                None,
                "Nenhum projeto encontrado. Verifique <code>[scan] roots</code> ou adicione um \
                 <code>[[projects]]</code> no config.toml. Para criar um: <code>/new new-project</code>.",
            )
            .await;
            Ok(())
        }
        [so] => mostra_projetos(app, so.clone()).await,
        _ => mostra_pastas(app).await,
    }
}

/// `/new <palavras>`: projeto, projeto e branch, ou `new-project`. Modelo e esforço já saíram.
pub async fn direto(
    app: &Arc<App>,
    palavras: &[&str],
    model: Option<String>,
    effort: Option<String>,
) -> anyhow::Result<()> {
    if palavras == ["new-project"] {
        return mostra_pastas_para_criar(app).await;
    }
    let projetos = app.cfg.projects_available();
    let com_flags = |mut p: Project| {
        p.model = model.clone().or(p.model);
        p.effort = effort.clone().or(p.effort);
        p
    };
    if let Some(p) = crate::roteador::achar(&projetos, &palavras.join(" ")) {
        return mostra_branches(app, com_flags(p)).await;
    }
    // A última palavra é a branch: nome de branch não tem espaço, e nome de projeto pode ter.
    if let [projeto @ .., branch] = palavras
        && !projeto.is_empty()
        && let Some(p) = crate::roteador::achar(&projetos, &projeto.join(" "))
    {
        return abre_branch_pelo_nome(app, com_flags(p), branch).await;
    }
    avisa(
        app,
        None,
        &format!(
            "Não achei o projeto <b>{}</b>.",
            escapa(&palavras.join(" "))
        ),
    )
    .await;
    Ok(())
}

/// Um toque num botão `e:<n>`.
pub async fn toque(
    app: &Arc<App>,
    dado: &str,
    canal: Option<&Canal>,
    msg: Option<&MsgId>,
) -> anyhow::Result<()> {
    let Some(escolha) = dado.parse().ok().and_then(|n| app.novo.tira(n)) else {
        if let Some(m) = msg {
            app.frontend.apaga(m).await;
        }
        avisa(app, None, "Essa lista já mudou. Mande /new de novo.").await;
        return Ok(());
    };
    // O teclado já cumpriu o papel. A pergunta do nome continua: o toque em "gerar" ou
    // "cancelar" a resolve, e ela some junto.
    if let Some(m) = msg {
        app.frontend.apaga(m).await;
    }
    match escolha {
        Escolha::Pastas => mostra_pastas(app).await,
        Escolha::Pasta(p) => mostra_projetos(app, p).await,
        Escolha::Projeto(p) => mostra_branches(app, p).await,
        Escolha::Branch { projeto, branch } => abre_branch(app, projeto, &branch, None).await,
        Escolha::NovaBranch { projeto, base } => pergunta_nome_da_branch(app, projeto, base).await,
        Escolha::GeraNome => {
            let Some((Espera::NomeDaBranch { projeto, base }, _)) = app.novo.tira_espera() else {
                return Ok(());
            };
            let nome = nome_gerado(Path::new(&projeto.path)).await;
            abre_branch(app, projeto, &nome, Some(&base)).await
        }
        Escolha::Cancela => {
            app.novo.tira_espera();
            Ok(())
        }
        Escolha::Continuar { projeto, sessao } => {
            crate::roteador::abrir(app, &projeto, None, None, Some(&sessao)).await
        }
        Escolha::DoZero(projeto) => crate::roteador::abrir(app, &projeto, None, None, None).await,
        Escolha::NovoProjeto => mostra_pastas_para_criar(app).await,
        Escolha::NovoProjetoEm(pasta) => pergunta_nome_do_projeto(app, pasta).await,
        Escolha::FechaMantendo(id) => fecha(app, &id).await,
        Escolha::FechaApagando(id) => apaga_ou_pergunta(app, &id, canal).await,
        Escolha::ApagaMesmo(id) => fecha_e_apaga(app, &id).await,
    }
}

/// Texto solto no canal principal. Devolve `false` quando não havia pergunta esperando por ele.
pub async fn texto(app: &Arc<App>, texto: &str) -> anyhow::Result<bool> {
    if app.novo.ve_espera().is_none() {
        return Ok(false);
    }
    let Some((espera, pergunta)) = app.novo.tira_espera() else {
        return Ok(false);
    };
    let nome = texto.trim();
    match espera {
        Espera::NomeDaBranch { projeto, base } => {
            let raiz = Path::new(&projeto.path);
            if !worktree::nome_valido(raiz, nome).await {
                app.novo
                    .espera(Espera::NomeDaBranch { projeto, base }, pergunta);
                avisa(
                    app,
                    None,
                    &format!(
                        "<b>{}</b> não serve para nome de branch. Mande outro.",
                        escapa(nome)
                    ),
                )
                .await;
                return Ok(true);
            }
            if worktree::existe_branch(raiz, nome).await {
                app.novo
                    .espera(Espera::NomeDaBranch { projeto, base }, pergunta);
                avisa(
                    app,
                    None,
                    &format!(
                        "A branch <b>{}</b> já existe: escolha-a na lista, ou mande outro nome.",
                        escapa(nome)
                    ),
                )
                .await;
                return Ok(true);
            }
            if let Some(m) = &pergunta {
                app.frontend.apaga(m).await;
            }
            abre_branch(app, projeto, nome, Some(&base)).await?;
        }
        Espera::NomeDoProjeto(pasta) => {
            if let Err(motivo) = nome_de_projeto_valido(&pasta, nome) {
                app.novo.espera(Espera::NomeDoProjeto(pasta), pergunta);
                avisa(app, None, &format!("{motivo} Mande outro nome.")).await;
                return Ok(true);
            }
            if let Some(m) = &pergunta {
                app.frontend.apaga(m).await;
            }
            cria_projeto(app, &pasta, nome).await?;
        }
    }
    Ok(true)
}

/// `/kill` de uma sessão: pergunta o que fazer com a worktree, se ela roda numa; senão fecha.
///
/// A pergunta vai no canal da sessão quando o `/kill` veio de lá, e no principal quando veio
/// com o id.
pub async fn kill(app: &Arc<App>, s: &Session, canal: Option<&Canal>) -> anyhow::Result<()> {
    let Some(w) = app.store.worktree_em(&s.cwd)? else {
        return fecha(app, &s.session_id).await;
    };
    let botoes = vec![
        Botao::new(
            "🗂 Fechar e manter a worktree",
            app.novo
                .guarda(Escolha::FechaMantendo(s.session_id.clone())),
        ),
        Botao::new(
            "🗑 Fechar e apagar worktree e branch",
            app.novo
                .guarda(Escolha::FechaApagando(s.session_id.clone())),
        ),
    ];
    let texto = format!(
        "Esta sessão roda na worktree de <b>{}</b>.\n<i>Mantida, dá para continuar nela pelo /new.</i>",
        escapa(&w.branch)
    );
    if let Ok(m) = app.frontend.envia(canal, &texto, &botoes, None).await {
        crate::frontend::efemera(app.frontend.clone(), m, TTL_TECLADO);
    }
    Ok(())
}

// ------------------------------------------------------------------ etapas do /new

async fn mostra_pastas(app: &Arc<App>) -> anyhow::Result<()> {
    let mut botoes: Vec<Botao> = pastas(app)
        .into_iter()
        .map(|p| {
            Botao::new(
                format!("📁 {} ({})", p.nome, p.projetos.len()),
                app.novo.guarda(Escolha::Pasta(p)),
            )
        })
        .collect();
    botoes.push(Botao::new(
        "➕ Projeto novo",
        app.novo.guarda(Escolha::NovoProjeto),
    ));
    teclado(app, "Abrir sessão em qual pasta?", botoes).await
}

async fn mostra_projetos(app: &Arc<App>, pasta: Pasta) -> anyhow::Result<()> {
    let mut botoes: Vec<Botao> = pasta
        .projetos
        .iter()
        .take(40)
        .map(|p| Botao::new(p.name.clone(), app.novo.guarda(Escolha::Projeto(p.clone()))))
        .collect();
    if pastas(app).len() > 1 {
        botoes.push(Botao::new("« pastas", app.novo.guarda(Escolha::Pastas)));
    }
    teclado(
        app,
        &format!("Qual projeto em <b>{}</b>?", escapa(&pasta.nome)),
        botoes,
    )
    .await
}

/// As branches do projeto. Repositório sem commit (ou pasta sem git) não tem de onde tirar
/// worktree: a sessão abre na pasta, como antes.
async fn mostra_branches(app: &Arc<App>, p: Project) -> anyhow::Result<()> {
    let raiz = PathBuf::from(&p.path);
    if !worktree::tem_commit(&raiz).await {
        return escolhe_retomada(app, p).await;
    }
    let principal = worktree::principal(&raiz).await;
    let branches = worktree::branches(&raiz).await?;
    let nossas: Vec<Worktree> = app
        .store
        .worktrees_de(&p.path)?
        .into_iter()
        .filter(|w| Path::new(&w.caminho).exists())
        .collect();

    let mut botoes = Vec::new();
    for w in &nossas {
        let aberta = app
            .store
            .live_by_cwd(&w.caminho, "")
            .ok()
            .flatten()
            .is_some();
        botoes.push(Botao::new(
            format!("🌿 {}{}", w.branch, if aberta { " 🟢" } else { "" }),
            app.novo.guarda(Escolha::Branch {
                projeto: p.clone(),
                branch: w.branch.clone(),
            }),
        ));
    }
    for b in &branches {
        if botoes.len() >= TETO_DE_BRANCHES {
            break;
        }
        if Some(&b.nome) == principal.as_ref() || nossas.iter().any(|w| w.branch == b.nome) {
            continue;
        }
        // Em checkout noutro lugar (a pasta do repositório, ou uma worktree que não é do bot):
        // o git não deixa a mesma branch em duas worktrees, então ela vira base de uma nova.
        let (rotulo, escolha) = match &b.em_checkout {
            Some(_) => (
                format!("🔀 {} (em uso: nova a partir dela)", b.nome),
                Escolha::NovaBranch {
                    projeto: p.clone(),
                    base: b.nome.clone(),
                },
            ),
            None => (
                b.nome.clone(),
                Escolha::Branch {
                    projeto: p.clone(),
                    branch: b.nome.clone(),
                },
            ),
        };
        botoes.push(Botao::new(rotulo, app.novo.guarda(escolha)));
    }
    if let Some(base) = &principal {
        botoes.push(Botao::new(
            format!("🆕 Nova branch a partir de {base}"),
            app.novo.guarda(Escolha::NovaBranch {
                projeto: p.clone(),
                base: base.clone(),
            }),
        ));
    }
    teclado(
        app,
        &format!(
            "<b>{}</b>: em qual branch?\n<i>cada branch abre na worktree dela; 🌿 já tem uma, 🟢 tem sessão aberta</i>",
            escapa(&p.name)
        ),
        botoes,
    )
    .await
}

/// `/new <projeto> <branch>`: a branch principal pede nome de branch nova; a que existe abre;
/// a que não existe é criada a partir da principal.
async fn abre_branch_pelo_nome(app: &Arc<App>, p: Project, branch: &str) -> anyhow::Result<()> {
    let raiz = PathBuf::from(&p.path);
    if !worktree::tem_commit(&raiz).await {
        avisa(
            app,
            None,
            &format!(
                "<b>{}</b> não é um repositório git com commit: não há branch para abrir. \
                 Mande só <code>/new {}</code>.",
                escapa(&p.name),
                escapa(&p.name)
            ),
        )
        .await;
        return Ok(());
    }
    let principal = worktree::principal(&raiz).await;
    if principal.as_deref() == Some(branch) {
        return pergunta_nome_da_branch(app, p, branch.to_string()).await;
    }
    if worktree::existe_branch(&raiz, branch).await {
        return abre_branch(app, p, branch, None).await;
    }
    if !worktree::nome_valido(&raiz, branch).await {
        avisa(
            app,
            None,
            &format!("<b>{}</b> não serve para nome de branch.", escapa(branch)),
        )
        .await;
        return Ok(());
    }
    let Some(base) = principal else {
        avisa(app, None, "Não achei a branch principal do repositório.").await;
        return Ok(());
    };
    abre_branch(app, p, branch, Some(&base)).await
}

async fn pergunta_nome_da_branch(app: &Arc<App>, p: Project, base: String) -> anyhow::Result<()> {
    let botoes = vec![
        Botao::new("🎲 Gerar um nome", app.novo.guarda(Escolha::GeraNome)),
        Botao::new("Cancelar", app.novo.guarda(Escolha::Cancela)),
    ];
    let texto = format!(
        "<b>{}</b>: nome da branch nova, a partir de <b>{}</b>?\n<i>Mande o nome aqui (por exemplo <code>feat/login</code>).</i>",
        escapa(&p.name),
        escapa(&base)
    );
    let pergunta = app.frontend.envia(None, &texto, &botoes, None).await.ok();
    if let Some(m) = &pergunta {
        crate::frontend::efemera(app.frontend.clone(), m.clone(), TTL_TECLADO);
    }
    app.novo
        .espera(Espera::NomeDaBranch { projeto: p, base }, pergunta);
    Ok(())
}

/// Garante a worktree da branch e segue para a escolha de continuar ou começar do zero. Branch
/// com sessão aberta não ganha outra: você é mandado para a que já existe.
async fn abre_branch(
    app: &Arc<App>,
    p: Project,
    branch: &str,
    base: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(w) = app.store.worktree_da_branch(&p.path, branch)?
        && let Some(viva) = app.store.live_by_cwd(&w.caminho, "")?
    {
        avisa(
            app,
            None,
            &format!(
                "Já há uma sessão aberta em <b>{}</b>: fale com ela no canal <b>{}</b>.",
                escapa(branch),
                escapa(&crate::app::nome_do_canal(&viva.project, Some(&w)))
            ),
        )
        .await;
        return Ok(());
    }
    let w = match garante_worktree(app, &p, branch, base).await {
        Ok(w) => w,
        Err(e) => {
            avisa(
                app,
                None,
                &format!(
                    "❌ Não consegui preparar a worktree de <b>{}</b>: {}",
                    escapa(branch),
                    escapa(&format!("{e:#}"))
                ),
            )
            .await;
            return Ok(());
        }
    };
    let na_worktree = Project {
        path: w.caminho.clone(),
        ..p
    };
    escolhe_retomada(app, na_worktree).await
}

/// A worktree da branch, criada se ainda não existe. Registrada no banco nos dois casos, para a
/// ordem do `/new` seguir o uso.
async fn garante_worktree(
    app: &App,
    p: &Project,
    branch: &str,
    base: Option<&str>,
) -> anyhow::Result<Worktree> {
    let raiz = PathBuf::from(&p.path);
    let caminho = worktree::caminho(&app.raiz_worktrees, &ld_core::paths::home(), &raiz, branch);
    let registro = Worktree {
        caminho: caminho.to_string_lossy().into_owned(),
        projeto: p.name.clone(),
        raiz: p.path.clone(),
        branch: branch.to_string(),
        criada_em: 0,
        usada_em: 0,
    };

    // A que o banco conhece e ainda está no disco.
    if let Some(w) = app.store.worktree_da_branch(&p.path, branch)? {
        if Path::new(&w.caminho).exists() {
            app.store.registra_worktree(&w)?;
            return Ok(w);
        }
        app.store.esquece_worktree(&w.caminho)?;
    }

    let existente = worktree::branches(&raiz)
        .await?
        .into_iter()
        .find(|b| b.nome == branch);
    match existente.as_ref().and_then(|b| b.em_checkout.as_ref()) {
        // Já está em checkout exatamente onde a queremos (o banco a esqueceu, o git não).
        Some(onde) if *onde == caminho => {}
        Some(onde) => anyhow::bail!("a branch está em checkout em {}", onde.display()),
        None => {
            let base = if existente.is_some() { None } else { base };
            if existente.is_none() && base.is_none() {
                anyhow::bail!("a branch não existe e não há de onde criá-la");
            }
            worktree::cria(&raiz, &caminho, branch, base).await?;
            info!(projeto = %p.name, branch, caminho = %caminho.display(), "worktree criada");
        }
    }
    app.store.registra_worktree(&registro)?;
    Ok(app
        .store
        .worktree_da_branch(&p.path, branch)?
        .unwrap_or(registro))
}

/// Pergunta se a sessão continua a conversa anterior daquela pasta (do projeto, ou da worktree)
/// ou começa do zero.
///
/// Só pergunta quando há o que continuar, e quando a conversa anterior não está aberta em outro
/// lugar: retomar uma sessão que já está rodando geraria duas cópias da mesma conversa.
pub async fn escolhe_retomada(app: &Arc<App>, p: Project) -> anyhow::Result<()> {
    let anterior = app.agente.ultima_sessao(&p.path).filter(|a| {
        app.store
            .get(&a.session_id)
            .ok()
            .flatten()
            .is_none_or(|s| s.ended_at.is_some())
    });
    let Some(a) = anterior else {
        return crate::roteador::abrir(app, &p, None, None, None).await;
    };
    let onde = match app.store.worktree_em(&p.path).ok().flatten() {
        Some(w) => format!("{} · {}", p.name, w.branch),
        None => p.name.clone(),
    };
    let botoes = vec![
        Botao::new(
            format!("▶️ Continuar ({})", crate::roteador::ha_quanto(a.quando)),
            app.novo.guarda(Escolha::Continuar {
                projeto: p.clone(),
                sessao: a.session_id.clone(),
            }),
        ),
        Botao::new("🆕 Começar do zero", app.novo.guarda(Escolha::DoZero(p))),
    ];
    teclado(
        app,
        &format!(
            "<b>{}</b> tem conversa anterior:\n<i>{}</i>",
            escapa(&onde),
            escapa(&a.resumo)
        ),
        botoes,
    )
    .await
}

/// `lukadispatch new <projeto> --branch <b>`: o mesmo que `/new <projeto> <b>`, sem perguntar
/// nada. A branch principal é recusada (não há a quem perguntar o nome da nova), e a que não
/// existe nasce dela.
pub async fn abre_na_branch(
    app: &Arc<App>,
    p: Project,
    branch: &str,
    continuar: bool,
) -> anyhow::Result<()> {
    let raiz = PathBuf::from(&p.path);
    if !worktree::tem_commit(&raiz).await {
        anyhow::bail!("{} não é um repositório git com commit", p.name);
    }
    let principal = worktree::principal(&raiz).await;
    if principal.as_deref() == Some(branch) {
        anyhow::bail!(
            "{branch} é a branch principal, e ela não abre direto: passe o nome de uma branch nova"
        );
    }
    if let Some(w) = app.store.worktree_da_branch(&p.path, branch)?
        && app.store.live_by_cwd(&w.caminho, "")?.is_some()
    {
        anyhow::bail!("já há uma sessão aberta em {branch}");
    }
    let base = if worktree::existe_branch(&raiz, branch).await {
        None
    } else {
        if !worktree::nome_valido(&raiz, branch).await {
            anyhow::bail!("{branch} não serve para nome de branch");
        }
        principal
    };
    let w = garante_worktree(app, &p, branch, base.as_deref()).await?;
    let retomar = continuar
        .then(|| app.agente.ultima_sessao(&w.caminho))
        .flatten()
        .filter(|a| {
            app.store
                .get(&a.session_id)
                .ok()
                .flatten()
                .is_none_or(|s| s.ended_at.is_some())
        })
        .map(|a| a.session_id);
    let na_worktree = Project {
        path: w.caminho.clone(),
        ..p
    };
    app.create_session(
        &na_worktree,
        na_worktree.model.as_deref(),
        na_worktree.effort.as_deref(),
        retomar.as_deref(),
    )
    .await?;
    Ok(())
}

/// Um nome livre para branch nova: `ld/<data>-<hora>`, com sufixo se já existir.
async fn nome_gerado(raiz: &Path) -> String {
    let agora =
        time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let base = format!(
        "ld/{:04}-{:02}-{:02}-{:02}{:02}",
        agora.year(),
        u8::from(agora.month()),
        agora.day(),
        agora.hour(),
        agora.minute()
    );
    let mut nome = base.clone();
    let mut n = 2;
    while worktree::existe_branch(raiz, &nome).await {
        nome = format!("{base}-{n}");
        n += 1;
    }
    nome
}

// ------------------------------------------------------------------ projeto novo

async fn mostra_pastas_para_criar(app: &Arc<App>) -> anyhow::Result<()> {
    let raizes: Vec<PathBuf> = app
        .cfg
        .scan
        .roots
        .iter()
        .map(|r| expand_tilde(r))
        .filter(|r| r.is_dir())
        .collect();
    if raizes.is_empty() {
        avisa(
            app,
            None,
            "Não há pasta de projetos para criar um: configure <code>[scan] roots</code>.",
        )
        .await;
        return Ok(());
    }
    let botoes = raizes
        .into_iter()
        .map(|r| {
            let nome = r
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| r.to_string_lossy().into_owned());
            Botao::new(
                format!("📁 {nome}"),
                app.novo.guarda(Escolha::NovoProjetoEm(r)),
            )
        })
        .collect();
    teclado(app, "Criar o projeto em qual pasta?", botoes).await
}

async fn pergunta_nome_do_projeto(app: &Arc<App>, pasta: PathBuf) -> anyhow::Result<()> {
    let botoes = vec![Botao::new("Cancelar", app.novo.guarda(Escolha::Cancela))];
    let texto = format!(
        "Nome do projeto novo em <b>{}</b>?\n<i>Mande o nome aqui: letras, números, <code>.</code>, <code>_</code> e <code>-</code>.</i>",
        escapa(&pasta.to_string_lossy())
    );
    let pergunta = app.frontend.envia(None, &texto, &botoes, None).await.ok();
    if let Some(m) = &pergunta {
        crate::frontend::efemera(app.frontend.clone(), m.clone(), TTL_TECLADO);
    }
    app.novo.espera(Espera::NomeDoProjeto(pasta), pergunta);
    Ok(())
}

/// O nome serve para pasta de projeto? Só o que não precisa de aspas em lugar nenhum, e nada que
/// já exista.
fn nome_de_projeto_valido(pasta: &Path, nome: &str) -> Result<(), String> {
    let caracteres_ok = !nome.is_empty()
        && !nome.starts_with(['.', '-'])
        && nome
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !caracteres_ok {
        return Err(format!(
            "<b>{}</b> não serve para nome de projeto.",
            escapa(nome)
        ));
    }
    if pasta.join(nome).exists() {
        return Err(format!(
            "Já existe <b>{}</b> em {}.",
            escapa(nome),
            escapa(&pasta.to_string_lossy())
        ));
    }
    Ok(())
}

/// Cria o projeto e abre a sessão nele. A sessão abre na pasta do projeto, e não numa worktree:
/// num projeto que acabou de nascer não há checkout de ninguém para proteger.
async fn cria_projeto(app: &Arc<App>, pasta: &Path, nome: &str) -> anyhow::Result<()> {
    let caminho = pasta.join(nome);
    if let Err(e) = worktree::inicia_projeto(&caminho).await {
        avisa(
            app,
            None,
            &format!(
                "❌ Não consegui criar <b>{}</b>: {}",
                escapa(nome),
                escapa(&format!("{e:#}"))
            ),
        )
        .await;
        return Ok(());
    }
    info!(projeto = %caminho.display(), "projeto criado a pedido do chat");
    let p = Project {
        name: nome.to_string(),
        path: caminho.to_string_lossy().into_owned(),
        permission_mode: None,
        model: None,
        effort: None,
    };
    crate::roteador::abrir(app, &p, None, None, None).await
}

// ------------------------------------------------------------------ fechar

async fn fecha(app: &Arc<App>, session_id: &str) -> anyhow::Result<()> {
    let Some(s) = app.store.get(session_id)? else {
        return Ok(());
    };
    app.end_session(session_id, true).await?;
    avisa(
        app,
        None,
        &format!("Fechei <b>{}</b>.", escapa(&nome_da_sessao(app, &s))),
    )
    .await;
    Ok(())
}

/// Apagar sem perguntar quando não há nada a perder; senão, diz o que se perderia e pergunta.
async fn apaga_ou_pergunta(
    app: &Arc<App>,
    session_id: &str,
    canal: Option<&Canal>,
) -> anyhow::Result<()> {
    let Some(s) = app.store.get(session_id)? else {
        return Ok(());
    };
    let Some(w) = app.store.worktree_em(&s.cwd)? else {
        return fecha(app, session_id).await;
    };
    let principal = worktree::principal(Path::new(&w.raiz)).await;
    let pendencias = match worktree::pendencias(Path::new(&w.caminho), principal.as_deref()).await {
        Ok(p) => p,
        // Sem conseguir contar, não dá para dizer que não há nada a perder.
        Err(e) => {
            warn!(erro = %format!("{e:#}"), "não consegui contar as pendências da worktree");
            worktree::Pendencias {
                sem_commit: usize::MAX,
                sem_push: 0,
            }
        }
    };
    if pendencias.nenhuma() {
        return fecha_e_apaga(app, session_id).await;
    }
    let botoes = vec![
        Botao::new(
            "🗑 Apagar mesmo assim",
            app.novo.guarda(Escolha::ApagaMesmo(session_id.to_string())),
        ),
        Botao::new(
            "🗂 Fechar e manter",
            app.novo
                .guarda(Escolha::FechaMantendo(session_id.to_string())),
        ),
    ];
    let texto = format!(
        "⚠️ <b>{}</b> tem trabalho que se perderia:\n{}\nApagar mesmo assim?",
        escapa(&w.branch),
        descreve_pendencias(pendencias)
    );
    if let Ok(m) = app.frontend.envia(canal, &texto, &botoes, None).await {
        crate::frontend::efemera(app.frontend.clone(), m, TTL_TECLADO);
    }
    Ok(())
}

fn descreve_pendencias(p: worktree::Pendencias) -> String {
    let mut linhas = Vec::new();
    if p.sem_commit == usize::MAX {
        linhas.push("• não consegui conferir o que há sem commit".to_string());
    } else if p.sem_commit > 0 {
        linhas.push(format!(
            "• {} arquivo(s) mudado(s) sem commit",
            p.sem_commit
        ));
    }
    if p.sem_push > 0 {
        linhas.push(format!(
            "• {} commit(s) que não estão em remoto nenhum nem na branch principal",
            p.sem_push
        ));
    }
    linhas.join("\n")
}

/// Fecha a sessão e só então apaga: com o agente ainda rodando, a pasta sumiria debaixo dele.
async fn fecha_e_apaga(app: &Arc<App>, session_id: &str) -> anyhow::Result<()> {
    let Some(s) = app.store.get(session_id)? else {
        return Ok(());
    };
    let Some(w) = app.store.worktree_em(&s.cwd)? else {
        return fecha(app, session_id).await;
    };
    app.end_session(session_id, true).await?;
    if let Err(e) = worktree::apaga(Path::new(&w.raiz), Path::new(&w.caminho), &w.branch).await {
        avisa(
            app,
            None,
            &format!(
                "Fechei a sessão, mas não consegui apagar a worktree de <b>{}</b>: {}",
                escapa(&w.branch),
                escapa(&format!("{e:#}"))
            ),
        )
        .await;
        return Ok(());
    }
    app.store.esquece_worktree(&w.caminho)?;
    if let Err(e) = app.memoria.worktree_apagada(&w).await {
        warn!(branch = %w.branch, erro = %format!("{e:#}"), "a memória não esqueceu a worktree apagada");
    }
    info!(branch = %w.branch, caminho = %w.caminho, "worktree e branch apagadas");
    avisa(
        app,
        None,
        &format!(
            "🗑 Fechei a sessão e apaguei a worktree e a branch <b>{}</b>.",
            escapa(&w.branch)
        ),
    )
    .await;
    Ok(())
}

fn nome_da_sessao(app: &App, s: &Session) -> String {
    let w = app.store.worktree_em(&s.cwd).ok().flatten();
    crate::app::nome_do_canal(&s.project, w.as_ref())
}

// ------------------------------------------------------------------ miúdos

async fn teclado(app: &Arc<App>, texto: &str, botoes: Vec<Botao>) -> anyhow::Result<()> {
    if let Ok(m) = app.frontend.envia(None, texto, &botoes, None).await {
        crate::frontend::efemera(app.frontend.clone(), m, TTL_TECLADO);
    }
    Ok(())
}

async fn avisa(app: &Arc<App>, canal: Option<&Canal>, texto: &str) {
    crate::frontend::responde_efemero(&app.frontend, canal, texto, TTL_RESPOSTA).await;
}

#[cfg(test)]
#[path = "novo_testes.rs"]
mod testes;
