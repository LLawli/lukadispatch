# lukadispatch

Dirige as sessões de Claude Code desta máquina pelo Telegram. Cada sessão vira um tópico de um
supergrupo: você conversa com ela ali, e fechar a sessão apaga o tópico. O tópico General, que o
Telegram não deixa apagar, carrega um painel editado ao vivo com o contexto e os tokens de cada
sessão e as janelas de limite de 5h e 7 dias.

**Não há injeção de teclas nem leitura de terminal em lugar nenhum.** Os três canais são
determinísticos:

| Canal | Mecanismo |
|---|---|
| Telegram para o Claude | um `Monitor` armado dentro da sessão lê `lukadispatch listen`; cada mensagem vira um evento no meio do turno |
| Claude para o Telegram | hook `Stop` manda a resposta; `PreToolUse`/`PostToolUse` editam a mensagem de status |
| Perguntas e permissões | hooks `PreToolUse:AskUserQuestion` e `PermissionRequest` abrem card no Telegram **e** janela GTK4 no PC; vale quem responder primeiro |

Nada disso depende de o agente resolver avisar alguém: quem fala é o hook.

## Componentes

- `lukadispatchd`: o daemon. Fala com o Telegram, cria e mata as sessões em tmux, mantém o
  painel e serve o socket de controle.
- `lukadispatch`: o CLI. É o binário dos hooks (`lukadispatch hook <evento>`), o `listen` que o
  Monitor consome e os comandos locais (`ls`, `kill`, `new`, `send`, `install`).
- `lukadispatch-ask`: a janela GTK4 que aparece no PC quando há pergunta ou permissão.

As sessões sobem como `ai-memory run claude` dentro de um tmux próprio, então continuam
aparecendo na memória de longo prazo e dá para anexar no PC com `tmux attach -t ld-<projeto>-<id>`.

## Pré-requisitos

- Claude Code 2.1.274 ou mais novo (é a versão que traz `PermissionRequest`, `async` e
  `asyncRewake` nos hooks).
- tmux e `ai-memory` no PATH.
- Rust 1.85+ para compilar; gtk4 e libadwaita para a janela de pergunta.

## Configurar o Telegram

1. Fale com o [@BotFather](https://t.me/BotFather), mande `/newbot` e guarde o token.
2. Em `/mybots > seu bot > Bot Settings > Group Privacy`, **desligue** o modo de privacidade.
   Sem isso o bot só recebe mensagens que começam com `/`, e o ponto aqui é conversar normal.
3. Crie um grupo, adicione o bot e abra `Editar > Tópicos` para ligar os tópicos. O Telegram
   converte o grupo em supergrupo nessa hora.
4. Promova o bot a administrador com **Gerenciar tópicos** e **Apagar mensagens**. Sem
   "Gerenciar tópicos" ele não cria nem apaga tópico nenhum.
5. Descubra o `chat_id`: mande qualquer mensagem no grupo e rode
   `curl -s "https://api.telegram.org/bot<TOKEN>/getUpdates" | jq '.result[-1].message.chat.id'`.
   Um supergrupo começa com `-100`.
6. Descubra o seu `user_id` na mesma saída (`.result[-1].message.from.id`).

## Instalar

```bash
./ci.sh                                   # fmt, clippy, testes e build release
install -Dm755 target/release/lukadispatchd    ~/.local/bin/lukadispatchd
install -Dm755 target/release/lukadispatch     ~/.local/bin/lukadispatch
install -Dm755 target/release/lukadispatch-ask ~/.local/bin/lukadispatch-ask

mkdir -p ~/.config/lukadispatch
cp .env.example ~/.config/lukadispatch/.env   # preencha token e chat_id
chmod 600 ~/.config/lukadispatch/.env

lukadispatch install --global                 # hooks: sessões do bot + telemetria da máquina
install -Dm644 dist/lukadispatch.service ~/.config/systemd/user/lukadispatch.service
systemctl --user daemon-reload
systemctl --user enable --now lukadispatch
```

`~/.config/lukadispatch/config.toml`:

```toml
default_permission_mode = "auto"
trust_projects = true            # marca a pasta como confiada antes de abrir (veja abaixo)

[telegram]
chat_id = -1001234567890
allowed_user_ids = [123456789]   # vazio nega todo mundo, de propósito

[scan]
enabled = true
roots = ["~/Personal", "~/Projetos"]
depth = 1

[[projects]]
name = "lukadispatch"
path = "~/Personal/lukadispatch"
# permission_mode = "acceptEdits"   # opcional, por projeto
```

### O que `install --global` faz, e o que ele não faz

Ele escreve duas coisas diferentes, e a diferença é deliberada:

- `~/.local/state/lukadispatch/bot-settings.json`: os hooks das sessões **do bot**. Inclui os que
  respondem por você (pergunta e permissão vão para o Telegram).
- `~/.claude/settings.json`: **só telemetria** (`SessionStart`, `SessionEnd`, ferramentas,
  notificações), para as sessões que você abre no terminal entrarem no painel. O menu nativo de
  pergunta continua sendo o seu nessas sessões, que é o certo para quem já está no teclado.

Todos os hooks de telemetria são `async: true`: o Claude Code dispara e não espera. Se o daemon
estiver fora do ar, o hook tenta subir o serviço (`systemctl --user start`) e sai. **Nenhum hook
consegue travar uma sessão sua.**

Para sair sem deixar rastro: `lukadispatch uninstall` (ele faz backup `.bak` antes de escrever).

## Usar

No tópico General:

- `/new` mostra os projetos em botões; `/new <projeto>` abre direto.
- `/ls` lista as sessões vivas.
- `/kill <id>` fecha uma.

Dentro do tópico de uma sessão, qualquer mensagem vai para o Claude. `/kill` ali fecha a sessão e
apaga o tópico.

No PC:

```bash
lukadispatch ls
lukadispatch send <id> "texto"     # entrega sem passar pelo Telegram
lukadispatch kill <id>
tmux attach -t ld-<projeto>-<id>   # a sessão é um tmux de verdade
```

## Coisas que mordem (e já morderam)

- **`ai-memory run` só aceita um workstream ativo por vez.** Ele responde `409 Conflict:
  workstream is already active`, e a sessão morre ao subir. Por isso cada sessão do bot pede um
  workstream próprio (`--new lukadispatch-<id>`). O nome precisa ser inédito: `--new` com nome
  existente também dá 409.
- **`--fresh` não combina com `--session-id`.** O ai-memory recusa com "cannot be combined with a
  native session".
- **Diálogo de confiança de pasta.** Abrir uma pasta que o Claude Code ainda não confia mostra um
  diálogo esperando tecla, e pelo celular isso aparece como uma sessão muda. A confiança vale para
  a árvore, então com a sua home confiada quase nada cai nesse caso; para o resto, `trust_projects`
  marca a pasta antes de abrir (só para projetos que o próprio config oferece, nunca um caminho
  qualquer).
- **O `Monitor` expira em 30 minutos.** O hook `Stop` percebe que o canal caiu e, via
  `asyncRewake`, acorda a sessão para re-armar. Depois de três lembretes sem sucesso o daemon
  desiste e avisa no tópico em vez de insistir para sempre.
- **Caminho de socket unix tem limite de tamanho** (`SUN_LEN`, ~108 bytes). O padrão
  (`$XDG_RUNTIME_DIR/lukadispatch.sock`) cabe folgado; um caminho de teste muito fundo não.
- **Texto do agente vai sem `parse_mode`.** Resposta de Claude tem crase, asterisco e colchete o
  tempo todo; em MarkdownV2 isso vira erro 400 e a mensagem não chega.

## Desenvolver e testar sem bot

Dá para exercitar o sistema inteiro (tmux, hooks, Monitor, fila) sem Telegram nenhum:

```bash
export LUKADISPATCH_OFFLINE=1
cargo run -p ld-daemon
lukadispatch new <projeto>
lukadispatch ls
lukadispatch send <id> "quanto é 2+2?"
tmux attach -t ld-<projeto>-<id>     # para ver o que a sessão fez
```

Em modo offline nenhum tópico é criado e nada é enviado; o resto funciona igual.

`./ci.sh` roda fmt, clippy com `-D warnings`, os testes e o build release.

## Segurança

- Só os `allowed_user_ids` são obedecidos; qualquer outro update é descartado sem ser lido.
- O socket de controle fica no runtime dir do usuário (0700) e recebe 0600. Sem token, porque
  quem consegue abrir o arquivo já é o dono da máquina.
- O token do bot só existe no ambiente, nunca no config nem no git.
