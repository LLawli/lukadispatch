# Documentação do lukadispatch

O `README.md` da raiz diz que problema o lukadispatch resolve, como instalar e como usar. Esta
pasta é para quem vai mexer nele: como o projeto é por dentro, por que ele é assim, e como trocar
uma peça sem reescrever o resto.

| Documento | Para quando você quer |
|---|---|
| [arquitetura.md](arquitetura.md) | entender o todo: os binários, as camadas e o caminho de uma mensagem |
| [responsabilidades.md](responsabilidades.md) | saber onde mora uma coisa, e o que cada módulo se recusa a fazer |
| [portas.md](portas.md) | trocar o Telegram, o agente de código, o motor de voz, o divisor de arquivos ou o tmux |
| [decisoes/](decisoes/) | saber por que algo é como é, e o que faria a decisão mudar |
| [armadilhas.md](armadilhas.md) | não redescobrir do jeito difícil o que já foi medido |
| [contribuir.md](contribuir.md) | compilar, rodar sem bot, passar nos portões, fazer deploy e soltar uma versão |

## Decisões registradas

| # | Decisão |
|---|---|
| [0001](decisoes/0001-ponte-por-eventos.md) | A ponte com o Claude Code é por eventos e hooks, nunca por injeção de teclas |
| [0002](decisoes/0002-portas-e-adaptadores.md) | O daemon fala com o mundo por traits, escolhidas no config |
| [0003](decisoes/0003-ids-opacos.md) | Id de canal e de mensagem é texto opaco, e o banco antigo continua valendo |
| [0004](decisoes/0004-marcacao-neutra.md) | A formatação do domínio é um subconjunto de HTML, traduzido pelo adaptador |
| [0005](decisoes/0005-seguranca-no-adaptador.md) | Allowlist e reply automático são responsabilidade do adaptador |
| [0006](decisoes/0006-permissoes.md) | "Perguntar no celular" é `dontAsk` com um portão no `PreToolUse` |
| [0007](decisoes/0007-transcricao.md) | Voz vira texto por um motor escolhido no config, e só vai com o seu aval |
| [0008](decisoes/0008-arquivos.md) | Arquivo sai por marcador na resposta, e o que não cabe é partido por uma cadeia de divisores |
| [0009](decisoes/0009-marca-de-pedido.md) | Só vai para o canal a resposta de um turno que alguém pediu, e a marca mora no banco |
| [0010](decisoes/0010-painel-e-status.md) | O canal principal é um painel editado, e o status de cada sessão é uma mensagem só |
| [0011](decisoes/0011-armazenamento-nao-e-porta.md) | O SQLite não é uma porta, e o que faria isso mudar |
| [0012](decisoes/0012-agente-e-envelope.md) | O agente de código é uma porta, e o `ai-memory run` é um envelope separado dele |
| [0013](decisoes/0013-preview-em-texto.md) | O preview de uma pergunta vai como texto, não como imagem |

## Como escrever uma decisão nova

Um arquivo por decisão em `decisoes/`, numerado em sequência, com quatro partes: **Contexto**
(o problema, com o que foi medido), **Decisão** (o que se escolheu, em uma ou duas frases),
**Consequências** (o que fica mais fácil, o que fica mais caro) e **Quando reabrir** (o fato
novo que tornaria a decisão errada). A última parte é a que mais importa: decisão sem gatilho
vira dogma, e decisão reaberta sem gatilho vira retrabalho.
