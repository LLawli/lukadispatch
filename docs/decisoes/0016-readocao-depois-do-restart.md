# 0016. O daemon relança no mesmo tópico a sessão que caiu com o servidor do herdr

Decidido em 25/09/2026, na issue #1, depois de medir o herdr 0.8.2. Depende de
[0015](0015-vida-pelo-processo.md).

## Contexto

Um restart do servidor do herdr (`herdr server stop`, `herdr update` sem `--handoff`, reboot)
mata o processo de cada sessão do bot. O herdr restaura o layout: o pane volta com o mesmo
rótulo, como um shell. Medido numa sessão nomeada:

- Com a integração do Claude Code instalada, o herdr religa sozinho, sem cliente anexado,
  `claude --resume <id>` no pane que a integração registrou como agente. O religado não tem o
  `--settings` do bot: sem hooks, sem pergunta pelo Telegram e sem o `Monitor`, ou seja, um
  Claude vivo que não recebe as mensagens do tópico.
- Parar o servidor dispara o hook `SessionEnd` com o motivo `other` em cada Claude, e o daemon
  encerrava a sessão ali mesmo, apagando o tópico.
- Sem `HERDR_ENV` e `HERDR_PANE_ID` no ambiente, a integração não registra o pane, e ele volta
  como um `bash` parado, sem filhos.

## Decisão

- **O daemon relança** (escolha do Luka): fecha o pane restaurado e sobe a sessão pelo caminho
  normal, com `--resume`, no mesmo id e no **mesmo tópico**, com um aviso de que ela caiu e
  voltou. Volta idêntica a uma troca de modelo: settings, hooks, modo, modelo, MCP, envelope,
  `script` e o `Monitor` rearmado pelo prompt. Rejeitadas: adotar o Claude que o herdr religa
  (fica sem os hooks do bot) e só oferecer um botão de retomar (a sessão ficaria parada até
  alguém ver).
- **O herdr não religa os panes do bot, e só eles**: o comando do pane faz
  `unset HERDR_ENV HERDR_PANE_ID` antes da partida. O `resume_agents_on_restore` continua
  valendo para as sessões do usuário. Custo nenhum no estado idle/working: o herdr já não o via
  nos panes do bot (ver as armadilhas).
- **O hospedeiro diz `Situacao::Restaurada`** quando o processo morreu e o pane do rótulo existe.
  A reconciliação relança uma vez; se falhar, encerra a sessão e avisa no canal principal, em
  vez de tentar de novo a cada volta.
- **O `SessionEnd` com `other` não encerra sessão do bot na hora**: agenda uma reconciliação para
  dali a 5 s. Os outros motivos (`/exit`, `/clear`, logout) continuam encerrando na hora, e a
  sessão aberta no terminal também, porque a reconciliação não cuida dela.
- **Com o processo morto e o servidor fora do ar, o `situacao` sobe o servidor** (escolha do
  Luka): depois de um reboot ninguém mais o sobe, e é o restore dele que devolve o lugar da
  sessão. Conferir sessão viva continua sem subir servidor.
- **Uma reconciliação por vez**: além do relógio de um minuto, o `SessionEnd` agenda voltas, e
  duas ao mesmo tempo relançariam a mesma sessão duas vezes.
- **A readoção não respeita a guarda de "trabalhando"** do `/model`: o turno em andamento se
  perdeu com o processo.

## Consequências

- Na sessão padrão do herdr, o servidor que o daemon sobe depois de um reboot religa também os
  agentes do usuário, sem terminal aberto. Com `[herdr] sessao`, só o bot é afetado.
- Um `herdr server stop` feito de propósito é desfeito em segundos se houver sessão do bot para
  retomar.
- Uma sessão encerrada de propósito não volta: o `kill` fecha o pane antes, e o servidor que
  para limpo grava o layout sem ele. Se o servidor cair nos 5 s de debounce do save, o pane
  volta, e a varredura de órfãs o fecha pelo rótulo.
- O que estava em andamento no turno se perde, e o aviso no tópico diz isso.

## Quando reabrir

- Se o herdr ganhar um jeito de desligar o religamento por pane na API, ou de o religamento
  levar o argv inteiro do pane, o `unset` pode sair.
- Se o herdr passar a emitir evento de restore concluído no `events.subscribe`, a detecção pode
  deixar de depender da reconciliação.
