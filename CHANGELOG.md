# Changelog

O formato segue o [Keep a Changelog](https://keepachangelog.com/pt-BR/1.1.0/) e as versões
seguem o [Versionamento Semântico](https://semver.org/lang/pt-BR/). A release copia a seção da
versão para as notas do GitHub Release, então escreva para quem vai decidir se atualiza.

## [Unreleased]

### Adicionado

- Cada sessão aberta pelo bot roda numa git worktree da branch dela, em
  `~/.local/share/lukadispatch/worktrees`, fora do repositório. Duas sessões no mesmo projeto não
  pisam mais uma na outra, nem no checkout que você usa no terminal. Vale para tmux e herdr.
- O `/new` pergunta a pasta, o projeto e a branch, e se continua a conversa anterior daquela
  branch. A branch principal nunca abre direto: escolhê-la pede o nome de uma branch nova, digitado
  ou gerado. Branch que já tem sessão aberta aponta o canal dela.
- `/new <projeto> <branch>` abre direto na branch, criando-a se não existir.
- `/new new-project` cria um projeto numa das suas pastas (com `git init` e um commit vazio, sem
  assinatura) e abre a sessão nele.
- `/kill` numa sessão de worktree pergunta se mantém a worktree para continuar depois ou apaga
  worktree e branch, e avisa antes de apagar arquivo sem commit ou commit sem push.
- Com o ai-memory, a sessão numa worktree grava na memória do projeto (e não num projeto com o nome
  da branch), cada worktree tem o próprio registro de workstream, e o estado da branch fica numa
  página `worktrees/<branch>` em vez de handoff. O handoff manual que você deixou no terminal não
  é mais consumido por uma sessão do bot.

### Mudado

- Com o herdr, as sessões do bot sobem numa sessão própria, `lukadispatch`, e não mais na padrão,
  ao lado das suas. `[herdr] sessao = "default"` volta ao jeito antigo. Sessão aberta antes da
  atualização continua na padrão até ser fechada.
- Relançar ou fechar uma sessão pede primeiro ao agente que saia, e só depois derruba o terminal:
  com o ai-memory, o fim da conversa entra no registro, e uma troca de modelo não esbarra mais no
  workstream preso.
- O canal, o painel e o `/ls` mostram a sessão como `projeto · branch`.
- A escolha do ai-memory no config passa a se chamar `[agente] memoria` (`"ai-memory"` ou
  `"nenhuma"`), e no setup `--memoria`. O config com `envelope = "nenhum"` e a flag `--envelope`
  continuam valendo; o setup, ao gravar, troca a chave velha pela nova.

## [0.2.3] - 2026-09-25

### Mudado

- `brew install` e `brew upgrade` não compilam mais nada: a fórmula instala o mesmo tarball da
  release que o `install.sh` e o mise usam.
- Daemon, CLI e proxy MCP saem estáticos (musl) e rodam em qualquer Linux. Antes exigiam glibc
  2.39, e não abriam, por exemplo, no Ubuntu 22.04 que a imagem oficial do Homebrew usa. Só a
  janela de pergunta do PC continua usando a gtk4 do sistema.

## [0.2.2] - 2026-09-24

### Corrigido

- A resposta de um turno podia sumir do chat. A regra de só mandar o que foi pedido (para o
  "Monitor rearmado" não virar mensagem) descartava também: a resposta final de um turno em que
  você tinha respondido a um card de pergunta ou de permissão, e a de um turno acordado pelo fim
  de um trabalho que a própria sessão deixou rodando (um CI, por exemplo). As duas agora chegam.

## [0.2.1] - 2026-09-24

### Corrigido

- Instalado pelo Homebrew, o serviço parava de subir depois de um `brew upgrade`: a unit do
  systemd e os hooks guardavam o caminho da pasta da versão (`Cellar/lukadispatch/<versão>`),
  que o upgrade apaga. Agora eles guardam o caminho estável pelo qual o lukadispatch é chamado.
  Quem já está nessa situação resolve com `lukadispatch setup`, que refaz a unit e os hooks.

## [0.2.0] - 2026-09-24

### Adicionado

- O `lukadispatch setup` oferece instalar a transcrição de voz, com dois motores: o whisper
  large-v3-turbo (erra menos, inclusive em jargão e palavra em inglês; baixa 574 MB) e o
  FastConformer-pt (~5x mais rápido e leve, 106 MB; erra mais em jargão e inglês). O programa vem
  pronto da release, o modelo tem o sha256 conferido, e a instalação só vale depois de transcrever
  um áudio de teste. Antes, sem o transcritor na máquina, o setup só oferecia desligar a voz.
- A release publica os programas de voz por arquitetura: `lukadispatch-whisper-linux-<arq>`
  (Vulkan e CPU) e `lukadispatch-sherpa-linux-<arq>`.
- `saida = "json"` no `[transcricao]`, para motor que imprime o texto numa linha JSON.
- `lukadispatch setup --refazer` pergunta de novo o que já está resolvido, para trocar de bot,
  de grupo ou de motor de voz.

### Mudado

- Rodar o `lukadispatch setup` de novo só resolve o que está pendente: o que já funciona (bot,
  grupo, preferências, motor de voz, hooks, serviço) é conferido e aparece como ok, sem
  pergunta. Sem flag, cada peça fica a que o config já tem, em vez de voltar ao padrão.

## [0.1.0] - 2026-09-23

Primeira versão pública.

### Adicionado

- Cada sessão de Claude Code vira um tópico de um grupo do Telegram: você conversa com ela pelo
  celular, e fechar a sessão apaga o tópico. `/new` abre sessão em qualquer projeto seu, com a
  opção de continuar a conversa anterior.
- Painel no tópico General com contexto e tokens de cada sessão e as janelas de limite de 5h e
  7 dias da conta.
- Perguntas e pedidos de permissão do Claude chegam como card com botões no celular e como janela
  no PC ao mesmo tempo; vale quem responder primeiro.
- `/model`, `/effort` e `/mode` trocam modelo, esforço e modo de permissão sem perder a conversa.
- Arquivo nos dois sentidos: anexo mandado no tópico chega à sessão como caminho em disco, e a
  sessão devolve arquivo escrevendo uma linha `@arquivo:` na resposta. Acima de 50 MB, vídeo é
  cortado por tempo e o resto vai em volumes de 7z.
- Mensagem de voz vira texto localmente (whisper.cpp por padrão, trocável no config) e só entra
  na sessão depois do seu aval, com a opção de corrigir a transcrição respondendo ao card.
- O que você digita no PC, num `tmux attach` da sessão, aparece no tópico.
- Diálogo de servidor MCP chega ao celular por um proxy entre a sessão e o servidor.
- Peças trocáveis pelo `config.toml`, sem recompilar: aplicativo de chat, agente, envelope,
  motor de transcrição e divisor de arquivos.
- `usuario` no config diz à sessão como chamar você.
- `lukadispatch setup` configura tudo conversando: cria o bot com você, acha o grupo e o seu id
  por uma mensagem que você manda nele, confere na API do Telegram o Group Privacy, os tópicos e
  os direitos do bot, e liga os hooks e o serviço. `--frontend`, `--agent`, `--session` e
  `--envelope` escolhem a implementação de cada peça.
- Instalação de um comando (`install.sh`, que emenda no setup), tarball por arquitetura com
  sha256, fórmula do Homebrew e instalação pelo mise.
- `hospedeiro` no config escolhe onde as sessões rodam (hoje, `tmux`).

### Corrigido

- `trust_projects` marcava a pasta como confiada só quando nenhum diretório acima era confiado.
  O Claude Code só herda a confiança até a raiz do repositório git, então um projeto com `.git`
  dentro de uma home confiada parava no diálogo de confiança, e do celular a sessão ficava muda.
- Uma sessão que morria ao subir podia sumir antes de a saída dela ser registrada, e o erro
  chegava sem o motivo. A partida agora espera o registro estar ligado.
- Com o `~/.claude/settings.json` ilegível, o `install` gravava por cima só os hooks do
  lukadispatch. Agora ele recusa e não altera nada.

[Unreleased]: https://github.com/LLawli/lukadispatch/compare/v0.2.3...HEAD
[0.2.3]: https://github.com/LLawli/lukadispatch/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/LLawli/lukadispatch/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/LLawli/lukadispatch/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/LLawli/lukadispatch/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/LLawli/lukadispatch/releases/tag/v0.1.0
