#!/usr/bin/env bash
# Standalone bash shell-integration smoke.
#
# Sources the production shell/termcmp.bash with a stub `termcmp` on PATH,
# drives _tc_report_buffer by assigning READLINE_LINE/READLINE_POINT
# directly, and asserts OSC 7772 emission + percent-encoding for ;, BEL,
# ESC, %, UTF-8, and the active-gate guard. Also covers the OSC 7773 env
# reporter (emission, dedup, 7774 truncation diagnostic, gate) and the
# shell/init.bash proxy decision (herdr-aware ancestor walk).
#
# Requires bash >= 4.4 (READLINE_LINE/READLINE_POINT in bind -x,
# compgen -e). On macOS that means `brew install bash`; the script uses
# `bash` from PATH, never /bin/bash directly.
#
# Exit codes: 0 = pass, 1 = assertion failure, 2 = missing tool.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SHELL_INTEGRATION="${REPO_ROOT}/shell/termcmp.bash"
INIT_BASH="${REPO_ROOT}/shell/init.bash"

if [[ ! -f "${SHELL_INTEGRATION}" ]]; then
    echo "FAIL: shell integration not found at ${SHELL_INTEGRATION}" >&2
    exit 1
fi

if ! command -v bash >/dev/null 2>&1; then
    echo "FAIL: bash not on PATH" >&2
    exit 2
fi

if ! bash -c '(( BASH_VERSINFO[0] > 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] >= 4) ))'; then
    echo "FAIL: bash >= 4.4 required (found $(bash -c 'echo "${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}"'))" >&2
    exit 2
fi

assert_contains() {
    local needle="$1" haystack="$2"
    if [[ "${haystack}" != *"${needle}"* ]]; then
        printf 'FAIL: expected %q in output but missing\nOutput:\n%s\n' \
            "${needle}" "${haystack}" >&2
        exit 1
    fi
}

assert_not_contains() {
    local needle="$1" haystack="$2"
    if [[ "${haystack}" == *"${needle}"* ]]; then
        printf 'FAIL: unexpected %q in output\nOutput:\n%s\n' \
            "${needle}" "${haystack}" >&2
        exit 1
    fi
}

# Count non-overlapping occurrences of a fixed byte string. grep may exit
# non-zero when nothing matches — that is a valid count of 0 here.
count_frames() {
    local haystack="$1" needle="$2"
    { printf '%s' "${haystack}" | grep -oF -- "${needle}" || true; } | wc -l | tr -d ' '
}

# A fake `termcmp` so the integration's `command -v termcmp` guard passes
# without installing the binary. Prepend to PATH in every child shell:
# the integration no-ops when termcmp is absent, which would make every
# assertion vacuous.
SMOKE_BIN="$(mktemp -d)"
printf '#!/bin/sh\nexit 0\n' > "${SMOKE_BIN}/termcmp"
chmod +x "${SMOKE_BIN}/termcmp"
trap 'rm -rf "${SMOKE_BIN}"' EXIT

echo "--- OSC 7772 buffer framing ---"

# Semicolon: the core ADR-0003 regression. Raw ; splits the OSC frame
# because vte treats it as a parameter separator, so it must be %3B.
out_semi=$(TERMCMP_ACTIVE=1 PATH="${SMOKE_BIN}:${PATH}" INTEGRATION="${SHELL_INTEGRATION}" \
    bash --noprofile --norc -c '
source "$INTEGRATION"
READLINE_LINE="git log; ls -la"
READLINE_POINT=16
_tc_report_buffer
' 2>&1)
assert_contains $'\e]7772;16;' "${out_semi}"
assert_contains 'git log%3B ls -la' "${out_semi}"
assert_not_contains $';16;git log;' "${out_semi}"  # raw ; would corrupt frame

# BEL (0x07) must be %07 — a raw BEL terminates the OSC sequence.
out_bel=$(TERMCMP_ACTIVE=1 PATH="${SMOKE_BIN}:${PATH}" INTEGRATION="${SHELL_INTEGRATION}" \
    bash --noprofile --norc -c '
source "$INTEGRATION"
READLINE_LINE=$'"'"'\x07alert'"'"'
READLINE_POINT=0
_tc_report_buffer
' 2>&1)
assert_contains '%07alert' "${out_bel}"

# ESC (0x1B) must be %1B — a raw ESC starts a new escape sequence.
out_esc=$(TERMCMP_ACTIVE=1 PATH="${SMOKE_BIN}:${PATH}" INTEGRATION="${SHELL_INTEGRATION}" \
    bash --noprofile --norc -c '
source "$INTEGRATION"
READLINE_LINE=$'"'"'\x1bX'"'"'
READLINE_POINT=0
_tc_report_buffer
' 2>&1)
assert_contains '%1B' "${out_esc}"
assert_contains 'X' "${out_esc}"

# Percent (0x25) must be %25 — raw % creates ambiguous escape sequences.
out_pct=$(TERMCMP_ACTIVE=1 PATH="${SMOKE_BIN}:${PATH}" INTEGRATION="${SHELL_INTEGRATION}" \
    bash --noprofile --norc -c '
source "$INTEGRATION"
READLINE_LINE="100%"
READLINE_POINT=4
_tc_report_buffer
' 2>&1)
assert_contains $'\e]7772;4;100%25' "${out_pct}"

# Multibyte UTF-8: κόσμε = CE BA CF 8C CF 83 CE BC CE B5 (10 bytes,
# 5 chars). Each byte must be individually percent-encoded.
out_utf=$(TERMCMP_ACTIVE=1 PATH="${SMOKE_BIN}:${PATH}" INTEGRATION="${SHELL_INTEGRATION}" \
    bash --noprofile --norc -c '
source "$INTEGRATION"
READLINE_LINE="κόσμε"
READLINE_POINT=5
_tc_report_buffer
' 2>&1)
assert_contains '%CE%BA%CF%8C%CF%83%CE%BC%CE%B5' "${out_utf}"

# Gate guard: without TERMCMP_ACTIVE the reporter is a no-op, so an OSC
# 7772 frame can never leak into terminal scrollback.
out_gate=$(env -u TERMCMP_ACTIVE PATH="${SMOKE_BIN}:${PATH}" INTEGRATION="${SHELL_INTEGRATION}" \
    bash --noprofile --norc -c '
source "$INTEGRATION"
READLINE_LINE="leaked"
READLINE_POINT=6
_tc_report_buffer
echo "after"
' 2>&1)
assert_not_contains $'\e]7772' "${out_gate}"
assert_contains 'after' "${out_gate}"

echo "--- OSC 7773 env reporting ---"

# A regular exported variable must reach the frame with `=` percent-encoded.
out_env=$(TERMCMP_ACTIVE=1 PATH="${SMOKE_BIN}:${PATH}" INTEGRATION="${SHELL_INTEGRATION}" \
    bash --noprofile --norc -c '
source "$INTEGRATION"
export TC_TEST_VAR=hello
_tc_report_env
' 2>&1)
assert_contains $'\e]7773;' "${out_env}"
assert_contains 'TC_TEST_VAR%3Dhello' "${out_env}"

# Fingerprint + payload dedup: an unchanged environment must not re-emit.
out_dedup=$(TERMCMP_ACTIVE=1 PATH="${SMOKE_BIN}:${PATH}" INTEGRATION="${SHELL_INTEGRATION}" \
    bash --noprofile --norc -c '
source "$INTEGRATION"
export TC_DEDUP_VAR=stable
_tc_report_env
_tc_report_env
' 2>&1)
dedup_count="$(count_frames "${out_dedup}" $'\e]7773;')"
if [[ "${dedup_count}" != "1" ]]; then
    printf 'FAIL: unchanged env emitted %s OSC 7773 frames, expected 1\nOutput:\n%s\n' \
        "${dedup_count}" "${out_dedup}" >&2
    exit 1
fi

# Per-value cap (16384 bytes) drops the oversized value and emits the
# one-shot 7774 truncation diagnostic; the small value survives.
out_trunc=$(TERMCMP_ACTIVE=1 PATH="${SMOKE_BIN}:${PATH}" INTEGRATION="${SHELL_INTEGRATION}" \
    bash --noprofile --norc -c '
source "$INTEGRATION"
printf -v TC_BIG "%020000d" 0
export TC_BIG
export TC_KEEP=ok
_tc_report_env
' 2>&1)
assert_contains $'\e]7774;env_truncated;' "${out_trunc}"
assert_contains 'TC_KEEP%3Dok' "${out_trunc}"

# Gate guard: env snapshots are gated on TERMCMP_ACTIVE too.
out_env_gate=$(env -u TERMCMP_ACTIVE PATH="${SMOKE_BIN}:${PATH}" INTEGRATION="${SHELL_INTEGRATION}" \
    bash --noprofile --norc -c '
source "$INTEGRATION"
export TC_GATED=1
_tc_report_env || true
echo "after"
' 2>&1)
assert_not_contains $'\e]7773' "${out_env_gate}"
assert_contains 'after' "${out_env_gate}"

echo "--- Init guard (herdr-aware proxy decision) ---"

# Guard cases run in fresh interactive bash processes (`-i` so `$-`
# contains `i`) with a canned ps stub so neither the stub nor the case
# environment can leak. Fake process table: the walker starts at this
# shell's real $PPID; unknown pids map per-case to self's parent
# (100 = herdr server, 90 = termcmp proxy, 1 = launchd). A fake
# `termcmp` is prepended to PATH so `command -v termcmp` succeeds.
# TERMCMP_INIT_SKIP_TTY_CHECK is the documented test seam: the guard
# requires an interactive tty, which a piped `bash -c` does not have.
GUARD_FAKE_BIN="$(mktemp -d)"
printf '#!/bin/sh\nexit 0\n' > "${GUARD_FAKE_BIN}/termcmp"
chmod +x "${GUARD_FAKE_BIN}/termcmp"

guard_case() {
    local name="$1"; shift
    # Hermetic: strip any termcmp/herdr env inherited from THIS shell
    # before each case; cases set exactly what they need.
    env -u TERMCMP_ACTIVE -u HERDR_ENV -u GHOSTTY_RESOURCES_DIR \
        -u TMUX -u TERMCMP_PANE -u TERM_PROGRAM \
        PATH="${GUARD_FAKE_BIN}:${PATH}" \
        TERMCMP_INIT_NO_AUTORUN=1 TERMCMP_INIT_SKIP_TTY_CHECK=1 \
        INIT_BASH="${INIT_BASH}" CASE="${name}" "$@" \
        bash --noprofile --norc -i -c '
ps() {
  [ "$CASE" = psfail ] && return 1
  case "$2" in
    ppid=*)
      # Only ancestors are queried; they chain toward init.
      echo 1 ;;
    comm=*)
      # Bare basenames: init.bash strips the directory prefix and a
      # leading "-" before comparing, so bare names are the right shape.
      case "$CASE" in
        herdr)             echo herdr ;;
        stack|tmux-direct) echo termcmp ;;
        *)                 echo launchd ;;
      esac ;;
  esac
}
source "$INIT_BASH"
if __termcmp_should_proxy; then st=0; else st=1; fi
if [[ -n "${TERMCMP_ACTIVE:-}" ]]; then v=set; else v=unset; fi
printf "GUARD_%s=%s VAR=%s\n" "$CASE" "$st" "$v"
'
}

GUARD_OUTPUT=""
GUARD_OUTPUT+="$(guard_case tmux-fresh TMUX=/tmp/tmux-0,default,999 GHOSTTY_RESOURCES_DIR=/fake TERMCMP_ACTIVE=1)"$'\n'
GUARD_OUTPUT+="$(guard_case herdr TERMCMP_ACTIVE=1)"$'\n'
GUARD_OUTPUT+="$(guard_case stack TERMCMP_ACTIVE=1)"$'\n'
GUARD_OUTPUT+="$(guard_case leak TERMCMP_ACTIVE=1 TERM_PROGRAM=ghostty)"$'\n'
GUARD_OUTPUT+="$(guard_case herdr-env HERDR_ENV=1 TERM_PROGRAM=ghostty)"$'\n'
GUARD_OUTPUT+="$(guard_case psfail TERMCMP_ACTIVE=1)"$'\n'
GUARD_OUTPUT+="$(guard_case tmux-direct TMUX=/tmp/tmux-0,default,999)"$'\n'

assert_guard_line() {
    local label="$1" expected="$2"
    if ! printf '%s\n' "${GUARD_OUTPUT}" | grep -qxF -- "${expected}"; then
        printf 'FAIL: guard %s — expected exact line %q\nOutput:\n%s\n' \
            "${label}" "${expected}" "${GUARD_OUTPUT}" >&2
        exit 1
    fi
    echo "  ok: ${label}"
}

assert_guard_line "herdr boundary launches own proxy (scenarios 1+2)" 'GUARD_herdr=0 VAR=unset'
assert_guard_line "no stacking under inherited guard var" 'GUARD_stack=1 VAR=set'
assert_guard_line "leaked var dropped, proxy still launched" 'GUARD_leak=0 VAR=unset'
assert_guard_line "HERDR_ENV fallback launches proxy" 'GUARD_herdr-env=0 VAR=unset'
assert_guard_line "inconclusive walk honors guard" 'GUARD_psfail=1 VAR=set'
assert_guard_line "tmux direct child of proxy skips" 'GUARD_tmux-direct=1 VAR=unset'
assert_guard_line "fresh tmux pane launches proxy" 'GUARD_tmux-fresh=0 VAR=set'

# The interactive guard has no override: a non-interactive shell must
# never exec the proxy, even when the tty-check seam is unset.
out_noninteractive=$(env -u TERMCMP_ACTIVE -u HERDR_ENV -u GHOSTTY_RESOURCES_DIR \
    -u TMUX -u TERMCMP_PANE -u TERM_PROGRAM -u TERMCMP_INIT_SKIP_TTY_CHECK \
    PATH="${GUARD_FAKE_BIN}:${PATH}" \
    TERMCMP_INIT_NO_AUTORUN=1 INIT_BASH="${INIT_BASH}" \
    bash --noprofile --norc -c '
source "$INIT_BASH"
if __termcmp_should_proxy; then st=0; else st=1; fi
printf "NONINTERACTIVE=%s\n" "$st"
' 2>&1)
assert_contains 'NONINTERACTIVE=1' "${out_noninteractive}"

rm -rf "${GUARD_FAKE_BIN}"

echo "--- Integration idempotency (tests/shell/test_bash_idempotency.sh) ---"
bash "${REPO_ROOT}/tests/shell/test_bash_idempotency.sh"

echo "OK: bash smoke passed"