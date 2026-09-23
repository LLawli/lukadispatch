# 0012. O agente de código é uma porta, e o `ai-memory run` é um envelope separado dele

Decidido em 23/09/2026, junto com [0002](0002-portas-e-adaptadores.md).

## Contexto

O Claude Code estava espalhado pelo daemon: o script de partida montava `claude --session-id
--settings --permission-mode ...`, o prompt de bootstrap mandava carregar a ferramenta `Monitor`,
o `App` lia o catálogo de modelos do binário do Claude Code e o transcript `.jsonl`, a lista de
modos de permissão era a do Claude Code, e a confiança de pasta mexia no `~/.claude.json`. Um
agente diferente (o Codex, por exemplo) exigiria reescrever o domínio.

E a sessão não roda o `claude` direto: ela roda `ai-memory run --new <workstream> claude ...`,
para entrar na memória de longo prazo. O `ai-memory` também embrulha outros agentes, então ele não
é parte do Claude Code, e o Claude Code não é parte dele.

## Decisão

Duas traits, em `crates/ld-daemon/src/agente/`:

- **`Agente`** (implementação: `ClaudeCode`) diz como o agente é chamado (`invocacao`: programa,
  argumentos e prompt inicial), prepara a máquina (os ganchos em `bot-settings.json`), tira da
  frente o diálogo de confiança de pasta, e responde o que o domínio pergunta sobre ele: catálogo
  de modelos, níveis de esforço, modos de permissão (e quais são recusados), a resposta de um
  turno, o histórico da conversa, o contexto ocupado e o uso da conta.
- **`Envelope`** (implementações: `AiMemory` e `Direto`) recebe a linha de comando do agente e a
  embrulha. O nome de workstream único, e o 409 do `ai-memory` que ele evita, são conhecimento do
  `AiMemory`, não do agente.

O script de partida é código neutro (`agente::escreve_partida`): junta envelope e invocação,
escreve o prompt num arquivo ao lado, e o `Hospedeiro` (o tmux) só roda o script. Três portas
colaboram numa partida, e cada uma sabe só a sua parte.

O **prompt de partida é do agente, mas o que ele diz sobre o chat vem do frontend**: o domínio
passa uma `DescricaoDoChat` (plataforma, onde a conversa acontece, teto de arquivo, se Markdown
aparece formatado). Assim trocar o Telegram pelo WhatsApp não exige mexer no prompt do Claude
Code, e trocar o Claude Code não exige saber qual é o chat.

Config:

```toml
[agente]
tipo = "claude-code"
envelope = "ai-memory"   # ou "nenhum"
```

## Consequências

- As leituras continuam em `ld_core` (o CLI também as usa); o `ClaudeCode` as reúne atrás da
  trait, com os caminhos explícitos em `Locais`, o que permite testá-lo num tempdir.
- **O lado de entrada do agente não é uma trait do daemon, e sim do CLI.** Quem traduz o que o
  agente emite (os ganchos) para o protocolo do socket é `lukadispatch hook <agente> <evento>`,
  com um tradutor por agente atrás da trait `Ganchos` (`crates/ld-cli/src/hook/`), e o protocolo
  (`ld_core::proto`) é neutro. Os ganchos gerados escrevem `hook claude <evento>`; sem agente,
  `hook <evento>` continua sendo o Claude Code, para os settings já instalados não quebrarem.
- **O que um agente novo precisa ter**, porque o lukadispatch depende disso e não há como
  emular: um jeito de receber mensagem no meio do turno (o Claude Code tem a ferramenta
  `Monitor`, que lê o `lukadispatch listen`), ganchos de fim de turno e de ferramenta, e um
  ponto onde uma decisão de permissão seja honrada (no Claude Code, o `PreToolUse`; ver
  [0006](0006-permissoes.md)). Sem conversa gravada legível, o domínio só perde o histórico ao
  retomar e o contador de contexto.

## Quando reabrir

Quando um segundo agente for implementado de verdade: é a primeira hora em que se descobre o
que a trait assumiu do Claude Code sem perceber. Prefira capacidade opcional (método com
implementação padrão que devolve "não suportado") a forçar o segundo agente a fingir.
