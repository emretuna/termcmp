//! Shell alias loading via subprocess (zsh, bash, fish).
//!
//! Aliases let the engine resolve `g` → `git`, `gco` → `git checkout` so that
//! a user-customized command triggers the right completion behavior. The proxy detects
//! the active shell and passes a [`ShellFamily`] through the engine so only
//! that shell's aliases are loaded.
//!
//! Loading strategy: subprocess is the single source of truth. Each shell
//! has a canonical command that dumps its full alias state:
//!
//! - **zsh**: `zsh -c 'alias -L'` → `alias name=value` lines
//! - **bash**: `bash -c 'alias'` → `alias name='value'` lines
//! - **fish**: `fish -c 'abbr --show'` + a functions query for
//!   `--wraps`-annotated wrappers (what `fish alias` generates)
//!
//! Static file parsing was removed: it could never be complete (sourced
//! files, conditional definitions, plugin managers, two different fish
//! syntaxes) and the subprocess output is always authoritative.
//!
//! The subprocess can take 100–500ms (oh-my-zsh cold start, fish plugin
//! loading). To stay under the <100ms startup budget the [`AliasStore`]
//! returned at startup is empty and a background thread runs the probe.
//! Aliases appear once the probe completes (~100-500ms).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Shell family for alias loading. The proxy detects the active shell and
/// passes it through so only that shell's aliases are loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellFamily {
    Zsh,
    Fish,
    Bash,
    /// Unknown shell — no alias loading.
    Other,
}

/// A single alias: its expansion tokens plus an optional description.
///
/// Fish exposes descriptions via `--description` on alias-generated
/// wrapper functions; zsh and bash have no description concept, so their
/// entries carry `None`.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AliasEntry {
    /// The command tokens the alias expands to (e.g. `["git", "checkout"]`).
    pub tokens: Vec<String>,
    /// Human-readable description, present only for fish aliases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl AliasEntry {
    /// Convenience constructor for a description-less entry (zsh/bash).
    pub fn new(tokens: Vec<String>) -> Self {
        Self {
            tokens,
            description: None,
        }
    }
}

/// Alias map shared by the synchronous suggestion engine and SSH expansion.
#[derive(Clone, Default)]
pub struct AliasStore {
    inner: Arc<RwLock<HashMap<String, AliasEntry>>>,
}

impl AliasStore {
    /// Load aliases in the background. Returns immediately with an empty store;
    /// a background thread spawns the shell probe and installs aliases when ready.
    pub fn load(shell: ShellFamily) -> Self {
        let store = Self::default();
        let store_clone = store.clone();
        std::thread::spawn(move || {
            let aliases = load_shell_aliases(shell);
            store_clone.install(aliases);
        });
        store
    }

    /// Build an empty store with no shell probe. Used by tests and injected
    /// engine constructors.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns a stable snapshot of all aliases and their metadata.
    pub fn entries(&self) -> Vec<(String, AliasEntry)> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard
            .iter()
            .map(|(name, entry)| (name.clone(), entry.clone()))
            .collect()
    }

    /// Returns the full token vector for `name`, or None if absent.
    pub fn get(&self, name: &str) -> Option<Vec<String>> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard.get(name).map(|e| e.tokens.clone())
    }

    /// Returns the full [`AliasEntry`] (tokens + description) for `name`.
    pub fn get_entry(&self, name: &str) -> Option<AliasEntry> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard.get(name).cloned()
    }

    /// Number of aliases currently in the store.
    pub fn len(&self) -> usize {
        self.inner.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Test/fixture helper — synchronously install a pre-built map.
    #[cfg(test)]
    pub(crate) fn populate(&self, map: HashMap<String, AliasEntry>) {
        self.install(map);
    }

    #[doc(hidden)]
    pub fn install(&self, map: HashMap<String, AliasEntry>) {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        *guard = map;
    }
}

/// Validate an alias/abbreviation name: ASCII alphanumerics, `_`, `.`, `-`,
/// non-empty, no leading `-`.
fn is_valid_alias_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// Tokenise an alias value, falling back to whitespace split when shlex
/// can't parse it. `None` when no usable tokens remain.
fn shlex_tokens(value: &str) -> Option<Vec<String>> {
    match shlex::split(value) {
        Some(toks) if !toks.is_empty() => Some(toks),
        Some(_) => None,
        None => {
            let fallback: Vec<String> = value.split_whitespace().map(String::from).collect();
            if fallback.is_empty() {
                None
            } else {
                Some(fallback)
            }
        }
    }
}

/// Parse zsh/bash `alias` output into name → [`AliasEntry`] pairs.
/// Handles both `alias name=value` (zsh `alias -L`) and `alias name='value'`
/// (bash `alias`) formats. Full tokens preserved via shlex. No descriptions.
pub fn parse_aliases(output: &str) -> HashMap<String, AliasEntry> {
    let mut map = HashMap::new();

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Strip "alias " prefix (bash format)
        let line = line.strip_prefix("alias ").unwrap_or(line);

        // Find the = separator
        let eq_idx = match line.find('=') {
            Some(i) => i,
            None => continue,
        };

        let alias_name = line[..eq_idx].trim();
        if alias_name.is_empty() {
            continue;
        }

        let mut value = line[eq_idx + 1..].trim();

        // Strip surrounding quotes
        if (value.starts_with('\'') && value.ends_with('\''))
            || (value.starts_with('"') && value.ends_with('"'))
        {
            value = &value[1..value.len() - 1];
        }

        let tokens = match shlex::split(value) {
            Some(toks) if !toks.is_empty() => toks,
            Some(_) => continue,
            None => {
                tracing::debug!("shlex failed to parse alias value for {alias_name:?}: {value:?}");
                let fallback: Vec<String> = value.split_whitespace().map(String::from).collect();
                if fallback.is_empty() {
                    continue;
                }
                fallback
            }
        };

        map.insert(alias_name.to_string(), AliasEntry::new(tokens));
    }

    map
}

/// Parse `fish -c "abbr --show"` output into name → [`AliasEntry`] pairs.
/// Output lines look like `abbr -a -- gco 'git checkout'` (fish ≥3.0):
/// after the `-- ` separator the first token is the name, the remainder is
/// the (possibly single-quoted) value. Abbreviations have no descriptions.
pub(crate) fn parse_fish_abbr_show(output: &str) -> HashMap<String, AliasEntry> {
    let mut out = HashMap::new();
    for raw in output.lines() {
        let line = raw.trim();
        let rest = if let Some(r) = line.strip_prefix("abbr -a -- ") {
            r
        } else if let Some(r) = line.strip_prefix("abbr --add -- ") {
            r
        } else {
            continue;
        };
        let mut parts = rest.splitn(2, char::is_whitespace);
        let Some(name) = parts.next() else {
            continue;
        };
        if !is_valid_alias_name(name) {
            continue;
        }
        let Some(value) = parts.next() else {
            continue;
        };
        let value = value.trim();
        let value = if (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
            || (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        if let Some(toks) = shlex_tokens(value) {
            out.insert(name.to_string(), AliasEntry::new(toks));
        }
    }
    out
}

/// Parse the output of the fish functions query into name → [`AliasEntry`]
/// pairs. Expected format is tab-separated lines: `name\twraps\tdescription`.
/// The description column (third) is captured when present.
pub(crate) fn parse_fish_functions(output: &str) -> HashMap<String, AliasEntry> {
    let mut out = HashMap::new();
    for raw in output.lines() {
        let mut cols = raw.splitn(3, '\t');
        let Some(name) = cols.next() else {
            continue;
        };
        let Some(wraps) = cols.next() else {
            continue;
        };
        let description = cols.next().map(|d| d.trim()).filter(|d| !d.is_empty());
        // Reject private/internal fish functions (__zoxide_cd, etc.)
        if name.starts_with('_') || !is_valid_alias_name(name) {
            continue;
        }
        if let Some(toks) = shlex_tokens(wraps) {
            out.insert(
                name.to_string(),
                AliasEntry {
                    tokens: toks,
                    description: description.map(String::from),
                },
            );
        }
    }
    out
}

/// Fish config sourcing prefix. Fish 4.x does not source config.fish or
/// conf.d/ in non-interactive (`-c`) mode, so we source them explicitly.
/// Errors are suppressed since missing files are normal.
const FISH_SOURCE_CONFIG: &str = "source $__fish_config_dir/config.fish 2>/dev/null; for f in $__fish_config_dir/conf.d/*.fish; source $f 2>/dev/null; end; ";

/// Fish one-liner that dumps all `--wraps`-annotated functions (what
/// `fish alias` generates) as tab-separated `name\twraps\tdescription`.
/// Uses `functions $fn` (not `functions --details`) because fish 4.x
/// changed `--details` to return the file path instead of the definition.
/// Regex capture groups extract quoted/unquoted values cleanly.
const FISH_FUNCTIONS_QUERY: &str = concat!(
    "source $__fish_config_dir/config.fish 2>/dev/null; ",
    "for f in $__fish_config_dir/conf.d/*.fish; source $f 2>/dev/null; end; ",
    "for fn in (functions -n); ",
    "set header (functions $fn | string match 'function *'); ",
    "if string match -q '*--wraps=*' -- $header; ",
    "set m (string match -r -- \"--wraps='([^']*)'|--wraps=(\\S+)\" $header); ",
    "if test -n \"$m[2]\"; set wraps $m[2]; else; set wraps $m[3]; end; ",
    "set d (string match -r -- \"--description[= ]'([^']*)'|--description[= ](\\S+)\" $header); ",
    "if test -n \"$d[2]\"; set desc $d[2]; else; set desc $d[3]; end; ",
    "printf '%s\\t%s\\t%s\\n' $fn $wraps $desc; ",
    "end; end",
);

/// Env vars that trigger terminal-emulator detection in shell init scripts.
/// Removed from the subprocess environment so init scripts (e.g. termcmp's
/// own init.fish) don't `exec` into a proxy during the alias probe.
const SHELL_PROBE_ENV_REMOVE: &[&str] = &[
    "TERM_PROGRAM",
    "GHOSTTY_RESOURCES_DIR",
    "KITTY_WINDOW_ID",
    "WEZTERM_UNIX_SOCKET",
    "ALACRITTY_SOCKET",
    "ZED_TERM",
    "VSCODE_IPC_HOOK_CLI",
    "ITERM_SESSION_ID",
];

/// Spawn a shell command with a polling deadline. Returns stdout on
/// success, empty string on any failure. Terminal-detection env vars are
/// stripped so shell init scripts don't hijack the probe shell.
fn run_shell_command(bin: &str, args: &[&str], timeout: Duration) -> String {
    tracing::debug!("spawning {bin} {}", args.join(" "));
    let mut command = std::process::Command::new(bin);
    command
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for var in SHELL_PROBE_ENV_REMOVE {
        command.env_remove(var);
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("failed to spawn {bin}: {e}");
            return String::new();
        }
    };

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    tracing::debug!("{bin} timed out, killing");
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                tracing::debug!("{bin} wait error: {e}");
                break None;
            }
        }
    };

    match status {
        Some(s) if s.success() => {
            if let Some(mut stdout) = child.stdout.take() {
                use std::io::Read;
                let mut text = String::new();
                if stdout.read_to_string(&mut text).is_ok() {
                    return text;
                }
            }
            String::new()
        }
        Some(s) => {
            tracing::debug!("{bin} exited with {s}");
            String::new()
        }
        None => String::new(),
    }
}

/// Load aliases by spawning the active shell's canonical alias-dump command.
/// This is the sole loading mechanism — no file parsing.
fn load_aliases_via_subprocess(
    shell: ShellFamily,
    timeout: Duration,
) -> HashMap<String, AliasEntry> {
    match shell {
        ShellFamily::Zsh => {
            // -i sources .zshrc where aliases are typically defined.
            let output = run_shell_command("zsh", &["-i", "-c", "alias -L"], timeout);
            let aliases = parse_aliases(&output);
            if !aliases.is_empty() {
                tracing::debug!("loaded {} aliases from zsh -ic 'alias -L'", aliases.len());
            }
            aliases
        }
        ShellFamily::Bash => {
            // -i sources .bashrc where aliases are typically defined.
            let output = run_shell_command("bash", &["-i", "-c", "alias"], timeout);
            let aliases = parse_aliases(&output);
            if !aliases.is_empty() {
                tracing::debug!("loaded {} aliases from bash -ic alias", aliases.len());
            }
            aliases
        }
        ShellFamily::Fish => {
            // Fish 4.x doesn't source config in -c mode, so both commands
            // explicitly source config.fish + conf.d/ via FISH_SOURCE_CONFIG.
            // Two sources: abbreviations + wraps-functions (alias wrappers).
            // Functions override abbreviations on name collision since they
            // represent the actual runtime state.
            let abbr_cmd = format!("{FISH_SOURCE_CONFIG}abbr --show");
            let abbr_output = run_shell_command("fish", &["-c", &abbr_cmd], timeout);
            let mut aliases = parse_fish_abbr_show(&abbr_output);

            let fn_output = run_shell_command("fish", &["-c", FISH_FUNCTIONS_QUERY], timeout);
            let fn_aliases = parse_fish_functions(&fn_output);
            aliases.extend(fn_aliases);

            if !aliases.is_empty() {
                tracing::debug!("loaded {} aliases from fish subprocess", aliases.len());
            }
            aliases
        }
        ShellFamily::Other => HashMap::new(),
    }
}

/// Load aliases for the active shell by spawning a subprocess.
///
/// Spawns the shell's alias dump command (100–500ms on oh-my-zsh/fish-plugin setups).
/// Returns an empty map if the subprocess fails or times out.
pub fn load_shell_aliases(shell: ShellFamily) -> HashMap<String, AliasEntry> {
    load_aliases_via_subprocess(shell, Duration::from_secs(5))
}

#[cfg(test)]
fn token_vec(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|s| (*s).to_string()).collect()
}

#[cfg(test)]
fn entry(tokens: &[&str]) -> AliasEntry {
    AliasEntry::new(token_vec(tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_aliases (zsh/bash subprocess output) ---

    #[test]
    fn test_parse_zsh_aliases() {
        let output = "\
g=git
k=kubectl
ll='ls -la'
";
        let aliases = parse_aliases(output);
        assert_eq!(aliases.get("g"), Some(&entry(&["git"])));
        assert_eq!(aliases.get("k"), Some(&entry(&["kubectl"])));
        assert_eq!(aliases.get("ll"), Some(&entry(&["ls", "-la"])));
    }

    #[test]
    fn test_parse_bash_aliases() {
        let output = "\
alias g='git'
alias k='kubectl'
alias ll='ls -la'
";
        let aliases = parse_aliases(output);
        assert_eq!(aliases.get("g"), Some(&entry(&["git"])));
        assert_eq!(aliases.get("k"), Some(&entry(&["kubectl"])));
        assert_eq!(aliases.get("ll"), Some(&entry(&["ls", "-la"])));
    }

    #[test]
    fn test_parse_double_quoted() {
        let output = "alias g=\"git\"\n";
        let aliases = parse_aliases(output);
        assert_eq!(aliases.get("g"), Some(&entry(&["git"])));
    }

    #[test]
    fn test_parse_empty_value_skipped() {
        let output = "empty=\n";
        let aliases = parse_aliases(output);
        assert!(!aliases.contains_key("empty"));
    }

    #[test]
    fn test_parse_empty_quoted_value_skipped() {
        let output = "alias x=''\nalias y=\"\"\nalias z=' '\n";
        let aliases = parse_aliases(output);
        assert!(!aliases.contains_key("x"));
        assert!(!aliases.contains_key("y"));
        assert!(!aliases.contains_key("z"));
    }

    #[test]
    fn test_parse_quoted_value_with_padding_trimmed() {
        let output = "alias k=' kubectl '\n";
        let aliases = parse_aliases(output);
        assert_eq!(aliases.get("k"), Some(&entry(&["kubectl"])));
    }

    #[test]
    fn test_parse_keeps_dollar_var_as_literal_token() {
        let output = "k='kubectl --context $CTX'\n";
        let aliases = parse_aliases(output);
        assert_eq!(
            aliases.get("k"),
            Some(&entry(&["kubectl", "--context", "$CTX"]))
        );
    }

    #[test]
    fn test_parse_empty_output() {
        let aliases = parse_aliases("");
        assert!(aliases.is_empty());
    }

    #[test]
    fn test_parse_complex_value_keeps_full_tokens() {
        let output = "glog='git log --oneline --graph'\n";
        let aliases = parse_aliases(output);
        assert_eq!(
            aliases.get("glog"),
            Some(&entry(&["git", "log", "--oneline", "--graph"]))
        );
    }

    #[test]
    fn test_parse_double_quoted_with_inner_spaces() {
        let output = "commit='git commit -m \"wip commit\"'\n";
        let aliases = parse_aliases(output);
        assert_eq!(
            aliases.get("commit"),
            Some(&entry(&["git", "commit", "-m", "wip commit"]))
        );
    }

    #[test]
    fn test_parse_escaped_space() {
        let output = "gx='git foo\\ bar'\n";
        let aliases = parse_aliases(output);
        assert_eq!(aliases.get("gx"), Some(&entry(&["git", "foo bar"])));
    }

    #[test]
    fn test_parse_falls_back_on_unbalanced_quote() {
        let output = "broken=git \"open\nok=ls\n";
        let aliases = parse_aliases(output);
        assert_eq!(
            aliases.get("broken"),
            Some(&entry(&["git", "\"open"])),
            "fallback must preserve every token, not just the first"
        );
        assert_eq!(
            aliases.get("ok"),
            Some(&entry(&["ls"])),
            "a single corrupt alias must not drop later entries"
        );
    }

    #[test]
    fn test_parse_single_word_unchanged() {
        let output = "ll=ls\n";
        let aliases = parse_aliases(output);
        assert_eq!(aliases.get("ll"), Some(&entry(&["ls"])));
    }

    #[test]
    fn test_parse_no_equals_skipped() {
        let output = "not an alias line\n";
        let aliases = parse_aliases(output);
        assert!(aliases.is_empty());
    }

    // --- parse_fish_abbr_show ---

    #[test]
    fn parse_fish_abbr_show_parses_output() {
        let output = concat!(
            "abbr -a -- gco 'git checkout'\n",
            "abbr -a -- g git\n",
            "abbr --add -- gst git status\n",
        );
        let map = parse_fish_abbr_show(output);
        assert_eq!(map.get("gco"), Some(&entry(&["git", "checkout"])));
        assert_eq!(map.get("g"), Some(&entry(&["git"])));
        assert_eq!(map.get("gst"), Some(&entry(&["git", "status"])));
    }

    // --- parse_fish_functions ---

    #[test]
    fn parse_fish_functions_parses_tab_separated_output() {
        let output = concat!(
            "l\teza $EZA_STANDARD_OPTIONS $EZA_L_OPTIONS\talias l eza $EZA_STANDARD_OPTIONS $EZA_L_OPTIONS\n",
            "v\tnvim\talias v nvim\n",
            "gs\tgit status\t\n",
        );
        let map = parse_fish_functions(output);
        let l = map.get("l").unwrap();
        assert_eq!(
            l.tokens,
            token_vec(&["eza", "$EZA_STANDARD_OPTIONS", "$EZA_L_OPTIONS"])
        );
        assert_eq!(
            l.description.as_deref(),
            Some("alias l eza $EZA_STANDARD_OPTIONS $EZA_L_OPTIONS")
        );
        let v = map.get("v").unwrap();
        assert_eq!(v.tokens, token_vec(&["nvim"]));
        assert_eq!(v.description.as_deref(), Some("alias v nvim"));
        let gs = map.get("gs").unwrap();
        assert_eq!(gs.tokens, token_vec(&["git", "status"]));
        assert_eq!(gs.description, None, "empty description column → None");
    }

    #[test]
    fn parse_fish_functions_rejects_invalid_names() {
        let output = concat!("__private\tsome cmd\t\n", "-flag\tcmd\t\n", "ok\tls\t\n",);
        let map = parse_fish_functions(output);
        assert!(!map.contains_key("__private"));
        assert!(!map.contains_key("-flag"));
        assert_eq!(map.get("ok"), Some(&entry(&["ls"])));
    }

    // --- AliasStore ---

    #[test]
    fn alias_store_starts_empty_then_fills() {
        let store = AliasStore::empty();
        assert!(store.is_empty(), "fresh store must be empty");
        assert_eq!(store.get("gco"), None);

        let mut map = HashMap::new();
        map.insert("gco".to_string(), entry(&["git", "checkout"]));
        map.insert("k".to_string(), entry(&["kubectl"]));
        store.populate(map);

        assert_eq!(store.len(), 2);
        assert_eq!(store.get("gco"), Some(token_vec(&["git", "checkout"])));
        assert_eq!(store.get("k"), Some(token_vec(&["kubectl"])));
        assert_eq!(store.get("not-an-alias"), None);
    }

    #[test]
    fn alias_store_clones_share_storage() {
        let store = AliasStore::empty();
        let store2 = store.clone();
        store.populate(HashMap::from([("g".to_string(), entry(&["git"]))]));
        assert_eq!(store2.get("g"), Some(token_vec(&["git"])));
    }

    #[test]
    fn alias_entries_returns_metadata_snapshot() {
        let store = AliasStore::empty();
        store.populate(HashMap::from([(
            "gco".to_string(),
            AliasEntry {
                tokens: token_vec(&["git", "checkout"]),
                description: Some("checkout".to_string()),
            },
        )]));
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].0, "gco");
        assert_eq!(
            store.entries()[0].1.description.as_deref(),
            Some("checkout")
        );
    }
}
