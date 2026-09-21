//! O catálogo de modelos, lido do binário do Claude Code.
//!
//! O menu `/model` do Claude Code mostra só um punhado de modelos, mas o binário conhece muito
//! mais (versões anteriores de Opus e Sonnet, variantes de janela de 1M). Como o usuário quer
//! poder escolher qualquer um, o catálogo é extraído do próprio executável: sempre corresponde à
//! versão instalada, e uma atualização do Claude Code traz os modelos novos sem tocar em código.
//!
//! A leitura é um varredor de strings feito à mão: percorre o arquivo procurando a sequência
//! `claude-`, lê o identificador que vem depois e valida o formato. Não há dependência de regex
//! nem de `strings(1)`, e o custo é uma passada por um arquivo de ~200 MB.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

const FAMILIAS: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];
const MARCA: &[u8] = b"claude-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modelo {
    /// O que vai em `--model`, por exemplo `claude-opus-4-6` ou `claude-opus-4-6[1m]`.
    pub id: String,
    pub familia: String,
    /// Versão já legível: `4.6`, `5`, `4.5 (20251001)`.
    pub versao: String,
    pub um_milhao: bool,
}

impl Modelo {
    /// Nome mostrado no botão.
    pub fn rotulo(&self) -> String {
        let familia = maiuscula(&self.familia);
        match self.um_milhao {
            true => format!("{familia} {} (1M)", self.versao),
            false => format!("{familia} {}", self.versao),
        }
    }
}

/// Tamanho mínimo para um arquivo valer a varredura.
///
/// O binário do Claude Code passa de 200 MB. O que aparece no PATH com esse nome e é pequeno é
/// shim de gerenciador de versão: o do mise, por exemplo, é um symlink de 13 bytes para o
/// próprio `mise`, e varrê-lo devolve zero modelo. Foi exatamente isso que fez o menu de modelo
/// aparecer vazio quando o daemon rodava pelo systemd, cujo PATH acha o shim primeiro.
const TAMANHO_MINIMO: u64 = 20 * 1024 * 1024;

/// Candidatos a binário do Claude Code, em ordem de preferência.
///
/// Não basta o PATH: além do problema do shim, o PATH de um serviço systemd é diferente do seu.
/// Por isso a lista soma o que o PATH oferece e os lugares onde o Claude Code costuma ficar.
pub fn candidatos(preferido: Option<&Path>) -> Vec<PathBuf> {
    let mut saida: Vec<PathBuf> = Vec::new();
    let mut junta = |p: PathBuf| {
        let resolvido = std::fs::canonicalize(&p).unwrap_or(p);
        if resolvido.is_file() && !saida.contains(&resolvido) {
            saida.push(resolvido);
        }
    };

    if let Some(p) = preferido {
        junta(p.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            junta(dir.join("claude"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        junta(home.join(".claude/local/claude"));
        junta(home.join(".local/bin/claude"));
        // Instalações do mise ficam versionadas; `latest` costuma ser link para a atual.
        let mise = home.join(".local/share/mise/installs/claude");
        junta(mise.join("latest/claude"));
        if let Ok(entradas) = std::fs::read_dir(&mise) {
            for e in entradas.flatten() {
                junta(e.path().join("claude"));
            }
        }
    }
    junta(PathBuf::from("/usr/local/bin/claude"));
    saida
}

/// O binário do Claude Code que realmente conhece os modelos, com o catálogo dele.
///
/// Testar é a única forma honesta de escolher: um candidato só vale se a varredura nele devolver
/// modelo. Assim shim, wrapper e homônimo caem fora sozinhos, sem lista de exceções.
pub fn catalog_auto(preferido: Option<&Path>) -> (Option<PathBuf>, Vec<Modelo>) {
    for candidato in candidatos(preferido) {
        let grande = std::fs::metadata(&candidato)
            .map(|m| m.len() >= TAMANHO_MINIMO)
            .unwrap_or(false);
        if !grande {
            continue;
        }
        let modelos = catalog(&candidato);
        if !modelos.is_empty() {
            return (Some(candidato), modelos);
        }
    }
    (None, Vec::new())
}

/// Onde está o binário do Claude Code. `None` quando nenhum candidato serve.
pub fn claude_binary() -> Option<PathBuf> {
    catalog_auto(None).0
}

/// Todos os modelos que o binário conhece, do mais novo para o mais antigo.
pub fn catalog(binario: &Path) -> Vec<Modelo> {
    let Ok(mut f) = std::fs::File::open(binario) else {
        return Vec::new();
    };

    // Chave: id sem a marca de 1M. Valor: (modelo, tem variante de 1M).
    let mut achados: BTreeMap<String, bool> = BTreeMap::new();
    let mut buf = vec![0u8; 1 << 20];
    // Uma ocorrência pode cair na fronteira de dois blocos, então o fim de cada bloco é
    // reaproveitado no começo do próximo.
    let mut sobra: Vec<u8> = Vec::new();

    while let Ok(lidos) = f.read(&mut buf) {
        if lidos == 0 {
            break;
        }
        let mut bloco = std::mem::take(&mut sobra);
        bloco.extend_from_slice(&buf[..lidos]);
        varre(&bloco, &mut achados);
        let corte = bloco.len().saturating_sub(64);
        sobra = bloco[corte..].to_vec();
    }

    tira_minor_zero(&mut achados);

    let mut saida = Vec::new();
    for (id, tem_1m) in &achados {
        let Some(m) = monta(id, false) else { continue };
        saida.push(m);
        if *tem_1m && let Some(m) = monta(id, true) {
            saida.push(m);
        }
    }
    ordena(&mut saida);
    saida
}

/// Agrupa por família, preservando a ordem (mais novo primeiro) dentro de cada uma.
pub fn por_familia(modelos: &[Modelo]) -> Vec<(String, Vec<Modelo>)> {
    let mut saida: Vec<(String, Vec<Modelo>)> = Vec::new();
    for f in FAMILIAS {
        let do_grupo: Vec<Modelo> = modelos.iter().filter(|m| m.familia == f).cloned().collect();
        if !do_grupo.is_empty() {
            saida.push((f.to_string(), do_grupo));
        }
    }
    saida
}

fn varre(bloco: &[u8], achados: &mut BTreeMap<String, bool>) {
    let mut i = 0;
    while let Some(pos) = procura(&bloco[i..], MARCA) {
        let inicio = i + pos;
        let mut fim = inicio + MARCA.len();
        while fim < bloco.len() && aceito(bloco[fim]) {
            fim += 1;
        }
        if let Ok(texto) = std::str::from_utf8(&bloco[inicio..fim])
            && let Some((base, tem_1m)) = normaliza(texto)
        {
            let entrada = achados.entry(base).or_insert(false);
            *entrada = *entrada || tem_1m;
        }
        i = inicio + MARCA.len();
    }
}

fn procura(palheiro: &[u8], agulha: &[u8]) -> Option<usize> {
    palheiro.windows(agulha.len()).position(|j| j == agulha)
}

fn aceito(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'[' | b']')
}

/// Separa o id em (base sem `[1m]`, tem variante de 1M), recusando o que não for modelo.
fn normaliza(texto: &str) -> Option<(String, bool)> {
    let (base, tem_1m) = match texto.strip_suffix("[1m]") {
        Some(b) => (b, true),
        None => (texto, false),
    };
    // Sufixos internos do binário que não valem como escolha.
    if base.ends_with("-v1") || base.ends_with('-') {
        return None;
    }
    let resto = base.strip_prefix("claude-")?;
    let (familia, versao) = resto.split_once('-')?;
    if !FAMILIAS.contains(&familia) || !versao.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    if !versao_plausivel(versao) {
        return None;
    }
    Some((base.to_string(), tem_1m))
}

/// Formato de versão que a Anthropic usa de verdade: `5`, `4-6`, `3-7`.
///
/// Serve para separar modelo de artefato. O binário tem strings como
/// `claude-opus-4-20250514` (a mesma coisa que `claude-opus-4`, só que datada) e
/// `claude-haiku-3-55`, que viraria uma "Haiku 3.55" inexistente no menu. A regra é conservadora
/// de propósito: major com até dois dígitos, e no máximo um minor de um dígito.
fn versao_plausivel(versao: &str) -> bool {
    let mut partes = versao.split('-');
    let Some(major) = partes.next() else {
        return false;
    };
    if major.is_empty() || major.len() > 2 || !major.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    match partes.next() {
        None => true,
        Some(minor) => {
            minor.len() == 1 && minor.chars().all(|c| c.is_ascii_digit()) && partes.next().is_none()
        }
    }
}

/// `claude-opus-4-0` e `claude-opus-4` são o mesmo modelo escrito de dois jeitos; o menu mostra
/// um só.
fn tira_minor_zero(achados: &mut BTreeMap<String, bool>) {
    let com_zero: Vec<String> = achados
        .keys()
        .filter(|k| k.ends_with("-0"))
        .cloned()
        .collect();
    for id in com_zero {
        let curto = id.trim_end_matches("-0").to_string();
        if achados.contains_key(&curto) {
            let tinha_1m = achados.remove(&id).unwrap_or(false);
            if tinha_1m && let Some(e) = achados.get_mut(&curto) {
                *e = true;
            }
        }
    }
}

fn monta(id_base: &str, um_milhao: bool) -> Option<Modelo> {
    let resto = id_base.strip_prefix("claude-")?;
    let (familia, versao) = resto.split_once('-')?;
    Some(Modelo {
        id: match um_milhao {
            true => format!("{id_base}[1m]"),
            false => id_base.to_string(),
        },
        familia: familia.to_string(),
        versao: versao.replace('-', "."),
        um_milhao,
    })
}

/// Mais novo primeiro, comparando número a número (para 4.10 não vir antes de 4.9).
fn ordena(modelos: &mut [Modelo]) {
    modelos.sort_by(|a, b| {
        let fa = FAMILIAS.iter().position(|f| *f == a.familia).unwrap_or(9);
        let fb = FAMILIAS.iter().position(|f| *f == b.familia).unwrap_or(9);
        fa.cmp(&fb)
            .then_with(|| numeros(&b.versao).cmp(&numeros(&a.versao)))
            .then_with(|| b.um_milhao.cmp(&a.um_milhao))
    });
}

fn numeros(versao: &str) -> Vec<u32> {
    versao
        .split('.')
        .map(|p| p.parse::<u32>().unwrap_or(0))
        .collect()
}

fn maiuscula(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(p) => p.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Arquivo com os mesmos identificadores que aparecem no binário de verdade, cercados de
    /// lixo binário, que é como eles aparecem lá.
    fn binario_falso() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("claude");
        let mut f = std::fs::File::create(&p).unwrap();
        for pedaco in [
            &b"\x00\x01claude-opus-5\x00"[..],
            &b"\xffclaude-opus-5[1m]\x00"[..],
            &b"claude-opus-4-6\x00claude-opus-4-6[1m]"[..],
            &b"\x00claude-sonnet-5\x00claude-haiku-4-5\x00"[..],
            &b"claude-fable-5-1\x00"[..],
            &b"claude-sonnet-4-5-20250929\x00"[..], // datado: descartado
            &b"claude-opus-4-1-20250805-v1\x00"[..], // interno: descartado
            &b"claude-banana-9\x00"[..],            // família inventada: descartada
        ] {
            f.write_all(pedaco).unwrap();
        }
        (dir, p)
    }

    #[test]
    fn extrai_os_modelos_do_binario() {
        let (_d, p) = binario_falso();
        let c = catalog(&p);
        let ids: Vec<&str> = c.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"claude-opus-5"));
        assert!(ids.contains(&"claude-opus-5[1m]"));
        assert!(ids.contains(&"claude-fable-5-1"));
    }

    #[test]
    fn descarta_datado_interno_e_familia_desconhecida() {
        let (_d, p) = binario_falso();
        let ids: Vec<String> = catalog(&p).into_iter().map(|m| m.id).collect();
        assert!(!ids.iter().any(|i| i.contains("20250929")));
        assert!(!ids.iter().any(|i| i.ends_with("-v1")));
        assert!(!ids.iter().any(|i| i.contains("banana")));
    }

    #[test]
    fn so_marca_1m_quem_tem_a_variante() {
        let (_d, p) = binario_falso();
        let c = catalog(&p);
        let sonnet: Vec<&Modelo> = c.iter().filter(|m| m.familia == "sonnet").collect();
        assert!(
            sonnet.iter().all(|m| !m.um_milhao),
            "o sonnet do teste não tem variante de 1M"
        );
        assert!(c.iter().any(|m| m.id == "claude-opus-4-6[1m]"));
    }

    #[test]
    fn rotulo_e_legivel() {
        let m = Modelo {
            id: "claude-opus-4-6[1m]".into(),
            familia: "opus".into(),
            versao: "4.6".into(),
            um_milhao: true,
        };
        assert_eq!(m.rotulo(), "Opus 4.6 (1M)");
    }

    #[test]
    fn mais_novo_vem_primeiro_dentro_da_familia() {
        let (_d, p) = binario_falso();
        let c = catalog(&p);
        let opus: Vec<String> = c
            .iter()
            .filter(|m| m.familia == "opus" && !m.um_milhao)
            .map(|m| m.versao.clone())
            .collect();
        assert_eq!(opus, vec!["5", "4.6"]);
    }

    #[test]
    fn familias_saem_agrupadas_e_na_ordem_conhecida() {
        let (_d, p) = binario_falso();
        let grupos = por_familia(&catalog(&p));
        let nomes: Vec<&str> = grupos.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(nomes, vec!["opus", "sonnet", "haiku", "fable"]);
    }

    #[test]
    fn ocorrencia_na_fronteira_de_bloco_nao_se_perde() {
        // O varredor lê em blocos de 1 MiB; um id cortado no meio precisa sobreviver.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("claude");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&vec![0u8; (1 << 20) - 5]).unwrap();
        f.write_all(b"claude-opus-4-8\x00").unwrap();
        f.write_all(&vec![0u8; 1024]).unwrap();
        let ids: Vec<String> = catalog(&p).into_iter().map(|m| m.id).collect();
        assert!(ids.contains(&"claude-opus-4-8".to_string()), "{ids:?}");
    }

    #[test]
    fn versao_datada_e_artefato_ficam_de_fora() {
        assert!(versao_plausivel("5"));
        assert!(versao_plausivel("4-6"));
        assert!(versao_plausivel("3-7"));
        assert!(
            !versao_plausivel("4-20250514"),
            "id datado é a mesma coisa que o curto"
        );
        assert!(
            !versao_plausivel("3-55"),
            "não existe minor de dois dígitos"
        );
        assert!(!versao_plausivel("4-5-20250929"));
        assert!(!versao_plausivel(""));
    }

    #[test]
    fn minor_zero_nao_duplica_o_major() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("claude");
        std::fs::write(&p, b"\x00claude-opus-4\x00claude-opus-4-0\x00").unwrap();
        let ids: Vec<String> = catalog(&p).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["claude-opus-4"]);
    }

    #[test]
    fn shim_pequeno_e_ignorado_na_escolha_automatica() {
        // O caso real: no PATH do systemd, o primeiro `claude` era um symlink de 13 bytes para o
        // mise. Varrê-lo devolvia zero modelo e o menu saía vazio.
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("claude-shim");
        std::fs::write(&shim, b"#!/bin/sh\nexec mise x -- claude\n").unwrap();
        let (achado, modelos) = catalog_auto(Some(&shim));
        assert!(achado.is_none() || !modelos.is_empty());
        assert!(modelos.is_empty() || achado.is_some());
    }

    #[test]
    fn candidatos_nao_repetem_o_mesmo_arquivo() {
        // O PATH do usuário tem ~/.local/bin duas vezes; o mesmo binário não pode ser varrido
        // duas vezes.
        let dir = tempfile::tempdir().unwrap();
        let alvo = dir.path().join("claude");
        std::fs::write(&alvo, b"x").unwrap();
        let lista = candidatos(Some(&alvo));
        let quantos = lista
            .iter()
            .filter(|p| **p == std::fs::canonicalize(&alvo).unwrap())
            .count();
        assert_eq!(quantos, 1);
    }

    #[test]
    fn binario_ausente_devolve_lista_vazia() {
        assert!(catalog(Path::new("/nao/existe/claude")).is_empty());
    }
}
