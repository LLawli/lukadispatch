# 0008. Arquivo sai por marcador na resposta, e o que não cabe é partido por uma cadeia de divisores

Decidido em 21/09/2026 (canal de arquivos) e 23/09/2026 (a trait `Divisor`).

## Contexto

Arquivo precisa atravessar o canal nos dois sentidos: o que você manda do celular tem de chegar à
sessão como algo que ela consiga ler, e o que a sessão produz (gráfico, log, build) tem de chegar
ao celular. As plataformas de chat têm tetos (o Bot API do Telegram manda 50 MB e baixa 20 MB).

## Decisão

**Entrada.** O adaptador baixa o anexo para `~/.local/share/lukadispatch/arquivos/<sessao>/` e a
sessão recebe o caminho, dentro do texto e no campo `files` da linha NDJSON.

- Fora do projeto (senão `git add -A` leva o anexo) e fora do `/tmp` (tmpfs: arquivo lá vira RAM
  presa).
- O nome vem de fora e é hostil: passa por `frontend::caminho_livre`, que fica só com o último
  componente, troca o que não é seguro por `_` e nunca sobrescreve (`nota-2.pdf`).
- Download pela metade ou vazio é apagado, não entregue.
- Os anexos morrem com a sessão; a reconciliação varre diretório de sessão que já não existe
  (o `/clear` troca o id sem encerrar nada).

**Saída.** O agente não chama ferramenta: escreve uma linha sozinha na resposta.

```
@arquivo: /caminho/absoluto | legenda opcional
@documento: /caminho/absoluto
```

- O reconhecimento é **estreito de propósito**: a linha precisa ser só o marcador, com caminho
  absoluto. No meio de frase, em crase ou depois de hífen de lista não conta, para o agente poder
  falar do formato sem disparar envio. O custo de errar é alto nos dois sentidos: falso positivo
  manda arquivo que ninguém pediu, falso negativo deixa sintaxe crua no celular.
- Texto e arquivo saem **na ordem em que foram escritos**: uma resposta que explica, mostra o
  gráfico e explica de novo só faz sentido nessa sequência.
- Imagem vai como foto (aparece na conversa); `@documento:` força o arquivo byte a byte. Foto
  recusada pela plataforma cai para documento.

**O que não cabe** no teto do frontend (`Limites::enviar`) é partido por uma **cadeia de
divisores**, tentados em ordem, cada um caindo para o próximo se falhar:

1. `video`: `ffmpeg -c copy -f segment`, corte por tempo sem recodificar. Cada trecho é um vídeo
   que toca sozinho no celular. Alvo de 80% do teto, porque o corte acontece no keyframe mais
   próximo e o trecho real passa do alvo.
2. `7z` (ou `rar`, pelo config): volumes de 90% do teto (45 MB para 50), com compressão mínima.
   7z e não `split` porque o ZArchiver e o RAR do celular abrem `.7z.001` sozinhos, e parte de
   `split` só se junta com `cat`.

O envio em partes acontece **em segundo plano**, porque o hook `Stop` desiste em 60 s. Depois da
última parte vai a instrução de juntar, que o próprio divisor escreve. Teto de 20 partes.

**Segredo só sai cifrado.** O prompt de bootstrap proíbe mandar chave, credencial, token ou
`.env` em claro; só cifrado para a chave pública que você fornecer, com a ferramenta escolhida
pelo formato da chave (`age -R` para SSH, `age -r` para `age1...`, `gpg` para PGP).

## Consequências

- O teto vem do frontend: o mesmo arquivo pode ir inteiro por uma plataforma e em partes por
  outra, sem código novo.
- A verificação é sempre o artefato em disco (partes existem, nenhuma vazia, nenhuma acima do
  teto), nunca o código de saída do 7z ou do ffmpeg.

## Quando reabrir

- Se a plataforma passar a aceitar arquivos grandes (o Bot API local do Telegram aceita 2 GB), o
  divisor deixa de ser chamado sozinho: basta o adaptador declarar o teto maior.
- Se a instrução de juntar não servir para quem recebe (outro sistema no celular), troque o
  divisor padrão no config.
