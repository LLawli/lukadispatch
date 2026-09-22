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
| PC para o Telegram | o que você digita no `tmux attach` vira mensagem no tópico (hook `UserPromptSubmit`), marcada como vinda do PC |
| Arquivos | anexo do Telegram é baixado para o disco e entregue como caminho; no sentido contrário, um marcador na resposta faz o hook `Stop` enviar o arquivo |

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

No tópico General (que se limpa sozinho: comando, resposta e teclado somem, e só o painel fica):

- `/new` mostra os projetos em botões. Quando o projeto já tem conversa anterior, ele pergunta
  entre **continuar de onde parou** e **começar do zero**; ao continuar, as últimas falas são
  despejadas no tópico, separadas por quem falou.
- `/new <projeto> [opus|claude-opus-4-8[1m]] [high|max]` abre direto, já com modelo e esforço.
- `/ls` redesenha o painel.
- `/kill <id>` fecha uma sessão.

Dentro do tópico de uma sessão, qualquer mensagem vai para o Claude. Além disso:

- `/model` abre a escolha em duas etapas (família, depois versão), com o catálogo lido do próprio
  binário do Claude Code: aparecem também os modelos que o menu `/model` dele não mostra, e as
  variantes de janela de 1M. `/model claude-opus-4-8[1m]` funciona direto, sem menu.
- `/effort` faz o mesmo para o nível de esforço.
- `/mode` troca o modo de permissão: auto, perguntar sempre, plano, liberar tudo. O modo é por
  sessão e sobrevive a um relançamento, então ele não volta ao padrão do projeto sozinho.
- `/kill` fecha a sessão e apaga o tópico.

**Arquivo nos dois sentidos.** Anexo que você manda no tópico (documento, foto, vídeo, animação,
nota de vídeo, figurinha, áudio, mensagem de voz) é baixado na hora para
`~/.local/share/lukadispatch/arquivos/<sessão>/` e chega à sessão como caminho absoluto, no campo
`files` da linha NDJSON e dentro do texto. A legenda vira a mensagem; sem legenda, o texto é a
própria linha do arquivo. O teto é 20 MB, que é o do Bot API. Os arquivos são apagados junto com a
sessão.

**Voz vira texto, depois do seu aval.** Mensagem de voz é transcrita fora do turno (ela leva mais
tempo que o hook `Stop` espera: a configuração padrão gasta ~44 s por minuto de fala), então o
tópico mostra `transcrevendo…` e, quando fica pronta, a transcrição aparece num card com três
saídas:

- **Enviar**: vai para a sessão como se você tivesse digitado, com o caminho do `.oga` junto.
- **Descartar**: some sem deixar rastro, e a sessão nunca soube que houve áudio.
- **Escrever qualquer coisa** no tópico: a transcrição vai junto com o que você escreveu, marcada
  como correção, e a sessão sabe que a versão escrita é a que vale. É para o caso de "está quase
  certo, só essa palavra" — reescrever a frase inteira anularia o ganho de ter falado.

O card responde ao áudio que o gerou, então a seta do Telegram diz de qual voz ele saiu. Resolvido,
ele perde os botões e vira o registro do que a sessão recebeu (`Transcrição` e, quando houve,
`Ratificação`); a mensagem que você digitou é apagada, para a mesma coisa não ficar espalhada em
três lugares. Só o descarte não deixa registro, que é o sentido dele. Áudio destinado à transcrição
também não gera o card de anexo com o caminho do arquivo.

**Um card por vez, em fila.** Dois áudios seguidos são duas mensagens suas e merecem duas decisões,
mas os dois cards juntos tornariam ambíguo a qual deles uma correção escrita se refere. Então o
segundo espera: resolvido o primeiro, o próximo sobe, como nos cards de pergunta e permissão. O
rodapé do card diz quantos ainda estão na fila. Tópicos diferentes têm filas independentes.

Comando (`/kill`, `/mode`…) continua sendo comando mesmo com um card aberto. O áudio guardado
expira em `transcricao.guardar_audio_dias`.

O motor é plugável e mora no `config.toml`, não no código, porque a escolha é do hardware. O padrão
é o que ganhou o benchmark num Ryzen 5700U com Radeon Vega sem VRAM dedicada: whisper.cpp com
`large-v3-turbo` quantizado em q5_0, no backend Vulkan (12,9% de erro por palavra em áudio real,
~1 GB de memória). Trocar é editar `transcricao.comando`, `transcricao.modelo` e `transcricao.saida`
— os marcadores são `{audio}` (WAV mono 16 kHz que o daemon prepara), `{modelo}` e `{saida}`. O tipo
`Transcricao` traz presets medidos para CPU e para o FastConformer-pt, que é 5x mais rápido e cabe
em 417 MB, cobrando quase o dobro de erro em jargão e nome próprio.

No sentido contrário, o agente **não chama ferramenta nenhuma**: ele escreve na resposta uma linha
sozinha com `@arquivo:` seguido do caminho absoluto (e, se quiser, ` | legenda`). O hook `Stop`
manda o arquivo antes do texto e tira a linha da mensagem. `@documento:` força documento quando os
bytes exatos importam; sem isso, imagem até 10 MB vai como foto e aparece na conversa.

Chave, credencial, token e `.env` não saem em claro: o prompt inicial manda a sessão cifrar para
uma chave pública sua antes de enviar, e recusar o envio enquanto você não tiver fornecido essa
chave na conversa. O tópico é um grupo do Telegram, e o Telegram guarda o arquivo nos servidores
dele.

Acima de 50 MB (o teto do Bot API) o arquivo não é recusado, e o corte depende do que ele é.
**Vídeo é cortado por tempo** com `ffmpeg -c copy`, sem recodificar: cada trecho é um vídeo de
verdade, vai como vídeo (com player) e toca sozinho no celular; remontar é opcional, com
`ffmpeg -f concat`. **O resto vai em volumes de 45 MB do 7z**, que o ZArchiver ou o RAR remontam a
partir do `.001` no celular, e `7z x nome.7z.001` no PC. Sem `ffmpeg`, ou quando o corte por tempo
falha, vídeo também cai nos volumes. O envio dividido sai em segundo plano, porque subir centenas
de MB demora mais que o prazo do hook `Stop`; o teto é 20 partes.

O reconhecimento é estreito de propósito: a linha precisa ser só o marcador, do começo ao fim.
Marcador no meio de uma frase, dentro de crase ou depois de hífen de lista é o agente falando do
formato, e não é envio. `lukadispatch send-file <caminho>` faz a mesma coisa pela linha de
comando, para quando você quer mandar algo do PC. O token continua só do lado do daemon.

Perguntas do Claude viram card com botões, e **também aceitam resposta escrita**: o que você
digitar no tópico com um card aberto é a resposta dele. Respondido, o card perde os botões e vira
o registro do que foi perguntado e escolhido; sem resposta (6h ou sessão morta), some.

**Como a troca de modelo funciona, e por que ela não perde nada.** `/model` e `/effort` são
comandos do frontend do Claude Code: nenhum evento consegue dispará-los, e digitar no terminal
está fora de questão aqui. O daemon reinicia o processo com `--resume <id>`, que volta com o
mesmo transcript e o mesmo id. A conversa continua exatamente de onde estava; o que se perde é o
Monitor, e a sessão recebe um prompt curto mandando armá-lo de novo.

No PC:

```bash
lukadispatch ls
lukadispatch models                  # catálogo lido do binário do Claude Code
lukadispatch new <projeto> [--continuar]
lukadispatch send <id> "texto"       # entrega sem passar pelo Telegram
lukadispatch send-file <caminho> --legenda "..." --session <id>   # devolve arquivo pelo tópico
lukadispatch model <id> claude-opus-4-8
lukadispatch effort <id> high
lukadispatch kill <id>
tmux attach -t ld-<projeto>-<id>     # a sessão é um tmux de verdade
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
- **O campo do `UserPromptSubmit` é `prompt`, não `user_prompt`.** A documentação diz o
  contrário, e o hook falha em silêncio se você seguir a documentação: o payload real traz `cwd`,
  `hook_event_name`, `permission_mode`, `prompt`, `prompt_id`, `session_id` e `transcript_path`.
- **`ai-memory run` só aceita um workstream ativo, e `--new` recusa nome repetido.** Por isso o
  workstream de um relançamento leva carimbo de tempo no nome.
- **Matar o tmux para relançar dispara o `SessionEnd`.** Sem uma marca de "relançando", o daemon
  entende isso como fim de sessão e apaga o tópico no meio da própria troca de modelo.
- **O cliente HTTP do teloxide tem timeout de 17s**, menor que um long polling de 30s. O daemon
  constrói o cliente com um teto maior; com o padrão, toda janela ociosa morre em erro de rede.
- **Texto do agente vai sem `parse_mode`.** Resposta de Claude tem crase, asterisco e colchete o
  tempo todo; em MarkdownV2 isso vira erro 400 e a mensagem não chega.
- **Com `auto` ligado, não há card de permissão para ver.** O classificador decide antes de o
  pedido virar prompt, e o mesmo vale para o diálogo próprio de um servidor MCP: ele nunca chega
  a ser acionado. Para exercitar permissão pelo celular, `/mode` e escolha "perguntar sempre".
- **Diálogo de servidor MCP (elicitation) não dá para responder pelo Telegram.** O hook existe,
  mas é só notificação: a documentação diz que a saída dele é ignorada nesse evento. O daemon
  avisa no tópico o que foi pedido e por qual `tmux attach` responder, e só.

## Caminho não percorrido: preview como imagem

As opções do `AskUserQuestion` podem trazer um `preview` (maquete em ASCII, trecho de código).
Hoje ele vai em bloco `<pre>` no Telegram e em fonte de largura fixa com rolagem lateral na
janela GTK4, e nos testes o desenho se manteve nos dois.

Se um dia aparecer uma maquete larga demais e o Telegram quebrar a linha em vez de rolar, o
conserto é **renderizar o preview como imagem** e mandar como foto, que é o que o `sdispath` faz
com todas as mensagens dele. A diferença proposta é fazê-lo **só nos previews que precisam** (por
exemplo, linha acima de ~40 colunas), mantendo o resto em texto, que é pesquisável e copiável.

Custo estimado: três crates (`usvg`, `resvg`, `tiny-skia`), montar um SVG com o texto em fonte
monoespaçada e rasterizar, mais a escolha da fonte em tempo de execução. Não foi feito porque o
`<pre>` resolveu; fica registrado para não ser redescoberto do zero.

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
