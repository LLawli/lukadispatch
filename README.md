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
| Claude para o Telegram | hook `Stop` manda a resposta; `PreToolUse`/`PostToolUse`/`MessageDisplay` editam a mensagem de status |
| Perguntas e permissões | hooks `PreToolUse:AskUserQuestion` e `PermissionRequest` abrem card no Telegram **e** janela GTK4 no PC; vale quem responder primeiro |

Nada disso depende de o agente resolver avisar alguém: quem fala é o hook.

## Componentes

- `lukadispatchd`: o daemon. Fala com o Telegram, cria e mata as sessões em tmux, mantém o
  painel e serve o socket de controle.
- `lukadispatch`: o CLI. É o binário dos hooks (`lukadispatch hook <evento>`), o `listen` que o
  Monitor consome e os comandos locais (`ls`, `kill`, `install`).
- `lukadispatch-ask`: a janela GTK4 que aparece no PC quando há pergunta ou permissão.

As sessões sobem como `ai-memory run claude` dentro de um tmux próprio, então continuam
aparecendo na memória de longo prazo e dá para anexar no PC com `tmux attach -t ld-<projeto>`.

## Pré-requisitos

- Claude Code 2.1.274 ou mais novo (é a versão que traz os hooks `MessageDisplay` e
  `PermissionRequest`).
- tmux, e `ai-memory` no PATH.
- Rust 1.85+ para compilar; gtk4 e libadwaita para a janela de pergunta.

## Configurar o Telegram

1. Fale com o [@BotFather](https://t.me/BotFather), mande `/newbot` e guarde o token.
2. Em `/mybots > seu bot > Bot Settings > Group Privacy`, **desligue** o modo de privacidade.
   Sem isso o bot só recebe mensagens que começam com `/`, e o ponto aqui é conversar normal.
3. Crie um grupo, adicione o bot e abra `Editar > Tópicos` para ligar os tópicos. O Telegram
   converte o grupo em supergrupo nessa hora.
4. Promova o bot a administrador com as permissões **Gerenciar tópicos** e **Apagar mensagens**.
   Sem "Gerenciar tópicos" ele não cria nem apaga tópico nenhum.
5. Descubra o `chat_id`: mande qualquer mensagem no grupo e rode
   `curl -s "https://api.telegram.org/bot<TOKEN>/getUpdates" | jq '.result[-1].message.chat.id'`.
   Um supergrupo começa com `-100`.
6. Descubra o seu `user_id` na mesma saída (`.result[-1].message.from.id`).

Depois:

```bash
cp .env.example .env     # preencha o token e o chat_id
mkdir -p ~/.config/lukadispatch
```

`~/.config/lukadispatch/config.toml`:

```toml
default_permission_mode = "auto"

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
```

## Compilar e rodar

```bash
./ci.sh                  # fmt, clippy, testes e build release
cargo run -p ld-daemon   # ou o serviço systemd, em dist/lukadispatch.service
```

## Segurança

- Só os `allowed_user_ids` são obedecidos; qualquer outro update é descartado.
- O socket de controle fica no runtime dir do usuário (modo 0700), então não carrega token.
- O token do bot só existe no ambiente, nunca no config nem no git.
