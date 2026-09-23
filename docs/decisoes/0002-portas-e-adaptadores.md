# 0002. O daemon fala com o mundo por quatro traits, escolhidas no config

Decidido em 23/09/2026.

## Contexto

Até aqui o daemon falava com o Telegram de dentro de tudo: o `App` guardava um cliente do
teloxide, o roteamento lia `Update` e `Message` direto, os cards montavam
`InlineKeyboardMarkup`, e o banco guardava `thread_id` como inteiro. A transcrição era um módulo
de funções, e o 7z e o ffmpeg eram chamados do meio do envio de arquivo. Trocar qualquer uma
dessas peças (o chat pelo WhatsApp, o motor de voz, o 7z pelo RAR) exigiria reescrever o domínio.

## Decisão

Separar o domínio das dependências externas por portas, cada uma uma trait:

| Porta | Trait | Por que é uma porta |
|---|---|---|
| chat | `Frontend` | é o que mais provavelmente muda, e o que mais tinha vazado para o domínio |
| agente de código | `Agente` (+ `Envelope`) | o Claude Code é o agente de hoje, e o `ai-memory run` que o embrulha é independente dele ([0012](0012-agente-e-envelope.md)) |
| voz | `Transcritor` | a escolha depende do hardware; uma API remota é plausível |
| arquivo grande | `Divisor` | o formato de volume é gosto e compatibilidade do celular |
| sessão | `Hospedeiro` | isola o tmux, e permite testar o ciclo de vida sem tmux nenhum |

Escolhas de implementação da ideia:

- **Objeto dinâmico (`Arc<dyn Trait>`), não genérico.** A implementação é escolhida em tempo de
  execução, pelo `config.toml` (e o modo offline, por variável de ambiente). Com genéricos, o
  `App` e tudo que o toca ganhariam parâmetros de tipo, e o `main` precisaria de um ramo por
  combinação. O custo do despacho dinâmico é irrelevante perto de uma chamada de rede.
- **`async-trait`.** Trait com `async fn` nativo não serve como objeto dinâmico. A crate é o
  caminho padrão para isso e custa uma alocação por chamada, de novo irrelevante aqui.
- **As portas são montadas só no `main`**, num `Portas`, e entregues ao `App`. Nenhum módulo do
  domínio escolhe implementação.
- **Vocabulário neutro no `frontend`**: `Canal`, `MsgId`, `Botao`, `Evento`, `Anexo`, `Limites`.
  O domínio não vê tipo nenhum do teloxide.
- **O modo offline virou uma implementação** (`frontend::nulo`), em vez de um `bool` espalhado.
- **Um frontend em memória** (`frontend::memoria`) registra cada chamada. Os testes de fluxo
  (`tests/fluxos.rs`) rodam o domínio inteiro contra ele, com dublês das outras três portas.
  Se um fluxo precisasse de algo que não passa pela trait, ele não seria escrevível ali: os
  testes são a prova de que a trait basta.
- **O Telegram fica atrás da feature `telegram` do cargo** (ligada por padrão). O crate compila
  sem ela, e o CI confere isso: se algum módulo do domínio voltar a importar o teloxide, o build
  sem a feature quebra.

## Consequências

- Trocar o chat é escrever um módulo que implemente `Frontend` e registrá-lo no `main`
  ([portas.md](../portas.md)). Nada no domínio muda.
- Os ids do chat passaram a ser texto opaco, o que exigiu mexer no banco
  ([0003](0003-ids-opacos.md)), e a formatação passou a ser uma marcação neutra
  ([0004](0004-marcacao-neutra.md)).
- Segurança de entrada (allowlist, reply automático) passou a ser explicitamente do adaptador
  ([0005](0005-seguranca-no-adaptador.md)).
- Os limites da plataforma (tamanho de mensagem, de arquivo) vêm do frontend, e não de
  constantes: o divisor parte arquivo no teto do chat em uso.

## Quando reabrir

- Se surgir um segundo frontend de verdade e a trait precisar de um método que só ele usa,
  prefira capacidade opcional (método com implementação padrão) a enfiar o conceito no domínio.
- O armazenamento ficou de fora de propósito ([0011](0011-armazenamento-nao-e-porta.md)).
