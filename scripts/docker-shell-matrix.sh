#!/usr/bin/env bash
# Runtime stage of the Docker Linux matrix: run every shell smoke plus
# `termcmp doctor` under a clean HOME (all Skip → exit 0). A Fail must
# fail the matrix — do not swallow doctor's exit status.
set -euo pipefail
cd /opt/termcmp
export PATH="/usr/local/bin:$PATH"
for shell in bash zsh fish; do
  echo "=== $shell smoke ==="
  case "$shell" in
    bash) bash scripts/check-bash-smoke.sh ;;
    zsh)  bash scripts/check-zsh-zle-smoke.sh ;;
    fish) bash scripts/check-fish-smoke.sh ;;
  esac
done
echo "=== doctor (clean HOME) ==="
HOME="$(mktemp -d)" termcmp doctor
echo "OK: Linux shell matrix passed"