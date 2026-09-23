#!/usr/bin/env bash
# Portões de validação do projeto. Roda tudo, não para no primeiro erro, e resume no fim:
# um relatório com as quatro linhas é mais útil que descobrir um problema de cada vez.
set -uo pipefail
cd "$(dirname "$0")" || exit 1

falhas=()
passo() {
  local nome="$1"; shift
  echo "==> $nome"
  if "$@"; then
    echo "    ok"
  else
    echo "    FALHOU"
    falhas+=("$nome")
  fi
}

passo "fmt"          cargo fmt --all --check
passo "clippy"        cargo clippy --all-targets --all-features -- -D warnings
# Prova de que o domínio não sabe que o Telegram existe: sem a feature, teloxide nem compila.
passo "sem telegram"  cargo clippy -p ld-daemon --no-default-features --all-targets -- -D warnings
passo "test"          cargo test --workspace
passo "build"         cargo build --workspace --release

echo
if ((${#falhas[@]})); then
  echo "FALHOU: ${falhas[*]}"
  exit 1
fi
echo "tudo verde"
