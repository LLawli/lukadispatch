# 0004. A formatação do domínio é um subconjunto de HTML, traduzido pelo adaptador

Decidido em 23/09/2026, como consequência de [0002](0002-portas-e-adaptadores.md).

## Contexto

O daemon escreve muita coisa formatada: status, painel, cards, avisos. Até aqui era HTML do
Telegram, montado com `format!` e escapado à mão. Com o frontend trocável, cada plataforma tem a
sua sintaxe (o WhatsApp usa `*negrito*`, `_itálico_` e crase), e o domínio não pode escolher uma.

Havia duas saídas: um tipo estruturado (uma árvore montada com construtores) ou uma marcação
textual que o adaptador traduz.

## Decisão

Uma marcação textual mínima: `<b>`, `<i>`, `<code>` e `<pre>`, com `&`, `<` e `>` escapados por
`formato::escapa`. É o que o domínio já escrevia, então a mudança não tocou nas dezenas de textos
existentes. E é a marcação que o Telegram aceita como está, então o adaptador dele não traduz
nada.

Para as outras plataformas, `formato::trechos` lê a marcação e devolve uma árvore
(`Texto`, `Negrito`, `Italico`, `Codigo`, `Bloco`); o adaptador percorre e escreve na sintaxe
dele. `formato::texto_puro` é o caso de plataforma sem formatação nenhuma.

A leitura é tolerante: etiqueta desconhecida vira texto literal e etiqueta que não fecha fecha no
fim. Quem escreve a marcação é o próprio daemon, e erro aqui pode custar formatação, nunca a
mensagem.

**Texto do agente não passa por marcação nenhuma.** A resposta do Claude vai por
`Frontend::envia_texto`, crua: ela tem crase, asterisco e sublinhado o tempo todo, e interpretar
isso faria a mensagem ser recusada (erro 400 do Telegram em MarkdownV2) ou sair deformada.

## Consequências

- Texto vindo de fora (seu, do agente, nome de arquivo) que entra num texto formatado tem de
  passar por `escapa`. Esquecer isso é o bug clássico: um `<` num nome de arquivo quebra o card.
- Fechamento explícito evita a ambiguidade do Markdown sobre onde um negrito termina.

## Quando reabrir

Se a marcação precisar crescer (link, lista, citação) ou se um segundo frontend exigir traduções
que a árvore não expressa, troque por um tipo estruturado montado pelo domínio.
