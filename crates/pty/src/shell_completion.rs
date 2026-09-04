//! Shell-native completion providers (fish, zsh).
//!
//! Each provider runs the shell's own completion engine in a PTY subprocess
//! and parses the output into suggestions. These are pure live providers:
//! every query triggers a fresh PTY invocation with no persistent caching.
//!
//! # Fish
//! `complete -C '<buffer>'` prints one completion per line, optionally
//! tab-separated from a description. Fish 4.x requires a controlling terminal
//! for `complete -C` (it calls `tcsetattr`), so we run it inside a PTY.
//!
//! # Zsh
//! A zpty-based capture widget runs `_main_complete` inside a real ZLE
//! context and collects all `compadd` matches.

use std::future::Future;
use std::io::Read;
use std::pin::Pin;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use suggest::{AsyncProvider, SuggestRequest, Suggestion, SuggestionSource};

/// Whether a completion entry is a filesystem path. The built-in
/// `FilesystemProvider` handles filesystem completion; shell providers
/// should not duplicate it. Pattern checks are fast paths.
fn is_filesystem_entry(text: &str) -> bool {
    text.ends_with('/')
        || text.starts_with("~/")
        || text.starts_with("./")
        || text.starts_with("../")
        || text.starts_with('/')
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

/// Run `complete -C '<buffer>'` in fish (inside a PTY) and return stdout.
fn fish_query(buffer: &str) -> Option<String> {
    let script = format!("complete -C {}", fish_escape(buffer));
    run_in_pty("fish", &["-c", &script], Duration::from_secs(5))
}

/// Async provider that queries fish's completion engine (`complete -C`).
pub struct FishCompletionProvider {
    max_results: usize,
}

impl FishCompletionProvider {
    pub fn new(max_results: usize) -> Self {
        Self { max_results }
    }
}

impl AsyncProvider for FishCompletionProvider {
    fn name(&self) -> &'static str {
        "fish"
    }

    fn suggest<'a>(
        &'a self,
        req: &'a SuggestRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<Suggestion>>> + Send + 'a>> {
        let buffer = req.buffer.to_string();
        let word = current_word(&buffer, req.cursor);
        let ctx = req.ctx.clone();
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
            let base_stdout = fish_query(&buffer);
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
                    fish_query(&expanded)
                } else {
                    None
                }
            } else if word_is_complete(&base, &word) {
                let expanded = format!("{} ", buffer);
                fish_query(&expanded)
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
}

impl ZshCompletionProvider {
    pub fn new(max_results: usize) -> Self {
        Self { max_results }
    }
}

impl AsyncProvider for ZshCompletionProvider {
    fn name(&self) -> &'static str {
        "zsh"
    }
    fn suggest<'a>(
        &'a self,
        req: &'a SuggestRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<Suggestion>>> + Send + 'a>> {
        let buffer = req.buffer.to_string();
        let cursor = req.cursor;
        let word = current_word(&buffer, cursor);
        let ctx = req.ctx.clone();
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
            let base_stdout = zsh_query(&buffer, cursor);
            let base = parse_zsh_stdout(base_stdout.as_deref());

            // Eager expansion: same logic as fish — at command position only
            // expand when the base query found exactly one exact match (the
            // command exists). An empty base means the command is unknown;
            // expanding would trigger _files and dump home-dir listings.
            let expanded_stdout = if ctx.word_index == 0 {
                if base.len() == 1 && base[0].completion == word {
                    let expanded = format!("{} ", buffer);
                    zsh_query(&expanded, expanded.len())
                } else {
                    None
                }
            } else if word_is_complete(&base, &word) {
                let expanded = format!("{} ", buffer);
                zsh_query(&expanded, expanded.len())
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

            Ok(suggestions)
        })
    }
}

/// Run the zsh completion capture script and return stdout.
fn zsh_query(buffer: &str, cursor: usize) -> Option<String> {
    let cursor_str = cursor.to_string();
    run_in_pty(
        "zsh",
        &["-c", ZSH_COMPLETION_SCRIPT, "--", buffer, &cursor_str],
        Duration::from_secs(5),
    )
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
}
