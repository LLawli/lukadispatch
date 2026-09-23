# 0001. A ponte com o Claude Code é por eventos e hooks, nunca por injeção de teclas

Decidido em 21/09/2026, na criação do projeto.

## Contexto

O ponto de partida foi o `sdispath`, que controla sessões do Claude Code pelo celular anexando
uma ponte ao console, **injetando teclas** (`tmux send-keys`) e lendo o transcript. As próprias
decisões daquele projeto registram o preço: o `/clear` desalinha o id da sessão, aparece card
fantasma, e é estruturalmente impossível ter card no celular e menu no terminal ao mesmo tempo
(negar a ferramenta, que é o que o modo hook faz, é justamente o que impede o menu de aparecer).
Injeção também é frágil com a TUI: texto e `Enter` na mesma chamada costumam perder o texto.

## Decisão

Três canais determinísticos, nenhum dependendo de o agente resolver avisar alguém:

1. **Entrada:** a sessão arma a ferramenta `Monitor` rodando `lukadispatch listen`. Cada
   mensagem do chat vira uma linha no stdout desse processo, e portanto um evento no meio do
   turno. O prompt inicial é argumento do `claude`, não tecla injetada.
2. **Saída:** o hook `Stop` entrega a resposta; `PreToolUse` e `PostToolUse` editam a mensagem
   de status.
3. **Pergunta e permissão:** o hook abre card no chat **e** janela GTK4 no PC. O árbitro da
   corrida é o **hook**, não o daemon, porque o hook roda dentro do tmux com o ambiente gráfico
   da sessão (um serviço systemd pode não ter `WAYLAND_DISPLAY`).

A resposta a uma pergunta volta ao Claude pelo `permissionDecisionReason` de um `deny`, que é o
único texto que um hook consegue entregar; por isso o texto diz explicitamente que aquilo é a
resposta, e não uma recusa.

Regras que sustentam o desenho:

- **Nenhum hook pode travar uma sessão.** A telemetria é `async: true` (dispara e esquece), o
  `Stop` é `asyncRewake`, e o cliente do socket tem prazo em tudo. Daemon fora do ar vira "não
  decidi" e o hook sai com 0.
- **O settings global leva só telemetria.** Sessões abertas no terminal entram no painel, mas
  mantêm o menu nativo de pergunta: sequestrá-lo para o celular de quem está no teclado seria pior
  que não ter painel.
- **Cada sessão do bot pede um workstream próprio do ai-memory** (`--new lukadispatch-<id>`),
  senão a segunda sessão do mesmo projeto morre com 409.

## Consequências

- `/model` e `/effort` são comandos do frontend do Claude Code e nenhum evento os dispara. A troca
  é feita reiniciando o processo com `--resume <id>`, que volta com o mesmo transcript e o mesmo
  id. O custo é o Monitor, que precisa ser re-armado (o prompt de re-arme cuida disso).
- O `Monitor` expira em 30 minutos. O hook `Stop` percebe a falta de ouvinte e acorda a sessão
  para re-armar, com teto de três tentativas antes de declarar a sessão surda.

## Quando reabrir

Se o Claude Code ganhar uma API de controle de sessão (entrada de mensagem e eventos de saída)
estável, ela substitui o `Monitor` e os hooks de saída com menos peças.
