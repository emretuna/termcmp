//! CLI parsing and routing coverage for real clap subcommands.

#[allow(dead_code)]
mod harness;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use harness::TermcmpProcess;
use tempfile::TempDir;

fn ghost_bin() -> PathBuf {
    env!("CARGO_BIN_EXE_termcmp").into()
}

fn isolated_home() -> TempDir {
    TempDir::new().expect("tempdir")
}

fn cmd_with_isolated_home(home: &Path) -> Command {
    let mut cmd = Command::new(ghost_bin());
    cmd.env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_STATE_HOME");
    cmd
}

fn assert_success(output: &std::process::Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn top_level_help_lists_real_subcommands() {
    let tmp = isolated_home();
    let output = cmd_with_isolated_home(tmp.path())
        .arg("--help")
        .output()
        .unwrap();

    assert_success(&output, "top-level --help");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for subcommand in ["install", "uninstall", "config", "doctor"] {
        assert!(
            stdout.contains(subcommand),
            "top-level --help missing {subcommand}; got:\n{stdout}",
        );
    }
}

#[test]
fn real_subcommand_help_exits_zero_and_lists_flags() {
    let cases: &[(&[&str], &[&str])] = &[
        (&["install"], &["--dry-run"]),
        (&["uninstall"], &[]),
        (&["config"], &[]),
        (&["doctor"], &[]),
    ];

    for (argv, expected) in cases {
        let tmp = isolated_home();
        let output = cmd_with_isolated_home(tmp.path())
            .args(*argv)
            .arg("--help")
            .output()
            .unwrap();

        assert_success(&output, &format!("{argv:?} --help"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        for needle in *expected {
            assert!(
                stdout.contains(needle),
                "{argv:?} --help missing {needle}; got:\n{stdout}",
            );
        }
    }
}

/// Pins clap's `ValueEnum` rejection of typo'd `--log-level` values at the
/// parse boundary. The `LogLevel` enum was introduced specifically so a
/// typo like `--log-level deubg` errors out instead of silently being
/// rewritten to `warn` inside `init_tracing`. If a future refactor reverted
/// the field to `log_level: String` (or dropped `value_enum` /
/// `default_value_t`), the rejection would silently disappear and the
/// silent-rewrite-to-warn behavior would return — this test fails loudly
/// in that scenario.
#[test]
fn invalid_log_level_rejected_at_parse_time() {
    let tmp = isolated_home();
    let output = cmd_with_isolated_home(tmp.path())
        .arg("--log-level")
        .arg("deubg")
        .arg("doctor")
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "typo'd --log-level must error at parse time; got success.\n\
         stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--log-level") && stderr.contains("invalid value"),
        "expected clap ValueEnum rejection mentioning `--log-level` and \
         `invalid value`. If this regressed, the `LogLevel` enum was likely \
         reverted to a free-form `String`, restoring the silent-rewrite-to-warn \
         fallback in init_tracing.\nstderr:\n{stderr}"
    );
    // Sanity check that clap is suggesting one of the legal values — pins
    // the `[possible values: ...]` rendering that ValueEnum produces.
    assert!(
        stderr.contains("possible values") || stderr.contains("debug"),
        "expected clap to surface `possible values` or a typo suggestion. \
         Its absence suggests the ValueEnum derive was dropped.\n\
         stderr:\n{stderr}"
    );
}

#[test]
fn unknown_flag_on_real_subcommand_fails() {
    let tmp = isolated_home();
    let output = cmd_with_isolated_home(tmp.path())
        .arg("install")
        .arg("--this-flag-does-not-exist")
        .output()
        .unwrap();

    assert!(!output.status.success(), "unknown flag must error");
}

#[test]
fn globals_parse_before_each_real_subcommand() {
    let cases: &[&[&str]] = &[&["install"], &["uninstall"], &["config"], &["doctor"]];

    for argv in cases {
        let tmp = isolated_home();
        let log_file = tmp.path().join("gc.log");
        let output = cmd_with_isolated_home(tmp.path())
            .arg("--config")
            .arg("/nonexistent/termcmp.toml")
            .arg("--log-level")
            .arg("debug")
            .arg("--log-file")
            .arg(&log_file)
            .args(*argv)
            .arg("--help")
            .output()
            .unwrap();

        assert_success(&output, &format!("globals before {argv:?}"));
    }
}

#[test]
fn globals_parse_after_each_real_subcommand() {
    let cases: &[&[&str]] = &[&["install"], &["uninstall"], &["config"], &["doctor"]];

    for argv in cases {
        let tmp = isolated_home();
        let log_file = tmp.path().join("gc.log");
        let output = cmd_with_isolated_home(tmp.path())
            .args(*argv)
            .arg("--config")
            .arg("/nonexistent/termcmp.toml")
            .arg("--log-level")
            .arg("debug")
            .arg("--log-file")
            .arg(&log_file)
            .arg("--help")
            .output()
            .unwrap();

        assert_success(&output, &format!("globals after {argv:?}"));
    }
}

#[test]
fn external_subcommand_falls_back_to_proxy_mode() {
    let mut proc = TermcmpProcess::spawn();
    proc.send_line("echo proxy-works");
    proc.expect_output("proxy-works");
    let code = proc.exit_with_code(0);
    assert_eq!(code, 0, "expected proxy fallback shell to exit 0");
}

/// Verifies the documented `--` escape hatch from `after_help`: a shell
/// binary whose name collides with a real subcommand can still be launched
/// by prefixing `--`. The test drives both halves — with `--`, clap must
/// route the name through the `External(Vec<OsString>)` arm into proxy
/// mode; without `--`, clap must claim it as that subcommand — so it fails
/// if clap ever stops honouring `--` for `external_subcommand` (the case
/// that would make the `after_help` advice misleading).
///
/// The escape token is `doctor`, a registered subcommand name, so
/// `--` is load-bearing rather than decorative. The positive case pins
/// `$PATH` to an empty directory and drops `RUST_LOG`, so neither the
/// proxy's shell lookup nor the log filter depends on the test runner's
/// environment.
///
/// The positive signal is the `--log-file` log, not the process's
/// stdout/stderr. `run_proxy` records `starting termcmp proxy` with
/// `shell=<argv[0]>` before handing off to `pty::run_proxy` — before any
/// terminal detection or terminal I/O — so it is captured wherever the
/// proxy later bails. The proxy's failure *message* is environment
/// dependent (`failed to spawn shell process` / `failed to exec shell` /
/// `failed to query terminal size`, depending on terminal detection and
/// whether `crossterm` can size a headless runner), so asserting on it
/// would re-couple this routing test to terminal state. Mirrors the
/// log-based signal in `proxy_with_no_args_uses_default_shell_from_env`.
#[test]
fn dash_dash_escape_routes_subcommand_named_shell_to_external() {
    // A registered subcommand name (`Command::Doctor`).
    let escape_token = "doctor";

    // With `--`: clap must route `doctor` through the External arm
    // so it becomes the proxy's shell.
    let tmp = isolated_home();
    let log_file = tmp.path().join("escape.log");
    // Pin the environment so this case is deterministic regardless of the
    // test runner: `$PATH` is an empty dir, so the proxy's lookup of
    // `doctor` always fails (no binary by that name can be found on
    // it); and `RUST_LOG` is dropped, so `--log-level info` — not an
    // inherited filter — governs whether the startup line is recorded.
    let empty_path = tmp.path().join("no-bin");
    std::fs::create_dir_all(&empty_path).expect("create empty PATH dir");
    let escaped = cmd_with_isolated_home(tmp.path())
        .env("PATH", &empty_path)
        .env_remove("RUST_LOG")
        .arg("--log-level")
        .arg("info")
        .arg("--log-file")
        .arg(&log_file)
        .arg("--")
        .arg(escape_token)
        .arg("--some-flag")
        .output()
        .unwrap();

    assert!(
        !escaped.status.success(),
        "spawning a non-existent shell must fail with non-zero exit.\n\
         stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&escaped.stdout),
        String::from_utf8_lossy(&escaped.stderr),
    );

    // clap's "unrecognized subcommand" / "unknown argument" / "for more
    // information, try '--help'" wording must NOT appear — those signal the
    // `External` arm did not catch the routing and clap rejected the input
    // at parse time.
    let escaped_stderr = String::from_utf8_lossy(&escaped.stderr);
    let clap_signatures = [
        "unrecognized subcommand",
        "error: unrecognized",
        "for more information, try",
    ];
    for sig in &clap_signatures {
        assert!(
            !escaped_stderr.to_lowercase().contains(&sig.to_lowercase()),
            "stderr must not contain clap-level `{sig}` — the `--` escape \
             hatch should route past clap into the External arm.\n\
             stderr:\n{escaped_stderr}"
        );
    }

    // Positive signal: the `External` arm reached `run_proxy`, which logs
    // `starting termcmp proxy` with `shell=<argv[0]>`. Both strings
    // present prove the `--`-escaped subcommand name was forwarded into
    // proxy mode as the shell. Reading the log keeps the assertion
    // independent of the environment-dependent failure message above.
    let log = std::fs::read_to_string(&log_file)
        .unwrap_or_else(|e| panic!("log file at {} unreadable: {e}", log_file.display()));
    assert!(
        log.contains("starting termcmp proxy"),
        "log must record the proxy startup line — proves the `--` escape \
         routed into the External arm and reached run_proxy.\nlog:\n{log}"
    );
    assert!(
        log.contains(&format!("shell={escape_token}")),
        "log must record `shell={escape_token}` — proves the `--`-escaped \
         subcommand name became the proxy's shell.\nlog:\n{log}"
    );

    // Negative control: the SAME argv without the leading `--`. clap must
    // now claim `doctor` as `Command::Doctor` and reject the
    // bogus `--some-flag` at parse time. Without this half a regression that
    // made `--` decorative would go unnoticed — the positive case alone
    // cannot tell "escaped by `--`" from "never collided in the first place".
    let tmp_claimed = isolated_home();
    let claimed = cmd_with_isolated_home(tmp_claimed.path())
        .arg(escape_token)
        .arg("--some-flag")
        .output()
        .unwrap();

    let claimed_stderr = String::from_utf8_lossy(&claimed.stderr);
    assert!(
        !claimed.status.success()
            && claimed_stderr.contains(escape_token)
            && claimed_stderr.contains("--some-flag"),
        "without `--`, `{escape_token}` must be claimed as a clap subcommand \
         that rejects `--some-flag` at parse time — proving the `--` in the \
         positive case is load-bearing.\nstderr:\n{claimed_stderr}"
    );
}

/// Pins the `None => run_proxy(..., Vec::new())` routing arm in `main.rs`.
/// Invokes termcmp with NO positional argv and no subcommand. The
/// proxy reads `$SHELL` via `resolve_default_shell()`, logs `"starting
/// termcmp proxy"` with that shell, then bails when `enable_raw_mode`
/// fails (stdin is routed through `Stdio::null()` and stdout/stderr through
/// `Stdio::piped()` — none of them is a TTY, so `enable_raw_mode` fails
/// fast). The log file is the deterministic signal: if the `None` arm
/// regressed (e.g. swapped with `External`, hard-coded a different shell,
/// or panicked), the recorded `shell=` line would not match the $SHELL
/// value we set — or the log would be empty because tracing never
/// initialised.
///
/// Avoids the PTY harness intentionally — the PTY-backed harness always
/// passes a positional `/bin/sh`, which exercises only the `External(...)`
/// arm. This test fills the gap for the None arm without touching the
/// harness module.
#[test]
fn proxy_with_no_args_uses_default_shell_from_env() {
    let tmp = isolated_home();
    let log_file = tmp.path().join("ghost.log");
    // Pick a recognisable shell path that differs from the host's $SHELL —
    // the assertion below checks this exact string appears in the log so
    // we know the None arm consulted $SHELL rather than a hard-coded
    // fallback.
    let marker_shell = "/tmp/termcmp-none-arm-marker-shell";

    let output = cmd_with_isolated_home(tmp.path())
        .env("SHELL", marker_shell)
        .arg("--log-level")
        .arg("info")
        .arg("--log-file")
        .arg(&log_file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    // Process exits non-zero because raw mode fails outside a TTY; this is
    // expected and indicates the None arm reached `run_proxy`. (A 0 exit
    // here would suggest a different code path ran — for example, a
    // refactor that turned the None arm into a no-op.)
    assert!(
        !output.status.success(),
        "expected non-zero exit from proxy when stdin is not a TTY (raw-mode \
         init fails), got success. The None routing arm likely regressed.\n\
         stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // The log file must record the proxy starting with our marker shell —
    // proving the None arm ran `resolve_default_shell` and called
    // `run_proxy` with the result.
    let log_contents = std::fs::read_to_string(&log_file)
        .unwrap_or_else(|e| panic!("log file at {} unreadable: {e}", log_file.display()));
    assert!(
        log_contents.contains("starting termcmp proxy"),
        "log must record the proxy startup line — proves the None arm \
         reached run_proxy.\nlog contents:\n{log_contents}"
    );
    assert!(
        log_contents.contains(marker_shell),
        "log must mention `shell={marker_shell}` — proves the None arm \
         resolved $SHELL via resolve_default_shell rather than using \
         a hard-coded fallback or routing to the wrong arm.\n\
         log contents:\n{log_contents}"
    );
}
