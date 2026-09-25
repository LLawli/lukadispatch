# As portas: como trocar uma peça

O daemon fala com o mundo por traits. Cada uma tem implementações prontas, escolhidas no
`config.toml`, e aceita implementações novas sem mexer no domínio.

| Porta | Trait | Implementações | Chave no config |
|---|---|---|---|
| chat | `frontend::Frontend` | `telegram`, `nulo` (offline), `memoria` (testes) | `frontend = "telegram"` |
| agente de código | `agente::Agente` | `claude-code` | `[agente] tipo = "claude-code"` |
| memória de longo prazo (opcional) | `agente::Memoria` | `ai-memory`, `nenhuma` | `[agente] memoria = "ai-memory"` |
| voz para texto | `transcritor::Transcritor` | `processo` (qualquer programa local) | `[transcricao] motor = "processo"` |
| arquivo grande | `divisor::Divisor` | `video` (ffmpeg), `7z`, `rar` | `[arquivos] divisor = "7z"`, `cortar_video = true` |
| onde a sessão roda | `sessions::Hospedeiro` | `tmux`, `herdr` | `hospedeiro = "tmux"`, `[herdr] sessao` |

Elas são montadas em `main.rs` e chegam ao `App` num `Portas`:

```rust
let app = App::new(cfg, store, Portas {
    frontend,     // Arc<dyn Frontend>
    agente,       // Arc<dyn Agente>
    memoria,      // Arc<dyn Memoria>
    transcritor,  // Option<Arc<dyn Transcritor>>, None = transcrição desligada
    divisores,    // Divisores, a cadeia ordenada
    hospedeiro,   // Arc<dyn Hospedeiro>
});
```

O porquê desse desenho (traits com `async-trait`, objeto dinâmico, escolha em tempo de execução)
está em [decisoes/0002](decisoes/0002-portas-e-adaptadores.md).

## O setup de uma implementação nova

`lukadispatch setup` escolhe uma implementação por porta, com os mesmos nomes do config:

```text
lukadispatch setup --frontend telegram --agent claude-code --session tmux --memoria ai-memory
```

Cada implementação traz o próprio passo de setup, a trait `Peca` em
`crates/ld-daemon/src/setup/pecas.rs`: `configura` confere o que ela precisa na máquina, pergunta
o que faltar e escreve no rascunho do config e do `.env`; `ativa`, opcional, roda depois de
gravar (é onde o Claude Code instala os hooks). Uma implementação nova se registra em dois
lugares, do mesmo jeito:

1. no `da_config` da porta (e na lista de nomes dela: `AGENTES`, `MEMORIAS`, `HOSPEDEIROS`), para
   o daemon subir com ela;
2. no registro da porta em `setup/pecas.rs` (`frontend`, `agente`, `hospedeiro`, `memoria`), para
   o setup saber configurá-la.

O passo do Telegram é o modelo para um frontend: a API fica atrás de uma trait (`ApiDoBot`), e o
teste roteiriza um bot de mentira que passa por todos os laços (token errado, privacidade ligada,
grupo sem tópicos, bot sem direitos). Um passo de setup que só funciona com o serviço de verdade
não tem como ser testado no CI.

## Regra de ouro ao escrever uma implementação

Cada trait tem, na doc do módulo, uma lista de contrato. Ela não é sugestão: o domínio conta com
ela. Os pontos que mais custaram caro até hoje, e que valem para qualquer implementação nova:

- **Código de saída zero não é prova.** Confira o artefato: texto não vazio, parte em disco com
  tamanho plausível, canal que de fato existe.
- **Falha é erro com motivo, nunca silêncio.** O domínio mostra o erro no canal da sessão; um
  `Ok` vazio some sem rastro.
- **Nada de pânico por dado velho.** Um id de canal no banco pode ter vindo de outro frontend
  numa execução anterior. Trate como "não existe", não como bug.

---

## Frontend: trocar o Telegram (pelo WhatsApp, por exemplo)

**Onde:** `crates/ld-daemon/src/frontend/`. Contrato em `frontend/mod.rs`.

### O vocabulário

| Conceito do domínio | Tipo | No Telegram | No WhatsApp |
|---|---|---|---|
| canal principal (painel e comandos) | `canal: None` | tópico General | um grupo "painel" |
| canal de uma sessão | `Canal` (texto opaco) | tópico do fórum (`thread_id`) | um grupo por sessão |
| mensagem enviada | `MsgId` (texto opaco) | `message_id` | id da mensagem, com o JID do chat junto se precisar |
| botão | `Botao { rotulo, dado }` | botão inline, `dado` vira `callback_data` | botão de resposta (até 3) ou lista (até 10) |
| texto formatado | marcação de `frontend::formato` | HTML, repassado como está | percorra `formato::trechos` e escreva `*negrito*`, `_itálico_`, crase |
| texto do agente | `envia_texto` | sem `parse_mode` | sem formatação |
| resposta a uma mensagem | `responde_a: Option<MsgId>` | `reply_to_message` | mensagem citada (`context.id`) |
| tetos da plataforma | `Limites` | 50 MB para enviar, 20 MB para baixar | 100 MB de documento |
| como o agente descreve o chat | `plataforma`, `onde`, `renderiza_markdown` | `o tópico "proj" de um grupo do Telegram`, sem Markdown | `o grupo "proj" do WhatsApp`, com a formatação dele |

Se a plataforma exige mais que um número para achar a mensagem de novo, codifique tudo no texto
do `MsgId` (por exemplo `"<jid>|<id>"`). O domínio nunca olha dentro.

### Passo a passo

1. **Crie `frontend/whatsapp.rs`** com uma struct que guarde o cliente da API, o destino e a
   allowlist, e implemente `Frontend` para ela. Use `frontend/telegram.rs` como modelo de
   estrutura e `frontend/memoria.rs` como a referência mais curta do contrato.
2. **Escreva a tradução de entrada como função pura**, do payload da plataforma para
   `Option<Evento>`, como `telegram::traduz`. É ali que moram as duas regras de segurança
   ([decisoes/0005](decisoes/0005-seguranca-no-adaptador.md)):
   - só vira `Evento` quem está na allowlist (e mensagem do próprio bot nunca vira);
   - `responde_a` só carrega resposta feita de propósito pela pessoa. Descubra se a plataforma
     põe reply automático em alguma situação e descarte-o aqui.

   Teste essa função com payloads reais copiados da plataforma, não com ids inventados: foi um
   teste com ids arbitrários que deixou passar o reply automático do fórum do Telegram.
3. **`escuta`** entrega os eventos para sempre. Webhook ou websocket, tanto faz: o domínio só vê
   o canal `mpsc`. Queda de rede é tratada aqui dentro (espera e tenta de novo); `escuta`
   retornar faz o daemon achar que a entrada morreu.
4. **`baixa`** grava o anexo dentro do diretório recebido, com o nome saído de
   `frontend::caminho_livre` (nunca o nome cru que a plataforma mandou), e apaga o arquivo se o
   download falhar ou vier vazio.
5. **`limites`** devolve os tetos da plataforma. O divisor usa `enviar` para decidir o tamanho
   das partes, então um número errado aqui vira upload recusado no fim.
   **`plataforma`, `onde` e `renderiza_markdown`** dizem ao agente, no prompt de partida, por
   onde a pessoa fala e o que funciona ali (`o grupo "proj" do WhatsApp`; Markdown aparece
   formatado ou cru). Têm implementação padrão, mas a frase padrão é genérica: sobrescreva.
6. **Registre** em `main.rs`, no `match` sobre `cfg.frontend`, com a configuração que ele
   precisar (uma seção `[whatsapp]` em `ld-core/src/config.rs`, com credencial vinda do
   ambiente, nunca do arquivo).
7. **Ponha atrás de uma feature do cargo**, como o Telegram está atrás de `telegram`: quem não
   usa não compila a dependência.
8. **Prove** rodando os testes de fluxo (`crates/ld-daemon/tests/fluxos.rs`): eles exercitam o
   domínio pelo frontend em memória, então o que eles cobrem vale para qualquer adaptador que
   cumpra o contrato. O que sobra para testar é a tradução (passo 2) e um teste manual pelo
   celular.

### O que o domínio espera e o WhatsApp pode não ter

- **Apagar canal.** Grupo de WhatsApp não se apaga como tópico. `apaga_canal` pode sair do grupo
  e arquivar a conversa, e devolver `Apagado`: o que o domínio quer é que o canal não fique
  aceitando mensagem para uma sessão morta.
- **Editar mensagem.** O WhatsApp só edita por 15 minutos. O status e o painel editam a mesma
  mensagem por horas; o adaptador pode, ao ver a edição recusada, apagar e mandar uma nova, e
  devolver `Ok`. O domínio já trata "mensagem sumiu" mandando outra, mas é mais barato resolver
  no adaptador.
- **Botão com dado.** Botão de resposta do WhatsApp devolve um `id` de até 256 caracteres: cabe
  o `dado` do domínio, que é mantido abaixo de `Limites::dado_botao` (64 bytes, o teto do
  Telegram, que é o mais apertado).

---

## Agente: trocar o Claude Code (pelo Codex, por exemplo)

**Onde:** `crates/ld-daemon/src/agente/`. Contrato em `agente/mod.rs`; o porquê em
[decisoes/0012](decisoes/0012-agente-e-envelope.md).

A sessão não roda o agente direto. São três peças numa partida, cada uma trocável sozinha:

```
ai-memory run --new lukadispatch-0123abcd-1790000000   ← Memoria (AiMemory)
  claude --session-id <id> --settings <...> "<prompt>"  ← Agente (ClaudeCode::invocacao)
num tmux ld-<projeto>-<id>, ou aba do herdr            ← Hospedeiro (Tmux, Herdr)
```

**Trocar só a memória** é config: `[agente] memoria = "nenhuma"` roda o agente sem o
`ai-memory`. Outra memória de longo prazo é uma implementação de `Memoria`, que recebe a linha
de comando do agente e devolve a final. Ela é opcional: nada fora da implementação pode depender
de haver uma. (A chave e a trait se chamavam `envelope`; o config antigo ainda carrega.)

**Trocar o agente** é implementar `Agente`:

1. **`invocacao`**: programa, argumentos e prompt inicial para sessão nova e para retomada. O
   prompt tem de ensinar o agente a (a) ouvir o canal de entrada, (b) devolver arquivo com a
   linha `@arquivo:`, (c) nunca tentar "mandar mensagem" por conta própria. O que ele diz sobre
   o chat vem pronto em `pedido.chat` (`DescricaoDoChat`): não escreva o nome de plataforma
   nenhum fixo no prompt.
2. **`prepara`**: instala os ganchos do agente apontando para `lukadispatch hook <agente>
   <evento>`, com o nome do agente novo. Roda na partida do daemon.
3. **`modos` e `valida_modo`**: os modos de permissão que o agente tem, e como o modo
   `perguntar` do lukadispatch se traduz para ele (no Claude Code, `dontAsk` mais o portão no
   `PreToolUse`).
4. **Leituras** (`historico`, `resposta_do_turno`, `contexto`, `uso`...): o que der para ler da
   conversa gravada. O que não der, devolva vazio: o domínio segue sem histórico ou sem contador.
5. **O lado de entrada fica no CLI**, não no daemon: os ganchos do agente chamam
   `lukadispatch hook <agente> <evento>`, e o tradutor daquele agente converte o payload dele
   para o protocolo neutro do socket (`ld_core::proto`). Um agente novo é um módulo em
   `crates/ld-cli/src/hook/` que implemente a trait `Ganchos` e uma linha em `hook::agentes`,
   produzindo as mesmas mensagens que o `hook/claude.rs` produz. Sem agente na linha de comando
   (`lukadispatch hook <evento>`), o CLI entende Claude Code: é a forma dos settings antigos.
6. **Registre** o nome em `agente::da_config`.

O que um agente precisa ter, porque não há como emular: receber mensagem no meio do turno (o
Claude Code faz isso com a ferramenta `Monitor` lendo o `lukadispatch listen`), ganchos de fim de
turno e de ferramenta, e um ponto em que a decisão de permissão de um gancho seja honrada.

---

## Transcritor: trocar o motor de voz

**Onde:** `crates/ld-daemon/src/transcritor/`. Contrato em `transcritor/mod.rs`.

### Primeiro: talvez não precise de código

O motor `processo` roda qualquer programa local. Trocar de modelo, de backend ou até de motor
(whisper.cpp, sherpa-onnx, faster-whisper) é editar o config:

```toml
[transcricao]
motor = "processo"
comando = ["~/.local/share/lukadispatch/asr/whisper-cli-vulkan", "-m", "{modelo}",
           "-f", "{audio}", "-l", "pt", "-t", "8", "-otxt", "-of", "{saida}", "-nt"]
modelo = "~/.local/share/lukadispatch/asr/modelos/ggml-large-v3-turbo-q5_0.bin"
saida = "arquivo"        # "arquivo" lê {saida}.txt; "stdout" lê a saída padrão
timeout_s = 900
```

`{audio}` é um WAV mono 16 kHz preparado pelo daemon, `{modelo}` é o campo `modelo`, `{saida}`
é o prefixo do arquivo de texto. Os presets medidos (CPU, FastConformer) estão documentados no
tipo `Transcricao` em `ld-core/src/config.rs`, e o benchmark que escolheu o padrão está em
[decisoes/0007](decisoes/0007-transcricao.md).

### Quando precisa: motor de outra natureza

Uma API remota ou uma biblioteca embutida no binário não é um programa com argumentos. Aí:

1. Crie `transcritor/<nome>.rs` com uma struct e `impl Transcritor`. `transcreve` recebe o
   caminho do áudio como chegou do chat (Opus, MP3, M4A): converter formato é problema seu.
2. Cumpra o contrato: texto vazio é erro, não sucesso; não bloqueie o runtime (use
   `spawn_blocking` para inferência local síncrona); tenha prazo.
3. Decida se precisa de fila. O `processo` roda uma transcrição por vez porque cada uma pede
   ~1 GB nesta máquina; uma API remota provavelmente não precisa disso.
4. Registre um nome novo de `motor` em `transcritor::da_config`, validando o config ali (erro
   na partida, e não no primeiro áudio).

---

## Divisor: trocar como arquivo grande é partido

**Onde:** `crates/ld-daemon/src/divisor/`. Contrato em `divisor/mod.rs`.

O divisor não é um só, é uma **cadeia**: o daemon tenta, em ordem, cada divisor que está
instalado e aceita o arquivo, e cai para o próximo se um falhar. A cadeia padrão é `video` (corte
por tempo, cada trecho toca sozinho) e depois `7z` (volumes).

### Trocar 7z por RAR

É uma linha:

```toml
[arquivos]
divisor = "rar"       # ou "7z"
cortar_video = true   # false tira o corte por tempo da frente da cadeia
```

O `rar` precisa estar no PATH (o binário da RARLAB em `~/.local/bin` basta). Sem ele, o arquivo
grande falha com um erro que diz qual divisor faltou.

### Um formato novo

1. Crie `divisor/<nome>.rs` com `impl Divisor`: `disponivel` (a ferramenta existe?), `aceita`
   (este arquivo é para mim?), `anuncio` (o aviso antes de começar), `divide` e o texto de
   `como_juntar` que vai depois da última parte.
2. Cumpra o contrato: nenhuma parte acima de `teto` (que vem de `Frontend::limites().enviar`),
   no máximo `MAX_PARTES`, confira as partes em disco em vez do código de saída, e em erro
   deixe o diretório vazio para o próximo da cadeia.
3. Registre o nome em `Divisores::da_config`. Se for um divisor especializado (como o de vídeo),
   ponha-o na frente da cadeia; se for genérico, ele entra como alternativa ao `7z`.

---

## Hospedeiro: trocar o tmux

**Onde:** `crates/ld-daemon/src/sessions/`, trait `Hospedeiro`. Ele recebe o script de partida
pronto (montado pelo agente e pela memória) e só o roda.

O tmux (`mod.rs`) foi o primeiro porque deixa a sessão anexável no PC
(`tmux attach -t ld-<projeto>-<id>`) e sobrevive a restart do daemon. O herdr (`herdr.rs`) é o
segundo: uma aba por sessão no workspace do projeto, falando JSON direto no socket dele (só o
`layout.apply` abre um pane já rodando um comando com ambiente, e a CLI não o expõe). Outro
multiplexador (zellij, screen) ou um contêiner por sessão entraria como outra implementação de
`lanca`, `vive`, `situacao`, `mata`, `nossas`, `descreve` e `como_anexar`. Os dois últimos são o que o
usuário lê: a ficha do tópico e o comando para anexar no PC, que vai nos avisos que só o teclado
resolve.

O id que `lanca` devolve vai para `Session::hospedagem` (coluna `hospedagem`): o tmux grava o
nome da sessão, o herdr grava `rótulo@terminal@pid:início`
([decisoes/0014](decisoes/0014-hospedagem-neutra.md)). Ele é opaco, com uma exceção: começa
pelo rótulo, que é o que `nossas` devolve e o que a varredura de órfãs compara
([decisoes/0015](decisoes/0015-vida-pelo-processo.md)).

Duas coisas que qualquer hospedeiro tem de preservar, porque o resto do sistema depende delas:

- **A sessão precisa de um terminal de verdade.** Com pipe no stdout, o Claude Code vira não
  interativo. O tmux resolve isso e espelha a saída por `pipe-pane`; o herdr roda o script sob
  `script -f`, que dá o terminal e espelha no mesmo passo. Redirecionar não serve.
- **O ambiente da sessão carrega `LD_SESSION` e `LUKADISPATCH_SOCKET`.** É assim que os hooks,
  rodando dentro dela, acham o daemon.

O hospedeiro sai da chave `hospedeiro` do config, e um nome novo se registra em
`sessions::da_config` e na lista `HOSPEDEIROS`.
