//! `lukadispatch-ask`: a janela que aparece no PC quando o Claude pergunta.
//!
//! É o outro lado da corrida. O card no Telegram e esta janela mostram a mesma pergunta ao mesmo
//! tempo, e quem responder primeiro decide; o perdedor é fechado por quem organizou a disputa (o
//! hook). Por isso este programa é simples de propósito:
//!
//! - lê a pergunta em JSON no stdin;
//! - escreve a resposta em JSON no stdout e sai com 0;
//! - **fechar a janela sem responder não escreve nada e sai com 1**, porque silêncio aqui
//!   significa "vou responder pelo celular", e não "responda vazio".

use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{Align, Box as GtkBox, CheckButton, Entry, Label, Orientation, ScrolledWindow};
use ld_core::ask::{Answer, AnswerItem, Ask};
use libadwaita as adw;
use libadwaita::prelude::*;

/// Uma pergunta na tela, com o que for preciso para ler a escolha dela depois.
struct Campo {
    header: String,
    question: String,
    multi: bool,
    botoes: Vec<(String, CheckButton)>,
    livre: Entry,
}

fn main() -> std::process::ExitCode {
    let mut bruto = String::new();
    if std::io::stdin().read_to_string(&mut bruto).is_err() {
        return std::process::ExitCode::from(1);
    }
    let ask: Ask = serde_json::from_str(&bruto).unwrap_or_default();
    if ask.is_empty() {
        return std::process::ExitCode::from(1);
    }

    let resposta: Rc<RefCell<Option<Answer>>> = Rc::new(RefCell::new(None));
    let app = adw::Application::builder()
        .application_id("dev.luka.lukadispatch.ask")
        // Sem gerência de sessão: podem existir duas perguntas abertas ao mesmo tempo, de sessões
        // diferentes, e cada uma é um processo próprio.
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    {
        let ask = ask.clone();
        let resposta = resposta.clone();
        app.connect_activate(move |app| monta(app, &ask, resposta.clone()));
    }
    app.run_with_args::<&str>(&[]);

    match resposta.borrow().as_ref() {
        Some(r) => {
            println!("{}", serde_json::to_string(r).unwrap_or_default());
            std::process::ExitCode::SUCCESS
        }
        None => std::process::ExitCode::from(1),
    }
}

fn monta(app: &adw::Application, ask: &Ask, resposta: Rc<RefCell<Option<Answer>>>) {
    let raiz = GtkBox::new(Orientation::Vertical, 12);
    raiz.set_margin_top(18);
    raiz.set_margin_bottom(18);
    raiz.set_margin_start(18);
    raiz.set_margin_end(18);

    let campos: Rc<RefCell<Vec<Campo>>> = Rc::new(RefCell::new(Vec::new()));

    for q in &ask.questions {
        let titulo = Label::new(Some(&q.question));
        titulo.set_halign(Align::Start);
        titulo.set_wrap(true);
        titulo.add_css_class("title-4");
        raiz.append(&titulo);

        let grupo = GtkBox::new(Orientation::Vertical, 6);
        grupo.set_margin_bottom(6);

        let mut botoes = Vec::new();
        let mut primeiro: Option<CheckButton> = None;
        for opt in &q.options {
            let b = CheckButton::with_label(&opt.label);
            if !q.multi_select {
                // Agrupar transforma as caixas em botões de rádio: escolha única de verdade.
                match &primeiro {
                    Some(p) => b.set_group(Some(p)),
                    None => primeiro = Some(b.clone()),
                }
            }
            grupo.append(&b);
            if !opt.description.is_empty() {
                let d = Label::new(Some(&opt.description));
                d.set_halign(Align::Start);
                d.set_wrap(true);
                d.set_margin_start(28);
                d.add_css_class("dim-label");
                d.add_css_class("caption");
                grupo.append(&d);
            }
            if let Some(preview) = &opt.preview {
                grupo.append(&bloco_de_preview(preview));
            }
            botoes.push((opt.label.clone(), b));
        }

        let livre = Entry::new();
        livre.set_placeholder_text(Some("ou escreva a sua resposta"));
        grupo.append(&livre);
        raiz.append(&grupo);

        campos.borrow_mut().push(Campo {
            header: q.header.clone(),
            question: q.question.clone(),
            multi: q.multi_select,
            botoes,
            livre,
        });
    }

    let responder = gtk4::Button::with_label("Responder");
    responder.add_css_class("suggested-action");
    responder.set_halign(Align::End);
    raiz.append(&responder);

    let rolagem = ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .child(&raiz)
        .build();

    let janela = adw::ApplicationWindow::builder()
        .application(app)
        .title("Claude está perguntando")
        .default_width(520)
        .default_height(480)
        .build();

    let cabecalho = adw::HeaderBar::new();
    let conteudo = GtkBox::new(Orientation::Vertical, 0);
    conteudo.append(&cabecalho);
    conteudo.append(&rolagem);
    rolagem.set_vexpand(true);
    janela.set_content(Some(&conteudo));

    {
        let campos = campos.clone();
        let janela = janela.clone();
        responder.connect_clicked(move |_| {
            let r = colher(&campos.borrow());
            // Responder sem escolher nada não fecha: fechar com resposta vazia obrigaria o Claude
            // a perguntar de novo, e o certo nesse caso é usar o celular ou fechar a janela.
            if r.items.iter().all(|i| i.answers.is_empty()) {
                return;
            }
            *resposta.borrow_mut() = Some(r);
            janela.close();
        });
    }

    janela.present();
}

/// Maquete ou trecho de código que a opção carrega.
///
/// Fonte de largura fixa e SEM quebra de linha: o preview é alinhado por espaços, e quebrar a
/// linha desmonta o desenho. O que passa da largura da janela ganha rolagem lateral, que é o
/// único jeito de mostrar uma maquete larga sem deformá-la.
fn bloco_de_preview(preview: &str) -> gtk4::Widget {
    let texto = Label::new(Some(preview.trim_end()));
    texto.set_halign(Align::Start);
    texto.set_wrap(false);
    texto.set_selectable(true);
    texto.add_css_class("monospace");
    texto.set_margin_top(4);
    texto.set_margin_bottom(4);
    texto.set_margin_start(8);
    texto.set_margin_end(8);

    let rolagem = ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Automatic)
        .vscrollbar_policy(gtk4::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(320)
        .child(&texto)
        .build();

    let moldura = gtk4::Frame::new(None);
    moldura.set_margin_start(28);
    moldura.set_margin_bottom(4);
    moldura.add_css_class("view");
    moldura.set_child(Some(&rolagem));
    moldura.upcast()
}

/// Lê a tela e monta a resposta. Texto livre ganha do botão: se você digitou, é porque nenhuma
/// das opções servia.
fn colher(campos: &[Campo]) -> Answer {
    Answer {
        items: campos
            .iter()
            .map(|c| {
                let livre = c.livre.text().trim().to_string();
                let answers = if !livre.is_empty() {
                    vec![livre]
                } else {
                    c.botoes
                        .iter()
                        .filter(|(_, b)| b.is_active())
                        .map(|(label, _)| label.clone())
                        .take(if c.multi { usize::MAX } else { 1 })
                        .collect()
                };
                AnswerItem {
                    header: c.header.clone(),
                    question: c.question.clone(),
                    answers,
                }
            })
            .collect(),
    }
}
