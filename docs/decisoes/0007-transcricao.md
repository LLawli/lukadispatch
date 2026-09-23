# 0007. Voz vira texto por um motor escolhido no config, e só vai com o seu aval

Decidido em 22/09/2026 (motor e fluxo) e 23/09/2026 (a trait `Transcritor`).

## Contexto

Mensagem de voz é o jeito natural de falar com uma sessão pelo celular, e um `.oga` não diz nada
à sessão. Transcrever tem três problemas: é lento, o melhor motor depende do hardware, e erra.

Um benchmark de 561 transcrições neste notebook (Ryzen 7 5700U, Radeon Vega sem VRAM dedicada,
~1,2 GB de RAM livre sob uso normal) mediu erro por palavra em áudios reais do usuário, tempo e
memória de pico. Resultados brutos em `~/.cache/asr-bench`.

## Decisão

**O motor sai do config, não do código.** A trait `Transcritor` tem hoje uma implementação,
`processo`, que chama qualquer programa local com marcadores (`{audio}`, `{modelo}`,
`{saida}`). Trocar de modelo ou de programa é editar o `config.toml`; motor de outra natureza
(API remota) é uma implementação nova da trait ([portas.md](../portas.md)).

**O padrão é whisper.cpp com `large-v3-turbo` em q5_0, no backend Vulkan:** 12,9% de erro, 44 s
por minuto de fala, ~1 GB de pico. Os descartados, e por quê:

| Motor | Erro | Por que não |
|---|---|---|
| large-v3 completo | 11,2% | 2248 MB numa máquina com ~1,2 GB livres |
| medium | pior | empata em velocidade e gasta MAIS memória que o turbo |
| qualquer um em fp16 | igual | 2x mais lento e o dobro de memória que em q5_0 |
| FastConformer-pt (sherpa-onnx) | 21,6% | 5,7x mais rápido e 417 MB, mas ruim em jargão e nome próprio, que é o vocabulário de trabalho. Fica como preset documentado |
| Parakeet TDT v3 | n/d | devolveu texto vazio com código de saída 0 num áudio ruim |
| Candle (Rust puro) | 22,8% | o exemplo não tem o fallback de temperatura do Whisper e entra em loop de repetição |

Só o whisper.cpp usa a GPU nesta máquina: os outros não têm backend Vulkan ou ROCm utilizável.

**Uma transcrição por vez, em todo o daemon.** Não é CPU (o Vulkan ocupa 0,3 dos 8 núcleos), é
memória: dois áudios juntos somam ~2 GB e empurram a máquina para o swap em zram, que comprime e
segura em vez de devolver.

**Fora do turno.** Nenhuma configuração medida transcreve um minuto de fala em menos de 39 s, e o
hook `Stop` desiste em 60 s. A transcrição roda em segundo plano e volta como mensagem própria.

**Com o seu aval.** A transcrição vira um card no canal, respondendo ao áudio, com três saídas:
**Enviar** (vai como se você tivesse digitado), **Descartar** (some sem rastro) e **responder ao
card** com a correção (os dois textos vão juntos, rotulados, e o escrito manda). Transcrever erra,
e a sessão agindo sobre algo que você não disse custa mais que o tempo que a voz economizou.

- **Um card por vez, em fila:** dois cards abertos tornariam ambíguo a qual deles uma correção se
  refere.
- **A correção exige responder ao card.** Na primeira versão qualquer texto virava correção, e
  não dava para falar de outro assunto com um card aberto.
- **Pendência aberta segura o canal:** com card de pergunta, permissão ou transcrição na tela, só
  passa a resposta ÀQUELE card e comando. O resto é apagado e vira aviso efêmero. Comando passa
  sempre: `/kill` é a válvula de escape para card preso.

**O áudio fica 7 dias** (`guardar_audio_dias`) e depois é apagado: quando a transcrição sai
estranha, ele é a única forma de saber se o erro foi do modelo ou da gravação.

## Consequências

- Sucesso é "veio texto não vazio", nunca "o processo saiu com 0".
- O RSS mente quando há GPU: no Vulkan o modelo sai do RSS e vai para a GTT. Medir memória exige
  somar `/sys/class/drm/card*/device/mem_info_gtt_used`.
- O que mais pesa na qualidade não é o motor: gravar pelo celular em vez do Telegram Desktop
  corta o erro pela metade (a captação do Desktop aplica supressão de ruído que come sílabas).

## Quando reabrir

- **Modelo quente** (`whisper-server` com o modelo carregado): medido e rejeitado, porque carregar
  custa 0,44 s de 9,6 s e prenderia ~1 GB entre mensagens. Reabra se o uso passar a ser muitos
  áudios curtos em sequência, ou se a máquina ganhar RAM.
- **Outro motor**: se a máquina mudar (GPU com VRAM, mais RAM), refaça o benchmark com áudio real,
  não com dataset público. No FLEURS pt-BR seis de oito configurações empataram em 2,8%; nos
  áudios reais a faixa abriu para 11% a 22% e a ordem mudou.
