# 0015. A sessão herdr vive enquanto o processo dela vive, e a hospedagem começa pelo rótulo

Decidido em 25/09/2026, na issue #1, depois de medir o herdr 0.8.2. Refina
[0014](0014-hospedagem-neutra.md).

## Contexto

A hospedagem de uma sessão herdr era `rótulo@terminal`, e a sessão vivia enquanto o terminal
existisse. Medido numa sessão nomeada:

- **Restart do servidor:** o processo morre, e o pane volta com o mesmo rótulo e terminal novo.
- **Live handoff** (`herdr update --handoff`, `herdr server live-handoff`): o processo sobrevive,
  com o mesmo pid, e o terminal de **todo** pane muda do mesmo jeito.

Pelo terminal, os dois casos são iguais, e o daemon dava o segundo como morte: encerrava a
sessão viva, apagava o tópico e deixava o Claude rodando sem ninguém que falasse com ele. A
varredura de órfãs também não o achava, porque comparava a hospedagem inteira, terminal
incluído.

## Decisão

- A hospedagem herdr passa a ser `rótulo@terminal@pid:início`. O shell que o `script` abre
  escreve o próprio `$$` num arquivo antes do `exec`, e esse pid é o do agente até ele sair,
  porque cada `exec` troca o programa e mantém o pid. O início (campo 22 do `/proc/<pid>/stat`)
  distingue o nosso processo de outro que ganhe o mesmo pid depois de um reboot.
- **Viva é o processo existir.** O `vive` não pergunta ao servidor: um servidor lento no `ping`
  não mata mais sessão nenhuma.
- **O terminal continua na hospedagem** porque é o que o `herdr terminal attach` pede. Quando um
  handoff o troca, o hospedeiro devolve `Situacao::Mudou` com a hospedagem nova, e a
  reconciliação a grava.
- **O `mata` acha o pane pelo terminal gravado, e na falta dele pelo rótulo**, que é o que o
  pane mantém num restart e num handoff.
- **A hospedagem começa pelo rótulo**: ela é o rótulo, ou o rótulo seguido de `@`. O `nossas`
  devolve rótulos, e a varredura de órfãs os compara com `Store::rotulo_de_sessao_morta`. É a
  única parte da hospedagem que deixa de ser opaca fora do hospedeiro, e o tmux já cumpria a
  regra sem mudar nada: a hospedagem dele é o próprio rótulo.

Rejeitado: `rótulo@pane_id`, com a morte vindo do hook `SessionEnd`. O `pane_id` público
(`w1:p3`) sobrevive aos dois casos, mas o hook não chega com o daemon fora do ar (reboot), nem de
um Claude que morre sem rodar hook.

## Consequências

- A hospedagem gravada antes desta versão (`rótulo@terminal`) continua valendo pelo terminal,
  como era, até a sessão ser relançada.
- Vivacidade pelo `/proc`: o hospedeiro herdr só roda no Linux, como o resto do daemon.
- O teste `live_handoff_mantem_a_sessao_viva_e_troca_o_terminal` faz o handoff de verdade numa
  sessão nomeada.

## Quando reabrir

Se o herdr passar a manter o `terminal_id` no handoff, ou a expor o pid do processo do pane na
API, a vida pode voltar a ser perguntada a ele.
