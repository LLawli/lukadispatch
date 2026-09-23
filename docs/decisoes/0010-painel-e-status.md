# 0010. O canal principal é um painel editado, e o status de cada sessão é uma mensagem só

Decidido em 21/09/2026.

## Contexto

Um turno do Claude produz um evento por ferramenta, às vezes vários por segundo, e as plataformas
limitam edições por mensagem (o Telegram responde 429 e a mensagem congela justo quando seria
útil). O canal principal, por sua vez, é o único que o Telegram não deixa apagar: se ele
acumulasse conversa, viraria entulho permanente.

## Decisão

**O canal principal é o painel, e só.** Uma mensagem fixada, editada no lugar, com o contexto e
os tokens de cada sessão viva (inclusive as abertas no terminal, se a telemetria global estiver
instalada) e as janelas de limite de 5 h e 7 dias da conta. Comando, resposta e teclado ali são
efêmeros: somem sozinhos depois de alguns segundos.

- O id da mensagem do painel fica no banco, para um restart adotar o painel existente em vez de
  deixar um morto para trás.
- Os tempos são relativos ("reseta em 2h13"): hora absoluta exigiria descobrir o fuso, e a
  leitura do fuso local falha em programa com várias threads.
- Os números de uso vêm do banco do XClaudeUsage, que envelhece quando nenhuma sessão roda; o
  painel mostra a idade deles.

**O status de cada sessão é uma mensagem só**, editada enquanto o turno anda, por uma tarefa por
sessão que junta os eventos: edita no máximo a cada 1,2 s, e quando vários eventos chegam durante
a espera, vale o último. Estado intermediário que ninguém chegou a ver não é perda. A mensagem é
apagada quando a resposta chega, e o id dela fica no banco para sobreviver a restart.

## Consequências

- Nenhum dos dois conhece a plataforma: pedem `envia`, `edita`, `apaga` e `fixa` ao `Frontend`.
  Numa plataforma sem fixar, `fixa` pode não fazer nada.
- Numa plataforma que limita o tempo de edição (o WhatsApp, 15 minutos), o adaptador resolve
  apagando e reenviando; ver [portas.md](../portas.md).

## Quando reabrir

Se a plataforma nova não tiver um lugar permanente equivalente ao General, o painel pode virar
um comando (`/ls`) em vez de uma mensagem viva.
