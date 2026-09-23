#!/bin/sh
# Instala o lukadispatch: os binários em ~/.local/bin e o serviço de usuário do systemd. Numa
# instalação nova, emenda no `lukadispatch setup`, que cria o bot, o grupo e o config.
#
#   curl -fsSL https://raw.githubusercontent.com/LLawli/lukadispatch/master/install.sh | sh
#
#   LUKADISPATCH_VERSAO=v0.1.0 sh install.sh   uma versão fixa, em vez da última
#   LUKADISPATCH_BIN=/outro/dir sh install.sh  outro diretório para os binários
#   sh install.sh --de <dir>                   de um pacote já extraído (o bin/deploy usa isto)
#
# Rodar de novo atualiza: os binários são trocados, o config não é tocado e, se o serviço estiver
# rodando, ele é reiniciado com a versão nova.
set -eu

REPO=LLawli/lukadispatch
BIN_DIR=${LUKADISPATCH_BIN:-$HOME/.local/bin}
# Fixo em ~/.config, e não em $XDG_CONFIG_HOME: é onde o serviço procura o .env (%h/.config).
CONF_DIR=$HOME/.config/lukadispatch
UNIT_DIR=$HOME/.config/systemd/user
BINARIOS="lukadispatchd lukadispatch lukadispatch-ask lukadispatch-mcp"

diz() { printf '==> %s\n' "$*"; }
avisa() { printf '    aviso: %s\n' "$*"; }
morre() {
  printf 'install.sh: %s\n' "$*" >&2
  exit 1
}

origem=""
case "${1:-}" in
  --de)
    [ $# -ge 2 ] || morre "--de precisa de um diretório"
    origem=$2
    ;;
  "") ;;
  *) morre "opção desconhecida: $1 (veja o começo deste arquivo)" ;;
esac

[ "$(uname -s)" = Linux ] || morre "só roda em Linux: o daemon é um serviço de usuário do systemd"

baixa() {
  case "$(uname -m)" in
    x86_64 | amd64) arq=x86_64 ;;
    aarch64 | arm64) arq=aarch64 ;;
    *) morre "sem binário pronto para $(uname -m); compile com cargo (veja o README)" ;;
  esac
  nome="lukadispatch-linux-$arq.tar.gz"
  if [ -n "${LUKADISPATCH_VERSAO:-}" ]; then
    url="https://github.com/$REPO/releases/download/$LUKADISPATCH_VERSAO"
  else
    url="https://github.com/$REPO/releases/latest/download"
  fi
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  diz "baixando $nome"
  curl -fsSL "$url/$nome" -o "$tmp/$nome"
  curl -fsSL "$url/$nome.sha256" -o "$tmp/$nome.sha256"
  (cd "$tmp" && sha256sum -c "$nome.sha256" >/dev/null 2>&1) || morre "o sha256 de $nome não confere"
  mkdir "$tmp/pacote"
  tar -xzf "$tmp/$nome" -C "$tmp/pacote"
  origem="$tmp/pacote"
}

[ -n "$origem" ] || baixa

for b in $BINARIOS; do
  [ -s "$origem/$b" ] || morre "o pacote em $origem não tem $b"
done

mkdir -p "$BIN_DIR"
for b in $BINARIOS; do
  # Copia ao lado e renomeia. Sobrescrever no lugar dá ETXTBSY, porque o `listen` de cada sessão
  # aberta mantém o lukadispatch em execução; o rename troca o arquivo e o processo vivo segue
  # com o antigo até terminar.
  cp "$origem/$b" "$BIN_DIR/.$b.novo"
  chmod 755 "$BIN_DIR/.$b.novo"
  mv -f "$BIN_DIR/.$b.novo" "$BIN_DIR/$b"
done
diz "binários em $BIN_DIR"

mkdir -p "$UNIT_DIR"
if [ "$BIN_DIR" = "$HOME/.local/bin" ]; then
  cp "$origem/lukadispatch.service" "$UNIT_DIR/lukadispatch.service"
else
  sed "s|%h/.local/bin|$BIN_DIR|" "$origem/lukadispatch.service" >"$UNIT_DIR/lukadispatch.service"
fi
diz "serviço em $UNIT_DIR/lukadispatch.service"

case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) avisa "$BIN_DIR não está no PATH; ponha para usar o comando lukadispatch" ;;
esac

# Com o token no .env, isto é uma atualização: basta o serviço subir com os binários novos.
if grep -q '^LUKADISPATCH_TELEGRAM_TOKEN=..*' "$CONF_DIR/.env" 2>/dev/null; then
  if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
    systemctl --user daemon-reload
    if systemctl --user is-active --quiet lukadispatch; then
      systemctl --user restart lukadispatch
      diz "serviço reiniciado com a versão nova"
    fi
  fi
  exit 0
fi

# Instalação nova: o setup pergunta o que falta. Com `curl | sh` o stdin é o próprio script,
# então a conversa vai pelo /dev/tty, quando há um terminal para conversar.
if (exec </dev/tty) 2>/dev/null; then
  diz "agora o setup: o bot, o grupo e as preferências"
  exec "$BIN_DIR/lukadispatch" setup </dev/tty
fi
diz "falta configurar: rode lukadispatch setup"
