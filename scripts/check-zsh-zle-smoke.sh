#!/usr/bin/env bash
# Standalone zsh/ZLE smoke. Runs /bin/zsh --no-rcs, sources the production
# shell integration, drives _tc_report_buffer through ZLE, asserts OSC 7772
# emission and percent-encoding for ;, BEL, ESC, %, UTF-8, and cursor
# positions. CI-required on macOS; never falls back silently.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SHELL_INTEGRATION="${REPO_ROOT}/shell/termcmp.zsh"

if [[ ! -f "${SHELL_INTEGRATION}" ]]; then
    echo "FAIL: shell integration not found at ${SHELL_INTEGRATION}" >&2
    exit 1
fi

if ! command -v zsh >/dev/null 2>&1; then
    echo "FAIL: zsh not on PATH" >&2
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

# Test: percent-encode semicolons in buffer.
out_semi=$(TERMCMP_ACTIVE=1 zsh --no-rcs -c "
source '${SHELL_INTEGRATION}'
BUFFER='git log; ls -la'
CURSOR=16
_tc_report_buffer
" 2>&1)
assert_contains $'\e]7772;16;' "${out_semi}"
assert_contains 'git log%3B ls -la' "${out_semi}"
assert_not_contains $';16;git log;' "${out_semi}"  # raw ; would corrupt frame

# Test: percent-encode BEL (0x07) in buffer.
out_bel=$(TERMCMP_ACTIVE=1 zsh --no-rcs -c "
source '${SHELL_INTEGRATION}'
BUFFER=\$'\x07alert'
CURSOR=0
_tc_report_buffer
" 2>&1)
assert_contains '%07alert' "${out_bel}"

# Test: percent-encode ESC (0x1B).
out_esc=$(TERMCMP_ACTIVE=1 zsh --no-rcs -c "
source '${SHELL_INTEGRATION}'
BUFFER=\$'\x1bX'
CURSOR=0
_tc_report_buffer
" 2>&1)
assert_contains '%1B' "${out_esc}"
assert_contains 'X' "${out_esc}"

# Test: percent-encode percent sign (literal % in buffer).
out_pct=$(TERMCMP_ACTIVE=1 zsh --no-rcs -c "
source '${SHELL_INTEGRATION}'
BUFFER='100%'
CURSOR=4
_tc_report_buffer
" 2>&1)
assert_contains $'\e]7772;4;100%25' "${out_pct}"

# Test: UTF-8 round-trip (κόσμε).
out_utf=$(TERMCMP_ACTIVE=1 zsh --no-rcs -c "
source '${SHELL_INTEGRATION}'
BUFFER='κόσμε'
CURSOR=5
_tc_report_buffer
" 2>&1)
assert_contains '%CE%BA%CF%8C%CF%83%CE%BC%CE%B5' "${out_utf}"

# Test: OSC 7 path encoder no longer leaks ';'.
out_osc7=$(zsh --no-rcs -c "
source '${SHELL_INTEGRATION}'
printf 'PATH=%s\n' \"\$(_tc_urlencode_path '/tmp/foo;bar/baz')\"
" 2>&1)
assert_contains 'PATH=/tmp/foo%3Bbar/baz' "${out_osc7}"

# Test: gate guard — without TERMCMP_ACTIVE, _tc_report_buffer is a no-op.
out_gate=$(env -u TERMCMP_ACTIVE zsh --no-rcs -c "
source '${SHELL_INTEGRATION}'
BUFFER='leaked'
CURSOR=6
_tc_report_buffer
echo 'after'
" 2>&1)
assert_not_contains $'\e]7772' "${out_gate}"
assert_contains 'after' "${out_gate}"

echo "--- Init guard (herdr-aware proxy decision) ---"

# Guard cases run in fresh zsh processes with a canned ps stub so neither
# the stub nor the case environment can leak. Fake process table: the
# walker starts at this shell's real $PPID; unknown pids map per-case to
# self's parent (100 = herdr server, 90 = termcmp proxy, 1 = launchd).
# A fake `termcmp` is prepended to PATH so `command -v termcmp` succeeds
# without installing the binary.
GUARD_FAKE_BIN="$(mktemp -d)"
printf '#!/bin/sh\nexit 0\n' > "${GUARD_FAKE_BIN}/termcmp"
chmod +x "${GUARD_FAKE_BIN}/termcmp"

guard_case() {
    local name="$1"; shift
    # Hermetic: strip any termcmp/herdr env inherited from THIS shell
    # before each case; cases set exactly what they need.
    env -u TERMCMP_ACTIVE -u HERDR_ENV -u GHOSTTY_RESOURCES_DIR \
        -u TMUX -u TERMCMP_PANE \
        PATH="${GUARD_FAKE_BIN}:${PATH}" \
        TERMCMP_INIT_NO_AUTORUN=1 CASE="${name}" "$@" \
        zsh --no-rcs -c "
ps() {
  [ \"\$CASE\" = psfail ] && return 1
  case \"\$2\" in
    ppid=*)
      # Only ancestors are queried; they chain toward init.
      echo 1 ;;
    comm=*)
      # Bare basenames: the zsh tmux branch compares comm output
      # literally against "termcmp", matching how a PATH-exec'd proxy
      # appears in ps. Unknown pid = this shell's parent, resolved
      # per topology.
      case \"\$CASE\" in
        herdr)             echo herdr ;;
        stack|tmux-direct) echo termcmp ;;
        *)                 echo launchd ;;
      esac ;;
  esac
}
source '${REPO_ROOT}/shell/init.zsh'
if __termcmp_should_proxy; then st=0; else st=1; fi
if [[ -n \"\${TERMCMP_ACTIVE:-}\" ]]; then v=set; else v=unset; fi
print -r -- \"GUARD_\${CASE}=\${st} VAR=\${v}\"
"
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
    if ! printf '%s\n' "$GUARD_OUTPUT" | grep -qxF -- "$expected"; then
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

rm -rf "${GUARD_FAKE_BIN}"

echo "OK: zsh/ZLE smoke passed"
