#!/usr/bin/env bash
# Portões de validação do projeto. Roda tudo, não para no primeiro erro, e resume no fim:
# um relatório com as quatro linhas é mais útil que descobrir um problema de cada vez.
set -uo pipefail
cd "$(dirname "$0")"

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

passo "fmt"    cargo fmt --all --check
passo "clippy" cargo clippy --all-targets --all-features -- -D warnings
passo "test"   cargo test --workspace
passo "build"  cargo build --workspace --release

echo
if ((${#falhas[@]})); then
  echo "FALHOU: ${falhas[*]}"
  exit 1
fi
echo "tudo verde"
