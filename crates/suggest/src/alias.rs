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
//! A per-shell on-disk cache (`aliases-cache-{shell}.json`) invalidated by
//! dotfile mtimes makes subsequent starts nearly free.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

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

impl ShellFamily {
    /// Suffix for the per-shell cache file: `aliases-cache-zsh.json`, etc.
    fn cache_suffix(self) -> &'static str {
        match self {
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::Bash => "bash",
            Self::Other => "other",
        }
    }

    /// Dotfiles whose mtimes invalidate this shell's alias cache. When any
    /// watched file changes, the cached subprocess output is stale and must
    /// be regenerated.
    fn source_files(self) -> &'static [&'static str] {
        match self {
            Self::Zsh => &[
                ".zshenv",
                ".zprofile",
                ".zshrc",
                ".zshrc.local",
                ".zsh_aliases",
                ".aliases",
            ],
            Self::Bash => &[".bashrc", ".bash_profile", ".bash_aliases", ".aliases"],
            Self::Fish => &[".config/fish/fish_variables", ".config/fish/config.fish"],
            Self::Other => &[],
        }
    }

    /// Directories whose recursive tree fingerprint invalidates the cache.
    fn source_dirs(self) -> &'static [&'static str] {
        match self {
            Self::Zsh => &[".oh-my-zsh/custom"],
            Self::Fish => &[".config/fish/conf.d", ".config/fish/functions"],
            _ => &[],
        }
    }
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

/// On-disk schema version; bump on incompatible CachedAliases changes.
const CURRENT_ALIAS_CACHE_VERSION: u32 = 3;

/// Fingerprint of a watched source file. We pair mtime-seconds with
/// subsecond precision AND the file length so rapid in-place edits inside
/// the same wall-clock second still invalidate the cache.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy)]
struct SourceFingerprint {
    secs: u64,
    #[serde(default)]
    nanos: u32,
    #[serde(default)]
    len: u64,
}

#[derive(Serialize, Deserialize)]
struct CachedAliases {
    /// Bump on incompatible CachedAliases shape changes; mismatch forces regeneration.
    #[serde(default)]
    format_version: u32,
    /// Maps each watched source file (by basename) to its fingerprint at
    /// the time of capture. On load, we compare the current fingerprint
    /// against the stored value: any difference invalidates.
    source_mtimes: HashMap<String, SourceFingerprint>,
    aliases: HashMap<String, AliasEntry>,
}

/// Resolve the per-shell cache file path: `aliases-cache-{shell}.json`.
/// Prefers `$XDG_STATE_HOME/termcmp`, falls back to
/// `~/.local/state/termcmp`.
fn alias_cache_path(shell: ShellFamily) -> Option<PathBuf> {
    let filename = format!("aliases-cache-{}.json", shell.cache_suffix());
    if let Ok(state) = std::env::var("XDG_STATE_HOME") {
        let p = PathBuf::from(state);
        if !p.as_os_str().is_empty() {
            return Some(p.join("termcmp").join(filename));
        }
    }
    let home = dirs::home_dir()?;
    Some(home.join(".local/state/termcmp").join(filename))
}

fn file_fingerprint(path: &Path) -> Option<SourceFingerprint> {
    let meta = std::fs::metadata(path).ok()?;
    let mt = meta.modified().ok()?;
    let d = mt.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    Some(SourceFingerprint {
        secs: d.as_secs(),
        nanos: d.subsec_nanos(),
        len: meta.len(),
    })
}

/// Fingerprint a directory tree using the same `(secs, nanos, len)` shape
/// we use for files. Walks children under the same budget as the original
/// walker and keeps the largest (`secs`, then `nanos`, then `len`) tuple,
/// so any edit inside the tree advances the fingerprint.
fn dir_tree_fingerprint(root: &Path) -> Option<SourceFingerprint> {
    let mut best = file_fingerprint(root)?;
    let mut stack: Vec<(PathBuf, u32)> = vec![(root.to_path_buf(), 0)];
    let mut files_seen: u32 = 0;

    while let Some((dir, depth)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            if files_seen >= DIR_WALK_MAX_FILES {
                return Some(best);
            }
            files_seen += 1;

            let path = entry.path();
            if let Some(fp) = file_fingerprint(&path) {
                if (fp.secs, fp.nanos, fp.len) > (best.secs, best.nanos, best.len) {
                    best = fp;
                }
            }

            let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            if is_dir && depth + 1 < DIR_WALK_MAX_DEPTH {
                stack.push((path, depth + 1));
            }
        }
    }

    Some(best)
}

/// Bounds for [`dir_tree_fingerprint`]. Covers a ~normal oh-my-zsh
/// installation (custom drop-ins + a few plugin subdirs) without letting a
/// pathological layout turn startup into a deep FS walk.
const DIR_WALK_MAX_DEPTH: u32 = 3;
const DIR_WALK_MAX_FILES: u32 = 500;

fn collect_source_mtimes(home: &Path, shell: ShellFamily) -> HashMap<String, SourceFingerprint> {
    let mut out = HashMap::new();
    for name in shell.source_files() {
        let p = home.join(name);
        if let Some(fp) = file_fingerprint(&p) {
            out.insert((*name).to_string(), fp);
        }
    }
    for name in shell.source_dirs() {
        let p = home.join(name);
        if let Some(fp) = dir_tree_fingerprint(&p) {
            out.insert((*name).to_string(), fp);
        }
    }
    out
}

/// Attempt to load the alias map from the on-disk cache. Returns `None`
/// when the cache is missing, unreadable, malformed, stale w.r.t. any
/// watched source file, or written by a different schema version.
fn load_alias_cache(
    home: &Path,
    cache_path: &Path,
    shell: ShellFamily,
) -> Option<HashMap<String, AliasEntry>> {
    let contents = std::fs::read_to_string(cache_path).ok()?;
    let cached: CachedAliases = serde_json::from_str(&contents).ok()?;
    if cached.format_version != CURRENT_ALIAS_CACHE_VERSION {
        return None;
    }
    let current = collect_source_mtimes(home, shell);
    if current != cached.source_mtimes {
        return None;
    }
    Some(cached.aliases)
}

/// Write the alias map plus the current source mtimes to the cache file.
/// Uses atomic write (tmp + rename). Best-effort: any failure is logged
/// at debug and ignored.
fn save_alias_cache(
    home: &Path,
    cache_path: &Path,
    aliases: &HashMap<String, AliasEntry>,
    shell: ShellFamily,
) {
    if aliases.is_empty() {
        return;
    }
    let parent = match cache_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if let Err(e) = std::fs::create_dir_all(&parent) {
        tracing::warn!("alias cache dir creation failed: {e}");
        return;
    }
    let payload = CachedAliases {
        format_version: CURRENT_ALIAS_CACHE_VERSION,
        source_mtimes: collect_source_mtimes(home, shell),
        aliases: aliases.clone(),
    };
    let json = match serde_json::to_string(&payload) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!("alias cache serialize error: {e}");
            return;
        }
    };
    let mut tmp = match tempfile::NamedTempFile::new_in(&parent) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("alias cache tmp create failed: {e}");
            return;
        }
    };
    if let Err(e) = std::io::Write::write_all(tmp.as_file_mut(), json.as_bytes()) {
        tracing::warn!("alias cache write failed at {}: {e}", cache_path.display());
        return;
    }
    if let Err(e) = tmp.persist(cache_path) {
        tracing::warn!(
            "alias cache persist failed at {}: {e}",
            cache_path.display()
        );
    }
}

/// Lazy alias map populated by a background loader.
///
/// Reads (`get`) take a non-blocking [`RwLock`] read guard so concurrent
/// suggestion lookups never serialize against each other. The single
/// background loader thread takes the write lock briefly, just long enough
/// to swap in the populated map.
#[derive(Clone, Default)]
pub struct AliasStore {
    inner: Arc<RwLock<HashMap<String, AliasEntry>>>,
}

impl AliasStore {
    /// Construct a store and immediately spawn a background thread to run
    /// [`load_shell_aliases`]. The store is observable as empty until the
    /// thread completes — this is a deliberate trade-off so startup never
    /// blocks on a slow shell probe.
    pub fn load_async(shell: ShellFamily) -> Self {
        let store = Self::default();
        let inner = Arc::clone(&store.inner);
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let map = load_shell_aliases(shell);
            let count = map.len();
            {
                let mut guard = inner.write().unwrap_or_else(|e| e.into_inner());
                *guard = map;
            }
            tracing::info!(
                "loaded {count} shell aliases in {}ms (background)",
                started.elapsed().as_millis()
            );
        });
        store
    }

    /// Build an empty store with no background load. Used by tests and the
    /// `with_providers` engine constructor.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns the full token vector for `name`, or None if absent or loader still pending.
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
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        *guard = map;
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

/// Load aliases for the active shell.
///
/// Fast path: on-disk cache keyed by rc-file mtimes. Slow path: subprocess
/// spawn (100–500ms on oh-my-zsh/fish-plugin setups), cached for next time.
pub fn load_shell_aliases(shell: ShellFamily) -> HashMap<String, AliasEntry> {
    let home = dirs::home_dir();
    let cache_path = alias_cache_path(shell);

    if let (Some(h), Some(cp)) = (home.as_ref(), cache_path.as_ref()) {
        if let Some(cached) = load_alias_cache(h, cp, shell) {
            tracing::debug!("loaded {} aliases from disk cache", cached.len());
            return cached;
        }
    }

    let aliases = load_aliases_via_subprocess(shell, Duration::from_secs(5));

    if aliases.is_empty() {
        tracing::debug!("no aliases loaded from subprocess");
    } else if let (Some(h), Some(cp)) = (home.as_ref(), cache_path.as_ref()) {
        save_alias_cache(h, cp, &aliases, shell);
    }
    aliases
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

    // --- Cache infrastructure ---

    #[test]
    fn alias_cache_roundtrip_and_invalidation() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".zshrc"), b"# empty\n").unwrap();

        let cache_path = home.path().join("aliases-cache.json");

        let mut aliases = HashMap::new();
        aliases.insert("g".to_string(), entry(&["git"]));
        aliases.insert("k".to_string(), entry(&["kubectl"]));
        save_alias_cache(home.path(), &cache_path, &aliases, ShellFamily::Zsh);
        assert!(cache_path.exists(), "cache file must be written");

        let loaded = load_alias_cache(home.path(), &cache_path, ShellFamily::Zsh)
            .expect("cache should load cleanly");
        assert_eq!(loaded, aliases, "loaded cache must match saved");

        // Bump the source file's mtime forward — cache should reject.
        let future = SystemTime::now() + std::time::Duration::from_secs(60);
        filetime::set_file_mtime(
            home.path().join(".zshrc"),
            filetime::FileTime::from_system_time(future),
        )
        .unwrap();

        assert!(
            load_alias_cache(home.path(), &cache_path, ShellFamily::Zsh).is_none(),
            "cache must be rejected after source file changes"
        );
    }

    #[test]
    fn alias_cache_skips_empty_result() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".zshrc"), b"# empty\n").unwrap();
        let cache_path = home.path().join("aliases-cache.json");

        save_alias_cache(home.path(), &cache_path, &HashMap::new(), ShellFamily::Zsh);
        assert!(
            !cache_path.exists(),
            "empty alias results must not be cached"
        );
    }

    #[test]
    fn alias_cache_invalidates_when_new_source_appears() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".zshrc"), b"# empty\n").unwrap();
        let cache_path = home.path().join("aliases-cache.json");

        let mut aliases = HashMap::new();
        aliases.insert("g".to_string(), entry(&["git"]));
        save_alias_cache(home.path(), &cache_path, &aliases, ShellFamily::Zsh);
        assert!(load_alias_cache(home.path(), &cache_path, ShellFamily::Zsh).is_some());

        // Create .zshenv after the cache was saved.
        std::fs::write(home.path().join(".zshenv"), b"# new file\n").unwrap();
        assert!(
            load_alias_cache(home.path(), &cache_path, ShellFamily::Zsh).is_none(),
            "new source file must invalidate cache"
        );
    }

    #[test]
    fn alias_cache_invalidates_when_fish_config_edited() {
        let home = tempfile::tempdir().unwrap();
        let fish_dir = home.path().join(".config/fish");
        std::fs::create_dir_all(&fish_dir).unwrap();
        std::fs::write(fish_dir.join("config.fish"), b"abbr -a g git\n").unwrap();
        let cache_path = home.path().join("aliases-cache.json");

        let mut aliases = HashMap::new();
        aliases.insert("g".to_string(), entry(&["git"]));
        save_alias_cache(home.path(), &cache_path, &aliases, ShellFamily::Fish);
        assert!(load_alias_cache(home.path(), &cache_path, ShellFamily::Fish).is_some());

        let future = SystemTime::now() + std::time::Duration::from_secs(60);
        filetime::set_file_mtime(
            fish_dir.join("config.fish"),
            filetime::FileTime::from_system_time(future),
        )
        .unwrap();

        assert!(
            load_alias_cache(home.path(), &cache_path, ShellFamily::Fish).is_none(),
            "editing .config/fish/config.fish must invalidate the cache"
        );
    }

    #[test]
    fn alias_cache_invalidates_when_existing_omz_dropin_is_edited() {
        let home = tempfile::tempdir().unwrap();
        let custom = home.path().join(".oh-my-zsh/custom");
        std::fs::create_dir_all(&custom).unwrap();
        let dropin = custom.join("aliases.zsh");
        std::fs::write(&dropin, b"alias g=git\n").unwrap();

        let past = SystemTime::now() - std::time::Duration::from_secs(3600);
        filetime::set_file_mtime(&custom, filetime::FileTime::from_system_time(past)).unwrap();
        filetime::set_file_mtime(&dropin, filetime::FileTime::from_system_time(past)).unwrap();

        let cache_path = home.path().join("aliases-cache.json");
        let mut aliases = HashMap::new();
        aliases.insert("g".to_string(), entry(&["git"]));
        save_alias_cache(home.path(), &cache_path, &aliases, ShellFamily::Zsh);
        assert!(load_alias_cache(home.path(), &cache_path, ShellFamily::Zsh).is_some());

        let future = SystemTime::now() + std::time::Duration::from_secs(60);
        filetime::set_file_mtime(&dropin, filetime::FileTime::from_system_time(future)).unwrap();
        filetime::set_file_mtime(&custom, filetime::FileTime::from_system_time(past)).unwrap();

        assert!(
            load_alias_cache(home.path(), &cache_path, ShellFamily::Zsh).is_none(),
            "editing an existing drop-in inside a tracked dir must invalidate the cache"
        );
    }

    #[test]
    fn alias_cache_invalidates_on_same_second_subsecond_edit() {
        let home = tempfile::tempdir().unwrap();
        let rc = home.path().join(".zshrc");
        std::fs::write(&rc, b"a").unwrap();
        let cache_path = home.path().join("aliases-cache.json");

        let mut aliases = HashMap::new();
        aliases.insert("g".to_string(), entry(&["git"]));
        save_alias_cache(home.path(), &cache_path, &aliases, ShellFamily::Zsh);
        assert!(load_alias_cache(home.path(), &cache_path, ShellFamily::Zsh).is_some());

        let fp = file_fingerprint(&rc).unwrap();
        std::fs::write(&rc, b"ab").unwrap();
        filetime::set_file_mtime(
            &rc,
            filetime::FileTime::from_unix_time(fp.secs as i64, fp.nanos),
        )
        .unwrap();

        assert!(
            load_alias_cache(home.path(), &cache_path, ShellFamily::Zsh).is_none(),
            "edit that preserves mtime-seconds but changes length must invalidate"
        );
    }

    #[test]
    fn dir_tree_fingerprint_walks_recursively_within_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let leaf_dir = tmp.path().join("plugins/myplugin");
        std::fs::create_dir_all(&leaf_dir).unwrap();
        let leaf = leaf_dir.join("myplugin.plugin.zsh");
        std::fs::write(&leaf, b"alias x=y\n").unwrap();

        let before = dir_tree_fingerprint(tmp.path()).expect("walk must succeed");

        let future = SystemTime::now() + std::time::Duration::from_secs(120);
        filetime::set_file_mtime(&leaf, filetime::FileTime::from_system_time(future)).unwrap();

        let after = dir_tree_fingerprint(tmp.path()).expect("walk must succeed");
        assert!(
            (after.secs, after.nanos, after.len) > (before.secs, before.nanos, before.len),
            "fingerprint must advance after nested-file edit (before={before:?} after={after:?})"
        );
    }

    #[test]
    fn alias_cache_invalidates_when_omz_custom_dir_changes() {
        let home = tempfile::tempdir().unwrap();
        let custom = home.path().join(".oh-my-zsh/custom");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("aliases.zsh"), b"alias g=git\n").unwrap();

        let cache_path = home.path().join("aliases-cache.json");
        let mut aliases = HashMap::new();
        aliases.insert("g".to_string(), entry(&["git"]));
        save_alias_cache(home.path(), &cache_path, &aliases, ShellFamily::Zsh);
        assert!(load_alias_cache(home.path(), &cache_path, ShellFamily::Zsh).is_some());

        std::fs::write(custom.join("work.zsh"), b"alias w=workflow\n").unwrap();
        let future = SystemTime::now() + std::time::Duration::from_secs(60);
        filetime::set_file_mtime(&custom, filetime::FileTime::from_system_time(future)).unwrap();

        assert!(
            load_alias_cache(home.path(), &cache_path, ShellFamily::Zsh).is_none(),
            "adding a drop-in to ~/.oh-my-zsh/custom must invalidate the cache"
        );
    }

    #[test]
    fn save_alias_cache_leaves_no_stale_tmp_files() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".zshrc"), b"# empty\n").unwrap();
        let cache_path = home.path().join("aliases-cache.json");

        let mut aliases = HashMap::new();
        aliases.insert("g".to_string(), entry(&["git"]));
        save_alias_cache(home.path(), &cache_path, &aliases, ShellFamily::Zsh);

        assert!(cache_path.exists(), "cache file must be written");

        let leftover: Vec<_> = std::fs::read_dir(home.path())
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".tmp") || n.contains(".json.tmp"))
            .collect();
        assert!(
            leftover.is_empty(),
            "save_alias_cache must not leave stale temp files; found: {leftover:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn save_alias_cache_refuses_pre_seeded_shared_tmp_symlink() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".zshrc"), b"# empty\n").unwrap();
        let cache_path = home.path().join("aliases-cache.json");

        let victim = home.path().join("victim.txt");
        std::fs::write(&victim, b"DO NOT CLOBBER").unwrap();

        let predictable = cache_path.with_extension("json.tmp");
        std::os::unix::fs::symlink(&victim, &predictable).unwrap();

        let mut aliases = HashMap::new();
        aliases.insert("g".to_string(), entry(&["git"]));
        save_alias_cache(home.path(), &cache_path, &aliases, ShellFamily::Zsh);

        assert_eq!(
            std::fs::read(&victim).unwrap(),
            b"DO NOT CLOBBER",
            "victim must be untouched by alias cache save"
        );
        assert!(
            cache_path.exists(),
            "cache must still be written to its real path"
        );
    }

    #[test]
    fn alias_cache_rejects_old_format_version() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join(".zshrc"), b"# empty\n").unwrap();
        let cache_path = home.path().join("aliases-cache.json");

        let legacy = serde_json::json!({
            "source_mtimes": {
                ".zshrc": { "secs": 0, "nanos": 0, "len": 0 },
            },
            "aliases": { "g": "git" },
        });
        std::fs::write(&cache_path, legacy.to_string()).unwrap();

        assert!(
            load_alias_cache(home.path(), &cache_path, ShellFamily::Zsh).is_none(),
            "v1 cache (missing format_version) must be rejected on load"
        );

        let stale = serde_json::json!({
            "format_version": 1u32,
            "source_mtimes": {},
            "aliases": {},
        });
        std::fs::write(&cache_path, stale.to_string()).unwrap();
        assert!(
            load_alias_cache(home.path(), &cache_path, ShellFamily::Zsh).is_none(),
            "format_version != CURRENT_ALIAS_CACHE_VERSION must be rejected"
        );
    }

    #[test]
    fn concurrent_save_alias_cache_does_not_collide() {
        use std::sync::Arc;

        let home = Arc::new(tempfile::tempdir().unwrap());
        std::fs::write(home.path().join(".zshrc"), b"# empty\n").unwrap();
        let cache_path = home.path().join("aliases-cache.json");

        let mut handles = vec![];
        for t in 0..4 {
            let home = Arc::clone(&home);
            let cache_path = cache_path.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..20 {
                    let mut aliases = HashMap::new();
                    aliases.insert(format!("k_{t}_{i}"), entry(&["cmd"]));
                    save_alias_cache(home.path(), &cache_path, &aliases, ShellFamily::Zsh);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert!(cache_path.exists(), "final cache file must exist");
        let leftover: Vec<_> = std::fs::read_dir(home.path())
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftover.is_empty(),
            "concurrent saves must not leave temp files behind; found: {leftover:?}"
        );
    }
}
