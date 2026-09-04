#!/usr/bin/env bash
# Canonical: rebuild from source (clang builds ttyshim.c fresh); committed crates/pty/share/ttyshim.dylib retained for local runs.
# ttyshim DYLD_INSERT_LIBRARIES scrub regression test.
#
# The shim's constructor must strip its own entry from DYLD_INSERT_LIBRARIES
# so descendants of the termcmp-wrapped shell (direnv, herdr, ...) never load
# the interpose. Validates three cases against a freshly built dylib:
#   A) sole entry -> variable unset in the injected child
#   B) foreign entries preserved, own entry removed
#   C) interpose still active for the injected process itself
#
# Children are launched via python3 os.execve with an explicit environment:
# some sandboxes strip DYLD_* variables from inherited environments, and an
# explicit execve env is the only way to guarantee delivery. The child is a
# tiny C program (not /usr/bin/env under /bin/sh) because platform binaries
# sanitize DYLD_* out of their own environ view.
#
# Exit codes: 0 = pass, 1 = assertion failure, 2 = missing tool.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SHIM_SRC="$REPO_ROOT/crates/pty/share/ttyshim.c"

command -v clang >/dev/null 2>&1 || { echo "SKIP: clang not on PATH"; exit 2; }
command -v python3 >/dev/null 2>&1 || { echo "SKIP: python3 not on PATH"; exit 2; }
[ "$(uname)" = "Darwin" ] || { echo "SKIP: macOS only (DYLD_INSERT_LIBRARIES semantics)"; exit 2; }
[ -f "$SHIM_SRC" ] || { echo "FAIL: $SHIM_SRC not found"; exit 1; }

FAILURES=0
fail() { echo "FAIL: $*"; FAILURES=$((FAILURES + 1)); }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

clang -arch arm64 -arch x86_64 -dynamiclib \
    -o "$TMP/ttyshim.dylib" "$SHIM_SRC" \
    || { echo "FAIL: shim build failed"; exit 1; }

# Child probe: reports whether tcgetpgrp(0) was interposed to return its own
# pgrp, plus whether DYLD_INSERT_LIBRARIES is still set (and to what).
cat > "$TMP/probe.c" <<'EOF'
#include <stdio.h>
#include <unistd.h>
#include <stdlib.h>
int main(void) {
    const char* v = getenv("DYLD_INSERT_LIBRARIES");
    printf("tcgetpgrp_eq=%d\n", tcgetpgrp(0) == getpgrp());
    printf("dyld_var_set=%d\n", v != NULL);
    if (v != NULL)
        printf("dyld_var_value=%s\n", v);
    return 0;
}
EOF
clang -o "$TMP/probe" "$TMP/probe.c" || { echo "FAIL: probe build failed"; exit 1; }

# Launch PROBE with LIBS injected via explicit-env execve.
run_injected() {
    python3 -c 'import os, sys
os.execve(sys.argv[1], [sys.argv[1]], {"DYLD_INSERT_LIBRARIES": sys.argv[2], "PATH": "/usr/bin:/bin"})' \
        "$TMP/probe" "$1"
}

echo "--- Case A: sole entry is scrubbed ---"
A_OUT="$(run_injected "$TMP/ttyshim.dylib")"
if grep -q '^dyld_var_set=1' <<<"$A_OUT"; then
    fail "child still sees DYLD_INSERT_LIBRARIES: $(sed -n 's/^dyld_var_value=//p' <<<"$A_OUT")"
fi

echo "--- Case B: foreign entries preserved, own entry removed ---"
B_OUT="$(run_injected "/usr/lib/libz.1.dylib:$TMP/ttyshim.dylib")"
if ! grep -q '^dyld_var_set=1' <<<"$B_OUT"; then
    fail "variable was dropped entirely; foreign entries must survive"
fi
B_ENV="$(sed -n 's/^dyld_var_value=//p' <<<"$B_OUT")"
if [ "$B_ENV" != "/usr/lib/libz.1.dylib" ]; then
    fail "expected only foreign entry to survive, got: ${B_ENV:-<unset>}"
fi

echo "--- Case C: interpose still active for injected process ---"
if ! grep -q '^tcgetpgrp_eq=1' <<<"$A_OUT"; then
    fail "tcgetpgrp(0) != getpgrp() under injection"
fi

if [ "$FAILURES" -gt 0 ]; then
    echo ""
    echo "FAILED: $FAILURES assertion(s)"
    exit 1
fi

echo ""
echo "OK: ttyshim scrub passed"
