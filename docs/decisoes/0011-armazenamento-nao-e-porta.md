# 0011. O SQLite não é uma porta, e o que faria isso mudar

Decidido em 23/09/2026, junto com [0002](0002-portas-e-adaptadores.md).

## Contexto

Ao separar o daemon em portas, o armazenamento era candidato natural a mais uma trait. Ele é uma
dependência externa (SQLite via `rusqlite`), e o objetivo era poder trocar qualquer parte.

## Decisão

O armazenamento continua concreto (`ld_core::state::Store`), por três razões:

- **Não há troca plausível.** O estado é pequeno (sessões vivas, uma fila curta, meia dúzia de
  chaves), é de uma máquina só, e o SQLite embutido é a escolha certa para isso em qualquer
  cenário previsível. Uma porta sem segunda implementação real é custo sem retorno.
- **Os testes já não dependem de disco.** `Store::open_memory()` dá um banco em memória, e é o
  que os testes de fluxo usam. A razão de testabilidade, que justificou o `Hospedeiro`, não vale
  aqui.
- **O CLI também lê o banco**, e o CLI é std síncrono de propósito. Uma trait assíncrona para o
  armazenamento teria de ter duas versões ou arrastar o tokio para o caminho quente dos hooks.

O que foi feito, em vez da porta, foi tirar do banco o que era do Telegram: os ids viraram texto
opaco ([0003](0003-ids-opacos.md)).

## Consequências

- Mudar o esquema é mexer em `ld-core/src/state.rs`, com migração em `migra` e teste de banco
  antigo, como foi feito com os ids.

## Quando reabrir

Se o daemon passar a rodar em mais de uma máquina com estado compartilhado, ou se o estado
precisar sair da máquina (backup contínuo, painel web), aí sim o `Store` vira uma trait, com a
implementação SQLite atual como a primeira.
