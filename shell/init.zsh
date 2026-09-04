# Termcmp — terminal init (sourced near the top of .zshrc)
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
_tc_ancestor_is_proxy() {
  local pid=$PPID comm
  local -i depth=0
  while [[ "$pid" != "1" && "$pid" != "0" && -n "$pid" ]]; do
    if ! comm=$(ps -o comm= -p "$pid" 2>/dev/null); then
      return 2
    fi
    [[ -z "$comm" ]] && return 2
    [[ "${comm##*/}" == "termcmp" ]] && return 0
    [[ "${comm##*/}" == "herdr" ]] && return 3
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

# Decide whether this shell should exec the termcmp proxy.
# Returns 0 (proxy wanted) or 1 (skip). Pure decision — its only side
# effect is dropping a leaked TERMCMP_ACTIVE when the walk proves the
# variable came from a sibling terminal.
__termcmp_should_proxy() {
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
    [[ "$(ps -o comm= -p "$PPID" 2>/dev/null)" == "termcmp" ]] && return 1
    [[ -n "$TERMCMP_PANE" && "$TERMCMP_PANE" == "$TMUX_PANE" ]] && return 1
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
        return 0
      fi
    fi
    return 1
  else
    # Outside tmux: TERMCMP_ACTIVE is normally a reliable recursion
    # guard, BUT editors like VSCode/Zed propagate env vars from a launching
    # shell into their integrated terminal. If a user runs `code .` from a
    # termcmp-managed shell, TERMCMP_ACTIVE=1 leaks into
    # VSCode's integrated zsh and would incorrectly disable the proxy there.
    # Fix: classify our ancestry with the walker instead of trusting the
    # variable. The walk runs whenever the guard is set or HERDR_ENV marks
    # a herdr-managed pane (herdr exports it in every pane shell).
    # A herdr boundary hit (status 3) means this is a FRESH herdr pane:
    # any termcmp higher in the ancestry wraps an outer terminal, so we
    # must launch our own proxy — exactly like a tmux pane would. The
    # supported-terminal check is bypassed because the herdr server above
    # us IS the real terminal; requiring inherited GHOSTTY_* vars would
    # make behavior depend on herdr's env propagation.
    if [[ -n "$TERMCMP_ACTIVE" || "$HERDR_ENV" == "1" ]]; then
      _tc_ancestor_is_proxy
      case $? in
        0) return 1 ;;  # A proxy already owns this lineage — never stack.
        2) return 1 ;;  # Uncertain: honoring the guard prevents stacking.
        3)
          # Fresh herdr pane: drop any leaked guard and take over.
          unset TERMCMP_ACTIVE
          if command -v termcmp >/dev/null 2>&1; then return 0; fi
          return 1 ;;
        1)
          # Confirmed no ancestor proxy: the variable was a leak from a
          # sibling terminal — drop it and re-evaluate below.
          unset TERMCMP_ACTIVE ;;
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
    # Covers plain terminals AND herdr panes whose server is not visible
    # in ancestry (HERDR_ENV=1 panes inherit the outer terminal's
    # GHOSTTY_* environment, so the support check still passes there).
    if [[ $supported -eq 1 ]] && command -v termcmp >/dev/null 2>&1; then
      return 0
    fi
    return 1
  fi
}

__termcmp_init() {
  if __termcmp_should_proxy; then
    export TERMCMP_ACTIVE=1
    exec termcmp
  fi
}

# Test seam: setting TERMCMP_INIT_NO_AUTORUN sources this file as pure
# definitions (used by scripts/check-zsh-zle-smoke.sh); normally the init
# runs immediately and the helpers are erased.
if [[ -z "${TERMCMP_INIT_NO_AUTORUN:-}" ]]; then
  __termcmp_init
  unset -f __termcmp_init __termcmp_should_proxy _tc_ancestor_is_proxy
fi
