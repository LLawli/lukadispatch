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

- **Abrir uma sessão nova** em qualquer projeto seu com `/new`, pelo celular: pasta, projeto e
  branch. Cada branch roda numa git worktree própria, fora do repositório, então duas sessões no
  mesmo projeto não pisam uma na outra nem no checkout do seu terminal. Dá para continuar a
  conversa anterior daquela branch, ou criar um projeto novo (`/new new-project`).
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
- **Voltar para o teclado** quando quiser: cada sessão é um terminal de verdade, no tmux
  (`tmux attach -t ld-<projeto>-<id>`) ou numa aba do [herdr](https://herdr.dev), e o que você
  digitar lá também aparece no tópico.

## Instalar

Linux x86_64 ou aarch64, com o systemd de usuário.

```bash
curl -fsSL https://raw.githubusercontent.com/LLawli/lukadispatch/master/install.sh | sh
```

O script baixa o binário da última release, confere o sha256, instala em `~/.local/bin` e emenda
no `lukadispatch setup`, que conversa com você até o bot estar respondendo:

1. confere o que a máquina tem (Claude Code, tmux ou herdr, ai-memory) e diz o que falta;
2. pede o token do bot que você cria no [@BotFather](https://t.me/BotFather), e confere nele se
   o Group Privacy está desligado;
3. explica como criar o grupo com tópicos e, por uma mensagem que você manda nele, descobre
   sozinho o grupo e o seu id;
4. confere se o bot é administrador com os direitos que ele usa, e diz o que ajustar se não for;
5. pergunta seu nome, onde ficam seus projetos e o modo de permissão;
6. oferece instalar a transcrição de voz, com dois motores: o whisper large-v3-turbo, que erra
   menos (inclusive em jargão e inglês) e baixa 574 MB, ou o FastConformer-pt, ~5x mais rápido e
   leve (106 MB), bom para fala corrida; baixa, confere e testa num áudio antes de ligar;
7. grava o config e o `.env` (só você lê), e liga os hooks e o serviço, perguntando antes.

Rodar o `lukadispatch setup` de novo é seguro: ele oferece o que já está configurado e edita o
config no lugar, com backup. Rodar o `install.sh` de novo atualiza.

Outros caminhos de instalação (depois de qualquer um deles, `lukadispatch setup`):

```bash
brew install LLawli/tap/lukadispatch                 # Homebrew no Linux
mise use -g github:LLawli/lukadispatch               # mise
cargo install --locked --git https://github.com/LLawli/lukadispatch ld-daemon ld-cli ld-ask ld-mcp
```

Ou baixe o `lukadispatch-linux-<arq>.tar.gz` da [página de releases](https://github.com/LLawli/lukadispatch/releases)
e rode o `install.sh --de .` que vem dentro dele.

**Precisa ter:** [Claude Code](https://docs.claude.com/en/docs/claude-code) 2.1.274 ou mais
novo e `tmux` ou [herdr](https://herdr.dev) (basta um; o setup usa o tmux se houver, senão o
herdr). O daemon e o CLI são estáticos e rodam em qualquer Linux; a janela de pergunta no
PC usa a gtk4 e a libadwaita 1.5+ do sistema, e sem elas a pergunta segue só pelo chat. O
[ai-memory](https://github.com/akitaonrails/ai-memory) é opcional: sem ele, o setup oferece rodar
as sessões direto. Para dividir arquivos grandes: `7z` (ou `rar`) e `ffmpeg`. A transcrição de
voz também precisa do `ffmpeg`; o motor em si o setup instala.

### Peças

O setup escolhe uma implementação para cada peça. Os padrões são as que existem hoje:

```bash
lukadispatch setup --frontend telegram --agent claude-code --session tmux --memoria ai-memory
```

Com `--session herdr`, cada sessão vira uma aba no workspace do projeto, numa sessão própria do
herdr, `lukadispatch` (veja com `herdr --session lukadispatch`), separada das suas. Outra sessão
se pede em `[herdr] sessao` no config, e `"default"` põe as do bot ao lado das suas. Se o
servidor do herdr reiniciar, o daemon relança cada sessão no mesmo tópico, com a conversa
inteira; num `herdr update --handoff` ela nem cai. Depois de um reboot, é o daemon que sobe o
servidor do herdr quando há sessão para retomar.

Outras entram como implementação nova de cada porta ([`docs/portas.md`](docs/portas.md)), e o
setup passa a oferecê-las pelo nome. Todas as opções do config, comentadas, estão no
[`config.example.toml`](dist/config.example.toml).

### O que os hooks mexem

Os hooks completos (os que respondem pergunta e permissão pelo celular) valem só nas sessões que
o bot abre. No `~/.claude/settings.json` entra apenas telemetria, para as sessões que você abre no
terminal aparecerem no painel; nelas, as perguntas continuam no seu terminal. Os hooks são
assíncronos: com o daemon fora do ar, nenhuma sessão sua trava. `lukadispatch uninstall` tira
tudo, com backup.

Depois de uma atualização, o daemon confere na partida se a telemetria do `~/.claude/settings.json`
ainda chama um binário que existe, com os hooks desta versão, e a regrava se não chamar (com
backup em `settings.json.bak`). Ele só corrige a que você já instalou: sem `install --global`,
nada entra. Se os hooks chamarem outra instalação do lukadispatch que funcione, ela fica como
está.

## Usar

No tópico **General**, onde fica o painel:

| Comando | Faz |
|---|---|
| `/new` | pergunta a pasta, o projeto e a branch, e se continua a conversa anterior da branch ou começa do zero. A branch principal nunca abre direto: escolhê-la pede o nome de uma branch nova a partir dela |
| `/new <projeto> [branch] [modelo] [esforço]` | abre direto, por exemplo `/new api feat/login opus high`; branch que não existe é criada |
| `/new new-project` | cria um projeto (pasta, `git init` e um commit vazio) e abre a sessão nele |
| `/ls` | redesenha o painel |
| `/kill <id>` | fecha uma sessão; numa worktree, pergunta se a mantém ou apaga junto com a branch |

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
tmux attach -t ld-<projeto>-<id>      # ou, no herdr, o comando que a ficha do tópico mostra
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
