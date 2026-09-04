//! Frecency-based score boosting for suggestions.
//!
//! Records timestamps of user-accepted suggestion texts and computes a
//! decayed score (fff-style exponential decay with a 10-day half-life).
//! The store boosts frizbee match scores for candidates whose text was
//! previously accepted, nudging familiar items upward within their source
//! groups.
//!
//! Persistence: a single JSON file at `$XDG_STATE_HOME/termcmp/frecency.json`
//! (fallback `~/.local/state/termcmp/frecency.json`). Writes are atomic
//! (tempfile + persist) and synchronous — acceptances are human-paced.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// Decay constant: ln(2) / 10 ≈ 0.0693. Gives a 10-day half-life.
const DECAY_CONSTANT: f64 = 0.0693;
/// Seconds per day.
const SECONDS_PER_DAY: f64 = 86400.0;
/// Timestamps older than this many days are pruned.
const MAX_HISTORY_DAYS: f64 = 30.0;
/// Maximum timestamps kept per entry (oldest evicted first).
const MAX_TIMESTAMPS_PER_ENTRY: usize = 32;
/// Maximum entries in the store before eviction.
const MAX_ENTRIES: usize = 1000;
/// Multiplier applied to normalized frecency score to produce a frizbee boost.
const BOOST_MULTIPLIER: f64 = 8.0;
/// On-disk format version; mismatch discards the file.
const FRECENCY_FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct FrecencyFile {
    format_version: u32,
    entries: HashMap<String, Vec<u64>>,
}

/// Frecency store: maps accepted suggestion texts to decayed-access timestamps.
pub struct FrecencyStore {
    inner: RwLock<HashMap<String, VecDeque<u64>>>,
    path: Option<PathBuf>,
}

impl FrecencyStore {
    /// Load from the default state-file path. Corrupt/missing/version-mismatch
    /// yields an empty store. Stale entries (all timestamps > 30 days old) are
    /// pruned on load.
    pub fn load() -> Self {
        Self::with_path(frecency_path())
    }

    /// Load from a custom path. Used by tests to avoid environment variable races.
    pub fn with_path(path: Option<PathBuf>) -> Self {
        let inner = match path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            Some(contents) => match serde_json::from_str::<FrecencyFile>(&contents) {
                Ok(file) if file.format_version == FRECENCY_FORMAT_VERSION => {
                    let now = now_unix_secs();
                    let cutoff = cutoff_secs(now);
                    let mut map: HashMap<String, VecDeque<u64>> = HashMap::new();
                    for (text, mut ts) in file.entries {
                        ts.retain(|&t| t >= cutoff);
                        if !ts.is_empty() {
                            ts.sort_unstable();
                            map.insert(text, VecDeque::from(ts));
                        }
                    }
                    map
                }
                _ => HashMap::new(),
            },
            None => HashMap::new(),
        };
        Self {
            inner: RwLock::new(inner),
            path,
        }
    }

    /// In-memory store with no disk persistence. Used by tests and test engines.
    pub fn in_memory() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            path: None,
        }
    }

    /// Record an acceptance of `text` at the current time. Pushes a timestamp,
    /// prunes stale/overflowing timestamps, evicts the lowest-scoring entry if
    /// the map exceeds `MAX_ENTRIES`, and flushes to disk.
    pub fn record(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let now = now_unix_secs();
        let cutoff = cutoff_secs(now);
        {
            let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
            let entry = map.entry(text.to_string()).or_default();
            entry.push_back(now);
            // Prune stale timestamps from the front.
            while entry.front().is_some_and(|&t| t < cutoff) {
                entry.pop_front();
            }
            // Cap timestamps per entry.
            while entry.len() > MAX_TIMESTAMPS_PER_ENTRY {
                entry.pop_front();
            }
            // Evict lowest-scoring entry if over capacity.
            if map.len() > MAX_ENTRIES {
                if let Some(key) = lowest_scoring_key(&map, now) {
                    map.remove(&key);
                }
            }
        }
        self.flush();
    }

    /// Normalized frecency score for `text`. Returns 0 when the text has no
    /// in-window timestamps.
    pub fn score(&self, text: &str) -> u32 {
        let now = now_unix_secs();
        let cutoff = cutoff_secs(now);
        let map = self.inner.read().unwrap_or_else(|e| e.into_inner());
        match map.get(text) {
            Some(timestamps) => {
                let total: f64 = timestamps
                    .iter()
                    .filter(|&&t| t >= cutoff)
                    .map(|&t| {
                        let days_ago = (now.saturating_sub(t)) as f64 / SECONDS_PER_DAY;
                        (-DECAY_CONSTANT * days_ago).exp()
                    })
                    .sum();
                normalize(total)
            }
            None => 0,
        }
    }

    /// Frizbee score boost for `text`. Scaled so a single fresh accept gives
    /// a mild nudge and heavy recent use dominates within a source group.
    pub fn boost(&self, text: &str) -> u32 {
        let s = self.score(text) as f64;
        (s * BOOST_MULTIPLIER).round() as u32
    }

    /// Flush the in-memory state to disk. Best-effort: errors are logged and
    /// ignored.
    fn flush(&self) {
        let Some(path) = &self.path else { return };
        let map = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let entries: HashMap<String, Vec<u64>> = map
            .iter()
            .map(|(k, v)| (k.clone(), v.iter().copied().collect()))
            .collect();
        let file = FrecencyFile {
            format_version: FRECENCY_FORMAT_VERSION,
            entries,
        };
        let json = match serde_json::to_string(&file) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!("frecency serialize error: {e}");
                return;
            }
        };
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        if let Err(e) = std::fs::create_dir_all(&parent) {
            tracing::warn!("frecency dir creation failed: {e}");
            return;
        }
        let mut tmp = match tempfile::NamedTempFile::new_in(&parent) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("frecency tmp create failed: {e}");
                return;
            }
        };
        if let Err(e) = tmp.write_all(json.as_bytes()) {
            tracing::warn!("frecency write failed: {e}");
            return;
        }
        if let Err(e) = tmp.persist(path) {
            tracing::warn!("frecency persist failed: {e}");
        }
    }
}

/// Normalize a raw frecency total into a 0..=u32 score.
fn normalize(total: f64) -> u32 {
    let n = if total <= 10.0 {
        total
    } else {
        10.0 + (total - 10.0).sqrt()
    };
    n.round().min(u32::MAX as f64) as u32
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn cutoff_secs(now: u64) -> u64 {
    let window = (MAX_HISTORY_DAYS * SECONDS_PER_DAY) as u64;
    now.saturating_sub(window)
}

fn frecency_path() -> Option<PathBuf> {
    // XDG_STATE_HOME must be absolute (per the XDG spec); a relative value
    // would resolve to a cwd-relative path and is ignored.
    if let Ok(state) = std::env::var("XDG_STATE_HOME") {
        let p = PathBuf::from(state);
        if p.is_absolute() && !p.as_os_str().is_empty() {
            return Some(p.join("termcmp").join("frecency.json"));
        }
    }
    let home = dirs::home_dir()?;
    Some(home.join(".local/state/termcmp/frecency.json"))
}

/// Find the key with the lowest raw frecency total (for eviction when the
/// store exceeds `MAX_ENTRIES`).
fn lowest_scoring_key(map: &HashMap<String, VecDeque<u64>>, now: u64) -> Option<String> {
    let cutoff = cutoff_secs(now);
    map.iter()
        .min_by(|a, b| {
            let sa: f64 =
                a.1.iter()
                    .filter(|&&t| t >= cutoff)
                    .map(|&t| {
                        let days_ago = (now.saturating_sub(t)) as f64 / SECONDS_PER_DAY;
                        (-DECAY_CONSTANT * days_ago).exp()
                    })
                    .sum();
            let sb: f64 =
                b.1.iter()
                    .filter(|&&t| t >= cutoff)
                    .map(|&t| {
                        let days_ago = (now.saturating_sub(t)) as f64 / SECONDS_PER_DAY;
                        (-DECAY_CONSTANT * days_ago).exp()
                    })
                    .sum();
            sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(k, _)| k.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_beats_old() {
        let store = FrecencyStore::in_memory();
        let now = now_unix_secs();
        {
            let mut map = store.inner.write().unwrap();
            // "recent" accepted 1 day ago, "old" accepted 25 days ago.
            let recent_ts = now - 86400;
            let old_ts = now - 25 * 86400;
            map.insert("recent".to_string(), VecDeque::from([recent_ts]));
            map.insert("old".to_string(), VecDeque::from([old_ts]));
        }
        assert!(
            store.score("recent") > store.score("old"),
            "recent acceptance must score higher than old"
        );
    }

    #[test]
    fn normalization_cap() {
        // A huge raw total should still produce a finite u32.
        let n = normalize(10000.0);
        assert!(n > 10, "normalization should produce > 10 for large input");
        assert!(n < 10000, "normalization should compress large input");
    }

    #[test]
    fn per_entry_timestamp_cap() {
        let store = FrecencyStore::in_memory();
        let now = now_unix_secs();
        // Insert more than MAX_TIMESTAMPS_PER_ENTRY timestamps.
        for i in 0..(MAX_TIMESTAMPS_PER_ENTRY + 10) {
            let ts = now - (i as u64) * 100;
            let mut map = store.inner.write().unwrap();
            let entry = map.entry("x".to_string()).or_default();
            entry.push_back(ts);
            while entry.len() > MAX_TIMESTAMPS_PER_ENTRY {
                entry.pop_front();
            }
        }
        let map = store.inner.read().unwrap();
        assert_eq!(
            map.get("x").unwrap().len(),
            MAX_TIMESTAMPS_PER_ENTRY,
            "timestamps must be capped per entry"
        );
    }

    #[test]
    fn max_entries_eviction() {
        let store = FrecencyStore::in_memory();
        let now = now_unix_secs();
        // Fill beyond MAX_ENTRIES.
        for i in 0..(MAX_ENTRIES + 5) {
            let mut map = store.inner.write().unwrap();
            let key = format!("item{i}");
            map.insert(key, VecDeque::from([now]));
            if map.len() > MAX_ENTRIES {
                if let Some(evict_key) = lowest_scoring_key(&map, now) {
                    map.remove(&evict_key);
                }
            }
        }
        let map = store.inner.read().unwrap();
        assert!(
            map.len() <= MAX_ENTRIES,
            "store must not exceed MAX_ENTRIES"
        );
    }

    #[test]
    fn load_prunes_stale() {
        let dir = tempfile::tempdir().unwrap();
        let frecency_dir = dir.path().join("termcmp");
        std::fs::create_dir_all(&frecency_dir).unwrap();
        let path = frecency_dir.join("frecency.json");
        let now = now_unix_secs();
        let stale_ts = now - 31 * 86400; // 31 days ago, beyond window
        let fresh_ts = now - 86400; // 1 day ago
        let file = FrecencyFile {
            format_version: FRECENCY_FORMAT_VERSION,
            entries: HashMap::from([
                ("stale".to_string(), vec![stale_ts]),
                ("fresh".to_string(), vec![fresh_ts]),
            ]),
        };
        std::fs::write(&path, serde_json::to_string(&file).unwrap()).unwrap();

        // Use with_path to avoid environment variable races in parallel tests
        let store = FrecencyStore::with_path(Some(path));
        // "stale" should have been pruned entirely.
        assert_eq!(store.score("stale"), 0, "stale entry should be pruned");
        assert!(store.score("fresh") > 0, "fresh entry should survive");
    }

    #[test]
    fn roundtrip_via_tempdir() {
        let dir = tempfile::tempdir().unwrap();
        let frecency_dir = dir.path().join("termcmp");
        std::fs::create_dir_all(&frecency_dir).unwrap();
        let path = frecency_dir.join("frecency.json");

        // Use with_path to avoid environment variable races in parallel tests
        let store = FrecencyStore::with_path(Some(path.clone()));
        store.record("git status");
        store.record("git status");
        assert!(store.score("git status") > 0);
        // Reload from disk.
        let store2 = FrecencyStore::with_path(Some(path));
        assert!(
            store2.score("git status") > 0,
            "persistence roundtrip should preserve scores"
        );
    }
    #[test]
    fn corrupt_file_yields_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let frecency_dir = dir.path().join("termcmp");
        std::fs::create_dir_all(&frecency_dir).unwrap();
        let path = frecency_dir.join("frecency.json");
        std::fs::write(&path, "not json{{{").unwrap();
        // Use with_path to avoid environment variable races in parallel tests
        let store = FrecencyStore::with_path(Some(path));
        assert_eq!(store.score("anything"), 0);
    }

    #[test]
    fn wrong_version_yields_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let frecency_dir = dir.path().join("termcmp");
        std::fs::create_dir_all(&frecency_dir).unwrap();
        let path = frecency_dir.join("frecency.json");
        let file = serde_json::json!({
            "format_version": 999,
            "entries": { "git": [1, 2, 3] }
        });
        std::fs::write(&path, file.to_string()).unwrap();
        // Use with_path to avoid environment variable races in parallel tests
        let store = FrecencyStore::with_path(Some(path));
        assert_eq!(store.score("git"), 0);
    }
    #[test]
    fn empty_text_not_recorded() {
        let store = FrecencyStore::in_memory();
        store.record("");
        assert_eq!(store.score(""), 0);
    }
}
