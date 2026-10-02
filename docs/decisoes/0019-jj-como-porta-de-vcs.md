# 0019. O controle de versão vira porta, e o jj entra como opção ao lado do git

Decidido em 02/10/2026, numa conversa com o Luka, depois de medir o jj 0.45.1, o ai-memory 2.1.2 e
o Claude Code 2.1.285 num repositório descartável. Reabre a frase do módulo de worktree ("o git não
é uma porta: não há outro para pôr no lugar") e estende a [0017](0017-worktree-por-sessao.md) e a
[0018](0018-memoria-do-projeto-na-worktree.md).

## Contexto

O Luka está migrando para o jj (Jujutsu, colocado sobre o git) todos os projetos em que usa IA. O
jj da máquina dele tem dois guardas: um wrapper que roda `jj git init --colocate` sozinho em
repositório que só tem `.git`, e um hook que bloqueia escrita pelo git. Numa git worktree, o `.git`
é um arquivo: o wrapper não consegue colocar o jj ali, todo `jj` falha com "no jj repo" e o hook
barra o git. O agente da sessão fica sem como commitar.

O jj tem o equivalente, o workspace (`jj workspace add`), e ele é diferente da worktree em pontos
que mudam o desenho. Medido:

- **Não há `.git` no workspace**, só `.jj/` apontando para o repositório. O `git` falha dentro
  dele.
- **O mesmo bookmark pode ser base de vários workspaces.** O jj não tem a regra do git de uma
  branch em checkout num lugar só; cada workspace tem o seu commit `@`.
- **`jj workspace forget` não apaga nada**: a pasta fica (sem uso, "No working copy") e os commits
  com conteúdo continuam no repositório. Só o `@` vazio some.
- **`trunk()` só resolve bookmark remoto.** Sem remoto ele é o `root()`; a branch principal precisa
  de um fallback para os bookmarks locais.
- **Leitura sem `--ignore-working-copy` faz snapshot** do checkout em que roda e grava uma
  operação. Com a flag, nenhuma.
- **Escrita concorrente em dois workspaces se reconcilia sozinha** (10 commits simultâneos, nenhum
  divergente), e reescrever de fora o `@` ou o pai de um workspace foi absorvido sem perder arquivo
  sujo. O `jj workspace update-stale` é no-op quando não há o que recuperar.
- **Colocar o jj num repositório git que já tem worktrees git** não quebra as worktrees.
- **ai-memory sem `.git`**: a chave do workstream vira o cwd exato. Cada workspace vê só o seu,
  pedir o de outro dá 404, e um workspace recriado no mesmo caminho recupera o dele.
- **Claude Code sem `.git`**: não pede confiança (ele não trata `.jj` como raiz e sobe até a pasta
  confiada, como o `trust.rs` já supõe), roda com `Is a git repository: false` e sem `gitStatus`.
- **`gh pr create` sem `.git`** funciona com `--repo`, `--head`, `--title` e `--body`; sem `--repo`,
  falha procurando o repositório.

## Decisão

- **O controle de versão é uma porta, `vcs::Vcs`, com dois adaptadores: `git` e `jj`.** O jj é
  uma opção, não uma troca: o git continua sendo o padrão e o que roda em repositório de terceiro.
- **A escolha é do setup** (`lukadispatch setup --vcs jj`, chave `vcs = "jj"`), global, e não por
  projeto. Com o jj escolhido, um repositório que só tem `.git` recebe `jj git init --colocate`
  pelo daemon no primeiro `/new`, sem perguntar (pedido do Luka: criar worktree git atrasaria a
  migração). O daemon recusa se houver merge, rebase, cherry-pick ou bisect do git pela metade,
  como o wrapper.
- **Uso natural do jj, sem forçar o fluxo do git.** A identidade da sessão é o nome do workspace, e
  nenhum bookmark é criado pelo bot: o agente cria e avança o seu na hora de publicar. No `/new`,
  os workspaces do bot aparecem como hoje, e todo bookmark (o principal incluído) é base de um
  workspace novo, com o nome perguntado. Não há "em uso".
- **O `/kill` com jj oferece três saídas**: manter; apagar o workspace (`forget` e a pasta), que não
  perde commit nenhum e por isso não pede confirmação; e apagar abandonando os commits que são só
  dele, que pede confirmação quando há o que perder. Antes do `forget`, um snapshot no workspace
  grava o que o agente deixou sem commit. "Só dele" é
  `::ws@ ~ ::(trunk() | remote_bookmarks() | (working_copies() ~ ws@))`; as pendências contam
  contra `trunk()` e `remote_bookmarks()`, e não contra `bookmarks()`, senão o trabalho num
  bookmark local ainda não publicado sumiria da conta.
- **O workstream do ai-memory é um por workspace**, e funciona sem mudança no ai-memory, pela chave
  de cwd. O marcador da 0018 continua dando o projeto.
- **A porta contribui para a partida**: instruções no prompt (workspace, sem `.git`, bookmark na
  hora de publicar, `update-stale`, `gh` com `--repo`) e, opcionalmente, servidores MCP para
  injetar na sessão (`[jj] mcp`), espaço reservado para o MCP de jj que o Luka está escrevendo.
  Antes de cada partida num workspace o daemon roda `update-stale`.
- **Regras de toda chamada do daemon ao jj**: `--ignore-working-copy` em tudo que roda na pasta do
  repositório (o checkout do usuário), `--config signing.behavior=drop` (a chave pode pedir toque,
  e quem pediu está no celular; o mesmo motivo do `--no-gpg-sign` da 0017), template explícito e
  `--color=never` em leitura (alias e template da config do usuário não mudam a saída que o daemon
  lê), e `JJ_AUTO_INIT=0` (o wrapper nunca inicializa um repositório por efeito colateral).
- **O banco registra o vcs de cada worktree** (coluna `vcs`). As worktrees git que o bot abriu
  antes da troca continuam sendo git, e são apagadas pelo adaptador git.
- **A varredura de projetos aceita `.jj`**, além de `.git`: um repositório jj não colocado é
  projeto.

## Consequências

- O domínio continua falando "branch" com você: no jj, ela é o nome do workspace. Nada muda no
  Telegram, no CLI (`--branch`) nem no protocolo.
- Dentro de um workspace o agente não tem git: build script que chame `git rev-parse` (vergen e
  parecidos) quebra ali. O lukadispatch não tem nenhum.
- O CI passa a instalar o jj, e os testes do adaptador rodam contra o jj de verdade num tempdir,
  como os do git.
- Uma sessão aberta numa worktree git antiga de um projeto que virou jj continua funcionando, mas
  o agente ali não tem jj; o `/new` a marca como git.

## Quando reabrir

- Se o jj ganhar workspace colocado (com `.git` funcional): as instruções sobre `gh` e a ausência
  de git saem.
- Se o ai-memory passar a reconhecer o jj (por `.jj`), e a chave do workstream deixar de ser o cwd.
- Se a escolha global atrapalhar (um projeto que precisa continuar em git com o jj escolhido): aí a
  escolha passa a ser por projeto, no `[[projects]]`.
- Se o MCP de jj do Luka ficar pronto e cobrir o que o prompt ensina: o prompt encolhe.
