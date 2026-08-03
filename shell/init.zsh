# Termcmp — terminal init (sourced near the top of .zshrc)
# Detects the terminal emulator and exec's termcmp as a PTY proxy.

# Walk PPID ancestry looking for the termcmp binary. Returns 0 if
# found, 1 if confirmed absent (walk reached init/root), 2 if the walk could
# not complete (ps failure, disappeared PID, pathological depth). Callers
# should treat 2 as "uncertain" and take the safe path (honor the guard).
_tc_ancestor_is_proxy() {
  local pid=$PPID comm
  local -i depth=0
  while [[ "$pid" != "1" && "$pid" != "0" && -n "$pid" ]]; do
    if ! comm=$(ps -o comm= -p "$pid" 2>/dev/null); then
      return 2
    fi
    [[ -z "$comm" ]] && return 2
    [[ "${comm##*/}" == "termcmp" ]] && return 0
    if ! pid=$(ps -o ppid= -p "$pid" 2>/dev/null); then
      return 2
    fi
    pid="${pid// /}"
    [[ -z "$pid" ]] && return 2
    (( depth++ ))
    (( depth > 32 )) && return 2
  done
  return 1
}

__termcmp_init() {
  if [[ -n "$TMUX" ]]; then
    # Inside tmux: two guards prevent stacking proxies.
    #
    # 1) PPID check — catches the direct child shell. Works because
    #    `exec termcmp` replaces the shell process, so the spawned
    #    inner shell's PPID is the termcmp binary itself.
    # 2) TERMCMP_PANE — catches subshells (zsh/bash typed at the
    #    prompt). spawn.rs sets TERMCMP_PANE=$TMUX_PANE in the child
    #    env; subshells inherit it. A new tmux pane gets a fresh env without
    #    this variable, so it correctly launches a new proxy.
    #
    # We cannot use TERMCMP_ACTIVE here because it is always present
    # in tmux — set by proxy.rs (tmux setenv) for future-pane propagation,
    # and inherited from the outer terminal shell that launched tmux.
    [[ "$(ps -o comm= -p "$PPID" 2>/dev/null)" == "termcmp" ]] && return
    [[ -n "$TERMCMP_PANE" && "$TERMCMP_PANE" == "$TMUX_PANE" ]] && return
    if [[ -n "$GHOSTTY_RESOURCES_DIR" ]] || \
       [[ -n "$KITTY_WINDOW_ID" ]] || \
       [[ -n "$WEZTERM_UNIX_SOCKET" ]] || \
       [[ -n "$ALACRITTY_SOCKET" ]] || \
       [[ -n "$ZED_TERM" ]] || \
       [[ -n "$VSCODE_IPC_HOOK_CLI" ]] || \
       [[ -n "$ITERM_SESSION_ID" ]] || \
       [[ "$TERM_PROGRAM" == "rio" ]] || \
       [[ "$TERM_PROGRAM" == "otty" ]]; then
      if command -v termcmp >/dev/null 2>&1; then
        export TERMCMP_ACTIVE=1
        exec termcmp
      fi
    fi
  else
    # Outside tmux: TERMCMP_ACTIVE is normally a reliable recursion
    # guard, BUT editors like VSCode/Zed propagate env vars from a launching
    # shell into their integrated terminal. If a user runs `code .` from a
    # termcmp-managed shell, TERMCMP_ACTIVE=1 leaks into
    # VSCode's integrated zsh and would incorrectly disable the proxy there.
    # Fix: if our parent-process ancestry does not include a termcmp
    # process, the variable is a leak from a sibling terminal — drop it. We walk the
    # full PPID ancestry (not just $PPID) so subshells like `zsh`/`bash`
    # typed at the prompt still hit the guard via their grandparent
    # termcmp process. If the walk is inconclusive (ps failure),
    # default to honoring the guard — preventing recursive proxy stacking
    # is more important than recovering from a leaked env var.
    if [[ -n "$TERMCMP_ACTIVE" ]]; then
      _tc_ancestor_is_proxy
      case $? in
        0) return ;;
        1) unset TERMCMP_ACTIVE ;;
        *) return ;;
      esac
    fi
    local supported=0
    if [[ -n "$KITTY_WINDOW_ID" ]] \
      || [[ -n "$WEZTERM_UNIX_SOCKET" ]] \
      || [[ -n "$ALACRITTY_SOCKET" ]] \
      || [[ -n "$ZED_TERM" ]] \
      || [[ -n "$VSCODE_IPC_HOOK_CLI" ]]; then
      supported=1
    else
      case "$TERM_PROGRAM" in
        ghostty|otty|WezTerm|rio|iTerm.app|Apple_Terminal|zed|vscode) supported=1 ;;
      esac
    fi
    if [[ $supported -eq 1 ]] && command -v termcmp >/dev/null 2>&1; then
      export TERMCMP_ACTIVE=1
      exec termcmp
    fi
  fi
}
__termcmp_init
unset -f __termcmp_init
