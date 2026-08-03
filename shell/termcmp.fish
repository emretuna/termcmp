# Termcmp -- Fish integration
# Source this in config.fish.

# Percent-encode a path for file:// URIs (RFC 8089).
function _termcmp_urlencode_path
    set -l path $argv[1]
    set -l encoded ""
    for hex in (printf '%s' "$path" | od -An -tx1 | string split ' ')
        test -z "$hex"; and continue
        set -l dec (math "0x$hex")
        if begin
            test $dec -ge 0x61 -a $dec -le 0x7a
            or test $dec -ge 0x41 -a $dec -le 0x5a
            or test $dec -ge 0x30 -a $dec -le 0x39
            or test $dec -eq 0x2e -o $dec -eq 0x5f -o $dec -eq 0x7e
            or test $dec -eq 0x2f -o $dec -eq 0x2d
        end
            set encoded "$encoded"(printf "%b" "\\x$hex")
        else
            set encoded "$encoded"(printf '%%%s' (string upper $hex))
        end
    end
    echo -n $encoded
end

# True when the host terminal natively parses OSC 133 for its own prompt
# tracking (or emits its own proprietary markers on top, like VSCode's
# OSC 633). In those terminals our OSC 7771 fallback is redundant — the
# terminal already understands the OSC 133 we emit below, and OSC 7771
# only exists for terminals that mangle OSC 133. Currently covers
# Ghostty, Otty (a Ghostty fork), Zed, and VSCode (the latter only once
# its shell integration is active, signalled by VSCODE_INJECTION being set).
function _termcmp_native_osc133
    if test "$TERM_PROGRAM" = ghostty -o -n "$GHOSTTY_RESOURCES_DIR"
        return 0
    end
    # Otty is a Ghostty fork; it parses OSC 133 natively like Ghostty.
    if test "$TERM_PROGRAM" = otty
        return 0
    end
    if test -n "$ZED_TERM"
        return 0
    end
    if test -n "$VSCODE_INJECTION"
        return 0
    end
    return 1
end

function _termcmp_prompt --on-event fish_prompt
    printf '\e]133;A\a'
    if not _termcmp_native_osc133
        printf '\e]7771;A\a'
    end
    # Report current working directory via OSC 7 for filesystem completions
    printf '\e]7;file://%s%s\a' "$hostname" (_termcmp_urlencode_path "$PWD")
    # Report exported environment variables for context-aware completions
    _termcmp_report_env
end

function _termcmp_preexec --on-event fish_preexec
    printf '\e]133;C\a'
    if not _termcmp_native_osc133
        printf '\e]7771;C\a'
    end
end

# Percent-encode a buffer string for OSC 7772 transport.
# Strict alphabet: [a-zA-Z0-9._~ /-] pass through, everything else %XX.
function _termcmp_urlencode_buffer
    set -l input $argv[1]
    set -l encoded ""
    for hex in (printf '%s' "$input" | od -An -tx1 | string split ' ')
        test -z "$hex"; and continue
        set -l dec (math "0x$hex")
        if begin
            test $dec -ge 0x61 -a $dec -le 0x7a
            or test $dec -ge 0x41 -a $dec -le 0x5a
            or test $dec -ge 0x30 -a $dec -le 0x39
            or test $dec -eq 0x2e -o $dec -eq 0x5f -o $dec -eq 0x7e
            or test $dec -eq 0x20 -o $dec -eq 0x2f -o $dec -eq 0x2d
        end
            set encoded "$encoded"(printf "%b" "\\x$hex")
        else
            set encoded "$encoded"(printf '%%%s' (string upper $hex))
        end
    end
    echo -n $encoded
end

# Report buffer via OSC 7772 (percent-encoded)
function _termcmp_report_buffer
    set -l buf (commandline)
    set -l cursor (commandline -C)
    set -l encoded (_termcmp_urlencode_buffer "$buf")
    printf '\e]7772;%d;%s\a' $cursor "$encoded"
end

# Encode a single KEY=VALUE pair for OSC 7773 transport.
# Returns 1 (prints nothing) when the key should be skipped or the
# encoded entry exceeds the per-value cap.
function _termcmp_env_entry --argument-names key cap
    # Skip termcmp-internal vars so we never leak our own state.
    switch $key
        case 'TERMCMP_*' '_TERMCMP_*'
            return 1
    end
    # Only valid shell variable names.
    string match -qr '^[a-zA-Z_][a-zA-Z0-9_]*$' -- $key; or return 1
    # Must be exported.
    set -qx -- $key; or return 1
    set -l entry (_termcmp_urlencode_buffer "$key=$$key")
    test (string length -- $entry) -le $cap; or return 1
    echo -n $entry
end

# Report exported environment variables via OSC 7773.
# Budget: 524288 bytes total, 16384 per value.
# High-priority vars first, then auth-prefix vars, then everything else.
# Deduplicates against the previous payload to avoid redundant frames.
function _termcmp_report_env
    test -n "$TERMCMP_ACTIVE"; or return

    set -l total_budget 524288
    set -l per_value_cap 16384
    set -l payload ""
    set -l used 0
    set -l truncated 0
    set -l seen "|"

    # High-priority vars first so PATH/credentials survive a tight budget.
    set -l essentials PATH HOME USER SHELL PWD OLDPWD LANG TERM \
        GHOSTTY_RESOURCES_DIR KITTY_WINDOW_ID WEZTERM_UNIX_SOCKET \
        ALACRITTY_SOCKET ZED_TERM VSCODE_INJECTION ITERM_SESSION_ID
    set -l auth_prefixes AWS_ GITHUB_ GH_ GOOGLE_ DOCKER_ KUBECONFIG \
        SSH_AUTH_SOCK XDG_

    for key in $essentials
        set -l entry (_termcmp_env_entry $key $per_value_cap)
        if test $status -eq 0
            set payload "$payload$entry%00"
            set used (math $used + (string length -- $entry) + 3)
            set seen "$seen$key|"
        end
    end

    # Auth-prefix vars next.
    for key in (set -x --names | sort)
        set -l dominated 0
        for prefix in $auth_prefixes
            if string match -q "$prefix*" -- $key
                set dominated 1
                break
            end
        end
        test $dominated -eq 1; or continue
        string match -q "*|$key|*" -- $seen; and continue
        if set -l entry (_termcmp_env_entry $key $per_value_cap)
            if test (math $used + (string length -- $entry) + 3) -gt $total_budget
                set truncated 1
                break
            end
            set payload "$payload$entry%00"
            set used (math $used + (string length -- $entry) + 3)
            set seen "$seen$key|"
        else
            set truncated 1
        end
    end

    # Remaining exported vars.
    for key in (set -x --names | sort)
        string match -q "*|$key|*" -- $seen; and continue
        if set -l entry (_termcmp_env_entry $key $per_value_cap)
            if test (math $used + (string length -- $entry) + 3) -gt $total_budget
                set truncated 1
                break
            end
            set payload "$payload$entry%00"
            set used (math $used + (string length -- $entry) + 3)
            set seen "$seen$key|"
        else
            set truncated 1
        end
    end

    # Dedup: skip if payload unchanged.
    if test "$payload" = "$_TERMCMP_LAST_ENV_PAYLOAD"
        return
    end
    set -g _TERMCMP_LAST_ENV_PAYLOAD "$payload"
    printf '\e]7773;%s\a' "$payload"

    # One-shot truncation diagnostic on OSC 7774.
    if test $truncated -eq 1; and not set -q _TERMCMP_ENV_TRUNCATED_REPORTED
        set -g _TERMCMP_ENV_TRUNCATED_REPORTED 1
        printf '\e]7774;env_truncated;%d\a' $used
    end
end

# Bind Ctrl+/ as manual trigger (0x1F). Fish auto-trigger works via the
# proxy's BufferModel (handler.rs tracks keystrokes directly for non-zsh
# shells), so no per-character bindings are needed.
# Guard with a sentinel so re-sourcing the script (e.g. on config reload)
# doesn't stack duplicate bindings — fish's `bind` happily appends the same
# binding multiple times.
if not set -q __termcmp_bindings_installed
    set -g __termcmp_bindings_installed 1
    bind \x1f '_termcmp_report_buffer'
end