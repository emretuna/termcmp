//! Shell-native completion providers (fish, zsh).
//!
//! Each provider runs the shell's own completion engine in a PTY subprocess
//! and parses the output into suggestions. Results are stored in a persistent,
//! file-backed [`CompletionTreeCache`] keyed by command path (e.g. `"supabase"`,
//! `"supabase backups"`). The cache is consulted synchronously on the trigger
//! path via [`CompletionTreeCache::resolve`]; on a hit the caller skips the
//! backfill providers (fish/zsh) while live providers (LLM) still fire.
//!
//! # Fish
//! `complete -C '<buffer>'` prints one completion per line, optionally
//! tab-separated from a description. Fish 4.x requires a controlling terminal
//! for `complete -C` (it calls `tcsetattr`), so we run it inside a PTY.
//!
//! # Zsh
//! A zpty-based capture widget runs `_main_complete` inside a real ZLE
//! context and collects all `compadd` matches.

use std::collections::HashMap;
use std::future::Future;
use std::io::Read;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use buffer::CommandContext;
use config::MatchMode;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde::{Deserialize, Serialize};
use suggest::fuzzy::rank_with_mode;
use suggest::{AsyncProvider, SuggestRequest, Suggestion, SuggestionSource};

/// How long a cached completion node stays fresh (30 days).
const CACHE_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Maximum number of nodes kept in the tree cache before evicting the oldest.
const MAX_CACHED_NODES: usize = 500;

/// On-disk format version; a mismatch discards the file and starts fresh.
const FORMAT_VERSION: u32 = 1;

/// Background flush interval for dirty cache state.
const FLUSH_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum candidate nodes considered during fuzzy command resolution.
const MAX_FUZZY_CANDIDATES: usize = 200;

// ---------------------------------------------------------------------------
// Persistent tree cache
// ---------------------------------------------------------------------------

/// A single cached completion entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedEntry {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

/// A cached node: completions for one command path, with a refresh timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedNode {
    completions: Vec<CachedEntry>,
    /// Unix timestamp (seconds) of the last refresh.
    refreshed_at: u64,
    /// Monotonic insertion sequence. Breaks `refreshed_at` ties (many inserts
    /// land in the same second) so "oldest" eviction is deterministic.
    #[serde(default)]
    seq: u64,
}

/// The on-disk representation of the completion tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CompletionCacheFile {
    format_version: u32,
    shell: String,
    /// Next monotonic sequence value handed to an inserted node.
    #[serde(default)]
    next_seq: u64,
    nodes: HashMap<String, CachedNode>,
}

/// A persistent, file-backed completion tree cache.
///
/// Nodes are keyed by command path (`"supabase"`, `"supabase backups"`).
/// The cache is loaded from disk on construction, mutated in memory during
/// the session, and flushed back periodically (via [`spawn_flush_task`]) and
/// on drop.
pub struct CompletionTreeCache {
    inner: RwLock<CompletionCacheFile>,
    path: Option<PathBuf>,
    dirty: AtomicBool,
}

impl CompletionTreeCache {
    /// Create an empty cache (no persistence). Useful for tests.
    pub fn new(shell: &str) -> Self {
        Self {
            inner: RwLock::new(CompletionCacheFile {
                format_version: FORMAT_VERSION,
                shell: shell.to_string(),
                next_seq: 0,
                nodes: HashMap::new(),
            }),
            path: None,
            dirty: AtomicBool::new(false),
        }
    }

    /// Load the cache from the platform state directory, or start fresh if
    /// the file is missing, corrupt, or has a mismatched format version.
    pub fn load(shell: &str) -> Self {
        Self::load_from(Self::path_for(shell), shell)
    }

    /// Load the cache from an explicit file path, or start fresh if the
    /// file is missing, corrupt, or has a mismatched format version or shell.
    /// Takes the path as a parameter so tests can point at a temp file.
    fn load_from(path: Option<PathBuf>, shell: &str) -> Self {
        let inner = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<CompletionCacheFile>(&s).ok())
            .filter(|f| f.format_version == FORMAT_VERSION && f.shell == shell)
            .unwrap_or_else(|| CompletionCacheFile {
                format_version: FORMAT_VERSION,
                shell: shell.to_string(),
                next_seq: 0,
                nodes: HashMap::new(),
            });
        Self {
            inner: RwLock::new(inner),
            path,
            dirty: AtomicBool::new(false),
        }
    }

    /// The cache file path: `$XDG_STATE_HOME/termcmp/completions-{shell}.json`,
    /// falling back to `~/.local/state/termcmp/...`.
    fn path_for(shell: &str) -> Option<PathBuf> {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("state")));
        state.map(|s| s.join("termcmp").join(format!("completions-{shell}.json")))
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    /// Look up a node by exact command path. Returns `None` if the node is
    /// missing or older than `CACHE_TTL`.
    fn get(&self, path: &str) -> Option<Vec<CachedEntry>> {
        let inner = self.inner.read().ok()?;
        let node = inner.nodes.get(path)?;
        let age = Self::now_secs().saturating_sub(node.refreshed_at);
        if age > CACHE_TTL.as_secs() {
            return None;
        }
        Some(node.completions.clone())
    }

    /// Insert or replace a node. Evicts the oldest node when the cache
    /// exceeds `MAX_CACHED_NODES`.
    fn insert(&self, path: &str, completions: Vec<CachedEntry>) {
        let Ok(mut inner) = self.inner.write() else {
            return;
        };
        // Evict oldest if at capacity and inserting a new key.
        if inner.nodes.len() >= MAX_CACHED_NODES && !inner.nodes.contains_key(path) {
            if let Some(oldest) = inner
                .nodes
                .iter()
                .min_by_key(|(_, n)| (n.refreshed_at, n.seq))
                .map(|(k, _)| k.clone())
            {
                inner.nodes.remove(&oldest);
            }
        }
        let seq = inner.next_seq;
        inner.next_seq = inner.next_seq.wrapping_add(1);
        inner.nodes.insert(
            path.to_string(),
            CachedNode {
                completions,
                refreshed_at: Self::now_secs(),
                seq,
            },
        );
        self.dirty.store(true, Ordering::Relaxed);
    }

    /// Test-only: seed a node from `(text, description)` pairs without
    /// exposing the private `CachedEntry` type across modules.
    #[cfg(test)]
    pub fn seed_for_test(&self, key: &str, entries: Vec<(String, Option<String>)>) {
        self.insert(
            key,
            entries
                .into_iter()
                .map(|(text, description)| CachedEntry { text, description })
                .collect(),
        );
    }

    /// Write the cache to disk atomically (temp file + rename).
    pub fn flush(&self) {
        if !self.dirty.swap(false, Ordering::Relaxed) {
            return;
        }
        let Some(path) = &self.path else {
            return;
        };
        let Ok(inner) = self.inner.read() else {
            return;
        };
        let Ok(json) = serde_json::to_string_pretty(&*inner) else {
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!("completion cache flush: create_dir_all failed: {e}");
            }
        }
        match tempfile::NamedTempFile::new_in(path.parent().unwrap_or(path)) {
            Ok(tmp) => {
                if let Err(e) = std::fs::write(tmp.path(), json.as_bytes()) {
                    tracing::warn!("completion cache flush: temp write failed: {e}");
                } else if let Err(e) = tmp.persist(path) {
                    tracing::warn!("completion cache flush: persist failed: {e}");
                }
            }
            Err(e) => tracing::warn!("completion cache flush: temp file create failed: {e}"),
        }
    }

    /// Resolve cached completions for the current command context.
    ///
    /// Returns `(suggestions, cache_hit)`. On a hit the caller skips the
    /// backfill providers (fish/zsh); live providers (LLM) still fire.
    ///
    /// Resolution order:
    /// 1. Exact command-path match (e.g. `"supabase"` for buffer `supabase b`).
    /// 2. Fuzzy match on the command word only (level 0), e.g. `s b` resolves
    ///    to `supabase backups` when `supabase` is the best fuzzy match for `s`.
    pub fn resolve(&self, ctx: &CommandContext, buffer: &str) -> (Vec<Suggestion>, bool) {
        // Command position: the user typed a bare command name (no trailing
        // space). If the word is a prefix of one or more cached first-segments,
        // serve their subcommands from cache — the same data the async
        // eager-query path would fetch via `complete -C 'supabase '`.
        // Frizbee in rerank_live filters and ranks the merged pool, so
        // non-matching entries from unrelated commands are dropped.
        if ctx.word_index == 0 {
            let word = &ctx.current_word;
            if !word.is_empty() {
                // Fast path: prefix match on the first segment. The command
                // name is (partially) typed, so its subcommands are the
                // natural candidates. cache_hit = true skips async providers.
                let matches = self.get_prefix_segments(word);
                if !matches.is_empty() {
                    let suggestions: Vec<_> = matches
                        .iter()
                        .flat_map(|(path, entries)| {
                            entries
                                .iter()
                                .filter(|e| !is_command_position_noise(&e.text))
                                .map(move |e| build_command_position_suggestion(word, path, e))
                        })
                        .collect();
                    if !suggestions.is_empty() {
                        return (suggestions, true);
                    }
                    // All entries were noise (e.g. poisoned cache node with
                    // only filesystem entries). Fall through to slow path.
                }
                // Slow path: no prefix match. Dump top-level cached entries
                // into the pool so frizbee can filter them — this covers fuzzy
                // subsequence matches ("supababack" → "supabase backups")
                // and substring matches ("base" → "supabase backups") that
                // prefix matching on the first segment alone cannot find.
                // Only single-segment paths (command nodes like "git",
                // "supabase") are relevant at word_index 0 — deep paths like
                // "git diff" hold argument completions (often filesystem
                // entries from zsh's _files) that are noise here.
                // cache_hit = false so async providers still fire as backfill.
                let all: Vec<_> = self
                    .get_all_entries()
                    .into_iter()
                    .filter(|(path, _)| !path.contains(' '))
                    .collect();
                if !all.is_empty() {
                    let suggestions = all
                        .iter()
                        .flat_map(|(path, entries)| {
                            entries
                                .iter()
                                .filter(|e| !is_command_position_noise(&e.text))
                                .map(move |e| build_command_position_suggestion(word, path, e))
                        })
                        .collect();
                    return (suggestions, false);
                }
            }
            return (vec![], false);
        }
        let Some(cmd) = ctx.command.as_deref() else {
            return (vec![], false);
        };

        // Build the command path: command + completed args before current word.
        let path = completion_path(ctx);

        // 1. Exact path match.
        if let Some(entries) = self.get(&path) {
            let suggestions = entries
                .iter()
                .filter(|e| !is_filesystem_entry(&e.text))
                .map(|e| build_cached_suggestion(buffer, &path, e, false))
                .collect();
            return (suggestions, true);
        }

        // 2. Fuzzy command resolution (level 0 only).
        if ctx.word_index == 1 && ctx.args.is_empty() {
            if let Some((resolved_path, entries)) = self.resolve_fuzzy(cmd) {
                let suggestions = entries
                    .iter()
                    .filter(|e| !is_filesystem_entry(&e.text))
                    .map(|e| build_cached_suggestion(buffer, &resolved_path, e, true))
                    .collect();
                return (suggestions, true);
            }
        }

        (vec![], false)
    }

    /// Fuzzy-match the typed command word against cached node keys (first
    /// path segment only). Returns the best match's full path and entries.
    fn resolve_fuzzy(&self, typed_cmd: &str) -> Option<(String, Vec<CachedEntry>)> {
        let inner = self.inner.read().ok()?;
        let now = Self::now_secs();

        // Collect unique first-segments with their best (most recent) node.
        let mut candidates: Vec<(String, String, u64)> = Vec::new(); // (segment, full_path, refreshed_at)
        for (path, node) in &inner.nodes {
            let age = now.saturating_sub(node.refreshed_at);
            if age > CACHE_TTL.as_secs() {
                continue;
            }
            let segment = path.split_whitespace().next().unwrap_or_default();
            if segment.is_empty() {
                continue;
            }
            // Keep the most recent node per segment.
            if let Some(existing) = candidates.iter_mut().find(|(s, _, _)| s == segment) {
                if node.refreshed_at > existing.2 {
                    existing.1 = path.clone();
                    existing.2 = node.refreshed_at;
                }
            } else {
                candidates.push((segment.to_string(), path.clone(), node.refreshed_at));
            }
        }

        if candidates.is_empty() {
            return None;
        }
        candidates.truncate(MAX_FUZZY_CANDIDATES);

        // Rank segments against the typed command using prefix+substring.
        let seg_suggestions: Vec<Suggestion> = candidates
            .iter()
            .map(|(seg, _, _)| Suggestion {
                text: seg.clone(),
                source: SuggestionSource::History, // neutral source for ranking
                ..Default::default()
            })
            .collect();

        let ranked = rank_with_mode(typed_cmd, seg_suggestions, 1, MatchMode::Substring);
        let best = ranked.first()?;

        // Find the full path for the winning segment.
        let (_, full_path, _) = candidates.iter().find(|(s, _, _)| s == &best.text)?;
        let node = inner.nodes.get(full_path)?;
        Some((full_path.clone(), node.completions.clone()))
    }

    /// Find all cached nodes whose first path segment starts with `word`.
    /// Returns one entry per unique segment (most recently refreshed wins).
    fn get_prefix_segments(&self, word: &str) -> Vec<(String, Vec<CachedEntry>)> {
        let Ok(inner) = self.inner.read() else {
            return Vec::new();
        };
        let now = Self::now_secs();
        // (segment, full_path, refreshed_at) — one per unique segment
        let mut best: Vec<(String, String, u64)> = Vec::new();
        for (path, node) in &inner.nodes {
            let age = now.saturating_sub(node.refreshed_at);
            if age > CACHE_TTL.as_secs() {
                continue;
            }
            let segment = path.split_whitespace().next().unwrap_or_default();
            if segment.is_empty() || !segment.starts_with(word) {
                continue;
            }
            match best.iter_mut().find(|(s, _, _)| s == segment) {
                Some(entry) if node.refreshed_at > entry.2 => {
                    entry.1 = path.clone();
                    entry.2 = node.refreshed_at;
                }
                Some(_) => {}
                None => best.push((segment.to_string(), path.clone(), node.refreshed_at)),
            }
        }
        best.into_iter()
            .filter_map(|(_, path, _)| {
                let node = inner.nodes.get(&path)?;
                Some((path, node.completions.clone()))
            })
            .collect()
    }

    /// Return all non-expired cached entries as `(path, entries)` pairs.
    /// Used as a fallback at word_index 0 when prefix matching finds nothing,
    /// so frizbee can filter the full candidate pool (fuzzy subsequence and
    /// substring matches that prefix-on-first-segment cannot reach).
    fn get_all_entries(&self) -> Vec<(String, Vec<CachedEntry>)> {
        let Ok(inner) = self.inner.read() else {
            return Vec::new();
        };
        let now = Self::now_secs();
        inner
            .nodes
            .iter()
            .filter(|(_, node)| now.saturating_sub(node.refreshed_at) <= CACHE_TTL.as_secs())
            .map(|(path, node)| (path.clone(), node.completions.clone()))
            .collect()
    }
}

/// Whether a completion entry is a filesystem path. The built-in
/// `FilesystemProvider` handles filesystem completion; shell providers
/// should not duplicate it. Pattern checks are fast paths; the final
/// `Path::exists` catches bare names (`Downloads`, `Applications`) that
/// the shell's `_files` fallback returns for the current directory.
fn is_filesystem_entry(text: &str) -> bool {
    text.ends_with('/')
        || text.starts_with("~/")
        || text.starts_with("./")
        || text.starts_with("../")
        || text.starts_with('/')
        || std::path::Path::new(text).exists()
}

/// Whether a completion entry is noise at command position (word_index 0).
/// Delegates path checks to `is_filesystem_entry` and adds an uppercase
/// heuristic: capitalized directory names (`Applications`, `Downloads`) are
/// only noise at command position — at arg positions they can be legitimate.
fn is_command_position_noise(text: &str) -> bool {
    is_filesystem_entry(text) || text.chars().next().is_some_and(|c| c.is_uppercase())
}

/// Check if a command exists in PATH. Used to short-circuit shell completion
/// queries for non-existent commands — the shell's _files fallback would
/// otherwise dump home-dir listings as noise.
fn command_exists_in_path(cmd: &str) -> bool {
    let Ok(path_var) = std::env::var("PATH") else {
        return false;
    };
    for dir in path_var.split(':') {
        let candidate = std::path::Path::new(dir).join(cmd);
        if candidate.is_file() {
            return true;
        }
    }
    false
}

impl Drop for CompletionTreeCache {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Build the command path from a `CommandContext`: the command word plus all
/// completed args before the current word, joined by spaces.
///
/// - `supabase b` → `"supabase"` (word_index=1, args=[])
/// - `supabase backups l` → `"supabase backups"` (word_index=2, args=["backups"])
fn completion_path(ctx: &CommandContext) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(cmd) = ctx.command.as_deref() {
        parts.push(cmd);
    }
    parts.extend(ctx.args.iter().map(|s| s.as_str()));
    parts.join(" ")
}

/// Build a suggestion from a cached entry.
///
/// When `fuzzy` is true the resolved path differs from the typed buffer, so
/// the full text is `resolved_path + " " + entry.text` and match indices cover
/// the entire buffer (acceptance replaces everything).
fn build_cached_suggestion(
    buffer: &str,
    resolved_path: &str,
    entry: &CachedEntry,
    fuzzy: bool,
) -> Suggestion {
    if fuzzy {
        let full_text = format!("{} {}", resolved_path, entry.text);
        let matched = common_prefix_char_count(buffer, &full_text);
        let match_indices: Vec<u32> = (0..matched as u32).collect();
        Suggestion {
            text: full_text,
            description: entry.description.clone(),
            score: 90,
            source: SuggestionSource::Provider,
            match_indices,
            ..Default::default()
        }
    } else {
        let (full_text, match_indices) = build_full_text(buffer, &entry.text);
        Suggestion {
            text: full_text,
            description: entry.description.clone(),
            score: 90,
            source: SuggestionSource::Provider,
            match_indices,
            ..Default::default()
        }
    }
}

/// Build a suggestion for word_index==0 cache resolution. The buffer is just
/// the command name (e.g. "supabase") with no trailing space, so full_text
/// must be `command + " " + entry` and match_indices cover the typed command.
fn build_command_position_suggestion(
    word: &str,
    resolved_path: &str,
    entry: &CachedEntry,
) -> Suggestion {
    let full_text = format!("{} {}", resolved_path, entry.text);
    let matched = common_prefix_char_count(word, &full_text);
    let match_indices: Vec<u32> = (0..matched as u32).collect();
    Suggestion {
        text: full_text,
        description: entry.description.clone(),
        score: 90,
        source: SuggestionSource::Provider,
        match_indices,
        ..Default::default()
    }
}

/// Spawn a background task that flushes the cache every `FLUSH_INTERVAL`.
/// Holds a `Weak` reference so it doesn't prevent drop.
pub fn spawn_flush_task(cache: &Arc<CompletionTreeCache>) {
    let weak = Arc::downgrade(cache);
    std::thread::spawn(move || loop {
        std::thread::sleep(FLUSH_INTERVAL);
        match weak.upgrade() {
            Some(cache) => cache.flush(),
            None => break,
        }
    });
}

// ---------------------------------------------------------------------------
// Completion line parsing
// ---------------------------------------------------------------------------

/// A parsed completion line: the completion text plus an optional description.
struct CompletionLine {
    completion: String,
    description: Option<String>,
}

/// Merge a base completion query with an eager "next level" query.
///
/// Shells only emit subcommand/argument completions once the command word is
/// followed by a space (`complete -C 'supabase '`), so typing `supabase`
/// (no trailing space) yields nothing useful even though the word is complete.
/// When the base query signals the current word is finished — an empty result
/// or a single exact match — the providers re-query with a trailing space and
/// pass both result sets here. Base rows are built against `buffer`; expanded
/// rows against `expanded_buffer` (the buffer plus the inserted space) so their
/// `full_text` carries the completed command (e.g. `supabase backups`). Rows
/// are deduplicated by `full_text` (base wins) and capped at `max_results`.
fn merge_completions(
    buffer: &str,
    expanded_buffer: &str,
    base: &[CompletionLine],
    expanded: &[CompletionLine],
    max_results: usize,
) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut push = |line: &CompletionLine, query_buffer: &str| {
        if out.len() >= max_results {
            return;
        }
        let s = build_provider_suggestion(query_buffer, &line.completion, line.description.clone());
        if seen.insert(s.text.clone()) {
            out.push(s);
        }
    };
    for line in base {
        push(line, buffer);
    }
    for line in expanded {
        push(line, expanded_buffer);
    }
    out
}

/// Whether the base completion result signals that the current word is
/// complete, making an eager trailing-space query worthwhile: either nothing
/// matched (the word is accepted as-is, e.g. `supabase`) or exactly one
/// completion matched and it equals the word verbatim. Multiple matches or a
/// single partial match mean the word is still being typed, so expanding would
/// only add a wasted shell round-trip.
fn word_is_complete(base: &[CompletionLine], word: &str) -> bool {
    match base.len() {
        0 => true,
        1 => base[0].completion == word,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// PTY subprocess
// ---------------------------------------------------------------------------

/// Run a command inside a PTY and return its stdout. Fish 4.x's `complete -C`
/// requires a controlling terminal (it calls `tcsetattr` for raw mode), so a
/// plain subprocess with piped stdio fails with "failed to enable raw mode".
fn run_in_pty(cmd: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .ok()?;

    let mut cb = CommandBuilder::new(cmd);
    for a in args {
        cb.arg(a);
    }
    let mut child = pair.slave.spawn_command(cb).ok()?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().ok()?;
    let start = Instant::now();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];

    loop {
        if start.elapsed() > timeout {
            break;
        }
        match reader.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
        if let Ok(Some(_)) = child.try_wait() {
            // Drain any remaining output after exit.
            loop {
                match reader.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    Err(_) => break,
                }
            }
            break;
        }
    }

    Some(String::from_utf8_lossy(&buf).to_string())
}

// ---------------------------------------------------------------------------
// Fish provider
// ---------------------------------------------------------------------------

/// Run `complete -C '<buffer>'` in fish (inside a PTY) and return stdout,
/// consulting the tree cache for exact query deduplication.
fn fish_query(buffer: &str, cache: &CompletionTreeCache) -> Option<String> {
    // Use the tree cache's get() for exact query dedup (avoids re-spawning
    // for the same buffer within a session). We key on a synthetic path
    // prefixed with "query:" to avoid collisions with command-path nodes.
    let query_key = format!("__query__ {}", buffer);
    if let Some(entries) = cache.get(&query_key) {
        // Cached query result: reconstruct stdout from entries.
        let stdout = entries
            .iter()
            .map(|e| match &e.description {
                Some(d) => format!("{}\t{}", e.text, d),
                None => e.text.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Some(stdout);
    }

    let script = format!("complete -C {}", fish_escape(buffer));
    let result = run_in_pty("fish", &["-c", &script], Duration::from_secs(5))?;

    // Cache the raw query result.
    let entries: Vec<CachedEntry> = result
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut parts = line.splitn(2, '\t');
            let completion = parts.next().unwrap_or_default().to_string();
            let description = parts.next().map(|d| d.trim().to_string());
            CachedEntry {
                text: completion,
                description,
            }
        })
        .collect();
    cache.insert(&query_key, entries);

    Some(result)
}

/// Async provider that queries fish's completion engine (`complete -C`).
pub struct FishCompletionProvider {
    max_results: usize,
    cache: Arc<CompletionTreeCache>,
}

impl FishCompletionProvider {
    pub fn new(max_results: usize, cache: Arc<CompletionTreeCache>) -> Self {
        Self { max_results, cache }
    }
}

impl AsyncProvider for FishCompletionProvider {
    fn name(&self) -> &'static str {
        "fish"
    }

    fn is_backfill_provider(&self) -> bool {
        true
    }

    fn suggest<'a>(
        &'a self,
        req: &'a SuggestRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<Suggestion>>> + Send + 'a>> {
        let buffer = req.buffer.to_string();
        let word = current_word(&buffer, req.cursor);
        let ctx = req.ctx.clone();
        let cache = Arc::clone(&self.cache);
        let max_results = self.max_results;

        // If the command doesn't exist in PATH, the shell's _files fallback
        // would dump home-dir listings as noise. Return empty.
        if ctx.word_index > 0 {
            if let Some(cmd) = ctx.command.as_deref() {
                if !command_exists_in_path(cmd) {
                    return Box::pin(async move { Ok(Vec::new()) });
                }
            }
        }
        Box::pin(async move {
            let base_stdout = fish_query(&buffer, &cache);
            let base = parse_fish_stdout(base_stdout.as_deref());

            // Eager expansion: re-query with a trailing space to surface the
            // next level of completions (subcommands, arguments).
            // At command position (word_index 0), only expand when the base
            // query found exactly one exact match — the command exists and is
            // complete. An empty base means the command is unknown; expanding
            // would trigger the shell's _files fallback and dump home-dir
            // listings as noise.
            let expanded_stdout = if ctx.word_index == 0 {
                if base.len() == 1 && base[0].completion == word {
                    let expanded = format!("{} ", buffer);
                    fish_query(&expanded, &cache)
                } else {
                    None
                }
            } else if word_is_complete(&base, &word) {
                let expanded = format!("{} ", buffer);
                fish_query(&expanded, &cache)
            } else {
                None
            };

            let expanded = parse_fish_stdout(expanded_stdout.as_deref());
            // Filter filesystem entries — the built-in FilesystemProvider owns these.
            let base: Vec<_> = base
                .into_iter()
                .filter(|l| !is_filesystem_entry(&l.completion))
                .collect();
            let expanded: Vec<_> = expanded
                .into_iter()
                .filter(|l| !is_filesystem_entry(&l.completion))
                .collect();
            let expanded_buffer = format!("{} ", buffer);
            let suggestions =
                merge_completions(&buffer, &expanded_buffer, &base, &expanded, max_results);

            // Backfill the tree cache with structured nodes.
            backfill_tree_cache(&ctx, &buffer, &base, &expanded, &cache);

            Ok(suggestions)
        })
    }
}

/// Parse fish `complete -C` stdout into completion lines.
fn parse_fish_stdout(stdout: Option<&str>) -> Vec<CompletionLine> {
    stdout
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut parts = line.splitn(2, '\t');
            let completion = parts.next().unwrap_or_default().to_string();
            let description = parts
                .next()
                .map(|d| d.trim().to_string())
                .filter(|d| !d.is_empty());
            CompletionLine {
                completion,
                description,
            }
        })
        .collect()
}

/// Backfill the tree cache from provider results.
///
/// - Base results are cached at the current command path only when
///   `current_word` is empty (unfiltered full set).
/// - Expanded results (from `buffer + " "`) are cached at the next-level path
///   (the completed word becomes part of the path).
fn backfill_tree_cache(
    ctx: &CommandContext,
    buffer: &str,
    base: &[CompletionLine],
    expanded: &[CompletionLine],
    cache: &CompletionTreeCache,
) {
    let path = completion_path(ctx);

    // Cache base results only when current_word is empty (unfiltered).
    if ctx.current_word.is_empty() && !base.is_empty() {
        let entries: Vec<CachedEntry> = base
            .iter()
            .map(|l| CachedEntry {
                text: l.completion.clone(),
                description: l.description.clone(),
            })
            .collect();
        cache.insert(&path, entries);
    }

    // Cache expanded results at the next-level path.
    if !expanded.is_empty() {
        // The next-level path is the current buffer's last word appended to
        // the current path. For buffer "supabase" with word "supabase", the
        // next path is "supabase". For buffer "git re" with word "re" and
        // base=["rebase","remote"], expansion doesn't fire (word not complete).
        // When expansion fires, the word is complete, so next path = path + word.
        let word = current_word(buffer, buffer.len());
        let next_path = if path.is_empty() {
            word.clone()
        } else {
            format!("{} {}", path, word)
        };
        let entries: Vec<CachedEntry> = expanded
            .iter()
            .map(|l| CachedEntry {
                text: l.completion.clone(),
                description: l.description.clone(),
            })
            .collect();
        cache.insert(&next_path, entries);
    }
}

// ---------------------------------------------------------------------------
// Zsh provider
// ---------------------------------------------------------------------------

/// Zsh completion capture script. Runs inside `zsh -c` and uses `zpty` to
/// spawn an interactive zsh with a real ZLE context, then triggers a custom
/// widget that captures all `compadd` matches.
///
/// The approach: `_main_complete` can only run inside a ZLE completion widget.
/// We spawn an interactive zsh via zpty, define a capture widget bound to
/// `^Xx`, type the target buffer into the ZLE prompt, trigger the widget,
/// and read results from a temp file.
///
/// Usage: zsh -c "$ZSH_COMPLETION_SCRIPT" -- "buffer" cursor
const ZSH_COMPLETION_SCRIPT: &str = r#"
zmodload zsh/zpty 2>/dev/null || exit 1

typeset _tc_buffer="$1"
typeset _tc_cursor="${2:-${#1}}"
typeset _tc_outfile="/tmp/.tc-zsh-comp.$$"
rm -f "$_tc_outfile"

# Spawn an interactive zsh in a pty (gives us a real ZLE context).
zpty ztc zsh -i 2>/dev/null || exit 1

# Wait for startup and drain.
sleep 1
zpty -r -t ztc 2>/dev/null

# Load compinit (full scan, not cached — we need all completion functions).
zpty -w ztc 'autoload -Uz compinit && compinit -u 2>/dev/null; echo TC_COMPINIT_OK'
sleep 2
zpty -r -t ztc 2>/dev/null

# Define the capture widget: overrides compadd to collect matches, runs
# _main_complete via a completion widget, writes results to a file.
zpty -w ztc 'function tc-capture { typeset -ga _tc_comps=(); function compadd { local -a m; builtin compadd -O m "$@" 2>/dev/null; _tc_comps+=("${m[@]}") }; zle -C tc-cap complete-word _main_complete; zle tc-cap 2>/dev/null; print -l "${_tc_comps[@]}" > '"$_tc_outfile"'; zle kill-whole-line }; zle -N tc-capture; bindkey "^Xx" tc-capture; echo TC_SETUP_OK'
sleep 1
zpty -r -t ztc 2>/dev/null

# Clear the line and type the target buffer (no newline — goes into ZLE buffer).
zpty -w -n ztc $'\x15'
sleep 0.2
zpty -r -t ztc 2>/dev/null

zpty -w -n ztc "$_tc_buffer"
sleep 0.3
zpty -r -t ztc 2>/dev/null

# Trigger the capture widget with Ctrl-X x.
zpty -w -n ztc $'\x18'
sleep 0.1
zpty -w -n ztc 'x'
sleep 1
zpty -r -t ztc 2>/dev/null

# Clean up the pty.
zpty -d ztc 2>/dev/null

# Output results.
cat "$_tc_outfile" 2>/dev/null
rm -f "$_tc_outfile"
"#;

/// Async provider that queries zsh's completion system (compsys) via a
/// zpty-based capture widget. The widget runs `_main_complete` inside a real
/// ZLE context and collects all `compadd` matches.
pub struct ZshCompletionProvider {
    max_results: usize,
    cache: Arc<CompletionTreeCache>,
}

impl ZshCompletionProvider {
    pub fn new(max_results: usize, cache: Arc<CompletionTreeCache>) -> Self {
        Self { max_results, cache }
    }
}

impl AsyncProvider for ZshCompletionProvider {
    fn name(&self) -> &'static str {
        "zsh"
    }

    fn is_backfill_provider(&self) -> bool {
        true
    }

    fn suggest<'a>(
        &'a self,
        req: &'a SuggestRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<Suggestion>>> + Send + 'a>> {
        let buffer = req.buffer.to_string();
        let cursor = req.cursor;
        let word = current_word(&buffer, cursor);
        let ctx = req.ctx.clone();
        let cache = Arc::clone(&self.cache);
        let max_results = self.max_results;

        // If the command doesn't exist in PATH, the shell's _files fallback
        // would dump home-dir listings as noise. Return empty.
        if ctx.word_index > 0 {
            if let Some(cmd) = ctx.command.as_deref() {
                if !command_exists_in_path(cmd) {
                    return Box::pin(async move { Ok(Vec::new()) });
                }
            }
        }

        Box::pin(async move {
            let base_stdout = zsh_query(&buffer, cursor, &cache);
            let base = parse_zsh_stdout(base_stdout.as_deref());

            // Eager expansion: same logic as fish — at command position only
            // expand when the base query found exactly one exact match (the
            // command exists). An empty base means the command is unknown;
            // expanding would trigger _files and dump home-dir listings.
            let expanded_stdout = if ctx.word_index == 0 {
                if base.len() == 1 && base[0].completion == word {
                    let expanded = format!("{} ", buffer);
                    zsh_query(&expanded, expanded.len(), &cache)
                } else {
                    None
                }
            } else if word_is_complete(&base, &word) {
                let expanded = format!("{} ", buffer);
                zsh_query(&expanded, expanded.len(), &cache)
            } else {
                None
            };

            let expanded = parse_zsh_stdout(expanded_stdout.as_deref());
            // Filter filesystem entries — the built-in FilesystemProvider owns these.
            let base: Vec<_> = base
                .into_iter()
                .filter(|l| !is_filesystem_entry(&l.completion))
                .collect();
            let expanded: Vec<_> = expanded
                .into_iter()
                .filter(|l| !is_filesystem_entry(&l.completion))
                .collect();
            let expanded_buffer = format!("{} ", buffer);
            let suggestions =
                merge_completions(&buffer, &expanded_buffer, &base, &expanded, max_results);

            // Backfill the tree cache.
            backfill_tree_cache(&ctx, &buffer, &base, &expanded, &cache);

            Ok(suggestions)
        })
    }
}

/// Run the zsh completion capture script and return stdout, consulting the
/// tree cache for exact query deduplication.
fn zsh_query(buffer: &str, cursor: usize, cache: &CompletionTreeCache) -> Option<String> {
    let query_key = format!("__query__ {} {}", buffer, cursor);
    if let Some(entries) = cache.get(&query_key) {
        let stdout = entries
            .iter()
            .map(|e| e.text.clone())
            .collect::<Vec<_>>()
            .join("\n");
        return Some(stdout);
    }

    let cursor_str = cursor.to_string();
    let result = run_in_pty(
        "zsh",
        &["-c", ZSH_COMPLETION_SCRIPT, "--", buffer, &cursor_str],
        Duration::from_secs(5),
    )?;

    let entries: Vec<CachedEntry> = result
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| CachedEntry {
            text: line.to_string(),
            description: None,
        })
        .collect();
    cache.insert(&query_key, entries);

    Some(result)
}

/// Parse zsh capture stdout into completion lines. Zsh compadd output is
/// one match per line, no descriptions.
fn parse_zsh_stdout(stdout: Option<&str>) -> Vec<CompletionLine> {
    stdout
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| CompletionLine {
            completion: line.trim().to_string(),
            description: None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Build the full replacement text for a provider completion.
///
/// Shell completions return only the current word's replacement (e.g. fish
/// returns `backups` for buffer `supabase b`), but accepting a suggestion
/// replaces the entire buffer. Prepend the buffer prefix before the current
/// word so the suggestion text is the full command line.
///
/// Returns `(full_text, match_indices)` where `match_indices` are the char
/// positions within `full_text` that match the already-typed on-screen buffer
/// prefix. The accept path uses the contiguous leading run of these indices to
/// determine how many backspaces to emit before inserting the replacement.
fn build_full_text(buffer: &str, completion: &str) -> (String, Vec<u32>) {
    let prefix = match buffer.rfind(' ') {
        Some(pos) => &buffer[..=pos], // includes the trailing space
        None => "",
    };
    let full_text = format!("{}{}", prefix, completion);
    // The on-screen buffer is a prefix of full_text up to the common length.
    // Mark those chars as matched so the highlighter and accept path see the
    // typed portion as already present.
    let matched = common_prefix_char_count(buffer, &full_text);
    let match_indices: Vec<u32> = (0..matched as u32).collect();
    (full_text, match_indices)
}

/// Build one provider suggestion from a raw completion line. Shared by the
/// fish and zsh providers.
fn build_provider_suggestion(
    buffer: &str,
    completion: &str,
    description: Option<String>,
) -> Suggestion {
    let (full_text, match_indices) = build_full_text(buffer, completion);
    Suggestion {
        text: full_text,
        description,
        score: 80,
        source: SuggestionSource::Provider,
        match_indices,
        ..Default::default()
    }
}

/// Escape a string for use inside fish single quotes.
fn fish_escape(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Extract the current word (token being completed) from the buffer.
fn current_word(buffer: &str, cursor: usize) -> String {
    let before = &buffer[..cursor.min(buffer.len())];
    before
        .split_whitespace()
        .next_back()
        .unwrap_or_default()
        .to_string()
}

/// Count the number of leading chars that are identical between two strings.
fn common_prefix_char_count(a: &str, b: &str) -> usize {
    a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // build_full_text
    // -----------------------------------------------------------------------

    #[test]
    fn full_text_prepends_buffer_prefix() {
        // Buffer "supabase b", fish returns "backups" → full text is
        // "supabase backups" so acceptance replaces the whole line.
        let (text, indices) = build_full_text("supabase b", "backups");
        assert_eq!(text, "supabase backups");
        // "supabase b" is a prefix of "supabase backups" → 10 matched chars.
        assert_eq!(indices, vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn full_text_no_space_uses_empty_prefix() {
        // Single-word buffer: no prefix to prepend.
        let (text, indices) = build_full_text("git", "git");
        assert_eq!(text, "git");
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn full_text_trailing_space_buffer() {
        // Buffer ends with space: prefix is the whole buffer, completion
        // appends after it.
        let (text, indices) = build_full_text("supabase ", "backups");
        assert_eq!(text, "supabase backups");
        // "supabase " (9 chars) is a prefix of "supabase backups".
        assert_eq!(indices, vec![0, 1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn full_text_multi_word_prefix() {
        let (text, indices) = build_full_text("git remote a", "add");
        assert_eq!(text, "git remote add");
        assert_eq!(indices, vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
    }

    #[test]
    fn full_text_completion_replaces_word() {
        // Fish returns the full word replacement, not just the suffix.
        let (text, _) = build_full_text("docker comp", "compose");
        assert_eq!(text, "docker compose");
    }

    // -----------------------------------------------------------------------
    // merge_completions
    // -----------------------------------------------------------------------

    fn line(completion: &str) -> CompletionLine {
        CompletionLine {
            completion: completion.to_string(),
            description: None,
        }
    }

    #[test]
    fn merge_expanded_rows_carry_completed_word() {
        // Base query "supabase" → ["supabase"] (self-match, uninteresting).
        // Expanded query "supabase " → ["backups", "db", "storage"].
        // Expanded rows are built against "supabase " so their full_text
        // includes the command.
        let base = vec![line("supabase")];
        let expanded = vec![line("backups"), line("db"), line("storage")];
        let out = merge_completions("supabase", "supabase ", &base, &expanded, 10);
        let texts: Vec<&str> = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "supabase",
                "supabase backups",
                "supabase db",
                "supabase storage"
            ]
        );
        // Expanded rows must have match_indices covering "supabase " (9 chars)
        // so the accept path replaces the whole typed buffer.
        assert_eq!(out[1].match_indices, vec![0, 1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn merge_deduplicates_by_full_text() {
        // Base and expanded both produce "supabase backups" → only once.
        let base = vec![line("backups")];
        let expanded = vec![line("backups"), line("db")];
        // Both built against the same buffer shape for this test.
        let out = merge_completions("supabase ", "supabase ", &base, &expanded, 10);
        let texts: Vec<&str> = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["supabase backups", "supabase db"]);
    }

    #[test]
    fn merge_caps_at_max_results() {
        let base = vec![line("a"), line("b")];
        let expanded = vec![line("c"), line("d"), line("e")];
        let out = merge_completions("x", "x ", &base, &expanded, 3);
        assert_eq!(out.len(), 3);
    }

    // -----------------------------------------------------------------------
    // word_is_complete
    // -----------------------------------------------------------------------

    #[test]
    fn word_complete_on_empty_result() {
        assert!(word_is_complete(&[], "supabase"));
    }

    #[test]
    fn word_complete_on_exact_self_match() {
        assert!(word_is_complete(&[line("supabase")], "supabase"));
    }

    #[test]
    fn word_not_complete_on_partial_match() {
        assert!(!word_is_complete(&[line("supabase-cli")], "supabase"));
    }

    #[test]
    fn word_not_complete_on_multiple_matches() {
        assert!(!word_is_complete(&[line("git"), line("gitk")], "git"));
    }

    // -----------------------------------------------------------------------
    // Tree cache
    // -----------------------------------------------------------------------

    #[test]
    fn tree_cache_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("completions-fish.json");

        // Create a cache with a known path, insert, flush.
        let cache = CompletionTreeCache {
            inner: RwLock::new(CompletionCacheFile {
                format_version: FORMAT_VERSION,
                shell: "fish".to_string(),
                next_seq: 0,
                nodes: HashMap::new(),
            }),
            path: Some(path.clone()),
            dirty: AtomicBool::new(false),
        };
        cache.insert(
            "supabase",
            vec![
                CachedEntry {
                    text: "backups".to_string(),
                    description: Some("Manage backups".to_string()),
                },
                CachedEntry {
                    text: "db".to_string(),
                    description: None,
                },
            ],
        );
        cache.flush();
        assert!(path.exists());

        // Reload from disk.
        let loaded = CompletionTreeCache {
            inner: RwLock::new(
                serde_json::from_str::<CompletionCacheFile>(
                    &std::fs::read_to_string(&path).unwrap(),
                )
                .unwrap(),
            ),
            path: Some(path),
            dirty: AtomicBool::new(false),
        };
        let entries = loaded.get("supabase").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "backups");
        assert_eq!(entries[0].description.as_deref(), Some("Manage backups"));
        assert_eq!(entries[1].text, "db");
    }

    #[test]
    fn tree_cache_eviction() {
        let cache = CompletionTreeCache::new("fish");
        // Insert MAX_CACHED_NODES + 1 entries.
        for i in 0..=MAX_CACHED_NODES {
            cache.insert(
                &format!("cmd{}", i),
                vec![CachedEntry {
                    text: "sub".to_string(),
                    description: None,
                }],
            );
        }
        let inner = cache.inner.read().unwrap();
        assert_eq!(inner.nodes.len(), MAX_CACHED_NODES);
        // The oldest (cmd0) should have been evicted.
        assert!(!inner.nodes.contains_key("cmd0"));
        assert!(inner
            .nodes
            .contains_key(&format!("cmd{}", MAX_CACHED_NODES)));
    }

    #[test]
    fn load_corrupt_json_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        std::fs::write(&path, "not json{{{").unwrap();
        let cache = CompletionTreeCache::load_from(Some(path), "fish");
        assert!(cache.inner.read().unwrap().nodes.is_empty());
        assert!(cache.get("git").is_none());
    }

    #[test]
    fn load_wrong_version_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let contents = serde_json::json!({
            "format_version": 999,
            "shell": "fish",
            "next_seq": 1,
            "nodes": {
                "git": {
                    "completions": [{ "text": "status", "description": null }],
                    "refreshed_at": 1,
                    "seq": 0
                }
            }
        })
        .to_string();
        std::fs::write(&path, contents).unwrap();
        let cache = CompletionTreeCache::load_from(Some(path), "fish");
        assert!(cache.inner.read().unwrap().nodes.is_empty());
        assert!(cache.get("git").is_none());
    }

    #[test]
    fn load_wrong_shell_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let contents = serde_json::json!({
            "format_version": FORMAT_VERSION,
            "shell": "zsh",
            "next_seq": 1,
            "nodes": {
                "git": {
                    "completions": [{ "text": "status", "description": null }],
                    "refreshed_at": 1,
                    "seq": 0
                }
            }
        })
        .to_string();
        std::fs::write(&path, contents).unwrap();
        let cache = CompletionTreeCache::load_from(Some(path), "fish");
        assert!(cache.inner.read().unwrap().nodes.is_empty());
        assert!(cache.get("git").is_none());
    }

    #[test]
    fn load_missing_file_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        let cache = CompletionTreeCache::load_from(Some(path), "fish");
        assert!(cache.inner.read().unwrap().nodes.is_empty());
        assert!(cache.get("git").is_none());
    }

    #[test]
    fn load_valid_file_restores_nodes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.json");
        let contents = serde_json::json!({
            "format_version": FORMAT_VERSION,
            "shell": "fish",
            "next_seq": 0,
            "nodes": {
                "git": {
                    "completions": [{ "text": "status", "description": null }],
                    "refreshed_at": CompletionTreeCache::now_secs(),
                    "seq": 0
                }
            }
        })
        .to_string();
        std::fs::write(&path, contents).unwrap();
        let cache = CompletionTreeCache::load_from(Some(path), "fish");
        let inner = cache.inner.read().unwrap();
        assert!(!inner.nodes.is_empty());
        assert!(inner.nodes.contains_key("git"));
        drop(inner);
        let entries = cache.get("git").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "status");
    }

    /// Build a context the way the trigger path does: cursor at end of buffer.
    fn ctx(buffer: &str) -> CommandContext {
        buffer::parse_command_context(buffer, buffer.chars().count())
    }

    #[test]
    fn resolve_exact_path() {
        let cache = CompletionTreeCache::new("fish");
        cache.insert(
            "supabase",
            vec![
                CachedEntry {
                    text: "backups".to_string(),
                    description: None,
                },
                CachedEntry {
                    text: "db".to_string(),
                    description: None,
                },
            ],
        );

        let (suggestions, hit) = cache.resolve(&ctx("supabase b"), "supabase b");
        assert!(hit);
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].text, "supabase backups");
        assert_eq!(suggestions[1].text, "supabase db");
    }

    #[test]
    fn resolve_fuzzy_command() {
        let cache = CompletionTreeCache::new("fish");
        cache.insert(
            "supabase",
            vec![CachedEntry {
                text: "backups".to_string(),
                description: None,
            }],
        );

        let (suggestions, hit) = cache.resolve(&ctx("s b"), "s b");
        assert!(hit);
        assert_eq!(suggestions.len(), 1);
        // Fuzzy: full text is resolved path + entry text.
        assert_eq!(suggestions[0].text, "supabase backups");
        // Match indices cover the common prefix of buffer and full text.
        // "s b" vs "supabase backups" — only 's' is a literal prefix match.
        assert_eq!(suggestions[0].match_indices, vec![0]);
    }

    #[test]
    fn resolve_cache_miss() {
        let cache = CompletionTreeCache::new("fish");
        let (suggestions, hit) = cache.resolve(&ctx("unknown x"), "unknown x");
        assert!(!hit);
        assert!(suggestions.is_empty());
    }

    #[test]
    fn resolve_at_word_index_zero_no_prefix_falls_back_to_all_entries() {
        let cache = CompletionTreeCache::new("fish");
        cache.insert(
            "supabase",
            vec![CachedEntry {
                text: "backups".to_string(),
                description: None,
            }],
        );
        // "xyz" is not a prefix of any cached segment, but the all-entries
        // fallback still returns candidates so frizbee can filter them.
        // cache_hit is false so async providers still fire as backfill.
        let (suggestions, hit) = cache.resolve(&ctx("xyz"), "xyz");
        assert!(!hit, "fallback must not claim a cache hit");
        assert!(
            suggestions.iter().any(|s| s.text == "supabase backups"),
            "all-entries fallback must include cached rows: {suggestions:?}"
        );
    }

    #[test]
    fn resolve_at_word_index_zero_fuzzy_subsequence_reaches_frizbee() {
        let cache = CompletionTreeCache::new("zsh");
        cache.insert(
            "supabase",
            vec![CachedEntry {
                text: "backups".to_string(),
                description: None,
            }],
        );
        // "supababack" is NOT a prefix of "supabase", but it IS a fuzzy
        // subsequence of "supabase backups". The cache must return the
        // candidate so rerank_live/frizbee can match it.
        let (suggestions, hit) = cache.resolve(&ctx("supababack"), "supababack");
        assert!(!hit);
        assert!(
            suggestions.iter().any(|s| s.text == "supabase backups"),
            "fuzzy subsequence input must reach frizbee: {suggestions:?}"
        );
    }

    #[test]
    fn resolve_at_word_index_zero_substring_reaches_frizbee() {
        let cache = CompletionTreeCache::new("zsh");
        cache.insert(
            "supabase",
            vec![CachedEntry {
                text: "backups".to_string(),
                description: None,
            }],
        );
        // "base" is NOT a prefix of "supabase", but it IS a contiguous
        // substring of "supabase backups". The cache must return the
        // candidate so rerank_live/frizbee can match it in substring mode.
        let (suggestions, hit) = cache.resolve(&ctx("base"), "base");
        assert!(!hit);
        assert!(
            suggestions.iter().any(|s| s.text == "supabase backups"),
            "substring input must reach frizbee: {suggestions:?}"
        );
    }

    #[test]
    fn resolve_at_word_index_zero_prefix_match() {
        let cache = CompletionTreeCache::new("zsh");
        cache.insert(
            "supabase",
            vec![
                CachedEntry {
                    text: "backups".to_string(),
                    description: None,
                },
                CachedEntry {
                    text: "branches".to_string(),
                    description: None,
                },
            ],
        );
        // "supa" is a prefix of "supabase" → subcommands served from cache.
        let c = ctx("supa");
        assert_eq!(c.word_index, 0);
        let (suggestions, hit) = cache.resolve(&c, "supa");
        assert!(hit);
        assert!(suggestions.iter().any(|s| s.text == "supabase backups"));
        assert!(suggestions.iter().any(|s| s.text == "supabase branches"));
    }

    #[test]
    fn resolve_at_word_index_zero_multiple_prefix_matches() {
        let cache = CompletionTreeCache::new("zsh");
        cache.insert(
            "supabase",
            vec![CachedEntry {
                text: "backups".to_string(),
                description: None,
            }],
        );
        cache.insert(
            "superctl",
            vec![CachedEntry {
                text: "status".to_string(),
                description: None,
            }],
        );
        // "sup" prefixes both "supabase" and "superctl" → both served.
        let c = ctx("sup");
        assert_eq!(c.word_index, 0);
        let (suggestions, hit) = cache.resolve(&c, "sup");
        assert!(hit);
        assert!(suggestions.iter().any(|s| s.text == "supabase backups"));
        assert!(suggestions.iter().any(|s| s.text == "superctl status"));
    }

    #[test]
    fn resolve_at_word_index_zero_exact_match() {
        let cache = CompletionTreeCache::new("zsh");
        cache.insert(
            "supabase",
            vec![
                CachedEntry {
                    text: "backups".to_string(),
                    description: None,
                },
                CachedEntry {
                    text: "branches".to_string(),
                    description: None,
                },
            ],
        );
        // Buffer "supabase" (no trailing space) → word_index=0, current_word="supabase"
        let c = ctx("supabase");
        assert_eq!(c.word_index, 0); // precondition
        let (suggestions, hit) = cache.resolve(&c, "supabase");
        assert!(hit);
        assert!(suggestions.iter().any(|s| s.text == "supabase backups"));
        assert!(suggestions.iter().any(|s| s.text == "supabase branches"));
    }

    #[test]
    fn resolve_deeper_path() {
        let cache = CompletionTreeCache::new("fish");
        cache.insert(
            "supabase backups",
            vec![CachedEntry {
                text: "list".to_string(),
                description: None,
            }],
        );
        let (suggestions, hit) = cache.resolve(&ctx("supabase backups l"), "supabase backups l");
        assert!(hit);
        assert_eq!(suggestions[0].text, "supabase backups list");
    }

    #[test]
    fn filesystem_entry_detection() {
        // Pattern-based fast paths.
        assert!(is_filesystem_entry("src/"));
        assert!(is_filesystem_entry("~/Documents"));
        assert!(is_filesystem_entry("./foo"));
        assert!(is_filesystem_entry("../bar"));
        assert!(is_filesystem_entry("/usr/bin"));
        // Path::exists fallback — Cargo.toml exists in the crate root.
        assert!(is_filesystem_entry("Cargo.toml"));
        // Non-filesystem entries.
        assert!(!is_filesystem_entry("--verbose"));
        assert!(!is_filesystem_entry("zzz_not_a_real_file"));
    }

    #[test]
    fn merge_after_filter_excludes_filesystem_entries() {
        let base = vec![
            CompletionLine {
                completion: "src/".to_string(),
                description: None,
            },
            CompletionLine {
                completion: "backups".to_string(),
                description: None,
            },
        ];
        let expanded = vec![CompletionLine {
            completion: "~/docs".to_string(),
            description: None,
        }];
        // Simulate the suggest pipeline: filter then merge.
        let base: Vec<_> = base
            .into_iter()
            .filter(|l| !is_filesystem_entry(&l.completion))
            .collect();
        let expanded: Vec<_> = expanded
            .into_iter()
            .filter(|l| !is_filesystem_entry(&l.completion))
            .collect();
        let suggestions = merge_completions("git ", "git ", &base, &expanded, 50);
        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].text.contains("backups"));
    }

    #[test]
    fn resolve_filters_filesystem_entries_from_cache() {
        let cache = CompletionTreeCache::new("fish");
        cache.insert(
            "git",
            vec![
                CachedEntry {
                    text: "add".to_string(),
                    description: None,
                },
                CachedEntry {
                    text: "src/".to_string(),
                    description: None,
                },
                CachedEntry {
                    text: "~/docs".to_string(),
                    description: None,
                },
            ],
        );
        let (suggestions, hit) = cache.resolve(&ctx("git "), "git ");
        assert!(hit);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text, "git add");
    }

    // -----------------------------------------------------------------------
    // fish_escape / current_word / common_prefix_char_count
    // -----------------------------------------------------------------------

    #[test]
    fn fish_escape_simple() {
        assert_eq!(fish_escape("hello"), "'hello'");
    }

    #[test]
    fn fish_escape_backslash_and_quote() {
        // "it's a \path" → wrapped in single quotes, backslash doubled,
        // embedded single quote escaped.
        assert_eq!(fish_escape("it's a \\path"), "'it\\'s a \\\\path'");
    }

    #[test]
    fn current_word_end_of_buffer() {
        assert_eq!(current_word("git stat", 8), "stat");
    }

    #[test]
    fn current_word_cursor_mid_word() {
        // Cursor at byte 8 of "git status" slices to "git stat".
        assert_eq!(current_word("git status", 8), "stat");
    }

    #[test]
    fn current_word_empty_buffer() {
        assert_eq!(current_word("", 0), "");
    }

    #[test]
    fn current_word_cursor_beyond_len() {
        // Cursor is clamped to the buffer length.
        assert_eq!(current_word("ls", 99), "ls");
    }

    #[test]
    fn common_prefix_identical() {
        assert_eq!(common_prefix_char_count("abc", "abc"), 3);
    }

    #[test]
    fn common_prefix_partial() {
        assert_eq!(common_prefix_char_count("abc", "abd"), 2);
    }

    #[test]
    fn common_prefix_empty() {
        assert_eq!(common_prefix_char_count("", "abc"), 0);
    }

    #[test]
    fn common_prefix_unicode() {
        // Char-based, not byte-based: "café" is 4 chars (5 bytes).
        assert_eq!(common_prefix_char_count("café", "café"), 4);
    }
}
