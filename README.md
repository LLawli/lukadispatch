# lukadispatch

[![ci](https://github.com/LLawli/lukadispatch/actions/workflows/ci.yml/badge.svg)](https://github.com/LLawli/lukadispatch/actions/workflows/ci.yml)

**Continue suas sessões de Claude Code pelo celular quando sair do computador.**

Você deixa o Claude trabalhando numa tarefa longa e sai da mesa. Meia hora depois ele parou para
pedir uma permissão, ou terminou e está esperando o próximo passo, e você só vai descobrir quando
voltar. Com o lukadispatch, cada sessão de Claude Code da sua máquina vira um tópico de um grupo
do Telegram: você lê as respostas, manda a próxima instrução, responde às perguntas e aprova
permissões de onde estiver. As sessões continuam rodando no seu computador, com os seus
projetos, as suas ferramentas e a sua conta.

## O que dá para fazer

- **Abrir uma sessão nova** em qualquer projeto seu com `/new`, pelo celular, e continuar a
  conversa anterior daquele projeto se quiser.
- **Conversar com a sessão** pelo tópico dela, por texto ou por **mensagem de voz** (transcrita
  na sua máquina, e só enviada depois do seu aval).
- **Responder perguntas e permissões** num card com botões. A mesma pergunta aparece numa janela
  no PC, e vale quem responder primeiro.
- **Mandar e receber arquivos**: foto, PDF, log, vídeo. O que passa do limite do Telegram é
  dividido em partes.
- **Trocar modelo, esforço e modo de permissão** (`/model`, `/effort`, `/mode`) sem perder a
  conversa.
- **Ver o consumo**: um painel fixo mostra o contexto e os tokens de cada sessão e quanto resta
  das janelas de 5h e 7 dias da conta.
- **Voltar para o teclado** quando quiser: cada sessão é um tmux de verdade
  (`tmux attach -t ld-<projeto>-<id>`), e o que você digitar lá também aparece no tópico.

## Instalar

Linux x86_64 ou aarch64, com o systemd de usuário.

```bash
curl -fsSL https://raw.githubusercontent.com/LLawli/lukadispatch/master/install.sh | sh
```

O script baixa o binário da última release, confere o sha256, instala em `~/.local/bin`, põe o
serviço do systemd e deixa um `.env` e um `config.toml` de exemplo em `~/.config/lukadispatch/`.
Rodar de novo atualiza. Outros caminhos:

```bash
brew install llawli/tap/lukadispatch                 # Homebrew no Linux (compila do fonte)
mise use -g github:LLawli/lukadispatch               # mise
cargo install --locked --git https://github.com/LLawli/lukadispatch ld-daemon ld-cli ld-ask ld-mcp
```

Ou baixe o `lukadispatch-linux-<arq>.tar.gz` da [página de releases](https://github.com/LLawli/lukadispatch/releases)
e rode o `install.sh --de .` que vem dentro dele.

**Precisa ter:** [Claude Code](https://docs.claude.com/en/docs/claude-code) 2.1.274 ou mais
novo, `tmux`, e gtk4 e libadwaita 1.5+ para a janela de pergunta no PC. Por padrão as sessões
sobem dentro do [ai-memory](https://github.com/akitaonrails/ai-memory); sem ele, use
`envelope = "nenhum"` no config. Para dividir arquivos grandes: `7z` (ou `rar`) e `ffmpeg`.
Para transcrever voz: um programa local de voz para texto (o padrão é o whisper.cpp).

## Configurar

**1. O bot e o grupo.**

1. Fale com o [@BotFather](https://t.me/BotFather), mande `/newbot` e guarde o token.
2. Em `/mybots > seu bot > Bot Settings > Group Privacy`, **desligue** o modo de privacidade.
   Sem isso o bot só recebe mensagens que começam com `/`.
3. Crie um grupo, adicione o bot e ligue os tópicos em `Editar > Tópicos`. O Telegram converte o
   grupo em supergrupo nessa hora.
4. Promova o bot a administrador com **Gerenciar tópicos** e **Apagar mensagens**.
5. Mande qualquer mensagem no grupo e rode
   `curl -s "https://api.telegram.org/bot<TOKEN>/getUpdates" | jq '.result[-1].message | {chat: .chat.id, voce: .from.id}'`.
   O `chat` começa com `-100`; o `voce` é o seu `user_id`.

**2. Os arquivos.** Ponha o token em `~/.config/lukadispatch/.env`, e o `chat_id` e o seu
`user_id` em `~/.config/lukadispatch/config.toml`:

```toml
usuario = "Maria"                # como a sessão chama você; sem isto, "o seu usuário"

[telegram]
chat_id = -1001234567890
allowed_user_ids = [123456789]   # quem o bot obedece; vazio nega todo mundo

[scan]
roots = ["~/Projetos"]           # tudo com .git aqui dentro aparece no /new
```

O [`config.example.toml`](dist/config.example.toml) comenta todas as opções: modo de permissão
padrão, projetos fixados, motor de voz, divisor de arquivos.

**3. Ligar.**

```bash
lukadispatch install --global          # os hooks do Claude Code (desfaz com uninstall)
systemctl --user enable --now lukadispatch
```

O `install --global` escreve os hooks completos só nas sessões que o bot abre. No
`~/.claude/settings.json` ele põe apenas telemetria, para as sessões que você abre no terminal
aparecerem no painel. Nessas, as perguntas continuam no seu terminal. Os hooks são assíncronos:
com o daemon fora do ar, nenhuma sessão sua trava.

## Usar

No tópico **General**, onde fica o painel:

| Comando | Faz |
|---|---|
| `/new` | mostra os projetos em botões; se o projeto já tem conversa, pergunta se continua ou começa do zero |
| `/new <projeto> [modelo] [esforço]` | abre direto, por exemplo `/new api opus high` |
| `/ls` | redesenha o painel |
| `/kill <id>` | fecha uma sessão |

No **tópico de uma sessão**, qualquer mensagem vai para o Claude. Além disso:

| Comando | Faz |
|---|---|
| `/model` | escolhe o modelo, incluindo os que o menu do Claude Code não mostra e as variantes de 1M |
| `/effort` | escolhe o nível de esforço |
| `/mode` | troca o modo de permissão: auto, perguntar sempre, plano, liberar tudo |
| `/kill` | fecha a sessão e apaga o tópico |

**Voz.** A mensagem de voz é transcrita na sua máquina e volta como card com três saídas:
**Enviar**, **Descartar**, ou **responder ao card** com a correção, quando só uma palavra saiu
errada. Enquanto houver um card esperando (transcrição, pergunta ou permissão), o tópico só
aceita a resposta a ele e comandos; texto solto volta como aviso, com o que você tinha escrito.

**Arquivos.** O que você manda no tópico chega à sessão como um caminho em disco. Para mandar
algo de volta, peça: a sessão sabe como. Acima de 50 MB, vídeo é cortado por tempo (cada parte
toca sozinha no celular) e o resto vai em volumes de 7z, que o ZArchiver abre a partir do
`.001`. Chave, token e `.env` só saem cifrados para uma chave pública que você fornecer na
conversa.

**Permissões.** Com o modo `auto`, o classificador do Claude Code decide antes de haver pergunta,
então nenhum card aparece. Para aprovar cada ação pelo celular, use `/mode` e escolha "perguntar
sempre".

**No PC:**

```bash
lukadispatch ls                       # as sessões e o consumo de cada uma
lukadispatch new <projeto>            # abre uma sessão sem passar pelo Telegram
lukadispatch send <id> "texto"        # manda uma mensagem para ela
lukadispatch send-file <caminho>      # devolve um arquivo pelo tópico da sessão
lukadispatch kill <id>
tmux attach -t ld-<projeto>-<id>
```

## Segurança

- O bot só obedece aos `allowed_user_ids`. Qualquer outro update é descartado sem ser lido.
- O token existe só no `.env` (modo 600), nunca no `config.toml` nem no git.
- O socket de controle fica no seu runtime dir, com permissão 0600.
- As sessões rodam com os seus privilégios, nos seus projetos. Quem controla o grupo controla
  essas sessões: não adicione mais ninguém nele.

## Para ir além

- [`docs/`](docs/README.md): como o projeto é por dentro, por que cada peça é como é, como trocar
  o Telegram, o agente ou o motor de voz, e as armadilhas já medidas.
- [`docs/contribuir.md`](docs/contribuir.md): rodar sem bot, os testes, o CI, o deploy e como
  sai uma release.
- [`CHANGELOG.md`](CHANGELOG.md): o que mudou em cada versão.

Licença [MIT](LICENSE).
