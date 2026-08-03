use anyhow::Result;
use std::path::{Path, PathBuf};

use crate::install::{INIT_BEGIN, INIT_END, SHELL_BEGIN, SHELL_END, ZSH_INIT, ZSH_INTEGRATION};
use crate::sanitize::sanitize_for_terminal;

enum Severity {
    Ok,
    Warn,
    Fail,
    Skip,
}

struct CheckResult {
    severity: Severity,
    message: String,
}

impl CheckResult {
    fn ok(msg: impl Into<String>) -> Self {
        Self {
            severity: Severity::Ok,
            message: msg.into(),
        }
    }
    fn warn(msg: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warn,
            message: msg.into(),
        }
    }
    fn fail(msg: impl Into<String>) -> Self {
        Self {
            severity: Severity::Fail,
            message: msg.into(),
        }
    }
    fn skip(msg: impl Into<String>) -> Self {
        Self {
            severity: Severity::Skip,
            message: msg.into(),
        }
    }
}

fn render_results<W: std::io::Write>(results: &[CheckResult], out: &mut W) -> std::io::Result<()> {
    writeln!(out, "Termcmp Doctor\n")?;

    for result in results {
        let (label, color) = match result.severity {
            Severity::Ok => ("[OK]  ", "\x1b[32m"),
            Severity::Warn => ("[WARN]", "\x1b[33m"),
            Severity::Fail => ("[FAIL]", "\x1b[31m"),
            Severity::Skip => ("[SKIP]", "\x1b[2m"),
        };
        // Messages are composed from attacker-controllable inputs: config
        // spec dirs, keybinding/theme values, shell paths, terminal display
        // strings, OS error text. Strip control chars at the print boundary
        // so a hostile `~/.config/termcmp/config.toml` can't smuggle
        // CSI/OSC sequences through `termcmp doctor` output.
        writeln!(
            out,
            "  {color}{label}\x1b[0m {}",
            sanitize_for_terminal(&result.message)
        )?;
    }

    let fails = results
        .iter()
        .filter(|r| matches!(r.severity, Severity::Fail))
        .count();
    let warns = results
        .iter()
        .filter(|r| matches!(r.severity, Severity::Warn))
        .count();

    writeln!(out)?;
    if fails == 0 && warns == 0 {
        writeln!(out, "All checks passed.")?;
    } else if fails == 0 {
        writeln!(out, "{warns} warning(s).")?;
    } else {
        writeln!(out, "{fails} issue(s) found.")?;
    }
    Ok(())
}

fn print_results(results: &[CheckResult]) {
    let _ = render_results(results, &mut std::io::stdout().lock());
}

/// Check 1: Config file valid
fn check_config(config_path: Option<&Path>) -> (CheckResult, Option<config::TermcmpConfig>) {
    let path = match config_path {
        Some(p) => p.to_path_buf(),
        None => {
            let Some(dir) = config::config_dir() else {
                // HOME unset — refuse to probe CWD for config.
                return (
                    CheckResult::warn("Config file: HOME unset, using defaults"),
                    Some(config::TermcmpConfig::default()),
                );
            };
            dir.join("config.toml")
        }
    };

    if !path.exists() {
        return (
            CheckResult::ok("Config file: using defaults (no config.toml found)"),
            Some(config::TermcmpConfig::default()),
        );
    }

    match config::TermcmpConfig::load(config_path) {
        Ok(config) => (
            CheckResult::ok(format!("Config file valid ({})", path.display())),
            Some(config),
        ),
        Err(e) => (
            CheckResult::fail(format!("Config file invalid ({}): {e}", path.display())),
            None,
        ),
    }
}

/// Check 2: Keybinding names valid
fn check_keybindings(config: &config::TermcmpConfig) -> CheckResult {
    let bindings = [
        ("accept", &config.keybindings.accept),
        ("accept_and_enter", &config.keybindings.accept_and_enter),
        ("dismiss", &config.keybindings.dismiss),
        ("navigate_up", &config.keybindings.navigate_up),
        ("navigate_down", &config.keybindings.navigate_down),
        ("trigger", &config.keybindings.trigger),
        ("toggle_match_mode", &config.keybindings.toggle_match_mode),
    ];

    let mut errors = Vec::new();
    for (name, value) in &bindings {
        if let Err(e) = pty::parse_key_name(value) {
            errors.push(format!("keybindings.{name} = \"{value}\" — {e}"));
        }
    }

    if errors.is_empty() {
        CheckResult::ok(format!("Keybindings valid ({} bindings)", bindings.len()))
    } else {
        CheckResult::fail(format!("Keybindings invalid: {}", errors.join("; ")))
    }
}

/// Check 3: Theme style strings valid
fn check_theme(config: &config::TermcmpConfig) -> CheckResult {
    let resolved = match config.theme.resolve(config::config_dir().as_deref()) {
        Ok(t) => t,
        Err(e) => return CheckResult::fail(format!("Theme preset: {e}")),
    };

    let styles = [
        ("selected", &resolved.selected),
        ("description", &resolved.description),
        ("match_highlight", &resolved.match_highlight),
        ("item_text", &resolved.item_text),
        ("scrollbar", &resolved.scrollbar),
        ("border", &resolved.border),
        ("feedback_loading", &resolved.feedback_loading),
        ("feedback_empty", &resolved.feedback_empty),
        ("feedback_error", &resolved.feedback_error),
        ("kind_icon", &resolved.kind_icon),
    ];

    let mut errors = Vec::new();
    for (name, value) in &styles {
        if let Err(e) = pty::parse_style(value) {
            errors.push(format!("[theme] {name} = \"{value}\" — {e}"));
        }
    }

    if errors.is_empty() {
        CheckResult::ok("Theme styles valid")
    } else {
        CheckResult::fail(format!("Theme style: {}", errors.join("; ")))
    }
}

/// Parsed outcome from probing a managed block's `source` line.
///
/// Distinct outcomes — encoded so the caller can match exhaustively and
/// pick a specific diagnostic instead of conflating them. `BlockNotFound`
/// is unreachable from `check_shell_integration` (which gates on
/// `content.contains(BEGIN)`), but the function does not know that, so
/// it must still encode the outcome rather than panic.
#[derive(Debug, PartialEq, Eq)]
enum BlockSource {
    /// Block well-formed, exactly one `source <path>` line parsed cleanly.
    Parsed(PathBuf),
    /// Block well-formed, but contains two or more parseable `source`
    /// lines. Clean installs only ever emit one per block — a hand edit
    /// or merge-conflict resolution that duplicated the `source` line
    /// silently makes the user's shell execute every listed path on
    /// startup. Surface as Fail with the first + all additional paths
    /// so the user can see the divergence in the diagnostic.
    MultipleSourceLines {
        first: PathBuf,
        additional: Vec<PathBuf>,
    },
    /// `BEGIN` marker present but no matching `END` marker.
    Unterminated,
    /// Block well-formed but contains no `source`/`builtin source` line —
    /// the pre-v0.9 install style inlined `exec termcmp` instead.
    NoSourceLine,
    /// Block contains a `source` line but the path is in a quoting style
    /// the parser doesn't recognize (corruption).
    UnparseableQuoting,
    /// `BEGIN` marker not present at all. Unreachable from
    /// `check_shell_integration`, but encoded for completeness.
    BlockNotFound,
}

/// Extract the file path that the given managed block sources, by parsing
/// the `[builtin ]source '<path>'` or `[builtin ]source "<path>"` line.
///
/// `shell_safe_path` (introduced in v0.7.1) single-quotes the path and
/// escapes embedded `'` with the `'\''` close-quote/escaped-quote/open-quote
/// idiom. The brief v0.6.1–v0.7.0 window wrote raw `path.display()` inside
/// double quotes with NO escaping (worked only because home paths never
/// contain `"`, `\`, `$`, or backticks). Accept both quoting styles so an
/// upgrading user with an intact older managed block gets meaningful doctor
/// output instead of "no parseable source line".
fn extract_block_source_path(content: &str, begin: &str, end: &str) -> BlockSource {
    let Some(block_start) = content.find(begin) else {
        return BlockSource::BlockNotFound;
    };
    let after_begin = &content[block_start..];
    let Some(block_end_offset) = after_begin.find(end) else {
        return BlockSource::Unterminated;
    };
    let block = &after_begin[..block_end_offset];

    let mut saw_source_line = false;
    let mut parsed_paths: Vec<PathBuf> = Vec::new();
    for line in block.lines() {
        let trimmed = line.trim();
        // Match only `[builtin ]source <quoted-path>` lines. Every other
        // line in the managed block (the `if [[ -f ... ]]` guard, the
        // else-branch warnings, the closing `fi`) is rejected by the
        // strip_prefix checks below.
        let after_kw = match trimmed.strip_prefix("builtin source ") {
            Some(s) => s,
            None => match trimmed.strip_prefix("source ") {
                Some(s) => s,
                None => continue,
            },
        };
        saw_source_line = true;
        let quoted = after_kw.trim();

        // shell_safe_path (v0.7.1+) wraps the path in single quotes and
        // escapes embedded `'` as `'\''`.
        if let Some(inner) = quoted.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
            let unescaped = inner.replace("'\\''", "'");
            parsed_paths.push(PathBuf::from(unescaped));
            continue;
        }
        // v0.6.1–v0.7.0 used naked `format!("source \"{}\"", path.display())` —
        // double-quoted, no escaping. Real legacy installs never contained
        // backslash-escape sequences, so the parser must pass the inner
        // bytes through untouched. A path whose home contains a literal
        // `\\` byte would otherwise be silently mangled into `\`.
        if let Some(inner) = quoted.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            parsed_paths.push(PathBuf::from(inner));
            continue;
        }
        // Unrecognized quoting on this line — keep scanning; another line
        // in the same block might still match.
    }
    match parsed_paths.len() {
        0 if saw_source_line => BlockSource::UnparseableQuoting,
        0 => BlockSource::NoSourceLine,
        1 => BlockSource::Parsed(parsed_paths.into_iter().next().expect("len == 1")),
        _ => {
            let mut iter = parsed_paths.into_iter();
            let first = iter.next().expect("len >= 2");
            let additional: Vec<PathBuf> = iter.collect();
            BlockSource::MultipleSourceLines { first, additional }
        }
    }
}

/// Check 4: Shell integration installed in ~/.zshrc.
///
/// Beyond verifying the init marker is present, this also surfaces:
///
/// * a stray init block with no shell-integration block (half-installed),
/// * duplicate init OR shell-integration blocks (a botched manual edit),
/// * a managed block whose `source` line cannot be parsed (hand-edited
///   beyond recognition),
/// * the file the managed block actually `source`s missing or unreadable
///   (so a stale .zshrc pointing at a long-gone install path Fails loudly,
///   even when the canonical install dir happens to have files),
/// * installed snippet contents that drift from the embedded copy
///   (`termcmp install` re-runs the embedded version onto disk),
/// * the legacy OSC 7770 reporter in `termcmp.zsh` (pre-OSC 7772
///   migration),
/// * the `.zshrc` sourcing from a non-canonical location, so the next
///   `termcmp install` won't silently swap the user's edits out.
///
/// Order (each step returns on its first finding so the output stays
/// focused on the most pressing issue):
///
/// 1. both managed blocks present (else half-installed Fail / clean Skip),
/// 2. no duplicate managed blocks (else hand-edit Fail),
/// 3. `source` line in each block is parseable (else class-specific Fail
///    for unterminated / unrecognized quoting / multiple sources / one
///    block missing the source line; both blocks missing is the pre-v0.9
///    install style and Warn-with-migration-nudge),
/// 4. referenced script files exist + are readable (else Fail),
/// 5. legacy OSC 7770 reporter Warn (checked before content drift so an
///    upgrading user with a pre-OSC 7772 file on disk still gets the
///    migration-specific hint),
/// 6. content drift Warn (installed snippets vs embedded),
/// 7. non-canonical path Warn.
///
/// A clean system that never ran `install` (no managed blocks present)
/// uses `Skip` so the doctor still exits 0; partial installs (one block
/// but not the other) are escalated to `Fail` with concrete remediation.
fn check_shell_integration() -> CheckResult {
    let Some(home) = dirs::home_dir() else {
        return CheckResult::skip("no $HOME — cannot check shell integration");
    };
    let zshrc = home.join(".zshrc");
    let content = match std::fs::read_to_string(&zshrc) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return CheckResult::skip(format!("no {} — clean system", zshrc.display()));
        }
        Err(e) => {
            return CheckResult::fail(format!(
                "cannot read {}: {} ({:?})",
                zshrc.display(),
                e,
                e.kind()
            ));
        }
    };

    // 1. Both managed blocks present?
    if !content.contains(INIT_BEGIN) {
        return CheckResult::skip("no termcmp managed block in .zshrc — run `termcmp install`");
    }
    if !content.contains(SHELL_BEGIN) {
        return CheckResult::fail(
            "missing shell-integration managed block — run `termcmp install` to repair",
        );
    }

    // 2. Duplicate managed blocks?
    let init_count = content.matches(INIT_BEGIN).count();
    let shell_count = content.matches(SHELL_BEGIN).count();
    if init_count > 1 || shell_count > 1 {
        return CheckResult::fail(format!(
            "duplicate managed blocks in .zshrc (init={init_count}, shell={shell_count}) — \
             run `termcmp uninstall` then reinstall",
        ));
    }

    // 3. Source line in each block is parseable?
    // Parse the actual source paths from each managed block; the
    // canonical install dir is a separate check below. This catches
    // stale .zshrc managed blocks pointing at long-gone install paths
    // (e.g. a previous XDG_CONFIG_HOME) — the previous canonical-only
    // probe would silently pass if files happened to exist at the
    // canonical location.
    //
    // If BOTH blocks lack a `source` line we treat that as the pre-v0.9
    // install style (which embedded `exec termcmp` inline and
    // had no external script files to verify). Surface that as Warn
    // with a migration nudge — the install is functional but laid out
    // in a layout we no longer write. Other failure modes (missing END
    // marker, unrecognized quoting, exactly one block missing its
    // source line) are hand-edit corruption: Fail loudly with a
    // class-specific message so the user can fix it rather than
    // silently degrading.
    let init_source = extract_block_source_path(&content, INIT_BEGIN, INIT_END);
    let script_source = extract_block_source_path(&content, SHELL_BEGIN, SHELL_END);

    let (init_path, script_path) = match (init_source, script_source) {
        (BlockSource::Parsed(init), BlockSource::Parsed(script)) => (init, script),
        (BlockSource::NoSourceLine, BlockSource::NoSourceLine) => {
            return CheckResult::warn(
                "termcmp managed blocks present in .zshrc but neither references \
                 an external script via `source` — this is the pre-v0.9 install style. \
                 Run `termcmp install` to migrate to the current layout.",
            );
        }
        (BlockSource::Unterminated, _) | (_, BlockSource::Unterminated) => {
            return CheckResult::fail(
                "termcmp managed block in .zshrc is missing its END marker — \
                 run `termcmp uninstall` then reinstall",
            );
        }
        (BlockSource::MultipleSourceLines { first, additional }, _) => {
            let count = 1 + additional.len();
            let paths = std::iter::once(first.display().to_string())
                .chain(additional.iter().map(|p| p.display().to_string()))
                .collect::<Vec<_>>()
                .join(", ");
            return CheckResult::fail(format!(
                "termcmp init block in .zshrc has {count} `source` lines \
                 ({paths}) — every listed path will run at shell startup. Run \
                 `termcmp uninstall` then reinstall to restore a single \
                 source line per block.",
            ));
        }
        (_, BlockSource::MultipleSourceLines { first, additional }) => {
            let count = 1 + additional.len();
            let paths = std::iter::once(first.display().to_string())
                .chain(additional.iter().map(|p| p.display().to_string()))
                .collect::<Vec<_>>()
                .join(", ");
            return CheckResult::fail(format!(
                "termcmp shell-integration block in .zshrc has {count} \
                 `source` lines ({paths}) — every listed path will run at shell \
                 startup. Run `termcmp uninstall` then reinstall to restore \
                 a single source line per block.",
            ));
        }
        (BlockSource::UnparseableQuoting, _) | (_, BlockSource::UnparseableQuoting) => {
            return CheckResult::fail(
                "termcmp managed block in .zshrc has a `source` line with \
                 unrecognized quoting around the path — run `termcmp uninstall` \
                 then reinstall",
            );
        }
        (BlockSource::NoSourceLine, _) => {
            return CheckResult::fail(
                "termcmp init block in .zshrc has no parseable `source` line — \
                 run `termcmp uninstall` then reinstall",
            );
        }
        (_, BlockSource::NoSourceLine) => {
            return CheckResult::fail(
                "termcmp shell-integration block in .zshrc has no parseable \
                 `source` line — run `termcmp uninstall` then reinstall",
            );
        }
        (BlockSource::BlockNotFound, _) | (_, BlockSource::BlockNotFound) => {
            // Gated above by `content.contains(BEGIN)` checks; encoded for
            // exhaustiveness only.
            return CheckResult::fail(
                "termcmp managed block in .zshrc disappeared between checks — \
                 run `termcmp uninstall` then reinstall",
            );
        }
    };

    // 4. Referenced script files exist + readable?
    let installed_init = match std::fs::read_to_string(&init_path) {
        Ok(s) => s,
        Err(e) => {
            return CheckResult::fail(format!(
                "cannot read {}: {} ({:?}) — run `termcmp install` to refresh, \
                 or check file permissions",
                init_path.display(),
                e,
                e.kind(),
            ));
        }
    };
    let installed_script = match std::fs::read_to_string(&script_path) {
        Ok(s) => s,
        Err(e) => {
            return CheckResult::fail(format!(
                "cannot read {}: {} ({:?}) — run `termcmp install` to refresh, \
                 or check file permissions",
                script_path.display(),
                e,
                e.kind(),
            ));
        }
    };

    // 5. Legacy OSC 7770 reporter present? Checked BEFORE the content-drift
    // comparison so an upgrading user with a pre-OSC 7772 termcmp.zsh
    // on disk gets a migration-specific message. The embedded snippet no
    // longer emits OSC 7770, so deferring this check until after the drift
    // comparison would render it unreachable on every upgrade path.
    if installed_script.contains("7770;") && !installed_script.contains("7772;") {
        return CheckResult::warn(
            "shell integration uses legacy OSC 7770 — run `termcmp install` to migrate to OSC 7772",
        );
    }

    // 6. Installed snippets match embedded versions?
    if installed_init != ZSH_INIT || installed_script != ZSH_INTEGRATION {
        return CheckResult::warn(format!(
            "shell integration files at {} / {} drifted from embedded version — run `termcmp install` to refresh",
            init_path.display(),
            script_path.display(),
        ));
    }

    // 7. Source paths point at the canonical install location?
    // Drift here is non-fatal (the user may deliberately ship their own
    // copy), but flag it so the next `termcmp install` doesn't
    // appear to swap their edits out from under them.
    let canonical_shell_dir = home.join(".config/termcmp/shell");
    let canonical_init = canonical_shell_dir.join("init.zsh");
    let canonical_script = canonical_shell_dir.join("termcmp.zsh");
    if init_path != canonical_init || script_path != canonical_script {
        return CheckResult::warn(format!(
            ".zshrc managed block sources non-canonical paths ({}, {}) — expected ({}, {}); \
             run `termcmp install` to refresh",
            init_path.display(),
            script_path.display(),
            canonical_init.display(),
            canonical_script.display(),
        ));
    }

    CheckResult::ok("termcmp shell integration looks healthy")
}

/// Check 5: Running inside a supported terminal
///
/// Uses `TerminalProfile::detect()` as the single source of truth for which
/// terminal is running, avoiding divergence between detect() and is_supported().
fn check_terminal(config: &config::TermcmpConfig) -> CheckResult {
    let profile = terminal::TerminalProfile::detect();
    check_terminal_profile(&profile, config.experimental.multi_terminal)
}

/// Testable terminal check logic — pure function on profile.
fn check_terminal_profile(
    profile: &terminal::TerminalProfile,
    multi_terminal: bool,
) -> CheckResult {
    if !profile.terminal().is_known() {
        if multi_terminal {
            return CheckResult::ok(format!(
                "Unknown terminal ({}) — multi_terminal enabled, proceeding anyway",
                profile.display_name(),
            ));
        }
        return CheckResult::warn(format!(
            "Unsupported terminal ({}) — supported: {}",
            profile.display_name(),
            terminal::Terminal::supported_terminals().join(", ")
        ));
    }

    let msg = format!(
        "Running inside {} (render: {}, prompt: {})",
        profile.display_name(),
        profile.render_strategy(),
        profile.prompt_detection()
    );

    CheckResult::ok(msg)
}

pub fn run_doctor(config_path: Option<&Path>) -> Result<()> {
    let mut results = Vec::new();

    // Check 1: Config file
    let (config_result, config) = check_config(config_path);
    results.push(config_result);

    // Checks 2 & 3 depend on valid config
    match &config {
        Some(cfg) => {
            results.push(check_keybindings(cfg));
            results.push(check_theme(cfg));
        }
        None => {
            results.push(CheckResult::skip("Keybindings — config invalid"));
            results.push(CheckResult::skip("Theme styles — config invalid"));
        }
    }

    // Check 4: Shell integration
    results.push(check_shell_integration());

    // Check 5: Terminal support (needs config for experimental flag)
    match &config {
        Some(cfg) => results.push(check_terminal(cfg)),
        None => results.push(CheckResult::skip(
            "Terminal support — config invalid, cannot check experimental flags",
        )),
    }

    print_results(&results);

    let has_fails = results.iter().any(|r| matches!(r.severity, Severity::Fail));
    if has_fails {
        std::process::exit(1);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirror the real managed-block markers so extraction tests exercise
    // the same delimiter strings the installer writes.
    const TEST_INIT_BEGIN: &str = "# >>> termcmp initialize >>>";
    const TEST_INIT_END: &str = "# <<< termcmp initialize <<<";

    #[test]
    fn test_check_terminal_ghostty_ok() {
        let profile = terminal::TerminalProfile::for_ghostty();
        let result = check_terminal_profile(&profile, false);
        assert!(matches!(result.severity, Severity::Ok));
        assert!(result.message.contains("Ghostty"));
    }

    #[test]
    fn test_check_terminal_kitty_ok() {
        let profile = terminal::TerminalProfile::for_kitty();
        let result = check_terminal_profile(&profile, false);
        assert!(matches!(result.severity, Severity::Ok));
        assert!(result.message.contains("Kitty"));
    }

    #[test]
    fn test_check_terminal_wezterm_ok() {
        let profile = terminal::TerminalProfile::for_wezterm();
        let result = check_terminal_profile(&profile, false);
        assert!(matches!(result.severity, Severity::Ok));
        assert!(result.message.contains("WezTerm"));
    }

    #[test]
    fn test_check_terminal_alacritty_ok() {
        let profile = terminal::TerminalProfile::for_alacritty();
        let result = check_terminal_profile(&profile, false);
        assert!(matches!(result.severity, Severity::Ok));
        assert!(result.message.contains("Alacritty"));
    }

    #[test]
    fn test_check_terminal_rio_ok() {
        let profile = terminal::TerminalProfile::for_rio();
        let result = check_terminal_profile(&profile, false);
        assert!(matches!(result.severity, Severity::Ok));
        assert!(result.message.contains("Rio"));
    }

    #[test]
    fn test_check_terminal_iterm2_ok() {
        let profile = terminal::TerminalProfile::for_iterm2();
        let result = check_terminal_profile(&profile, false);
        assert!(matches!(result.severity, Severity::Ok));
        assert!(result.message.contains("iTerm2"));
    }

    #[test]
    fn test_check_terminal_unknown_warns() {
        let profile = terminal::TerminalProfile::for_unknown("foot");
        let result = check_terminal_profile(&profile, false);
        assert!(matches!(result.severity, Severity::Warn));
        assert!(result.message.contains("Unsupported"));
    }

    #[test]
    fn test_check_terminal_unknown_with_multi_terminal_ok() {
        let profile = terminal::TerminalProfile::for_unknown("foot");
        let result = check_terminal_profile(&profile, true);
        assert!(matches!(result.severity, Severity::Ok));
        assert!(result.message.contains("multi_terminal"));
    }

    #[test]
    fn doctor_renders_sanitize_hostile_message() {
        let results = vec![CheckResult {
            severity: Severity::Fail,
            message: "\x1b[31mboom\x07nul\x00".to_string(),
        }];
        let mut buf = Vec::new();
        render_results(&results, &mut buf).unwrap();
        let emitted = String::from_utf8(buf).unwrap();

        let (_prefix, body) = emitted.split_once("[FAIL]\x1b[0m ").expect(
            "render output must contain the [FAIL] label with reset; \
             body starts after that: {emitted:?}",
        );
        let line_end = body.find('\n').unwrap_or(body.len());
        let rendered_message = &body[..line_end];

        assert!(
            !rendered_message.contains('\x1b'),
            "rendered message must not contain ESC bytes: {rendered_message:?}"
        );
        assert!(
            !rendered_message.contains('\x07'),
            "rendered message must not contain BEL bytes: {rendered_message:?}"
        );
        assert!(
            !rendered_message.contains('\x00'),
            "rendered message must not contain NUL bytes: {rendered_message:?}"
        );
    }

    #[test]
    fn extract_parses_single_quoted_canonical_path() {
        let block = format!(
            "{TEST_INIT_BEGIN}\nsource '/home/user/.config/termcmp/shell/init.zsh'\n{TEST_INIT_END}",
        );
        let got = extract_block_source_path(&block, TEST_INIT_BEGIN, TEST_INIT_END);
        assert_eq!(
            got,
            BlockSource::Parsed(PathBuf::from("/home/user/.config/termcmp/shell/init.zsh"))
        );
    }

    #[test]
    fn extract_unescapes_embedded_single_quote_via_idiom() {
        // shell_safe_path writes /home/o'brien/init.zsh as
        // '/home/o'\''brien/init.zsh'. The parser must invert that idiom.
        let block = format!(
            "{TEST_INIT_BEGIN}\nbuiltin source '/home/o'\\''brien/init.zsh'\n{TEST_INIT_END}",
        );
        let got = extract_block_source_path(&block, TEST_INIT_BEGIN, TEST_INIT_END);
        assert_eq!(
            got,
            BlockSource::Parsed(PathBuf::from("/home/o'brien/init.zsh"))
        );
    }

    #[test]
    fn extract_passes_through_double_quote_legacy() {
        // Legacy v0.6.1–v0.7.0 form: double-quoted, no escaping. Real
        // legacy installs never wrote backslash escapes — the inner bytes
        // must pass through untouched so a `$HOME` containing literal `\\`
        // (allowed on Unix) is not silently mangled into `\` and then
        // resolved to a non-existent file.
        let block = format!(
            "{TEST_INIT_BEGIN}\nsource \"/home/user/dir\\\\sub/init.zsh\"\n{TEST_INIT_END}",
        );
        let got = extract_block_source_path(&block, TEST_INIT_BEGIN, TEST_INIT_END);
        assert_eq!(
            got,
            BlockSource::Parsed(PathBuf::from("/home/user/dir\\\\sub/init.zsh"))
        );
    }

    #[test]
    fn extract_returns_unparseable_quoting_on_unrecognized_quotes() {
        // A source line with neither single- nor double-quoted path.
        let block = format!("{TEST_INIT_BEGIN}\nsource /home/user/init.zsh\n{TEST_INIT_END}",);
        let got = extract_block_source_path(&block, TEST_INIT_BEGIN, TEST_INIT_END);
        assert_eq!(got, BlockSource::UnparseableQuoting);
    }

    #[test]
    fn extract_skips_if_guard_line_finds_source() {
        // The init_block format wraps `builtin source` in an `if [[ -f ... ]]`
        // guard. The guard line must not be treated as a source line; only
        // the inner `builtin source` line gets extracted.
        let block = format!(
            "{TEST_INIT_BEGIN}\n\
             if [[ -f '/home/user/init.zsh' ]]; then\n  \
             builtin source '/home/user/init.zsh'\n\
             fi\n\
             {TEST_INIT_END}",
        );
        let got = extract_block_source_path(&block, TEST_INIT_BEGIN, TEST_INIT_END);
        assert_eq!(
            got,
            BlockSource::Parsed(PathBuf::from("/home/user/init.zsh"))
        );
    }

    #[test]
    fn extract_returns_no_source_line_on_pre_v0_9_exec_body() {
        // pre-v0.9 inlined `exec termcmp` inside the block — no
        // `source` line anywhere.
        let block = format!(
            "{TEST_INIT_BEGIN}\n\
             if [[ -z \"$TERMCMP_ACTIVE\" ]]; then\n  \
             export TERMCMP_ACTIVE=1\n  \
             exec termcmp\n\
             fi\n\
             {TEST_INIT_END}",
        );
        let got = extract_block_source_path(&block, TEST_INIT_BEGIN, TEST_INIT_END);
        assert_eq!(got, BlockSource::NoSourceLine);
    }

    #[test]
    fn extract_returns_unterminated_when_end_marker_missing() {
        let block =
            format!("{TEST_INIT_BEGIN}\nsource '/home/user/init.zsh'\n# (no end marker)\n",);
        let got = extract_block_source_path(&block, TEST_INIT_BEGIN, TEST_INIT_END);
        assert_eq!(got, BlockSource::Unterminated);
    }

    #[test]
    fn extract_returns_block_not_found_when_begin_marker_absent() {
        let block = "no managed block here at all";
        let got = extract_block_source_path(block, TEST_INIT_BEGIN, TEST_INIT_END);
        assert_eq!(got, BlockSource::BlockNotFound);
    }

    #[test]
    fn extract_returns_multiple_source_lines_when_block_has_two_sources() {
        // A hand edit or merge-conflict resolution that duplicated the
        // `source` line. Clean installs only ever emit one — surfacing
        // the divergence as a distinct variant lets the doctor Fail with
        // a remediation that names the actual symptom.
        let block = format!(
            "{TEST_INIT_BEGIN}\n\
             source '/home/user/init-a.zsh'\n\
             builtin source '/home/user/init-b.zsh'\n\
             {TEST_INIT_END}",
        );
        let got = extract_block_source_path(&block, TEST_INIT_BEGIN, TEST_INIT_END);
        assert_eq!(
            got,
            BlockSource::MultipleSourceLines {
                first: PathBuf::from("/home/user/init-a.zsh"),
                additional: vec![PathBuf::from("/home/user/init-b.zsh")],
            }
        );
    }
}
