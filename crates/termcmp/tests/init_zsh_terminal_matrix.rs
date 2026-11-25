//! Pin the shell init auto-launch contract: any terminal with a real,
//! ANSI-capable TTY launches the proxy. The shell side MUST NOT keep a
//! supported-terminal allow-list — that gate is what left rex (and every
//! other unknown terminal) without termcmp. Per-terminal rendering
//! knowledge lives only in the Rust `TerminalProfile` matrix; new
//! terminals are added there for their capabilities, while unknown
//! terminals already run on the safe rendering profile.

use terminal::TerminalProfile;

/// Env vars the old shell-side allow-list gated on. They must not appear
/// in init.zsh at all: any occurrence means auto-launch became conditional
/// on a per-terminal marker again.
const OLD_ALLOWLIST_ENV_VARS: &[&str] = &[
    "GHOSTTY_RESOURCES_DIR",
    "KITTY_WINDOW_ID",
    "WEZTERM_UNIX_SOCKET",
    "ALACRITTY_SOCKET",
    "ZED_TERM",
    "VSCODE_IPC_HOOK_CLI",
    "ITERM_SESSION_ID",
];

fn init_zsh_text() -> String {
    std::fs::read_to_string("../../shell/init.zsh").expect("read shell/init.zsh")
}

/// Extract the non-tmux direct branch from init.zsh.
///
/// The file has two branches delimited by these unique comment strings:
///   - `# Inside tmux:` — start of the tmux branch (inside `if [[ -n "$TMUX" ]]; then`)
///   - `# Outside tmux:` — start of the direct (non-tmux) branch (inside `else`)
///
/// The direct branch is everything after `# Outside tmux:`.
fn direct_branch(src: &str) -> &str {
    src.split("# Outside tmux:")
        .nth(1)
        .expect("init.zsh must contain the '# Outside tmux:' marker")
}

/// Extract the tmux branch from init.zsh.
///
/// The tmux branch is the text between `# Inside tmux:` and `# Outside tmux:`.
fn tmux_branch(src: &str) -> &str {
    src.split("# Inside tmux:")
        .nth(1)
        .expect("init.zsh must contain the '# Inside tmux:' marker")
        .split("# Outside tmux:")
        .next()
        .expect("tmux branch must be terminated by '# Outside tmux:'")
}

#[test]
fn init_zsh_has_no_supported_terminal_allowlist() {
    let src = init_zsh_text();
    for var in OLD_ALLOWLIST_ENV_VARS {
        assert!(
            !src.contains(var),
            "init.zsh must not gate auto-launch on ${var}; unknown terminals run the proxy on the safe profile"
        );
    }
    assert!(
        !src.contains("TERM_PROGRAM"),
        "init.zsh must not branch on TERM_PROGRAM; terminal capabilities are decided by the Rust profile, not the shell"
    );
}

#[test]
fn init_zsh_branches_launch_whenever_termcmp_is_on_path() {
    let src = init_zsh_text();
    let branches = [("tmux", tmux_branch(&src)), ("direct", direct_branch(&src))];
    for (name, branch) in branches {
        assert!(
            branch.contains("command -v termcmp >/dev/null 2>&1 && return 0"),
            "{name} branch must fall through to the PATH lookup unconditionally"
        );
    }
}

#[test]
fn init_zsh_requires_real_tty_and_drops_dumb_terminals() {
    let src = init_zsh_text();
    assert!(
        src.contains("[[ -t 0 && -t 1 ]] || [[ -n \"$TERMCMP_INIT_SKIP_TTY_CHECK\" ]] || return 1"),
        "init.zsh must require a real tty on stdin+stdout (with the documented test seam)"
    );
    assert!(
        src.contains("[[ \"$TERM\" == dumb ]] && return 1"),
        "init.zsh must never proxy a TERM=dumb terminal (Emacs shell-mode)"
    );
}

#[test]
fn rust_terminal_enum_has_wezterm_variant() {
    // Sanity: the WezTerm variant exists on the Rust side — the one place
    // per-terminal knowledge is allowed to live now that the shell gate is
    // gone. Env-based detection of WezTerm via socket is exercised by
    // terminal's own unit tests (test_detect_wezterm_direct_via_socket); we
    // can't drive that path from an integration test without mutating
    // process-global env vars, which is unsafe under cargo test's parallel
    // runner.
    let profile = TerminalProfile::for_wezterm();
    assert!(matches!(profile.terminal(), terminal::Terminal::WezTerm));
}

#[test]
fn init_zsh_recognises_herdr_panes() {
    // Rust `Multiplexer::Herdr` detection and the shell-side herdr guard
    // must stay in sync: adding a multiplexer needs both the Rust enum
    // variant and an init.zsh marker.
    let src = init_zsh_text();
    assert!(
        src.contains("HERDR_ENV"),
        "init.zsh must gate herdr panes via HERDR_ENV"
    );
    assert!(
        src.contains("== \"herdr\""),
        "init.zsh must stop the ancestor walk at a herdr process"
    );
}
