# 0003. Id de canal e de mensagem é texto opaco, e o banco antigo continua valendo

Decidido em 23/09/2026, como consequência de [0002](0002-portas-e-adaptadores.md).

## Contexto

O banco guardava `topic_id` e `status_message_id` como `INTEGER`, porque eram ids do Telegram.
Um grupo do WhatsApp é `120363025246125486@g.us`, e uma mensagem do WhatsApp é
`3EB0C767D82A1B5E9B2F`: não cabem num inteiro. E o banco que já existe na máquina tem essas
colunas como inteiro, com sessões vivas gravadas nelas.

## Decisão

- No domínio, `Canal` e `MsgId` são texto que só o adaptador interpreta. O domínio guarda,
  compara e devolve, e nunca olha dentro.
- No banco, as **colunas mantêm os nomes** (`topic_id`, `status_message_id`). Renomear exigiria
  recriar a tabela (o SQLite não renomeia coluna com segurança em todas as versões que se
  encontram por aí) e não compraria nada além de estética. Banco novo declara as duas como
  `TEXT`.
- **Leitura compatível:** a coluna é lida como valor dinâmico do SQLite e convertida (inteiro
  vira texto decimal). Um tópico gravado como `630` pela versão antiga volta como `"630"`, e a
  busca pelo texto `"630"` o acha.
- O campo no Rust mudou de nome (`canal_id`, `status_msg_id`), e o protocolo do socket aceita o
  nome antigo (`topic_id`) na leitura, para um `lukadispatch` de versão anterior não quebrar.

## Consequências

- O adaptador do Telegram converte texto em `ThreadId` e `MessageId` na borda. Um id que não é
  número (sobra do modo offline, `nulo-...`, ou de outro frontend) é tratado como "não existe",
  e a varredura de canal vazado o limpa do banco.
- O teste `banco_antigo_com_id_inteiro_continua_funcionando` cria um banco com o esquema antigo e
  confere leitura, busca e escrita nova.

## Quando reabrir

Se o esquema precisar de outra migração que recrie a tabela `sessions`, aproveite para renomear
as colunas para `canal_id` e `status_msg_id` e declarar `TEXT`.
