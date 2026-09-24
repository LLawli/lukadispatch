# 0009. Só vai para o canal a resposta de um turno que alguém pediu, e a marca mora no banco

Decidido em 21/09/2026.

## Contexto

O hook `Stop` dispara no fim de **qualquer** turno, e a sessão tem turnos que ninguém pediu: o
bootstrap, o re-arme depois de uma troca de modelo, e o re-arme do `Monitor`, que expira sozinho a
cada 30 minutos. Todos terminam com o agente dizendo algo como "Monitor rearmado", que ia para o
canal como se fosse resposta a você.

## Decisão

Só vai para o canal a resposta de um turno que alguém pediu, pelo chat ou pelo teclado do PC.
Quem entrega uma mensagem à sessão marca o pedido; o `Stop` consome a marca, uma vez só.

A marca mora no SQLite (`sessions.pedido`), consumida na mesma transação que a lê. Ela vivia em
memória, e trocar o binário do daemon entre a sua mensagem e o fim do turno apagava a marca: a
resposta inteira, com os arquivos marcados nela, era descartada em silêncio.

Dois casos contam como pedido além da mensagem (24/09/2026, depois de duas respostas sumirem
numa sessão real):

- **Responder a um card** (pergunta ou permissão, pelo chat ou pela janela do PC) marca o
  pedido. Quem respondeu está no turno, e a resposta final dele é para essa pessoa. O caso real:
  o turno foi acordado pelo fim de um CI, a sessão perguntou "faço o deploy?", a resposta veio
  pelo celular, e o resultado do deploy nunca chegou lá.
- **Turno aberto por trabalho da própria sessão** (a `task-notification` de um comando em
  segundo plano ou de um monitor dela) é entregue mesmo sem marca: ele continua o que foi pedido
  antes. O que abriu o turno se lê no transcript (`transcript::origem_do_turno`). Fica de fora o
  re-arme do canal, que chega como `Monitor "mensagens do ..." stream ended`: a descrição do
  monitor do canal é a constante `DESCRICAO_DO_CANAL`, a mesma no prompt que o arma e no
  classificador. E fica de fora a fala que é só recado de monitor, em qualquer turno.

Duas regras vizinhas, da mesma família:

- **A resposta é a fala que responde, não a última.** O agente costuma entregar o resultado e
  depois anunciar que re-armou o monitor. Quando a última fala é só esse anúncio, a resposta vem
  do transcript, a anterior do mesmo turno.
- **Eco não volta.** O hook `UserPromptSubmit` não distingue o que você digitou no PC do que
  chegou pelo celular. O que o daemon acabou de entregar (mesmo texto, nos últimos 120 s) não é
  republicado no canal.

## Consequências

- O `rekey` do `/clear` leva a marca junto: o pedido é da conversa, não do id.
- Reiniciar o daemon **no meio de um turno** ainda perde a marca daquele turno, porque ela foi
  gravada pelo processo antigo antes de o turno acabar. A receita para repor está em
  [armadilhas.md](../armadilhas.md).

## Quando reabrir

Se o Claude Code passar a dizer no payload do `Stop` o que originou o turno, a marca vira
desnecessária.
