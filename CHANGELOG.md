# Changelog

O formato segue o [Keep a Changelog](https://keepachangelog.com/pt-BR/1.1.0/) e as versões
seguem o [Versionamento Semântico](https://semver.org/lang/pt-BR/). A release copia a seção da
versão para as notas do GitHub Release, então escreva para quem vai decidir se atualiza.

## [Unreleased]

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

[Unreleased]: https://github.com/LLawli/lukadispatch/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/LLawli/lukadispatch/releases/tag/v0.1.0
