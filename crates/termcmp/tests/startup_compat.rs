//! Startup compatibility regressions.
//!
//! The proxy must never block terminal startup on the terminal type, a
//! malformed config file, or a `$SHELL` that resolves back to termcmp itself,
//! and it must refuse a non-interactive stdio pair with an actionable message
//! instead of a cryptic raw-mode error. Each test here pins one of those
//! contracts end-to-end against the real binary.

mod harness;

use harness::TermcmpProcess;
use parser::test_utils::build_osc7772_envelope;
use std::process::{Command, Stdio};
use std::time::Duration;

/// OSC 133;A (prompt start) followed by OSC 7772 (command buffer), then one
/// printable display byte.
///
/// 133;A must precede 7772: it is what sets `in_prompt` in the parser, and
/// without it the popup eligibility check rejects the buffer. The trailing
/// byte makes the deferred trigger resolve in the same PTY batch (the shell
/// parks on a `read` gate immediately after `printf`, so no later output is
/// guaranteed — an OSC-only batch stays deferred, as
/// `smoke::test_popup_defers_until_display_after_osc_only_pty_read` pins).
fn popup_envelope() -> Vec<u8> {
    let mut envelope = Vec::new();
    envelope.extend_from_slice(b"\x1b]133;A\x07");
    envelope.extend_from_slice(&build_osc7772_envelope("git ", 4));
    envelope.push(b'X');
    envelope
}

/// A terminal outside the tested set (`foot`) must run the proxy with the safe
/// rendering profile — no exec fallback — and the popup must still render.
///
/// Before the startup-gate removal the proxy replaced itself with a plain
/// shell whenever a terminal was unknown and `multi_terminal` was unset, so
/// no completion UI ever appeared.
#[test]
fn unknown_terminal_starts_and_completes() {
    // TERM_PROGRAM=foot is not a known terminal. The other terminal-specific
    // markers are removed so a developer's ambient kitty/wezterm/vscode env
    // cannot outrank it in detection (the harness only strips the multiplexer
    // markers by default).
    let mut proc = TermcmpProcess::spawn_with_env(
        &[("TERM_PROGRAM", "foot")],
        &[
            "KITTY_WINDOW_ID",
            "WEZTERM_UNIX_SOCKET",
            "ALACRITTY_SOCKET",
            "ZED_TERM",
            "VSCODE_IPC_HOOK_CLI",
            "ITERM_SESSION_ID",
        ],
    );

    proc.send_line("echo unknown_term_ok");
    proc.expect_output("unknown_term_ok");

    let mark = proc.output_len();
    proc.send_line(&harness::osc_printf_cmd(
        &popup_envelope(),
        "_tc_unknown_gate",
    ));

    let rendered = proc.wait_for_bytes_after(b"\x1b7", mark, Duration::from_secs(5));
    if !rendered {
        let snapshot = proc.output_snapshot();
        panic!(
            "unknown terminal produced no popup render (DECSC \\x1b7) within 5s.\n\
             Output since mark ({} bytes, lossy UTF-8):\n{:?}",
            snapshot.len().saturating_sub(mark),
            String::from_utf8_lossy(&snapshot[mark.min(snapshot.len())..]),
        );
    }

    // Dismiss the popup and release the `read` gate holding the shell.
    proc.write_raw(&[0x1b]);
    proc.send_line("");
    let code = proc.exit_with_code(0);
    assert_eq!(
        code, 0,
        "expected clean exit after the popup render, got {code}"
    );
}

/// A malformed config file must warn and fall back to defaults instead of
/// aborting startup — the same policy the hot-reload watcher uses.
#[test]
fn malformed_config_starts_with_defaults() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "this = is = not = valid = toml\n")
        .expect("write malformed config");

    let mut proc = TermcmpProcess::spawn_with_config(&config_path, &[], &[]);

    // The fallback notice is written before the shell spawns, so it is
    // deterministic; assert it directly instead of only observing a working
    // shell (which would also pass if the file were silently ignored).
    proc.expect_output("config load failed");
    proc.send_line("echo config_fallback_ok");
    proc.expect_output("config_fallback_ok");

    let code = proc.exit_with_code(0);
    assert_eq!(
        code, 0,
        "malformed config must fall back to defaults, not abort startup"
    );
}

/// `$SHELL` pointing at termcmp itself must not stack proxies: the None arm
/// resolves $SHELL, the recursion guard swaps in /bin/sh, and the session runs
/// normally.
#[test]
fn shell_pointing_at_termcmp_does_not_recurse() {
    let bin = env!("CARGO_BIN_EXE_termcmp");
    let mut proc = TermcmpProcess::spawn_no_shell_arg(&[("SHELL", bin)]);

    proc.send_line("echo recursion_guard_ok");
    proc.expect_output("recursion_guard_ok");

    let code = proc.exit_with_code(0);
    assert_eq!(
        code, 0,
        "self-referential $SHELL must fall back to /bin/sh, not recurse"
    );
}

/// A missing shell binary must name the offending path in the error and exit
/// non-zero — the actionable form of "termcmp did nothing and quit".
#[test]
fn missing_shell_binary_names_path() {
    let mut proc = TermcmpProcess::spawn_with_shell("/nonexistent/termcmp-test-shell", &[]);

    proc.expect_output("failed to spawn shell");
    proc.expect_output("/nonexistent/termcmp-test-shell");
    proc.wait_for_nonzero_exit(Duration::from_secs(10));
}

/// With no interactive stdin/stdout the proxy must exit 1 with a clear message
/// and a non-empty log, never a raw-mode error.
///
/// The log assertion also pins the placement of the isatty guard: the
/// `starting termcmp proxy` line is written before the guard runs (see
/// `cli_help_routing::proxy_with_no_args_uses_default_shell_from_env`, which
/// depends on the same ordering).
#[test]
fn not_a_tty_exits_with_clear_message() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let log_path = dir.path().join("proxy.log");
    let config_path = dir.path().join("config.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_termcmp"))
        .arg("--log-level")
        .arg("info")
        .arg("--log-file")
        .arg(&log_path)
        .arg("--config")
        .arg(&config_path)
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run termcmp");

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 for a non-tty stdio pair, got {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("stdin/stdout is not a terminal"),
        "stderr must explain the non-tty failure, got:\n{stderr}"
    );

    let log_contents = std::fs::read_to_string(&log_path).expect("proxy log file readable");
    assert!(
        log_contents.contains("starting termcmp proxy"),
        "log must record the proxy startup line (written before the isatty \
         guard), got:\n{log_contents}"
    );
}
