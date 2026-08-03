# Termcmp — terminal init (sourced near the top of config.fish)
# Detects the terminal emulator and exec's termcmp as a PTY proxy.

# Walk PPID ancestry looking for the termcmp binary. Returns 0 if
# found, 1 if confirmed absent (walk reached init/root), 2 if the walk could
# not complete (ps failure, disappeared PID, pathological depth). Callers
# should treat 2 as "uncertain" and take the safe path (honor the guard).
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
        # returns the launch path).
        set -l base (string replace -r '.*/' '' -- $comm)
        set base (string replace -r '^-' '' -- $base)
        if string match -q -- 'termcmp*' $base
            return 0
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

function __termcmp_init
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
                        return
                    end
                end
            end
        end
        if set -q TERMCMP_PANE; and test "$TERMCMP_PANE" = "$TMUX_PANE"
            return
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
            set -gx TERMCMP_ACTIVE 1
            exec termcmp
        end
    else
        # Outside tmux: TERMCMP_ACTIVE is normally a reliable recursion
        # guard, BUT editors like VSCode/Zed propagate env vars from a launching
        # shell into their integrated terminal. If a user runs `code .` from a
        # termcmp-managed shell, TERMCMP_ACTIVE=1 leaks into
        # VSCode's integrated fish and would incorrectly disable the proxy there.
        # Fix: if our parent-process ancestry does not include a termcmp
        # process, the variable is a leak from a sibling terminal — drop it. We walk the
        # full PPID ancestry (not just the direct parent) so subshells like `fish`
        # typed at the prompt still hit the guard via their grandparent
        # termcmp process. If the walk is inconclusive (ps failure),
        # default to honoring the guard — preventing recursive proxy stacking
        # is more important than recovering from a leaked env var.
        if set -q TERMCMP_ACTIVE
            _termcmp_ancestor_is_proxy
            switch $status
                case 0
                    return
                case 1
                    set -e TERMCMP_ACTIVE
                case '*'
                    return
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
        if test $supported -eq 1; and command -q termcmp
            set -gx TERMCMP_ACTIVE 1
            exec termcmp
        end
    end
end
__termcmp_init
functions -e __termcmp_init _termcmp_ancestor_is_proxy
