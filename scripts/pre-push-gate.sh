#!/usr/bin/env bash
# Pre-push gate: mirrors the CI jobs that must stay green
# (ci.yml: check, test, shell-smoke, clippy).
#
# Installed as the git pre-push hook via cargo-husky
# (user-hooks feature; see crates/termcmp/Cargo.toml) —
# no manual setup, every clone gets it on first test build.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT" || exit 1

fail() { echo "GATE FAIL: $*" >&2; exit 1; }

# Smoke scripts self-skip with exit 2 when their tool is absent
# (shell not installed, non-Darwin for ttyshim). CI matrix jobs guarantee
# the tools so SKIP never fires there; on a dev box a skip must not block
# the push — only exit 1 (assertion failure) is fatal. `if` guards the
# call so `set -e` doesn't trip on nonzero rc.
run_smoke() {
    local script="$1" label="$2"
    local rc=0
    if "./$script"; then
        echo "GATE OK: $label passed"
    else
        rc=$?
        if [ "$rc" -eq 2 ]; then
            echo "GATE SKIP: $label skipped (tool absent)"
        else
            fail "$script (exit $rc)"
        fi
    fi
}

cargo fmt --check || fail "cargo fmt --check"
cargo clippy --all-targets -- -D warnings || fail "cargo clippy --all-targets -- -D warnings"
cargo test || fail "cargo test"

run_smoke scripts/check-fish-smoke.sh "fish smoke"
run_smoke scripts/check-zsh-zle-smoke.sh "zsh smoke"
run_smoke scripts/check-ttyshim-scrub.sh "ttyshim scrub"

echo "GATE OK: fmt, clippy, tests, shell smokes all green"
