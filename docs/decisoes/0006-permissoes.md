# 0006. "Perguntar no celular" é `dontAsk` com um portão no `PreToolUse`

Decidido em 21/09/2026, depois de medir no Claude Code 2.1.274. Contraria a documentação.

## Contexto

O modo "perguntar no celular" precisa de duas coisas: a sessão nunca pode travar esperando uma
tecla no terminal (ninguém está lá), e toda ferramenta que mereça pergunta tem de virar card.

A documentação diz que a decisão do hook `PermissionRequest` suprime o prompt. **Não suprime.**
Medido com um hook que NEGA (um hook que libera não prova nada, porque o classificador do modo
auto pode ter liberado sozinho):

| modo | hook nega | conclusão |
|---|---|---|
| padrão (`defaultMode`) | comando rodou | decisão do hook ignorada |
| `manual` | prompt na tela | decisão do hook ignorada |
| `dontAsk` | comando bloqueado | foi o modo; o hook nem é chamado |
| `PreToolUse` com `allow` | rodou sem prompt | **honrado**, inclusive dentro do `dontAsk` |

Três formatos de saída foram testados no `PermissionRequest`, e os três são ignorados.

## Decisão

O modo `perguntar` é do lukadispatch, não do Claude Code. Por baixo:

- a sessão roda em **`dontAsk`**: o terminal nunca abre prompt, então nunca trava;
- um **portão no `PreToolUse`** é consultado para **toda** ferramenta. O daemon decide: as
  ferramentas da lista `ask_tools` do config viram card no chat e janela no PC; o resto é
  **liberado explicitamente**, porque no `dontAsk` o que não for liberado é negado em silêncio.

O modo `manual` é recusado pelo `/mode`: ele mostra o card aqui, você responde, e o prompt
continua esperando teclado no PC.

## Consequências

- O `PermissionRequest` fica só como observador.
- A lista `ask_tools` cobre escrita, execução e MCP (`mcp__` casa o servidor inteiro); leitura
  passa sem pergunta.
- Diálogo de servidor MCP (`elicitation/create`) é outro mecanismo, fora do sistema de
  permissões, e o hook `Elicitation` é só notificação. Para ele caber no celular existe o proxy
  `lukadispatch-mcp`, que fica entre a sessão e cada servidor e responde o diálogo pela mesma
  corrida de card e janela.

## Quando reabrir

Quando uma versão nova do Claude Code honrar a decisão do `PermissionRequest`. Meça com um hook
que nega, em sessão de verdade, pedindo algo que precise de permissão (escrever arquivo; `ls` é
liberado sozinho até no modo manual).
