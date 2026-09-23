# Arquitetura

O lukadispatch dirige as sessões de Claude Code de uma máquina por um aplicativo de mensagens.
Cada sessão ganha um canal de conversa (no Telegram, um tópico de um supergrupo); o canal
principal (o tópico General) é um painel com o estado de todas elas.

## Os binários

| Binário | Crate | Papel | Vive quanto |
|---|---|---|---|
| `lukadispatchd` | `ld-daemon` | o daemon: fala com o chat, sobe e mata sessões, mantém o painel, serve o socket | sempre (serviço systemd de usuário) |
| `lukadispatch` | `ld-cli` | os hooks do Claude Code, o `listen` que o Monitor da sessão lê, e os comandos locais | milissegundos por chamada |
| `lukadispatch-ask` | `ld-ask` | a janela GTK4 de pergunta e permissão no PC | enquanto a pergunta estiver aberta |
| `lukadispatch-mcp` | `ld-mcp` | proxy de stdio entre a sessão e cada servidor MCP, para o diálogo do servidor caber no celular | a vida do servidor MCP |

`ld-core` é a biblioteca que os quatro compartilham: config, protocolo do socket, banco,
leitura de transcript. Ela é magra de propósito (sem tokio, sem Telegram, sem GTK), porque o
binário de hook a carrega a cada chamada de ferramenta do Claude.

## As camadas do daemon

```
                 ┌──────────────────────────────────────────────┐
  chat           │  adaptadores de frontend                     │
  (Telegram,     │  frontend::telegram   frontend::nulo         │
   WhatsApp...)  │  frontend::memoria (testes)                  │
                 └───────────────┬──────────────────────────────┘
                                 │ trait Frontend  (Evento entra, envia/edita/apaga sai)
                 ┌───────────────▼──────────────────────────────┐
                 │  domínio                                     │
  hooks  ──────► │  roteador   app   cards   confirmacao        │
  (socket)       │  hub   panel   status   arquivos             │
                 └──┬───────────┬───────────┬────────────┬──────┘
       trait        │ Agente    │Transcritor│ Divisor    │ Hospedeiro
                    │ +Envelope │           │            │
                 ┌──▼────────┐┌─▼────────┐┌─▼─────────┐┌─▼────────┐
                 │ claude-   ││ processo ││ video, 7z,││ tmux     │
                 │ code;     ││ (whisper)││ rar       ││          │
                 │ ai-memory ││          ││           ││          │
                 └───────────┘└──────────┘└───────────┘└──────────┘
```

O domínio é o que o projeto É: a regra de que só a resposta pedida vai para o canal, a guarda
que segura o canal enquanto há pergunta aberta, o card de aval da transcrição, a ordem entre
texto e arquivo na resposta. Ele não sabe que o Telegram existe, nem que o agente é o Claude
Code. Tudo que ele pede ao mundo passa por uma das portas, e cada porta é uma trait com
implementações trocáveis pelo `config.toml`. O detalhe de cada uma está em [portas.md](portas.md), e o porquê do desenho em
[decisoes/0002](decisoes/0002-portas-e-adaptadores.md).

As portas são montadas uma vez, em `main.rs`, e entregues ao `App` num `Portas`. Nenhum
módulo do domínio escolhe implementação; ele recebe.

**Uma partida de sessão** mostra as portas colaborando sem que uma saiba da outra: o domínio
monta um `PedidoDePartida` (projeto, modelo, modo de permissão e a `DescricaoDoChat` tirada do
frontend); o `Agente` diz como o Claude Code é chamado e escreve o prompt; o `Envelope` embrulha
isso em `ai-memory run --new <workstream>`; `agente::escreve_partida` grava o script; e o
`Hospedeiro` roda o script num tmux ([decisoes/0012](decisoes/0012-agente-e-envelope.md)).

## O caminho de uma mensagem

**Do celular para a sessão.**

1. O adaptador do frontend recebe o update da plataforma, descarta quem não está na allowlist,
   tira o reply automático do fórum e entrega um `Evento` neutro
   ([decisoes/0005](decisoes/0005-seguranca-no-adaptador.md)).
2. O `roteador` decide o que o evento é: comando no canal principal, comando no canal de uma
   sessão, resposta a um card, anexo, ou conversa.
3. Conversa vai para `App::on_incoming`, que entrega ao `Hub`. O `Hub` tem o `listen` da
   sessão (o processo que o `Monitor` dentro do Claude Code está lendo) e escreve uma linha
   NDJSON nele. Sem monitor armado, a mensagem espera na fila do SQLite.
4. Anexo é baixado pelo adaptador para um diretório da sessão e entregue como caminho. Voz
   segue outro caminho: vira texto pelo `Transcritor`, fora do turno, e só chega à sessão
   depois do seu aval num card ([decisoes/0007](decisoes/0007-transcricao.md)).

**Da sessão para o celular.**

1. Os hooks do Claude Code (`lukadispatch hook <evento>`) falam com o daemon pelo socket
   unix. Nenhum hook fala com o chat.
2. `PreToolUse` e `PostToolUse` viram a mensagem de status do canal ("⚙️ Bash: cargo test"),
   editada no lugar e com debounce ([decisoes/0010](decisoes/0010-painel-e-status.md)).
3. `Stop` entrega a resposta do turno, se alguém a pediu
   ([decisoes/0009](decisoes/0009-marca-de-pedido.md)). Linhas `@arquivo:` na resposta viram
   envio de arquivo, na posição em que foram escritas, e o que não cabe no teto do frontend é
   partido pelo `Divisor` ([decisoes/0008](decisoes/0008-arquivos.md)).

**Pergunta e permissão.** O hook de pergunta (`PreToolUse:AskUserQuestion`) e o portão de
permissão abrem, ao mesmo tempo, um card no canal e a janela GTK4 no PC. Vale quem responder
primeiro: o árbitro é o próprio hook, que roda dentro do tmux com o ambiente gráfico da sessão
([decisoes/0001](decisoes/0001-ponte-por-eventos.md), [decisoes/0006](decisoes/0006-permissoes.md)).

## Onde o estado mora

| Estado | Onde | Por quê |
|---|---|---|
| sessões, vínculo sessão e canal, fila de mensagens, marca de pedido, id do painel | SQLite em `~/.local/state/lukadispatch/state.db` | precisa sobreviver a restart do daemon |
| quem está ouvindo, perguntas abertas | `Hub`, em memória | uma conexão `listen` e um hook bloqueado morrem junto com o daemon; persistir seria mentir |
| cards de pergunta, fila de transcrições | `Cards` e `Confirmacoes`, em memória | valem por um instante e só fazem sentido com o card à vista |
| anexos recebidos | `~/.local/share/lukadispatch/arquivos/<sessao>/` | fora do projeto (senão `git add -A` leva) e fora do `/tmp` (tmpfs, vira RAM presa) |
