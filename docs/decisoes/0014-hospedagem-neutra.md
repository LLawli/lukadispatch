# 0014. O id da sessão no hospedeiro se chama `hospedagem`, no Rust e no banco

Decidido em 25/09/2026, ao entrar o segundo hospedeiro (o herdr), como consequência de
[0002](0002-portas-e-adaptadores.md).

## Contexto

O id que o hospedeiro devolve ao subir uma sessão morava em `Session.tmux`, na coluna
`sessions.tmux` e em `Launched.tmux`. Com o herdr, esse valor passa a ser o id de um terminal
dele, e o nome `tmux` mente sobre o que está gravado. A [0003](0003-ids-opacos.md) manteve os
nomes de coluna `topic_id` e `status_message_id` porque renomear coluna exigiria recriar a
tabela em SQLite antigo.

## Decisão

- O campo no Rust vira `hospedagem` (`Session::hospedagem`, `Launched::hospedagem`,
  `Store::hospedagem_de_sessao_morta`, que a [0015](0015-vida-pelo-processo.md) trocou por
  `rotulo_de_sessao_morta`). O valor continua opaco: só o hospedeiro que o gravou sabe o que é.
- A coluna **também** muda de nome, por `ALTER TABLE sessions RENAME COLUMN tmux TO hospedagem`
  no `migra`. O motivo da 0003 para não renomear não vale aqui: o `rusqlite` usa o SQLite
  embutido (`bundled`), que tem `RENAME COLUMN` desde a 3.25, e ele não recria a tabela. Em
  banco novo a coluna velha não existe e o erro é ignorado, como nas outras migrações.

## Consequências

- O teste `banco_antigo_com_coluna_tmux_vira_hospedagem` abre um banco com o esquema antigo,
  confere que a sessão viva mantém o valor e que reabrir não falha.
- Voltar para uma versão anterior do daemon depois de rodar esta quebra a leitura das sessões:
  a versão velha procura a coluna `tmux`. Rebaixar exige renomear a coluna de volta à mão.

## Quando reabrir

Se `topic_id` e `status_message_id` forem renomeados (o gatilho da 0003), use o mesmo
`RENAME COLUMN` em vez de recriar a tabela.
