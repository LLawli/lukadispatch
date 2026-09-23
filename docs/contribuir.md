# Contribuir

O que você precisa para mexer no lukadispatch sem quebrar a instalação de ninguém: compilar,
rodar sem bot, os portões que um PR precisa passar, e como uma versão sai.

## Compilar

Rust 1.92 ou mais novo (o `rust-version` do `Cargo.toml`, verificado no CI), mais os cabeçalhos
de gtk4 e libadwaita 1.5 para a janela de pergunta:

```bash
# Arch
sudo pacman -S gtk4 libadwaita
# Debian 13, Ubuntu 24.04
sudo apt install libgtk-4-dev libadwaita-1-dev
# Fedora
sudo dnf install gtk4-devel libadwaita-devel

cargo build --workspace
```

## Rodar sem bot

Dá para exercitar o sistema inteiro (tmux, hooks, Monitor, fila) sem Telegram nenhum:

```bash
export LUKADISPATCH_OFFLINE=1
cargo run -p ld-daemon
lukadispatch new <projeto>
lukadispatch send <id> "quanto é 2+2?"
tmux attach -t ld-<projeto>-<id>     # para ver o que a sessão fez
```

Em modo offline o daemon usa o frontend nulo: cada sessão ganha um canal de mentira
(`nulo-<uuid>`), nada é enviado a lugar nenhum, e o resto funciona igual.

## Testes e portões

```bash
./ci.sh
```

Roda fmt, clippy com `-D warnings` (duas vezes: com o Telegram e com o crate compilado sem ele,
para provar que o domínio não depende do adaptador), os testes e o build release. Ele não para
no primeiro erro: o resumo no fim lista tudo que falhou.

O CI do GitHub roda o mesmo `./ci.sh`, e além dele o MSRV, o `cargo audit` e o `shellcheck` dos
scripts. PR entra com tudo verde.

Os fluxos do daemon (card de transcrição, guarda de pendência, envio em partes, fim de turno)
têm testes em `crates/ld-daemon/tests/fluxos.rs`, que rodam o domínio inteiro contra um frontend
em memória e dublês das outras portas: sem rede, sem tmux e sem modelo de voz. Os testes de
corte de vídeo e de volumes precisam de `ffmpeg` e `7z` na máquina; sem eles, retornam cedo.

Duas regras que vieram de bug real (mais em [armadilhas.md](armadilhas.md#testes)):

- A tradução de entrada de um adaptador se testa com payload copiado da plataforma, não com um
  JSON inventado.
- Teste de unidade não substitui o teste pelo celular depois de mexer em fluxo de card ou reply.

## Mudar o comportamento

Antes de mudar algo que parece estranho, procure em [armadilhas.md](armadilhas.md) e em
[decisoes/](decisoes/). Muita coisa aqui contradiz a documentação do Claude Code ou do Telegram
de propósito, porque foi medida. Uma decisão nova segue o formato descrito em
[docs/README.md](README.md#como-escrever-uma-decisão-nova).

Mudança que o usuário percebe entra no `CHANGELOG.md`, na seção `[Unreleased]`, escrita para
quem vai decidir se atualiza: o que mudou para ele, não como foi feito.

## Instalar o que está no checkout

```bash
bin/deploy
```

Compila, monta o pacote com o `dist/empacota` e passa pelo mesmo `install.sh` de quem instala
pela internet, que troca os binários e reinicia o serviço se ele estiver rodando. Cuidado se
você estiver dentro de uma sessão do bot: reiniciar o daemon no meio de um turno perde a
resposta daquele turno ([armadilhas.md](armadilhas.md#operação)).

## Soltar uma versão

1. Acerte `version` em `[workspace.package]` no `Cargo.toml` e rode `cargo check` para o
   `Cargo.lock` acompanhar.
2. No `CHANGELOG.md`, troque `## [Unreleased]` por `## [X.Y.Z] - AAAA-MM-DD` e abra um
   `## [Unreleased]` vazio acima dela.
3. Commite, crie o tag e empurre:

   ```bash
   git tag vX.Y.Z
   git push origin master vX.Y.Z
   ```

O workflow `release` confere que o tag, o `Cargo.toml` e o `CHANGELOG.md` concordam, compila e
testa em x86_64 e aarch64 (cada um no runner nativo), monta os tarballs com sha256, publica o
GitHub Release com a seção do changelog como notas e atualiza a fórmula no
`LLawli/homebrew-tap`.

O job da fórmula segue o mesmo padrão da release do `note`: empurra por SSH com uma deploy key
de escrita no `LLawli/homebrew-tap` (secret `HOMEBREW_TAP_DEPLOY_KEY`) e só roda com a variável
de repositório `HOMEBREW_TAP=true`. Sem ela, a release sai do mesmo jeito e a fórmula fica na
versão anterior.

Os nomes dos tarballs (`lukadispatch-linux-<arq>.tar.gz`) são contrato: o `install.sh` e o mise
dependem deles. O conteúdo é definido num lugar só, o `dist/empacota`.
