# Responsabilidades

Onde mora cada coisa, e, tão importante quanto, o que cada parte se recusa a fazer. Quando uma
mudança parecer pedir que um módulo faça algo da coluna "não faz", é sinal de que ela está no
lugar errado.

## Os crates

| Crate | Faz | Não faz |
|---|---|---|
| `ld-core` | tipos e leituras compartilhados: config, protocolo do socket, banco, transcript, catálogo de modelos, geração dos hooks | rede, tokio, Telegram, GTK. É carregado a cada chamada de hook, então tudo que entra aqui entra no caminho quente |
| `ld-daemon` | o daemon (`lukadispatchd`): domínio, portas e adaptadores | falar com o Claude Code por outro caminho que não o socket e os processos que ele mesmo lança |
| `ld-cli` | o binário `lukadispatch`: hooks, `listen`, comandos locais. O `hook` é o lado de entrada do Claude Code: traduz o que ele emite para o protocolo neutro do socket | tokio, teloxide. É std síncrono para nascer e morrer em milissegundos |
| `ld-ask` | a janela GTK4 de pergunta no PC | decidir quem ganhou a corrida (quem decide é o hook) |
| `ld-mcp` | proxy de stdio que intercepta `elicitation/create` dos servidores MCP | nada além de repassar linhas e responder o diálogo |

## `ld-daemon` por dentro

### Domínio (não conhece plataforma nenhuma)

| Módulo | Responsável por | Não faz |
|---|---|---|
| `app` | o ciclo de vida da sessão (criar, relançar continuando a conversa, encerrar, reconciliar), a entrega de mensagem ao `Hub`, o fim de turno, o envio de arquivo, a abertura dos cards de pergunta e permissão. É o único lugar onde "sessão morreu" quer dizer as quatro coisas que precisa (matar no hospedeiro, apagar o canal, fechar as perguntas, marcar no banco) | escolher implementação de porta; formatar para uma plataforma específica |
| `roteador` | decidir o que um `Evento` é: comando no canal principal, comando no canal da sessão, toque em botão, resposta a um card, anexo, conversa. Mora aqui a guarda que segura o canal enquanto há pergunta aberta e a fila de cards de transcrição | falar com a plataforma diretamente; conhecer allowlist (o adaptador já filtrou) |
| `cards` | a máquina de estados do card de pergunta (várias sub-perguntas, múltipla escolha, resposta escrita) e o desenho dele em marcação e botões neutros | enviar ou editar mensagem (devolve um `Efeito` e quem chama executa) |
| `confirmacao` | a fila de transcrições esperando aval: um card por vez por canal, casamento da correção pelo id do card respondido | transcrever; desenhar o card |
| `hub` | o estado vivo: quem está ouvindo (`listen`), perguntas abertas e seus `oneshot`, a contagem de re-arme. Não sobrevive a restart, de propósito | persistir nada |
| `status` | a mensagem de status de cada sessão, uma tarefa por sessão com debounce, adotando a mensagem do banco depois de um restart | decidir o texto do status (recebe pronto) |
| `panel` | o painel do canal principal: uma mensagem, fixada, redesenhada com debounce a partir do banco e do uso da conta | responder comando |
| `arquivos` | o lado do domínio dos arquivos: o marcador `@arquivo:` na resposta, a validação do que o agente pediu para mandar, a raiz de anexos por sessão, a limpeza de anexo de sessão morta e de áudio vencido | baixar da plataforma (é `Frontend::baixa`); partir arquivo (é `Divisor`) |
| `socket` | servir o protocolo NDJSON do `ld-core::proto` aos hooks e ao `listen` | lógica de negócio (repassa para o `app`) |
| `setup` | o `lukadispatchd setup`: os seletores, a conversa no terminal (`Tela`), o rascunho do config e do `.env` editado no lugar (`Rascunho`), e o passo de cada implementação de porta (`Peca`), com o do Telegram atrás de `ApiDoBot` | gravar antes do fim da conversa; confiar no que a pessoa diz quando a API pode conferir (privacidade, fórum e direitos são perguntados ao Telegram) |
| `main` | ler o config, montar as portas, preparar a máquina para o agente, subir o socket, a escuta do frontend e o relógio do painel | qualquer regra; ele só liga as peças |

### Portas (traits) e adaptadores

| Módulo | Responsável por |
|---|---|
| `frontend` (mod.rs) | a trait `Frontend` e o vocabulário neutro (`Canal`, `MsgId`, `Botao`, `Evento`, `Anexo`, `Limites`), mais `caminho_livre`, que é por onde todo nome de arquivo vindo de fora passa |
| `frontend::formato` | a marcação neutra do domínio (`escapa`) e a leitura dela para quem precisa traduzir (`trechos`, `texto_puro`), mais a quebra de texto no limite da plataforma |
| `frontend::telegram` | tudo que é Telegram: teloxide, long polling, allowlist por id numérico, `answer_callback_query`, reply automático do fórum, tetos do Bot API, tradução dos ids |
| `frontend::nulo` | o modo offline: aceita tudo e não manda nada |
| `frontend::memoria` | o dublê dos testes de fluxo, que registra cada chamada |
| `transcritor` (mod.rs) | a trait `Transcritor` e a escolha do motor pelo config |
| `transcritor::processo` | chamar um programa local como transcritor: WAV 16 kHz, marcadores no comando, uma transcrição por vez, prazo, texto vazio como erro |
| `divisor` (mod.rs) | a trait `Divisor` e a cadeia `Divisores`, com fallback e o teto de partes |
| `divisor::video`, `sete_z`, `rar` | as três formas de partir: trechos de vídeo por tempo, volumes de 7z, volumes de RAR |
| `agente` (mod.rs) | as traits `Agente` e `Envelope`, a `DescricaoDoChat` que o domínio passa ao agente, e o script de partida neutro (`escreve_partida`), que junta envelope e invocação |
| `agente::claude_code` | tudo que é Claude Code: flags da linha de comando, prompts de partida e de re-arme, `bot-settings.json` com os ganchos, confiança de pasta, catálogo de modelos, modos de permissão, leitura do transcript e do uso da conta |
| `sessions` | a trait `Hospedeiro` e o `Tmux`: rodar o script de partida num terminal de verdade, saber se está vivo, matar, listar |

A direção das dependências é uma só: o domínio depende das traits; os adaptadores dependem das
traits; nenhum adaptador depende do domínio, e o domínio não importa nenhum adaptador (só o
`main` importa). O Telegram fica atrás da feature `telegram` do cargo, e o crate compila sem ela:
é a prova de que o domínio não depende dele.

## `ld-core` por dentro

| Módulo | Responsável por |
|---|---|
| `config` | o `config.toml` e as variáveis de ambiente, incluindo os seletores das portas (`frontend`, `[agente]`, `[transcricao] motor`, `[arquivos] divisor`). Credencial nunca vem do arquivo |
| `state` | o SQLite: sessões, fila, marca de pedido, chave e valor. Ids de canal e mensagem são texto opaco |
| `proto` | o protocolo do socket, uma mensagem JSON por linha |
| `paths` | onde cada coisa mora, respeitando XDG |
| `hooks` | os dois conjuntos de hooks: o das sessões do bot e o de telemetria da máquina |
| `ask`, `race` | a pergunta como tipo comum aos três lados, e a corrida entre card e janela, que o hook e o proxy MCP usam |
| `context`, `usage`, `models`, `transcript` | leituras do que o Claude Code grava: contexto usado, janelas de limite, catálogo de modelos, histórico da conversa |
| `labels` | o rótulo curto de cada ferramenta na mensagem de status |
| `mcp` | a configuração de MCP das sessões, com cada servidor passando pelo proxy |
| `trust` | marcar a pasta como confiada antes de abrir a sessão, para ela não travar no diálogo de confiança |

## `ld-cli` por dentro

| Módulo | Responsável por |
|---|---|
| `main` | os subcomandos, com parser feito à mão (dependência de CLI pesaria em cada hook) |
| `hook` | `hook [agente] <evento>`: escolhe o tradutor de ganchos do agente (trait `Ganchos`; sem agente, Claude Code) e nunca trava a sessão por agente desconhecido ou entrada ilegível |
| `hook::claude` | os ganchos do Claude Code: um evento por subcomando, traduzido para o protocolo do socket. Todos saem com 0, menos o `stop`, que sai com 2 para o `asyncRewake` acordar a sessão quando o monitor precisa voltar |
| `ask` | os hooks que perguntam (pergunta e permissão), traduzindo a resposta para o formato do Claude Code |
| `listen` | o fluxo de entrada que o `Monitor` da sessão lê: uma linha por mensagem, com flush |
| `client` | o cliente síncrono do socket, com prazo em toda chamada. Qualquer falha vira "não decidi", nunca trava a sessão |
