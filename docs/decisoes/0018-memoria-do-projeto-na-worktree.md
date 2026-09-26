# 0018. A sessão numa worktree grava na memória do projeto, e o registro dela é da worktree

Decidido em 25/09/2026, com o Luka, depois de medir o ai-memory 2.1.2 (código e documentação da
tag, e uma worktree de teste). Depende de [0017](0017-worktree-por-sessao.md). Troca o nome da
porta da [0012](0012-agente-e-envelope.md).

## Contexto

O requisito do Luka: a memória é do projeto real, vista por todas as worktrees; o registro das
ações de cada sessão (o workstream do ai-memory) fica preso à worktree, porque cada uma trabalha
numa coisa diferente. Nada do ai-memory muda (sem PR no upstream). Medido:

- **Projeto.** Sem `.ai-memory.toml`, os hooks do ai-memory mandam só o cwd, e o servidor dá à
  sessão o projeto `basename(cwd)`: a worktree `.../api/feat-x` viraria o projeto `feat-x`. Os
  comandos do CLI, na mesma pasta, resolviam o projeto certo, então a mesma sessão ficaria
  partida em dois. Não há variável de ambiente que force o projeto nos hooks.
- **Workstream.** A chave é `(workspace, projeto, repositório, worktree)`: numa worktree nova,
  "No managed workstreams for this checkout". Um escritor por vez, com trava de 90 s renovável;
  o `ai-memory run` espera 5 s por uma trava ocupada e desiste com 409. Um `ai-memory run`
  derrubado junto com o painel não solta a trava (ela expira sozinha) e não importa o fim da
  conversa; quando o agente sai e ele termina, solta e importa.
- **Handoff.** O automático (do `SessionEnd`) é entregue por cwd, à mesma pasta ou uma de dentro:
  fica na worktree. O manual (`memory_handoff_begin`) vale para o projeto inteiro, tem prioridade
  e é de uso único; o `cwd` dele é só registro. A sessão de uma worktree consumiria o handoff que
  o Luka deixou no terminal, e vice-versa. O webhook de admissão não recebe cwd nem id, e o dono
  do handoff só separa com autenticação multiusuário no servidor.

## Decisão

- **A porta `Envelope` passa a se chamar `Memoria`** (pedido do Luka): ela deixou de ser só o
  embrulho da linha de comando. Continua **opcional**: `[agente] memoria = "nenhuma"` roda o agente
  direto, e cada capacidade nova tem um padrão que não faz nada. A chave `envelope` e o valor
  `"nenhum"` continuam aceitos.
- **Marcador.** Antes de cada partida numa worktree, o `AiMemory` grava um `.ai-memory.toml` na
  pasta que junta as worktrees do projeto, com `workspace` e `project` explícitos, iguais aos que
  o repositório principal resolve pelas regras do marcador (o primeiro que declara escopo subindo
  até o `$HOME`; sem nenhum, `default` e o nome da pasta). Fora de qualquer checkout; um marcador
  versionado no repositório aparece dentro da worktree e continua vencendo.
- **Um workstream por worktree, com o nome da branch** (sem `/`, que o ai-memory recusa):
  `--new <branch>` na primeira partida, `--workstream <branch>` depois. Sem `--fresh`: o
  `--session-id` do agente já é um seletor explícito, e os dois juntos o ai-memory recusa. Fora
  de worktree, o de sempre: um workstream inédito por partida.
- **Trava presa.** A partida que morre com o workstream ocupado espera e tenta de novo, por até
  100 s; passado isso, sobe com um workstream só dela (`<branch>-<carimbo>`) em vez de deixar a
  pessoa esperando mais.
- **Parar com calma.** Antes de derrubar uma sessão (relançar, `/kill`), o daemon manda `SIGTERM`
  ao agente, filho do `ai-memory run`, e espera até 10 s: o ai-memory solta a trava e importa o
  fim da conversa. O `Hospedeiro` passou a dizer o pid do painel, e a `Memoria`, que processo
  sinalizar.
- **Página da branch no lugar do handoff manual.** A sessão da worktree é instruída, no prompt de
  partida, a ler `worktrees/<branch>.md` no primeiro pedido e a reescrevê-la ao fechar um trabalho,
  e a não usar `memory_handoff_begin`. A página não é consumida e todas as worktrees a veem. O
  `/kill` que apaga a worktree apaga a página junto.
- **Tirar da fila e devolver.** Antes de uma partida numa worktree, o daemon aceita os handoffs
  manuais abertos do projeto (com um cwd que não existe, para os automáticos ficarem de fora) e os
  recria com o mesmo conteúdo, na mesma ordem, depois que o `SessionStart` da sessão chegou (mais
  5 s, ou em até um minuto). Fala MCP por HTTP com o endpoint `/mcp` do servidor, no endereço
  que o `ai-memory status --json` diz (é a resolução do próprio ai-memory). Não pela ponte de
  stdio (`ai-memory mcp-bridge`): ela exige `CLAUDE_CODE_SESSION_ID` e sai sem ela, e o daemon não
  é uma sessão do Claude Code. O teste de ponta a ponta tinha passado com a ponte só porque o
  daemon do teste herdou essa variável do shell que o subiu, dentro de um Claude Code.

## Consequências

- O handoff manual do terminal sobrevive às sessões do bot. Um terminal que abrir nos poucos
  segundos entre tirar e devolver fica sem ele: aceito.
- O daemon passa a depender, com o ai-memory ligado, de `ai-memory status --json`,
  `ai-memory workstreams --json`, `ai-memory delete-page` e do endpoint `/mcp` por `http://`
  (com `AI_MEMORY_AUTH_TOKEN` no ambiente se o servidor pedir token). Falha em qualquer um deles vira aviso
  no log, nunca sessão que não sobe.
- A sessão nova numa worktree recebe do ai-memory o que ainda não viu do registro da branch, e
  isso gasta contexto. "Começar do zero" é uma conversa nova, não uma branch sem passado.
- Uma troca de modelo espera o agente sair antes de relançar (até 10 s).

## Quando reabrir

- Se o ai-memory ganhar handoff manual restrito ao checkout: o tirar e devolver sai.
- Se ele passar a resolver o projeto pelo repositório principal sem marcador (`repo-root` como
  padrão da instalação): o marcador sai.
- Se o ai-memory soltar a trava quando o `ai-memory run` recebe `SIGTERM`: a parada com calma
  pode sinalizar ele, e não o agente.
- Se o `ai-memory run` deixar de aceitar `--workstream` para um nome de outro checkout, ou mudar a
  chave do workstream.
