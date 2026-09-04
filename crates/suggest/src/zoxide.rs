//! Directory suggestions backed by the zoxide database.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use buffer::CommandContext;

use crate::provider::Provider;
use crate::types::{Suggestion, SuggestionKind, SuggestionSource};

const QUERY_TIMEOUT: Duration = Duration::from_millis(500);

/// Well-known install locations beyond `$PATH`. The proxy process often runs
/// with a login-context `PATH` while the inner interactive shell (fish/zsh
/// via `fish_add_path`, `.zprofile`, etc.) adds Homebrew/cargo bins only to
/// its own environment — so `Command::new("zoxide")` fails in the proxy even
/// though `zoxide` works in the terminal. These fallbacks close that gap.
fn fallback_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ];
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".local/bin"));
    }
    dirs
}
/// Resolve the `zoxide` binary: the shell-reported `$PATH` first (the inner
/// interactive shell usually has the full user `PATH`), then the proxy
/// process `$PATH`, then [`fallback_bin_dirs`]. Returns a plain `"zoxide"`
/// lookup when nothing resolves so the spawn error path stays unchanged.
fn resolve_binary(shell_path: Option<&str>) -> PathBuf {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(extra) = shell_path {
        dirs.extend(std::env::split_paths(extra));
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.extend(fallback_bin_dirs());
    dirs.into_iter()
        .map(|dir| dir.join("zoxide"))
        .find(|candidate| is_executable(candidate))
        .unwrap_or_else(|| PathBuf::from("zoxide"))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.is_file()
            && path
                .metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

pub struct ZoxideProvider {
    max_results: usize,
}

impl ZoxideProvider {
    pub fn new(max_results: usize) -> Self {
        Self { max_results }
    }

    /// Queries zoxide for a `z`/`zi` command, including aliases resolved by the
    /// suggestion engine before this method is called. `shell_path` is the
    /// inner shell's `$PATH` (when the engine has it) so the binary resolves
    /// even when the proxy process itself was launched with a minimal `PATH`.
    pub fn provide_for_command(
        &self,
        ctx: &CommandContext,
        cwd: &Path,
        command: &str,
        shell_path: Option<&str>,
    ) -> Result<Vec<Suggestion>> {
        if !is_zoxide_command(command) || ctx.is_flag {
            return Ok(Vec::new());
        }

        let cwd = cwd.to_string_lossy();
        let binary = resolve_binary(shell_path);
        let mut child = Command::new(&binary)
            .args(["query", "--list", "--exclude", cwd.as_ref()])
            .args(ctx.current_word.split_whitespace())
            .current_dir(cwd.as_ref())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                tracing::warn!("zoxide spawn failed (tried {}): {e}", binary.display());
                e
            })?;
        let deadline = Instant::now() + QUERY_TIMEOUT;
        loop {
            if child.try_wait()?.is_some() {
                let output = child.wait_with_output()?;
                return Ok(parse_paths(&output.stdout, self.max_results));
            }
            if Instant::now() >= deadline {
                tracing::warn!("zoxide query timed out after {QUERY_TIMEOUT:?}");
                let _ = child.kill();
                let _ = child.wait();
                return Ok(Vec::new());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Provider for ZoxideProvider {
    fn provide(&self, ctx: &CommandContext, cwd: &Path) -> Result<Vec<Suggestion>> {
        let Some(command) = ctx.command.as_deref() else {
            return Ok(Vec::new());
        };
        self.provide_for_command(ctx, cwd, command, None)
    }
}

/// Whether `command` names the zoxide jump command — directly (`z`/`zi`) or
/// via its resolved shell function/alias (`__zoxide_z`/`__zoxide_zi`), which
/// is what `expand_alias_head` produces for an aliased zoxide.
pub(crate) fn is_zoxide_command(command: &str) -> bool {
    matches!(command, "z" | "zi" | "__zoxide_z" | "__zoxide_zi")
}

fn parse_paths(output: &[u8], max_results: usize) -> Vec<Suggestion> {
    String::from_utf8_lossy(output)
        .lines()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .filter(|path| Path::new(path).is_dir())
        .take(max_results)
        .map(|path| Suggestion {
            text: path.to_string(),
            description: Some("zoxide directory".to_string()),
            kind: SuggestionKind::Zoxide,
            source: SuggestionSource::Zoxide,
            ..Default::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffer::QuoteState;

    #[test]
    fn parse_paths_filters_missing_directories_and_caps_results() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let output = format!("{}\nmissing\n{}\n", first.display(), second.display());
        let results = parse_paths(output.as_bytes(), 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].kind, SuggestionKind::Zoxide);
        assert_eq!(results[0].source, SuggestionSource::Zoxide);
    }

    #[test]
    fn provider_ignores_non_zoxide_commands() {
        let provider = ZoxideProvider::new(10);
        let ctx = CommandContext {
            command: Some("cd".to_string()),
            args: Vec::new(),
            current_word: String::new(),
            word_index: 1,
            is_flag: false,
            is_long_flag: false,
            preceding_flag: None,
            in_pipe: false,
            in_redirect: false,
            quote_state: QuoteState::None,
            is_first_segment: true,
        };
        assert!(provider
            .provide(&ctx, Path::new("/tmp"))
            .unwrap()
            .is_empty());
    }
    #[test]
    fn is_zoxide_command_accepts_aliases() {
        assert!(is_zoxide_command("z"));
        assert!(is_zoxide_command("zi"));
        assert!(is_zoxide_command("__zoxide_z"));
        assert!(is_zoxide_command("__zoxide_zi"));
        assert!(!is_zoxide_command("cd"));
        assert!(!is_zoxide_command("za"));
        assert!(!is_zoxide_command("zoxide"));
    }
    #[test]
    fn resolve_binary_prefers_shell_path() {
        // A fake `zoxide` on the shell-reported PATH wins even when the
        // process PATH cannot see it — the proxy/inner-shell PATH split.
        let temp = tempfile::tempdir().unwrap();
        let shim = temp.path().join("zoxide");
        std::fs::write(&shim, "#!/bin/sh\necho /tmp\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&shim).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&shim, perms).unwrap();
        }
        let resolved = resolve_binary(Some(temp.path().to_str().unwrap()));
        assert_eq!(resolved, shim);
    }

    #[test]
    fn provide_for_command_uses_shell_path_binary() {
        // End-to-end through a shim binary found only via `shell_path`:
        // proves completions survive a proxy PATH without `zoxide`.
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target_dir");
        std::fs::create_dir(&target).unwrap();
        let shim = temp.path().join("zoxide");
        std::fs::write(&shim, format!("#!/bin/sh\necho {}\n", target.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&shim).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&shim, perms).unwrap();
        }
        let provider = ZoxideProvider::new(10);
        let ctx = CommandContext {
            command: Some("z".to_string()),
            args: Vec::new(),
            current_word: "Code".to_string(),
            word_index: 1,
            is_flag: false,
            is_long_flag: false,
            preceding_flag: None,
            in_pipe: false,
            in_redirect: false,
            quote_state: QuoteState::None,
            is_first_segment: true,
        };
        let results = provider
            .provide_for_command(
                &ctx,
                Path::new("/tmp"),
                "z",
                Some(temp.path().to_str().unwrap()),
            )
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].text, target.to_string_lossy());
        assert_eq!(results[0].source, SuggestionSource::Zoxide);
    }
}
