//! Shell-native completion providers (fish, zsh, bash).
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
//!
//! # Bash
//! Sources the system bash-completion, resolves the command's compspec via
//! `complete -p`, and invokes the `-F` function (or prints the `-W` wordlist)
//! with the `COMP_*` variables set. Runs on plain pipes — bash completion
//! needs no tty.

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

/// Env vars that trigger terminal-emulator detection in shell init scripts.
/// Removed from the completion probe environment so a sourced
/// bash-completion script (or an init hook) cannot `exec` into a proxy.
const COMPLETION_ENV_REMOVE: &[&str] = &[
    "TERM_PROGRAM",
    "GHOSTTY_RESOURCES_DIR",
    "KITTY_WINDOW_ID",
    "WEZTERM_UNIX_SOCKET",
    "ALACRITTY_SOCKET",
    "ZED_TERM",
    "VSCODE_IPC_HOOK_CLI",
    "ITERM_SESSION_ID",
    "TERMCMP_ACTIVE",
];

/// Run a command with piped stdout and a polling deadline. Bash completion
/// needs no controlling terminal, so this skips the PTY machinery entirely.
/// Returns stdout on success, `None` on spawn failure, non-zero exit, or
/// timeout.
fn run_piped_with_timeout(cmd: &str, args: &[&str], timeout: Duration) -> Option<String> {
    tracing::debug!("spawning {cmd} {}", args.join(" "));
    let mut command = std::process::Command::new(cmd);
    command
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for var in COMPLETION_ENV_REMOVE {
        command.env_remove(var);
    }
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("failed to spawn {cmd}: {e}");
            return None;
        }
    };

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    tracing::debug!("{cmd} timed out, killing");
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                tracing::debug!("{cmd} wait error: {e}");
                break None;
            }
        }
    };

    match status {
        Some(s) if s.success() => {
            let mut stdout = child.stdout.take()?;
            let mut text = String::new();
            stdout.read_to_string(&mut text).ok()?;
            Some(text)
        }
        Some(s) => {
            tracing::debug!("{cmd} exited with {s}");
            None
        }
        None => None,
    }
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
// Bash provider
// ---------------------------------------------------------------------------

/// Bash completion query script. Invoked as
/// `bash --norc --noprofile -c <script> -- <buffer> <point-bytes>`.
///
/// Sources the system bash-completion when present, reconstructs
/// `COMP_WORDS`/`COMP_CWORD` from the buffer, resolves the command's compspec
/// via `complete -p`, then either calls the `-F` completion function with the
/// `COMP_*` convention or prints the `-W` wordlist. No compspec → exit 0 →
/// empty result (the sync Commands/Filesystem lanes still serve the popup).
const BASH_COMPLETION_SCRIPT: &str = r#"
# Load the system bash-completion if present (distro paths vary).
for f in /usr/share/bash-completion/bash_completion /etc/bash_completion /opt/homebrew/etc/profile.d/bash_completion.sh; do
  [[ -r "$f" ]] && source "$f" 2>/dev/null && break
done
COMP_LINE="$1"; COMP_POINT="$2"
read -ra COMP_WORDS <<< "$COMP_LINE"
if [[ "$COMP_LINE" =~ [[:space:]]$ || -z "$COMP_LINE" ]]; then
  COMP_CWORD=${#COMP_WORDS[@]}
else
  COMP_CWORD=$(( ${#COMP_WORDS[@]} - 1 ))
fi
cmd="${COMP_WORDS[0]:-}"
[[ -n "$cmd" ]] || exit 0
spec="$(complete -p "$cmd" 2>/dev/null)" || exit 0
# Extract the -F function name from the compspec. If the compspec has
# no -F (e.g. -W wordlist only), fall back to printing the -W words.
if [[ "$spec" =~ -F[[:space:]]+([^[:space:]]+) ]]; then
  fn="${BASH_REMATCH[1]}"
  # prev-word convention: at CWORD 0 the prev is the command itself;
  # COMP_WORDS[CWORD-1] would negative-index the last element otherwise.
  prev="$cmd"
  (( COMP_CWORD > 0 )) && prev="${COMP_WORDS[COMP_CWORD-1]}"
  COMPREPLY=()
  "$fn" "$cmd" "${COMP_WORDS[COMP_CWORD]:-}" "$prev" 2>/dev/null
  printf '%s\n' "${COMPREPLY[@]}"
elif [[ "$spec" =~ -W[[:space:]]+\'([^\']*)\' ]]; then
  printf '%s\n' ${BASH_REMATCH[1]}
fi
"#;

/// Run the bash completion query script and return stdout. `point` is a byte
/// offset into `buffer` (bash's `COMP_POINT` unit).
fn bash_query(buffer: &str, point: usize) -> Option<String> {
    let point_str = point.to_string();
    run_piped_with_timeout(
        "bash",
        &[
            "--norc",
            "--noprofile",
            "-c",
            BASH_COMPLETION_SCRIPT,
            "--",
            buffer,
            &point_str,
        ],
        Duration::from_secs(5),
    )
}

/// Async provider that queries bash's completion engine via the command's
/// registered compspec.
pub struct BashCompletionProvider {
    max_results: usize,
}

impl BashCompletionProvider {
    pub fn new(max_results: usize) -> Self {
        Self { max_results }
    }
}

impl AsyncProvider for BashCompletionProvider {
    fn name(&self) -> &'static str {
        "bash"
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
            let point = buffer::char_to_byte_offset(&buffer, cursor);
            let base_stdout = bash_query(&buffer, point);
            let base = parse_bash_stdout(base_stdout.as_deref());

            // Eager expansion: re-query with a trailing space to surface the
            // next level of completions (subcommands, arguments). At command
            // position (word_index 0), only expand when the base query found
            // exactly one exact match — the command exists and is complete.
            // An empty base means the command is unknown; expanding would
            // trigger the shell's _files fallback and dump home-dir listings.
            let expanded_stdout = if ctx.word_index == 0 {
                if base.len() == 1 && base[0].completion == word {
                    let expanded = format!("{} ", buffer);
                    bash_query(&expanded, expanded.len())
                } else {
                    None
                }
            } else if word_is_complete(&base, &word) {
                let expanded = format!("{} ", buffer);
                bash_query(&expanded, expanded.len())
            } else {
                None
            };

            let expanded = parse_bash_stdout(expanded_stdout.as_deref());
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

/// Parse bash completion stdout into completion lines. The query script
/// prints one completion per line with no descriptions.
fn parse_bash_stdout(stdout: Option<&str>) -> Vec<CompletionLine> {
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

# Poll zpty output until a marker appears or a 2s deadline expires.
# Replaces the old fixed sleeps: typical compinit is ~200-400ms, not 2s.
tc_wait_marker() {
  local marker="$1" deadline=$(( SECONDS + 2 )) acc=""
  while (( SECONDS < deadline )); do
    acc+="$(zpty -r -t ztc 2>/dev/null)"
    [[ "$acc" == *"$marker"* ]] && return 0
    sleep 0.05
  done
  return 1
}

# Drain zpty output until it goes quiet. `zpty -r -t` only reads when
# output is pending; without draining, echoed characters arrive in
# fragments and a typed trigger key can interleave with the echo of a
# previous write (the widget binding then never fires).
tc_drain() {
  local idle=0
  while (( idle < 3 )); do
    if [[ -n "$(zpty -r -t ztc 2>/dev/null)" ]]; then idle=0; else idle+=1; fi
    sleep 0.02
  done
}

# Drain until quiet OR a deadline expires. Used for the initial interactive
# prompt, whose text is user-defined (custom PS1/oh-my-zsh) so no fixed marker
# is reliable; quiescence works for any prompt and the bound keeps a noisy rc
# from hanging the probe.
tc_drain_bounded() {
  local deadline=$(( SECONDS + 2 )) idle=0
  while (( SECONDS < deadline )); do
    if [[ -n "$(zpty -r -t ztc 2>/dev/null)" ]]; then idle=0; else idle+=1; fi
    (( idle >= 3 )) && return 0
    sleep 0.02
  done
  return 1
}

# Spawn an interactive zsh in a pty (gives us a real ZLE context). Keep the
# user's rc (-i, NOT -f): fpath+=/compdef completions installed by brew
# zsh-completions, oh-my-zsh, kubectl/docker etc. must stay visible to compsys.
zpty ztc zsh -i 2>/dev/null || exit 1

# Wait for the interactive prompt to settle. The prompt text is user-defined,
# so drain to quiescence (bounded 2s) rather than match a fixed marker.
tc_drain_bounded

# Load compinit ONLY if compsys isn't already up. The user's .zshrc normally
# runs compinit + compdef (brew zsh-completions, oh-my-zsh, kubectl/docker);
# re-running compinit here clears those _comps registrations. When compsys is
# already initialized keep it; otherwise initialize (skip the security check
# + dump rebuild on the hot path when a dump exists).
zpty -w ztc 'autoload -Uz compinit; if (( ! $+_comps )); then if [[ -f ~/.zcompdump ]]; then compinit -u -C; else compinit -u; fi; fi 2>/dev/null; echo TC_COMPINIT_OK'
tc_wait_marker TC_COMPINIT_OK

# Define the capture widget: overrides compadd to collect matches, runs
# _main_complete via a completion widget, writes results to a file.
zpty -w ztc 'function tc-capture { typeset -ga _tc_comps=(); function compadd { local -a m; builtin compadd -O m "$@" 2>/dev/null; _tc_comps+=("${m[@]}") }; zle -C tc-cap complete-word _main_complete; zle tc-cap 2>/dev/null; print -l "${_tc_comps[@]}" > '"$_tc_outfile"'; zle kill-whole-line }; zle -N tc-capture; bindkey "^Xx" tc-capture; echo TC_SETUP_OK'
tc_wait_marker TC_SETUP_OK

# Clear the line and type the target buffer (no newline — goes into ZLE buffer).
tc_drain
zpty -w -n ztc $'\x15'
tc_drain

zpty -w -n ztc "$_tc_buffer"
tc_drain

# Trigger the capture widget with Ctrl-X x, then poll for the outfile the
# widget writes. Poll for EXISTENCE (-e), not size (-s): the widget always
# creates the file via `print -l … > file`, even for zero matches, so -s
# would burn the full 2s deadline on every empty result.
zpty -w -n ztc $'\x18'
zpty -w -n ztc 'x'
typeset -i _tc_deadline=$(( SECONDS + 2 ))
while (( SECONDS < _tc_deadline )) && [[ ! -e "$_tc_outfile" ]]; do sleep 0.05; done
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
/// fish, zsh, and bash providers.
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
    // Bash provider: stdout parsing + merge
    // -----------------------------------------------------------------------

    #[test]
    fn bash_stdout_one_completion_per_line() {
        // Real bash completion output: one match per line, blanks skipped,
        // surrounding whitespace trimmed, no descriptions.
        let lines = parse_bash_stdout(Some("backups\ndb\n\n  storage  \n"));
        let texts: Vec<&str> = lines.iter().map(|l| l.completion.as_str()).collect();
        assert_eq!(texts, vec!["backups", "db", "storage"]);
        assert!(lines.iter().all(|l| l.description.is_none()));
    }

    #[test]
    fn bash_stdout_none_is_empty() {
        // No compspec / spawn failure / timeout all surface as None.
        assert!(parse_bash_stdout(None).is_empty());
    }

    #[test]
    fn bash_merge_expands_into_full_text() {
        // `supabase` (exact self-match) then the trailing-space query
        // yields subcommands whose full_text carries the completed word.
        let base = vec![line("supabase")];
        let expanded = vec![line("backups"), line("db")];
        let out = merge_completions("supabase", "supabase ", &base, &expanded, 10);
        let texts: Vec<&str> = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["supabase", "supabase backups", "supabase db"]);
    }
}
