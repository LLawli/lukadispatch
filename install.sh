#!/bin/sh
# Instala o lukadispatch: os binários em ~/.local/bin, o serviço de usuário do systemd e, se
# ainda não existirem, o .env e o config.toml de exemplo em ~/.config/lukadispatch.
#
#   curl -fsSL https://raw.githubusercontent.com/LLawli/lukadispatch/master/install.sh | sh
#
#   LUKADISPATCH_VERSAO=v0.1.0 sh install.sh   uma versão fixa, em vez da última
#   LUKADISPATCH_BIN=/outro/dir sh install.sh  outro diretório para os binários
#   sh install.sh --de <dir>                   de um pacote já extraído (o bin/deploy usa isto)
#
# Rodar de novo atualiza: os binários são trocados, o config existente não é tocado e, se o
# serviço estiver rodando, ele é reiniciado com a versão nova.
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

mkdir -p "$CONF_DIR"
falta_configurar=""
if [ ! -e "$CONF_DIR/.env" ]; then
  # O .env guarda o token do bot: nasce legível só pelo dono.
  (umask 077 && cp "$origem/env.example" "$CONF_DIR/.env")
  falta_configurar=1
  diz "criado $CONF_DIR/.env"
fi
if [ ! -e "$CONF_DIR/config.toml" ]; then
  cp "$origem/config.example.toml" "$CONF_DIR/config.toml"
  falta_configurar=1
  diz "criado $CONF_DIR/config.toml"
fi

for dep in tmux claude; do
  command -v "$dep" >/dev/null 2>&1 || avisa "$dep não está no PATH, e as sessões precisam dele"
done
command -v ai-memory >/dev/null 2>&1 ||
  avisa "ai-memory não está no PATH; sem ele, use envelope = \"nenhum\" em [agente] no config.toml"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) avisa "$BIN_DIR não está no PATH; ponha para usar o comando lukadispatch" ;;
esac

ativo=""
if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
  systemctl --user daemon-reload
  if systemctl --user is-active --quiet lukadispatch; then
    systemctl --user restart lukadispatch
    ativo=1
    diz "serviço reiniciado com a versão nova"
  fi
fi

[ -n "$ativo" ] && [ -z "$falta_configurar" ] && exit 0

cat <<EOF

Falta pouco:
  1. Crie o bot e o grupo (o README explica em seis passos) e ponha o token e o chat_id
     em $CONF_DIR/.env
  2. Ajuste [telegram] em $CONF_DIR/config.toml: chat_id e allowed_user_ids
  3. lukadispatch install --global        (os hooks do Claude Code; desfaz com uninstall)
  4. systemctl --user enable --now lukadispatch
EOF
