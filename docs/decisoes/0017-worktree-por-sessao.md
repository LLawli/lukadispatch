# 0017. Cada sessão do bot roda na git worktree da branch dela, fora do repositório

Decidido em 25/09/2026, numa entrevista com o Luka. Troca dois pontos do hospedeiro herdr como
ele entrou (PR #3): a sessão rodava na pasta do projeto, sem worktree, e `[herdr] sessao` vazio
era a sessão padrão do herdr. Muda também uma consequência da [0016](0016-readocao-depois-do-restart.md):
sem `[herdr] sessao` no config, o servidor que o daemon sobe depois de um reboot é o da sessão
própria do bot, e não religa mais os agentes do usuário.

## Contexto

Toda sessão do bot rodava na pasta do projeto. Duas sessões no mesmo projeto pisavam uma na
outra (o mesmo checkout, a mesma branch, os mesmos arquivos), e uma sessão do bot mexia no
checkout que você usa no terminal. O `/new` só sabia escolher o projeto.

O que o git impõe, e que molda o resto: a mesma branch não fica em checkout em duas worktrees, e
a worktree é uma raiz git própria (um arquivo `.git`), então o Claude Code pede confiança de
pasta de novo e guarda a conversa pelo cwd novo.

## Decisão

- **Uma worktree por branch, com qualquer hospedeiro** (tmux ou herdr). Ela mora em
  `~/.local/share/lukadispatch/worktrees/<caminho do projeto a partir do $HOME>/<branch>`: fora
  do repositório (o `git status` fica limpo e nenhuma ferramenta que varre o repo vê cópias),
  fora das raízes do `[scan]` (senão viraria projeto no `/new`), e pelo caminho do projeto, não
  pelo nome, porque `~/Personal/api` e `~/Trabalho/api` existem ao mesmo tempo. A pasta de uma
  branch não muda nunca: o `--resume` acha a conversa pelo cwd.
- **O `/new` é pasta, projeto, branch, e continuar ou do zero.** As pastas são as raízes do
  `[scan]`, mais "Fixados" para os `[[projects]]` fora delas. O seletor mostra só branches locais
  (sem fetch), as worktrees do bot primeiro.
- **A branch principal nunca abre direto**: escolhê-la pede o nome de uma branch nova a partir
  dela, digitado ou gerado (`ld/<data>-<hora>`). Branch em checkout noutro lugar vira base de
  uma nova do mesmo jeito.
- **Uma sessão por worktree.** Escolher a branch de uma sessão aberta aponta o canal dela.
- **`/new <projeto> <branch>`** abre direto; branch que não existe nasce da principal.
  **`/new new-project`** cria o projeto numa das pastas, com `git init` e um commit vazio, e abre
  a sessão na pasta dele (um projeto recém-nascido não tem checkout de ninguém a proteger).
- **O commit inicial vai sem assinatura.** A chave de quem assina pode pedir um toque físico, e
  quem pediu está no celular. É o único commit que o bot faz.
- **Projeto sem git, ou sem commit, abre na pasta**, sem worktree, como antes.
- **`/kill` numa worktree pergunta**: fechar e manter a worktree, ou apagar worktree e branch.
  Com arquivo sem commit ou commit que não está em remoto nenhum nem na principal, diz quanto e
  pede confirmação antes do `branch -D`. O `lukadispatch kill` do CLI e o fim por outro caminho
  (crash, `/exit`) sempre mantêm.
- **O banco guarda as worktrees do bot numa tabela nova do `state.db`**, e não atrás de uma porta
  de armazenamento. O pedido original era "estado plugável"; a [0011](0011-armazenamento-nao-e-porta.md)
  continua valendo, porque o gatilho dela (estado em mais de uma máquina) não aconteceu.
- **O herdr sobe as sessões do bot numa sessão própria, `lukadispatch`**, e não na padrão, onde
  estão as do usuário. `[herdr] sessao = "default"` volta ao comportamento antigo.
- **Canal, painel e `/ls` chamam a sessão de `projeto · branch`**, para duas do mesmo projeto não
  se confundirem. A sessão continua registrada com o nome do projeto, que é o que agrupa as abas
  no workspace do herdr.

## Consequências

- O que a sessão pede ao agente por projeto (os servidores MCP do `~/.claude.json`, a regra de
  permissão do `[[projects]]`) vem do repositório, e não da worktree, que não tem entrada. O
  pedido de partida passou a levar a pasta do repositório (`raiz`).
- Worktree cuja pasta foi apagada à mão some do `/new` (ele só lista a que existe em disco), e
  escolher a branch de novo a recria no mesmo lugar.
- Os botões do `/new` carregam só um número (`e:<n>`) e a escolha mora no daemon: caminho e nome
  de branch não cabem nos 64 bytes do dado de um botão do Telegram. Um restart do daemon invalida
  os teclados abertos.
- Sessão aberta no herdr antes da troca para a sessão própria continua viva na padrão; o `/kill`
  a encerra pedindo ao agente que saia, porque o socket do hospedeiro já é outro.

## Quando reabrir

- Se o seletor precisar de branches remotas (trabalho vindo de outra máquina), com o custo de um
  fetch com rede e credencial a cada `/new`.
- Se aparecer um segundo lugar que precise das worktrees fora deste daemon (outra máquina, outro
  processo que não lê o `state.db`): aí o gatilho da 0011 aconteceu.
- Se o Claude Code passar a guardar a conversa por repositório, e não por cwd: a pasta estável
  deixa de ser exigência.
