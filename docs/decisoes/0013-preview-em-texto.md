# 0013. O preview de uma pergunta vai como texto, não como imagem

Registrado em 23/09/2026, quando saiu do README, onde morava como "caminho não percorrido".

## Contexto

As opções do `AskUserQuestion` podem trazer um `preview`: maquete em ASCII, trecho de código,
exemplo de configuração. O card do Telegram e a janela GTK4 precisam mostrar isso sem quebrar o
desenho. O `sdispath`, que faz a mesma ponte por outro caminho, renderiza todas as mensagens como
imagem.

## Decisão

O preview vai em bloco `<pre>` no Telegram e em fonte de largura fixa com rolagem lateral na
janela GTK4. Nos testes, o desenho se manteve nos dois. Texto é pesquisável e copiável, e imagem
não é.

## Consequências

- Nenhuma dependência de rasterização no daemon.
- Uma maquete muito larga depende de o Telegram rolar a linha em vez de quebrá-la.

## Quando reabrir

Se aparecer uma maquete larga o bastante para o Telegram quebrar a linha. O conserto proposto é
renderizar como imagem **só os previews que precisam** (por exemplo, com linha acima de ~40
colunas) e mandar como foto, mantendo o resto em texto. O custo estimado: três crates (`usvg`,
`resvg`, `tiny-skia`), montar um SVG com o texto em fonte monoespaçada e rasterizar, mais a
escolha da fonte em tempo de execução.
