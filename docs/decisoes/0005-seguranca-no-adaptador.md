# 0005. Allowlist e reply automático são responsabilidade do adaptador

Decidido em 23/09/2026, como consequência de [0002](0002-portas-e-adaptadores.md).

## Contexto

Duas regras de entrada não podem falhar:

- **Só pessoas autorizadas mandam no bot.** Um bot que controla sessões com acesso à máquina e
  aceita qualquer um é um backdoor. Por isso a allowlist vazia nega todo mundo.
- **"Respondeu a qual mensagem" tem de ser verdade.** Num fórum do Telegram, **toda** mensagem
  dentro de um tópico chega com `reply_to_message` preenchido, apontando para a mensagem que
  abriu o tópico, cujo id é o próprio `thread_id`. Ler esse campo direto dá "respondeu" sempre.
  Em 22/09/2026 isso desligou em silêncio a guarda de pendência e a correção de transcrição por
  reply, com 228 testes verdes: nenhum teste montava uma mensagem com a forma real do fórum.

As duas dependem de detalhes que só a plataforma conhece: o formato da identidade (id numérico,
telefone) e as manias do protocolo (reply automático, mensagens de serviço).

## Decisão

As duas regras moram no adaptador, na tradução de entrada, e fazem parte do contrato da trait
`Frontend`:

- só vira `Evento` quem está na allowlist; mensagem de bot (inclusive a de serviço que o próprio
  bot gera ao criar tópico) nunca vira;
- `responde_a` só carrega resposta feita de propósito pela pessoa.

No Telegram, a tradução é a função pura `frontend::telegram::traduz`, testada com payloads na
forma real (incluindo a raiz do tópico como `reply_to_message`).

O domínio confia no evento que recebe. Isso é deliberado: se ele tentasse revalidar, precisaria
conhecer a identidade da plataforma, que é justamente o que a porta esconde.

## Consequências

- Um adaptador novo precisa reimplementar as duas regras, e o guia em
  [portas.md](../portas.md) diz isso no passo da tradução.
- A allowlist do Telegram continua em `[telegram] allowed_user_ids`. Um frontend novo traz a sua
  seção de config.

## Quando reabrir

Se houver dois frontends com a mesma identidade de pessoa (por exemplo, o mesmo usuário por
Telegram e WhatsApp) e for preciso uma autorização única, suba para o domínio um conceito de
pessoa autorizada, com o adaptador só traduzindo identidade.
