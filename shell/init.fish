# Termcmp — terminal init (sourced near the top of config.fish)
# Detects the terminal emulator and exec's termcmp as a PTY proxy.
# herdr-managed panes are recognized and each launches its own proxy.

# Walk PPID ancestry looking for a termcmp proxy process. Returns:
#   0 — proxy found (an ancestor is termcmp)
#   1 — confirmed absent (walk reached init/root)
#   2 — inconclusive (ps failure, disappeared PID, pathological depth);
#       callers treat this as "uncertain" and take the safe path (honor
#       the guard)
#   3 — a herdr server was hit first: this shell is a fresh herdr pane,
#       and any termcmp higher in the ancestry belongs to an OUTER
#       terminal wrapping herdr, not to this pane
function _termcmp_ancestor_is_proxy
    # fish has no $PPID; resolve our parent via ps.
    if not set -q fish_pid
        return 2
    end
    set -l pid (ps -o ppid= -p $fish_pid 2>/dev/null | string trim)
    if test -z "$pid"
        return 2
    end
    set -l depth 0
    while test -n "$pid"; and test "$pid" != "1"; and test "$pid" != "0"
        set -l comm (ps -o comm= -p "$pid" 2>/dev/null | string trim)
        if test -z "$comm"
            return 2
        end
        # Strip directory prefix and leading '-' (login shells), then
        # match basename starting with "termcmp" (macOS ps -o comm=
        # returns the launch path). A herdr server ancestor marks a
        # fresh herdr pane: return before any higher termcmp is seen.
        set -l base (string replace -r '.*/' '' -- $comm)
        set base (string replace -r '^-' '' -- $base)
        if string match -q -- 'termcmp*' $base
            return 0
        end
        if string match -q -- 'herdr*' $base
            return 3
        end
        set pid (ps -o ppid= -p "$pid" 2>/dev/null | string trim)
        if test -z "$pid"
            return 2
        end
        set depth (math $depth + 1)
        if test $depth -gt 32
            return 2
        end
    end
    return 1
end
# Decide whether this shell should exec the termcmp proxy.
# Returns 0 (proxy wanted) or 1 (skip). Pure decision — its only side
# effect is dropping a leaked TERMCMP_ACTIVE when the walk proves the
# variable came from a sibling terminal.
function __termcmp_should_proxy
    if set -q TMUX; and test -n "$TMUX"
        # Inside tmux: two guards prevent stacking proxies.
        #
        # 1) PPID check — catches the direct child shell. Works because
        #    `exec termcmp` replaces the shell process, so the spawned
        #    inner shell's PPID is the termcmp binary itself.
        # 2) TERMCMP_PANE — catches subshells (fish typed at the
        #    prompt). spawn.rs sets TERMCMP_PANE=$TMUX_PANE in the child
        #    env; subshells inherit it. A new tmux pane gets a fresh env without
        #    this variable, so it correctly launches a new proxy.
        #
        # We cannot use TERMCMP_ACTIVE here because it is always present
        # in tmux — set by proxy.rs (tmux setenv) for future-pane propagation,
        # and inherited from the outer terminal shell that launched tmux.
        if set -q fish_pid
            set -l ppid (ps -o ppid= -p $fish_pid 2>/dev/null | string trim)
            if test -n "$ppid"
                set -l pcomm (ps -o comm= -p "$ppid" 2>/dev/null | string trim)
                if test -n "$pcomm"
                    set -l pbase (string replace -r '.*/' '' -- $pcomm)
                    set pbase (string replace -r '^-' '' -- $pbase)
                    if string match -q -- 'termcmp*' $pbase
                        return 1
                    end
                end
            end
        end
        if set -q TERMCMP_PANE; and test "$TERMCMP_PANE" = "$TMUX_PANE"
            return 1
        end
        set -l termcmp_supported 0
        for var in GHOSTTY_RESOURCES_DIR KITTY_WINDOW_ID WEZTERM_UNIX_SOCKET ALACRITTY_SOCKET ZED_TERM VSCODE_IPC_HOOK_CLI ITERM_SESSION_ID
            if set -q $var; and test -n "$$var"
                set termcmp_supported 1
                break
            end
        end
        if test $termcmp_supported -eq 0; and set -q TERM_PROGRAM
            switch "$TERM_PROGRAM"
                case rio otty
                    set termcmp_supported 1
            end
        end
        if test $termcmp_supported -eq 1; and command -q termcmp
            return 0
        end
        return 1
    else
        # Outside tmux: TERMCMP_ACTIVE is normally a reliable recursion
        # guard, BUT editors like VSCode/Zed propagate env vars from a launching
        # shell into their integrated terminal. If a user runs `code .` from a
        # termcmp-managed shell, TERMCMP_ACTIVE=1 leaks into
        # VSCode's integrated fish and would incorrectly disable the proxy there.
        # Fix: classify our ancestry with the walker instead of trusting the
        # variable. The walk runs whenever the guard is set or HERDR_ENV marks
        # a herdr-managed pane (herdr exports it in every pane shell).
        # A herdr boundary hit (status 3) means this is a FRESH herdr pane:
        # any termcmp higher in the ancestry wraps an outer terminal, so we
        # must launch our own proxy — exactly like a tmux pane would. The
        # supported-terminal check is bypassed because the herdr server above
        # us IS the real terminal; requiring inherited GHOSTTY_* vars would
        # make behavior depend on herdr's env propagation.
        if set -q TERMCMP_ACTIVE; or test "$HERDR_ENV" = "1"
            _termcmp_ancestor_is_proxy
            switch $status
                case 0
                    # A proxy already owns this lineage — never stack.
                    return 1
                case 2
                    # Uncertain: honoring the guard prevents recursive stacking.
                    return 1
                case 3
                    # Fresh herdr pane: drop any leaked guard and take over.
                    set -e TERMCMP_ACTIVE
                    if command -q termcmp
                        return 0
                    end
                    return 1
                case 1
                    # Confirmed no ancestor proxy: the variable was a leak
                    # from a sibling terminal — drop it and re-evaluate below.
                    set -e TERMCMP_ACTIVE
            end
        end
        set -l supported 0
        for var in KITTY_WINDOW_ID WEZTERM_UNIX_SOCKET ALACRITTY_SOCKET ZED_TERM VSCODE_IPC_HOOK_CLI
            if set -q $var; and test -n "$$var"
                set supported 1
                break
            end
        end
        if test $supported -eq 0; and set -q TERM_PROGRAM
            switch "$TERM_PROGRAM"
                case ghostty otty WezTerm rio iTerm.app Apple_Terminal zed vscode
                    set supported 1
            end
        end
        # Covers plain terminals AND herdr panes whose server is not visible
        # in ancestry (HERDR_ENV=1 panes inherit the outer terminal's
        # GHOSTTY_* environment, so the support check still passes there).
        if test $supported -eq 1; and command -q termcmp
            return 0
        end
        return 1
    end
end

function __termcmp_init
    if __termcmp_should_proxy
        set -gx TERMCMP_ACTIVE 1
        exec termcmp
    end
end

# Test seam: setting TERMCMP_INIT_NO_AUTORUN sources this file as pure
# definitions (used by scripts/check-fish-smoke.sh); normally the init
# runs immediately and the helpers are erased.
if not set -q TERMCMP_INIT_NO_AUTORUN
    __termcmp_init
    functions -e __termcmp_init __termcmp_should_proxy _termcmp_ancestor_is_proxy
end
