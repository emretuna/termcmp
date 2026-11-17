//! End-to-end OSC 7772 round-trip: real bash emitter → real Rust parser.
//!
//! Spawns an actual `bash --noprofile --norc -c` for each fixture,
//! sources the production `shell/termcmp.bash` with a stub `termcmp` on
//! PATH (the integration no-ops without the binary), assigns
//! `READLINE_LINE`/`READLINE_POINT`, calls `_tc_report_buffer`, captures
//! stdout, and feeds the bytes through `parser::TerminalParser`. The
//! reconstructed buffer must equal the input. Every fixture asserts CWD
//! did not change before/after the round-trip; this is meaningful for the
//! OSC-injection fixture and proves the decoded bytes never re-entered the
//! VTE state machine for the rest (the narrow 7772 alphabet prevents
//! `;`/BEL/ESC splitting).
//!
//! Bash scalars cannot hold NUL bytes, and real Readline input lines never
//! contain one, so the NUL fixture pins the honest behavior: the emitter
//! observes the prefix before the NUL and the parser still decodes a clean
//! frame with a clamped cursor.
//!
//! `bash` must be >= 4.4 (READLINE_LINE/READLINE_POINT under `bind -x`,
//! `compgen -e`). Silently skipped on local dev systems without a new
//! enough bash on PATH; panics under CI (`CI` env var set) to fail loud.

use std::path::PathBuf;
use std::process::Command;

use parser::TerminalParser;

/// Encode arbitrary bytes into a bash `$'…'` literal. Every byte goes
/// through as `\xXX` — verbose but unambiguous, no quoting edge cases.
fn to_bash_literal(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 4 + 3);
    out.push_str("$'");
    for &b in bytes {
        out.push_str(&format!("\\x{b:02X}"));
    }
    out.push('\'');
    out
}

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("pty crate should be two dirs deep")
        .to_path_buf()
}

fn bash_version() -> Option<(u32, u32)> {
    let output = Command::new("bash")
        .arg("-c")
        .arg("echo \"${BASH_VERSINFO[0]}.${BASH_VERSINFO[1]}\"")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let (major, minor) = raw.trim().split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// bash >= 4.4 provides `READLINE_LINE`/`READLINE_POINT` under `bind -x`
/// and `compgen -e`, both used by the production integration.
fn bash_available() -> bool {
    matches!(bash_version(), Some((major, minor)) if major > 4 || (major == 4 && minor >= 4))
}

/// Skip locally when bash is missing/too old; fail loud under CI so a
/// controlled environment never silently loses this coverage.
fn require_bash(test: &str) -> bool {
    if bash_available() {
        return true;
    }
    let found = bash_version()
        .map(|(major, minor)| format!("{major}.{minor}"))
        .unwrap_or_else(|| "missing".to_string());
    if std::env::var_os("CI").is_some() {
        panic!(
            "{test}: bash >= 4.4 required but found {found} — install bash (brew install bash) \
             or mark this test #[ignore] explicitly"
        );
    }
    eprintln!("skipping {test}: bash >= 4.4 not on PATH (found {found}; local dev)");
    false
}

/// Create a directory containing a no-op `termcmp` stub. The integration
/// returns early without it, so every emit would otherwise be empty.
fn stub_bin_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create stub bin dir");
    let stub = dir.path().join("termcmp");
    std::fs::write(&stub, "#!/bin/sh\nexit 0\n").expect("write termcmp stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))
            .expect("chmod termcmp stub");
    }
    dir
}

/// Build a bash process whose PATH resolves the stub `termcmp` first.
fn bash_command(stub_dir: &std::path::Path) -> Command {
    let mut cmd = Command::new("bash");
    let inherited = std::env::var("PATH").unwrap_or_default();
    cmd.env("PATH", format!("{}:{inherited}", stub_dir.display()));
    cmd.arg("--noprofile").arg("--norc");
    cmd
}

/// Run the production emitter for `(buffer, cursor)` and return raw stdout.
fn emit_via_real_bash(buffer: &[u8], cursor: usize) -> Vec<u8> {
    let bash_init = repo_root().join("shell/termcmp.bash");
    assert!(bash_init.exists(), "shell/termcmp.bash not found");

    let stub = stub_bin_dir();
    let script = format!(
        "TERMCMP_ACTIVE=1; source {init_q}; READLINE_LINE={buf}; READLINE_POINT={cursor}; \
         _tc_report_buffer",
        init_q = shell_quote(bash_init.to_str().expect("path is utf-8")),
        buf = to_bash_literal(buffer),
    );

    let output = bash_command(stub.path())
        .arg("-c")
        .arg(&script)
        .output()
        .expect("bash -c failed to launch");
    assert!(
        output.status.success(),
        "bash exited non-zero: stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Run the production bash env reporter after applying the supplied shell body.
fn emit_env_via_real_bash(setup: &str) -> Vec<u8> {
    let bash_init = repo_root().join("shell/termcmp.bash");
    assert!(bash_init.exists(), "shell/termcmp.bash not found");

    let stub = stub_bin_dir();
    let script = format!(
        "TERMCMP_ACTIVE=1; source {init_q}; {setup}; _tc_report_env",
        init_q = shell_quote(bash_init.to_str().expect("path is utf-8")),
    );

    let output = bash_command(stub.path())
        .arg("-c")
        .arg(&script)
        .output()
        .expect("bash -c failed to launch");
    assert!(
        output.status.success(),
        "bash exited non-zero: stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

/// Count non-overlapping occurrences of `needle` in `haystack`.
fn count_subslices(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            count += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    count
}

/// POSIX-quote a string for safe interpolation as one bash argument.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

fn assert_roundtrips(label: &str, fixture: &[u8]) {
    let cursor = std::str::from_utf8(fixture)
        .expect("fixture is valid UTF-8")
        .chars()
        .count();
    assert_roundtrips_expected(
        label,
        fixture,
        std::str::from_utf8(fixture).unwrap(),
        cursor,
    );
}

/// Assert the parser reconstructed `expected` at `expected_cursor` from the
/// real bash emitter's output. The two differ only for the NUL fixture,
/// where bash cannot represent the NUL (see the module doc).
fn assert_roundtrips_expected(label: &str, fixture: &[u8], expected: &str, expected_cursor: usize) {
    let cursor = std::str::from_utf8(fixture)
        .expect("fixture is valid UTF-8")
        .chars()
        .count();
    let stdout = emit_via_real_bash(fixture, cursor);

    let mut p = TerminalParser::new(24, 80);
    let cwd_before = p.state().cwd().cloned();
    p.process_bytes(&stdout);

    assert_eq!(
        p.state().command_buffer(),
        Some(expected),
        "[{label}] reconstruction failed; raw bash stdout = {stdout:02X?}"
    );
    assert_eq!(
        p.state().buffer_cursor(),
        expected_cursor,
        "[{label}] buffer cursor not preserved through real-bash round-trip"
    );
    assert_eq!(
        p.state().cwd().cloned(),
        cwd_before,
        "[{label}] OSC 7772 dispatch must not change cwd"
    );
}

#[test]
fn osc7772_real_bash_roundtrip() {
    if !require_bash("osc7772_real_bash_roundtrip") {
        return;
    }

    // Each fixture exercises a different failure mode of the legacy raw
    // 7770 framing: ';' splitting, BEL early-terminate, ESC[…m colour
    // codes, multi-byte UTF-8, and a deliberate OSC 7 smuggle.
    assert_roundtrips("plain semicolon", b"echo a; ls -la");
    assert_roundtrips("compound", b"if true; then echo a; fi");
    assert_roundtrips("bel inside", b"x\x07y");
    assert_roundtrips("ansi colour", b"\x1b[31mred\x1b[0m");
    assert_roundtrips("cjk + semicolon", "日本語; cmd".as_bytes());

    // Smuggle attempt: the buffer LOOKS like OSC 7 (CWD update). After
    // round-trip the `cwd` MUST remain unchanged — the decoded bytes go
    // straight to `set_command_buffer`, not back through the VTE parser.
    assert_roundtrips("osc7 smuggle attempt", b"\x1b]7;file:///etc/passwd\x07");

    // bash scalars are NUL-terminated, so `READLINE_LINE=$'a\x00b'` holds
    // only "a". Real Readline lines cannot contain NUL either. Pin that
    // the emitter still produces a clean frame for the surviving prefix
    // and the parser clamps the reported cursor (3 > len 1) to the buffer
    // end instead of corrupting state.
    assert_roundtrips_expected("embedded NUL (bash truncates at NUL)", b"a\x00b", "a", 1);

    assert_roundtrips("embedded ESC+ST", b"foo\x1b\\bar");
    assert_roundtrips("all-encoded percent", b"abc%20def");
    assert_roundtrips("empty buffer", b"");
}

#[test]
fn osc7773_real_bash_env_roundtrip() {
    if !require_bash("osc7773_real_bash_env_roundtrip") {
        return;
    }

    let stdout = emit_env_via_real_bash(
        "export TERMCMP_ACTIVE=1; export AWS_PROFILE=dev; export EMPTY=; \
         export FOO=$'a;b\\nc'",
    );

    let mut p = TerminalParser::new(24, 80);
    p.process_bytes(&stdout);
    let env = p.state().shell_env().expect("OSC 7773 env report expected");

    assert_eq!(env.get("AWS_PROFILE").map(String::as_str), Some("dev"));
    assert_eq!(env.get("EMPTY").map(String::as_str), Some(""));
    assert_eq!(env.get("FOO").map(String::as_str), Some("a;b\nc"));
    assert!(
        env.get("TERMCMP_ACTIVE").is_none(),
        "TERMCMP_ACTIVE must not leak into the env snapshot"
    );
}

#[test]
fn osc7773_per_value_cap_drops_oversized_value_keeps_others() {
    if !require_bash("osc7773_per_value_cap_drops_oversized_value_keeps_others") {
        return;
    }

    // 20000 bytes > the 16384-byte per-value cap. Build the oversized
    // value inside bash so the Rust-side command line stays small.
    let stdout =
        emit_env_via_real_bash("printf -v TC_BIG '%020000d' 0; export TC_BIG; export BAR=baz");

    let mut p = TerminalParser::new(24, 80);
    p.process_bytes(&stdout);
    let env = p.state().shell_env().expect("OSC 7773 env report expected");

    assert_eq!(env.get("BAR").map(String::as_str), Some("baz"));
    assert!(
        env.get("TC_BIG").is_none(),
        "TC_BIG ({} bytes) should have been dropped by the per-value cap",
        env.get("TC_BIG").map(String::len).unwrap_or(0)
    );
}

#[test]
fn osc7773_env_truncated_diagnostic_when_dropping() {
    if !require_bash("osc7773_env_truncated_diagnostic_when_dropping") {
        return;
    }

    let stdout =
        emit_env_via_real_bash("printf -v TC_BIG '%020000d' 0; export TC_BIG; export BAR=baz");

    let needle = b"\x1b]7774;env_truncated;";
    assert!(
        count_subslices(&stdout, needle) >= 1,
        "expected at least one OSC 7774 env_truncated frame, got stdout = {stdout:02X?}"
    );
}

#[test]
fn osc7773_silent_when_inactive() {
    if !require_bash("osc7773_silent_when_inactive") {
        return;
    }

    // With TERMCMP_ACTIVE unset the proxy is not watching, and the raw
    // OSC frame would otherwise render into the terminal's scrollback.
    let bash_init = repo_root().join("shell/termcmp.bash");
    let stub = stub_bin_dir();
    let script = format!(
        "unset TERMCMP_ACTIVE; source {init_q}; export FOO=x; _tc_report_env || true",
        init_q = shell_quote(bash_init.to_str().expect("path is utf-8")),
    );
    let output = bash_command(stub.path())
        .arg("-c")
        .arg(&script)
        .output()
        .expect("bash -c failed to launch");
    assert!(output.status.success(), "bash exited non-zero");

    assert_eq!(
        count_subslices(&output.stdout, b"\x1b]7773;"),
        0,
        "no 7773 frame should be emitted when TERMCMP_ACTIVE is unset; stdout = {:02X?}",
        output.stdout
    );
}
