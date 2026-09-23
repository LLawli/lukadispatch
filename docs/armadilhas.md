# Armadilhas medidas

Tudo aqui foi descoberto rodando, não lendo documentação, e várias contradizem a documentação.
Quando um comportamento parecer estranho, procure aqui antes de "consertar".

## Claude Code

- **A decisão do hook `PermissionRequest` é ignorada** em todos os modos. Quem decide permissão por
  hook é o `PreToolUse`. Detalhes em [decisoes/0006](decisoes/0006-permissoes.md).
- **`--settings <arquivo>` mescla com o settings global**, não substitui.
- **`async: true` num hook é disparar e esquecer** (saída, código e decisão descartados).
  `asyncRewake: true` roda em segundo plano e acorda o Claude quando o hook sai com código 2.
- **O campo do `UserPromptSubmit` é `prompt`, não `user_prompt`** como diz a documentação. Para
  qualquer dúvida sobre payload de hook: um hook mínimo que faz `tee` num arquivo, rodado com
  `claude -p --settings`, mostra o payload inteiro.
- **`/model` e `/effort` são do frontend do Claude Code.** Nada os dispara; a troca é reiniciando
  com `--resume <id>` ([decisoes/0001](decisoes/0001-ponte-por-eventos.md)).
- **Matar o processo dispara o `SessionEnd`.** Quem mata para relançar precisa marcar isso (é a
  janela de relançamento do `App`), senão o próprio código trata a troca de modelo como fim de
  sessão.
- **A ferramenta `Monitor` é diferida** (o agente precisa carregá-la com `ToolSearch` antes) e
  **expira em 30 minutos**.
- **Diálogo de confiança de pasta** trava a partida esperando tecla. A confiança é herdada de um
  diretório acima **só até a raiz do repositório git** (medido no Claude Code 2.1.280): uma home
  confiada cobre pasta solta, mas não um projeto com `.git`. O daemon marca a pasta do projeto
  antes de abrir a sessão, com a mesma regra de subida que o Claude Code usa (`ld-core::trust`).
- **Com o modo `auto`, não há card de permissão para ver.** O classificador decide antes de o
  pedido virar prompt, e o mesmo vale para o diálogo de um servidor MCP. Para exercitar permissão
  pelo celular, use `/mode` e escolha "perguntar sempre".
- **O binário do Claude Code carrega o catálogo de modelos inteiro**, mais do que o menu `/model`
  mostra. É de lá que o `/model` do chat tira as opções.

## ai-memory

- **Um workstream ativo por vez.** Um segundo `ai-memory run` no mesmo projeto responde 409 e o
  agente não sobe; cada sessão do bot usa `--new lukadispatch-<id>`, e relançar exige carimbo de
  tempo no nome.
- **`--fresh` não combina com `--session-id`.** O ai-memory recusa com "cannot be combined with a
  native session".

## Telegram

- **Toda mensagem de tópico de fórum vem com `reply_to_message`** apontando para a raiz do
  tópico, cujo id é o `thread_id`. Tratado no adaptador
  ([decisoes/0005](decisoes/0005-seguranca-no-adaptador.md)).
- **O cliente HTTP padrão do teloxide tem timeout de 17 s**, menor que o long polling: toda
  janela ociosa morria em erro de rede. O adaptador constrói o `Bot` com um cliente de timeout
  maior que o do polling.
- **Legenda de mensagem de voz chega como mensagem separada.** Só arquivo de áudio (`.m4a`,
  `.mp3`) com legenda exercita o campo de legenda do card de transcrição.
- **O Bot API manda 50 MB e baixa 20 MB.** A assimetria é do Telegram, não engano.
- **`date: 0` numa mensagem, para o teloxide, é "mensagem inacessível"** (é o marcador do Bot API
  7.0). Um fixture de teste com data zero vira `MaybeInaccessibleMessage::Inaccessible` e perde o
  `thread_id` sem erro nenhum.
- **`Update` do teloxide só desserializa de texto.** Por `serde_json::from_value`, o `flatten`
  com o `Deserialize` feito à mão do `UpdateKind` perde a chave e cai em `UpdateKind::Error`, e a
  tradução devolve `None` como se o update fosse lixo. Nos testes: `from_str` do JSON em texto.
- **O teloxide trazia a feature `fs` do tokio de carona.** Com o Telegram atrás de uma feature do
  cargo, o crate sem ela deixou de compilar `tokio::fs`. Dependência que se usa direto se declara
  direto.

## Testes

- **Teste com ids inventados não pega bug de forma de dado.** Os dois furos do reply do fórum
  passaram por 228 testes verdes porque nenhum montava uma mensagem com a forma real. A tradução
  de entrada de um adaptador se testa com payload copiado da plataforma.
- **Teste de unidade não substitui o teste pelo celular.** Depois de mexer em fluxo de card ou de
  reply, teste na mão.
- **Asserção que compara duas contagens da mesma fonte passa com 0 == 0.** Compare com o
  resultado esperado concreto (qual arquivo sobrou, qual texto chegou).

## Operação

- **Instalar binário novo com `install` dá `ETXTBSY`**, porque o `lukadispatch listen` da sessão
  está com o arquivo aberto. Copie para `~/.local/bin/.<nome>.novo` e faça `mv`: o rename troca a
  entrada do diretório e o processo vivo segue com o arquivo antigo. O `install.sh` (e, por ele,
  o `bin/deploy`) já faz assim.
- **Reiniciar o daemon no meio de um turno perde a marca de pedido daquele turno**
  ([decisoes/0009](decisoes/0009-marca-de-pedido.md)). Reponha:

  ```bash
  python3 -c "
  import sqlite3, os
  c = sqlite3.connect(os.path.expanduser('~/.local/state/lukadispatch/state.db'))
  c.execute(\"UPDATE sessions SET pedido=1 WHERE session_id='<id>'\")
  c.commit()"
  ```

- O restart em si é seguro para a sessão: o `listen` reconecta sozinho, e o `KillMode=process` do
  serviço poupa o tmux.
- Um `listen` de versão anterior repassa linha NDJSON com campo que não conhece, então acrescentar
  campo ao protocolo não exige reiniciar as sessões.

## tmux e sistema

- **Caminho de socket unix tem limite (~108 bytes).** O socket fica em `$XDG_RUNTIME_DIR`.
- **Para espelhar a saída de uma sessão interativa, use `tmux pipe-pane`**, não redirecionamento:
  com pipe no stdout, o Claude Code vira não interativo.
- **`pkill -f <padrão>` casa com o próprio shell que o executa.** Use `pkill -x <nome>`.
- **A caixa de entrada do Claude Code mostra texto de sugestão**, que parece mensagem recebida ao
  ler `capture-pane`. Para saber o que entrou de verdade, leia o transcript.
- **Memória com GPU:** no Vulkan o modelo de voz sai do RSS e vai para a GTT. Some
  `/sys/class/drm/card*/device/mem_info_gtt_used`, senão a medição conclui o contrário da
  verdade.
