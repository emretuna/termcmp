#!/usr/bin/env bash
# Fish shell-integration smoke test.
#
# Validates the production shell/termcmp.fish OSC 7772 buffer framing
# end-to-end: percent-encoding of ;, BEL, ESC, %, and multibyte UTF-8,
# plus the OSC 7 path encoder and binding-install idempotency.
#
# Design: fish's `commandline` builtin requires a genuine interactive tty
# (unlike zsh's plain $BUFFER/$CURSOR variables), so we shadow it with a
# test function that returns a controlled buffer/cursor. The production
# _termcmp_report_buffer runs unmodified against the shadow — exercising
# the real encoder and printf framing. This is fully deterministic: no
# expect, no pty, no timing.
#
# Exit codes: 0 = pass, 1 = assertion failure, 2 = missing tool.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
INTEGRATION="$REPO_ROOT/shell/termcmp.fish"

command -v fish >/dev/null 2>&1 || { echo "SKIP: fish not on PATH"; exit 2; }
[ -f "$INTEGRATION" ] || { echo "FAIL: $INTEGRATION not found"; exit 1; }

FAILURES=0
fail() { echo "FAIL: $*"; FAILURES=$((FAILURES + 1)); }

# --- Generate the fish test script ------------------------------------------
# The shadow function replaces `commandline` so _termcmp_report_buffer reads
# a controlled buffer/cursor pair. Each case prints a labelled frame.
FISH_SCRIPT="$(mktemp)"
trap 'rm -f "$FISH_SCRIPT"' EXIT

cat > "$FISH_SCRIPT" <<'FISH'
source INTEGRATION_PLACEHOLDER

# Shadow commandline: _termcmp_report_buffer calls `commandline` (buffer)
# and `commandline -C` (cursor). We return controlled values.
function _tc_report_with --argument-names cur buf
    set -g _TC_CUR $cur
    set -g _TC_BUF $buf
    function commandline
        if contains -- -C $argv
            printf '%s' $_TC_CUR
        else
            printf '%s' $_TC_BUF
        end
    end
    _termcmp_report_buffer
    functions -e commandline
end

printf 'CASE_SEMI='; _tc_report_with 16 'git log; ls -la'; printf '\n'
printf 'CASE_BEL='; _tc_report_with 0 (printf '\aalert'); printf '\n'
printf 'CASE_ESC='; _tc_report_with 0 (printf '\x1bX'); printf '\n'
printf 'CASE_PCT='; _tc_report_with 4 '100%'; printf '\n'
printf 'CASE_UTF8='; _tc_report_with 5 'κόσμε'; printf '\n'

# OSC 7 path encoder: semicolons must be percent-encoded (vte splits on ;)
printf 'PATH_ENC='; _termcmp_urlencode_path '/tmp/foo;bar/baz'; printf '\n'

# Binding idempotency: source 3x, count must stay 1
source INTEGRATION_PLACEHOLDER
source INTEGRATION_PLACEHOLDER
printf 'BIND_COUNT='; bind | string match -r '_termcmp_report_buffer' | count; printf '\n'

# No legacy 7770 emission
printf 'LEGACY='; bind | string match -r '7770' | count; printf '\n'
FISH

# Replace placeholder with actual path (fish source needs a real path)
sed -i.bak "s|INTEGRATION_PLACEHOLDER|$INTEGRATION|g" "$FISH_SCRIPT"
rm -f "$FISH_SCRIPT.bak"

# --- Run and capture --------------------------------------------------------
OUTPUT="$(fish --no-config "$FISH_SCRIPT" 2>&1)"

assert_contains() {
    local label="$1" needle="$2"
    if printf '%s' "$OUTPUT" | grep -qF -- "$needle"; then
        echo "  ok: $label"
    else
        fail "$label — expected '$needle' in output"
    fi
}

assert_not_contains() {
    local label="$1" needle="$2"
    if printf '%s' "$OUTPUT" | grep -qF -- "$needle"; then
        fail "$label — '$needle' must NOT appear in output"
    else
        echo "  ok: $label"
    fi
}

assert_line() {
    local label="$1" expected="$2"
    if printf '%s\n' "$OUTPUT" | grep -qxF -- "$expected"; then
        echo "  ok: $label"
    else
        fail "$label — expected exact line '$expected'"
    fi
}

echo "--- OSC 7772 buffer framing ---"
# Semicolon: the core ADR-0003 regression. Raw ; corrupts the frame because
# vte splits OSC parameters on ;. Must be %3B.
assert_contains "semicolon encoded" 'CASE_SEMI='$'\e]7772;16;git log%3B ls -la'$'\a'
assert_not_contains "no raw semicolon frame" $';16;git log;'

# BEL (0x07) must be %07 — a raw BEL terminates the OSC sequence.
assert_contains "BEL encoded" 'CASE_BEL='$'\e]7772;0;%07alert'$'\a'

# ESC (0x1B) must be %1B — a raw ESC starts a new escape sequence.
assert_contains "ESC encoded" 'CASE_ESC='$'\e]7772;0;%1BX'$'\a'

# Percent (0x25) must be %25 — raw % creates ambiguous escape sequences.
assert_contains "percent encoded" 'CASE_PCT='$'\e]7772;4;100%25'$'\a'

# Multibyte UTF-8: κόσμε = CE BA CF 8C CF 83 CE BC CE B5 (10 bytes, 5 chars).
# Each byte must be individually percent-encoded.
assert_contains "UTF-8 encoded" 'CASE_UTF8='$'\e]7772;5;%CE%BA%CF%8C%CF%83%CE%BC%CE%B5'$'\a'

echo "--- OSC 7 path encoder ---"
assert_contains "path semicolon encoded" 'PATH_ENC=/tmp/foo%3Bbar/baz'

echo "--- Binding idempotency ---"
assert_line "exactly one binding after 3x source" 'BIND_COUNT=1'

echo "--- Legacy framing ---"
assert_line "no 7770 bindings" 'LEGACY=0'

# --- Verdict ----------------------------------------------------------------
if [ "$FAILURES" -gt 0 ]; then
    echo ""
    echo "FAIL: fish smoke failed ($FAILURES assertion(s))"
    echo "--- captured output ---"
    printf '%s\n' "$OUTPUT" | cat -v
    exit 1
fi

echo ""
echo "OK: fish smoke passed"
