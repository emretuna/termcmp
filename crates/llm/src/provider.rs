use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Result;
use regex::Regex;
use suggest::{
    common_prefix_char_count, AsyncProvider, SuggestRequest, Suggestion, SuggestionKind,
    SuggestionSource,
};

use crate::openai::{self, ApiFormat, Thinking};

/// Built-in system prompt template. `{max_results}` is substituted at runtime.
const DEFAULT_SYSTEM_PROMPT_TEMPLATE: &str = "\
You are the AI completion engine for termcmp, a terminal command autocompletion tool.

Your task is to predict the next command or continuation the user is most likely trying to execute.

You are given some or all of the following context:

* The user's current input.
* The current working directory (cwd).
* The operating system.
* The current shell.
* The directory contents.
* Previously executed commands.
* Git repository status (if available).

Rules:

1. Return exactly {max_results} completion candidates.
2. Each candidate must be a single terminal command or command continuation.
3. Do not include explanations, numbering, markdown, quotes, comments, or code fences.
4. Do not invent files, directories, branches, or commands that are not supported by the provided context.
5. Prefer existing files, directories, executables, Git branches, remotes, Docker containers, etc. from the supplied context.
6. Preserve the user's typing style and shell syntax.
7. Complete the user's current command instead of replacing it whenever appropriate.
8. Rank suggestions from most likely to least likely.
9. If multiple valid continuations exist, prioritize:
    * existing filesystem paths
    * recent commands
    * common command patterns
    * project-specific conventions
10. Never output placeholders such as <file>, <path>, or <branch>.
11. Never ask questions or provide conversational responses.
12. If there is insufficient context, return the most probable valid completions based on the partial command and current directory.

Output format:

One completion per line.

Example:

git commit -m \"Initial commit\"
git commit --amend
git commit --amend --no-edit
git status
git push";

/// Build the default system prompt with the configured `max_results` count.
pub fn default_system_prompt(max_results: usize) -> String {
    DEFAULT_SYSTEM_PROMPT_TEMPLATE.replace("{max_results}", &max_results.to_string())
}

/// Redact secrets from text before sending to LLM providers.
///
/// Strips:
/// - Bearer tokens: `Bearer <token>` → `Bearer <redacted>`
/// - AWS access keys: `AKIA[0-9A-Z]{16}` → `<redacted>`
/// - Generic secret prefixes: `sk-`, `ghp_`, `glpat-`, `xox[baprs]-` → `<redacted>`
/// - Env var assignments: `*_KEY`, `*_TOKEN`, `*_SECRET`, `*_PASSWORD`, `API_*`, `AUTH_*` → `VAR=<redacted>`
pub fn redact_secrets(text: &str) -> String {
    static BEARER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"Bearer\s+[^\s]+").unwrap());
    static AWS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"AKIA[0-9A-Z]{16}").unwrap());
    static GENERIC_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:sk-|ghp_|glpat-|xox[baprs]-)[^\s]+").unwrap());
    static ENV_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)\b([A-Za-z0-9_]*(?:_KEY|_TOKEN|_SECRET|_PASSWORD)|API_[A-Za-z0-9_]*|AUTH_[A-Za-z0-9_]*)\s*=\s*[^\s]+",
        )
        .unwrap()
    });

    let s = BEARER_RE.replace_all(text, "Bearer <redacted>");
    let s = AWS_RE.replace_all(&s, "<redacted>");
    let s = GENERIC_RE.replace_all(&s, "<redacted>");
    let s = ENV_RE.replace_all(&s, "$1=<redacted>");
    s.into_owned()
}

/// Fixed, non-editable system prompt for the on-demand "Ask AI" feature.
/// The response is injected into the terminal prompt area, so the model must
/// emit ONLY a runnable shell command — no prose, markdown, or code fences.
/// The user-editable `prompt.md` file does NOT affect this prompt — Ask AI
/// always uses this fixed text.
const ASK_AI_BASE_PROMPT: &str = "You are a shell command assistant. The user \
asks a question or describes a task; you answer with the shell command to run. \
Respond with ONLY the command line itself — no explanation, no comments, no \
markdown, no code fences, no leading '$' or prompt, no trailing newline. If \
multiple commands are needed, join them on one line with '&&'. Assume the user \
reviews and runs the command themselves.";

pub struct LlmProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    api_format: ApiFormat,
    model: String,
    provider_name: String,
    max_results: usize,
    max_tokens: u32,
    thinking_budget: Option<u32>,
    thinking: Thinking,
    system_prompt: String,
    extra_body: Option<serde_json::Value>,
    history: suggest::history::HistoryProvider,
    redact_secrets: bool,
}

/// Install the process-wide rustls crypto provider that reqwest needs.
///
/// reqwest is built with its `rustls-no-provider` feature to keep the release
/// binary small: the default `rustls` feature links aws-lc-rs, which adds
/// ~1.4 MB and would bust the CI binary-size gate. reqwest panics at client
/// construction when no provider is installed, so this must run before every
/// `Client` build. Idempotent — the first successful install wins and later
/// calls observe it via `get_default` or lose the install race harmlessly.
pub(crate) fn install_rustls_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_some() {
        return;
    }
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        // Another thread installed a provider between the check and here.
    }
}

impl LlmProvider {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: String,
        api_key: String,
        api_format: ApiFormat,
        model: String,
        provider_name: String,
        timeout: Duration,
        max_results: usize,
        max_tokens: u32,
        thinking_budget: Option<u32>,
        thinking: Thinking,
        system_prompt: String,
        extra_body: Option<serde_json::Value>,
        redact_secrets: bool,
    ) -> Result<Self> {
        // Resolve api_key: env-var name first, then literal token.
        let resolved_key = std::env::var(&api_key).unwrap_or(api_key);
        install_rustls_crypto_provider();
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build HTTP client: {e}"))?;
        // Cap history loaded for prompt context; the provider caches and
        // refreshes from disk on change, so this is read once per provider.
        let history = suggest::history::HistoryProvider::load(500, suggest::ShellFamily::Zsh);
        Ok(Self {
            client,
            base_url,
            api_key: resolved_key,
            api_format,
            model,
            provider_name,
            max_results,
            max_tokens,
            thinking_budget,
            thinking,
            system_prompt,
            extra_body,
            history,
            redact_secrets,
        })
    }
}

impl AsyncProvider for LlmProvider {
    fn name(&self) -> &'static str {
        "llm"
    }

    fn suggest<'a>(
        &'a self,
        req: &'a SuggestRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Suggestion>>> + Send + 'a>> {
        Box::pin(async move {
            let user_prompt = build_user_prompt(req, &self.history, self.redact_secrets);
            let system = self.system_prompt.as_str();
            tracing::debug!(
                "llm: suggest via {} model={} buffer={:?} cursor={}",
                self.provider_name,
                self.model,
                req.buffer,
                req.cursor
            );

            let raw_texts: Vec<String> = match self.api_format {
                ApiFormat::OpenAiChat => {
                    match openai::chat_completion(
                        &self.client,
                        &self.base_url,
                        &self.api_key,
                        &self.model,
                        system,
                        &user_prompt,
                        self.max_tokens,
                        self.thinking_budget,
                        self.thinking,
                        self.extra_body.as_ref(),
                    )
                    .await
                    {
                        Ok(texts) => texts,
                        Err(e) => {
                            tracing::warn!("llm chat/completions failed: {e:?}");
                            Vec::new()
                        }
                    }
                }
                ApiFormat::OpenAiResponses => {
                    match openai::responses_completion(
                        &self.client,
                        &self.base_url,
                        &self.api_key,
                        &self.model,
                        system,
                        &user_prompt,
                        self.max_tokens,
                        self.thinking,
                        self.extra_body.as_ref(),
                    )
                    .await
                    {
                        Ok(texts) => texts,
                        Err(e) => {
                            tracing::warn!("llm responses failed: {e:?}");
                            Vec::new()
                        }
                    }
                }
            };

            let desc = format!("{} ({})", self.provider_name, self.model);
            let suggestions = process_raw_texts(raw_texts, req.buffer, self.max_results, &desc);
            tracing::debug!("llm: {} suggestion(s) after dedup", suggestions.len());
            Ok(suggestions)
        })
    }
}

/// Build the user prompt from the suggestion request context. Delivers the
/// context the system prompt promises: current input, cwd, OS, shell,
/// directory contents, recent command history, and git status when available.
fn build_user_prompt(
    req: &SuggestRequest<'_>,
    history: &suggest::history::HistoryProvider,
    redact: bool,
) -> String {
    let buffer = if redact {
        redact_secrets(req.buffer)
    } else {
        req.buffer.to_string()
    };
    let mut prompt = format!(
        "Command: {}\nArgs: {}\nCurrent word: {}\nWord index: {}\nIs flag: {}\nIn redirect: {}\nIn pipe: {}\nCWD: {}\nOS: {}\nShell: {}\nBuffer: {}\nCursor: {}",
        req.ctx.command.as_deref().unwrap_or("(none)"),
        if req.ctx.args.is_empty() { "(none)".to_string() } else { req.ctx.args.join(" ") },
        if req.ctx.current_word.is_empty() { "(empty)".to_string() } else { req.ctx.current_word.clone() },
        req.ctx.word_index,
        req.ctx.is_flag,
        req.ctx.in_redirect,
        req.ctx.in_pipe,
        req.cwd.display(),
        std::env::consts::OS,
        shell_name(),
        buffer,
        req.cursor,
    );

    // Append a directory listing (up to 40 entries, sorted alphabetically).
    if let Ok(entries) = std::fs::read_dir(req.cwd) {
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect();
        names.sort();
        names.truncate(40);
        if !names.is_empty() {
            prompt.push_str("\n\nFiles in CWD:\n");
            prompt.push_str(&names.join("\n"));
        }
    }

    // Recent command history (most recent first), capped to keep tokens down.
    let mut recent: Vec<String> = history.recent_entries();
    if redact {
        recent = recent.into_iter().map(|e| redact_secrets(&e)).collect();
    }
    if !recent.is_empty() {
        recent.reverse();
        recent.truncate(15);
        prompt.push_str("\n\nRecent commands:\n");
        prompt.push_str(&recent.join("\n"));
    }

    // Git repository status, when cwd is inside a work tree.
    if let Some(git) = git_status(req.cwd) {
        let git = if redact { redact_secrets(&git) } else { git };
        prompt.push_str("\n\nGit status:\n");
        prompt.push_str(&git);
    }

    prompt
}

/// Basename of the user's shell from `$SHELL` (e.g. "zsh"), or "(unknown)".
fn shell_name() -> String {
    std::env::var("SHELL")
        .ok()
        .and_then(|s| {
            std::path::Path::new(&s)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "(unknown)".to_string())
}

/// Compact git status for the prompt: current branch plus porcelain change
/// lines. Returns `None` when cwd is not inside a git work tree or git is
/// unavailable. Output is capped so a dirty tree can't blow up the prompt.
fn git_status(cwd: &std::path::Path) -> Option<String> {
    use std::process::Command;
    let inside = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    if String::from_utf8_lossy(&inside.stdout).trim() != "true" {
        return None;
    }

    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "(unknown)".to_string());

    let mut lines = vec![format!("Branch: {branch}")];
    if let Some(o) = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
    {
        let mut changes: Vec<String> = String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.is_empty())
            .take(20)
            .map(str::to_string)
            .collect();
        if !changes.is_empty() {
            lines.push("Changes:".to_string());
            lines.append(&mut changes);
        } else {
            lines.push("Changes: (clean)".to_string());
        }
    }

    Some(lines.join("\n"))
}

/// Convert raw LLM response lines into deduplicated suggestions.
fn process_raw_texts(
    raw_texts: Vec<String>,
    buffer: &str,
    max_results: usize,
    desc: &str,
) -> Vec<Suggestion> {
    let trimmed_buffer = buffer.trim_end();
    // The word currently being typed (text after the last whitespace). The
    // LLM may return a completion of just this word rather than the full
    // command — e.g. buffer "cargo b", response "build". Appending that
    // verbatim to the buffer would double the partial word ("cargo bbuild"),
    // so it must replace the current word instead.
    let current_word = trimmed_buffer
        .rsplit_once(|c: char| c.is_whitespace())
        .map(|(_, w)| w)
        .unwrap_or(trimmed_buffer);
    let mut suggestions = Vec::with_capacity(raw_texts.len());
    for text in raw_texts {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let full_text = if trimmed_buffer.is_empty() {
                line.to_string()
            } else if line.starts_with(trimmed_buffer) {
                // LLM returned the full command including the buffer prefix.
                line.to_string()
            } else if !current_word.is_empty() && line.starts_with(current_word) {
                // LLM returned a completion of the current word (e.g. buffer
                // "cargo b", response "build"). Replace the word with the
                // completion so the partial word isn't doubled on screen.
                let word_start = trimmed_buffer.len() - current_word.len();
                format!("{}{}", &trimmed_buffer[..word_start], line)
            } else {
                // LLM returned only the completion suffix — prepend buffer.
                // Use the original buffer (preserving trailing space) for joining.
                format!("{}{}", buffer, line)
            };
            let full_text = full_text.trim().to_string();
            // Skip empty, identical-to-buffer, or buffer-only results.
            if full_text.is_empty() || full_text == trimmed_buffer {
                continue;
            }
            // Match indices: the common character prefix between the typed
            // buffer and the full suggestion text. This tells the accept path
            // how many characters of the on-screen word the suggestion covers.
            let matched = common_prefix_char_count(trimmed_buffer, &full_text);
            let match_indices: Vec<u32> = (0..matched as u32).collect();
            suggestions.push(Suggestion {
                text: full_text,
                description: Some(desc.to_string()),
                kind: SuggestionKind::Llm,
                source: SuggestionSource::Llm,
                match_indices,
                ..Default::default()
            });
        }
    }
    suggestions.truncate(max_results);
    suggestions
}

/// User message for Ask AI: the question plus shell context (OS, shell,
/// cwd, directory listing, recent history, git status) so the model can
/// tailor its answer to the user's environment.
fn build_ask_ai_user_message(
    question: &str,
    cwd: &std::path::Path,
    history: &suggest::history::HistoryProvider,
    redact: bool,
) -> String {
    let question = if redact {
        redact_secrets(question.trim())
    } else {
        question.trim().to_string()
    };
    let mut msg = format!(
        "Question: {}\nCWD: {}\nOS: {}\nShell: {}",
        question,
        cwd.display(),
        std::env::consts::OS,
        shell_name(),
    );
    if let Ok(entries) = std::fs::read_dir(cwd) {
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    format!("{name}/")
                } else {
                    name
                }
            })
            .collect();
        names.sort();
        names.truncate(40);
        if !names.is_empty() {
            msg.push_str("\nFiles in CWD:\n");
            msg.push_str(&names.join("\n"));
        }
    }
    let mut recent: Vec<String> = history.recent_entries();
    if redact {
        recent = recent.into_iter().map(|e| redact_secrets(&e)).collect();
    }
    if !recent.is_empty() {
        recent.reverse();
        recent.truncate(15);
        msg.push_str("\nRecent commands:\n");
        msg.push_str(&recent.join("\n"));
    }
    if let Some(git) = git_status(cwd) {
        let git = if redact { redact_secrets(&git) } else { git };
        msg.push_str("\nGit status:\n");
        msg.push_str(&git);
    }
    msg
}

impl LlmProvider {
    /// On-demand "Ask AI": send `question` to the LLM and return the cleaned
    /// command text (empty on any failure). Uses ONLY the fixed
    /// `ASK_AI_BASE_PROMPT`; the `system_prompt` field (inline completions,
    /// possibly overridden via `prompt.md`) is deliberately ignored.
    pub async fn ask_ai(&self, question: &str, cwd: &std::path::Path) -> String {
        let system = ASK_AI_BASE_PROMPT;
        let user = build_ask_ai_user_message(question, cwd, &self.history, self.redact_secrets);
        let texts: Vec<String> = match self.api_format {
            ApiFormat::OpenAiChat => {
                match openai::chat_completion(
                    &self.client,
                    &self.base_url,
                    &self.api_key,
                    &self.model,
                    system,
                    &user,
                    self.max_tokens,
                    self.thinking_budget,
                    self.thinking,
                    self.extra_body.as_ref(),
                )
                .await
                {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("ask_ai chat failed: {e:?}");
                        Vec::new()
                    }
                }
            }
            ApiFormat::OpenAiResponses => {
                match openai::responses_completion(
                    &self.client,
                    &self.base_url,
                    &self.api_key,
                    &self.model,
                    system,
                    &user,
                    self.max_tokens,
                    self.thinking,
                    self.extra_body.as_ref(),
                )
                .await
                {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::warn!("ask_ai responses failed: {e:?}");
                        Vec::new()
                    }
                }
            }
        };
        // Join all returned lines, trim, and strip accidental code fences.
        let joined = texts.join("\n");
        clean_command_text(&joined)
    }
}

/// Strip markdown code fences / leading prompt glyphs and trim, so the text
/// is safe to inject verbatim into a shell prompt.
fn clean_command_text(raw: &str) -> String {
    let trimmed = raw.trim();
    let body = trimmed
        .strip_prefix("```")
        .map(|s| s.trim_start_matches(|c: char| c.is_alphanumeric())) // ```bash / ```sh
        .and_then(|s| s.rsplit_once("```").map(|(before, _)| before))
        .unwrap_or(trimmed);
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffer::CommandContext;

    fn make_ctx() -> CommandContext {
        CommandContext {
            command: Some("cp".into()),
            args: vec![],
            current_word: String::new(),
            word_index: 1,
            is_flag: false,
            is_long_flag: false,
            preceding_flag: None,
            in_pipe: false,
            in_redirect: false,
            quote_state: buffer::QuoteState::None,
            is_first_segment: true,
        }
    }

    #[test]
    fn build_user_prompt_includes_context_and_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("foo.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("bar")).unwrap();

        let ctx = make_ctx();
        let req = SuggestRequest {
            ctx: &ctx,
            cwd: dir.path(),
            buffer: "cp ",
            cursor: 3,
        };
        let history = suggest::history::HistoryProvider::from_entries(vec!["ls -la".into()]);
        let prompt = build_user_prompt(&req, &history, false);

        assert!(prompt.contains("Command: cp"), "missing command field");
        assert!(prompt.contains("Word index: 1"), "missing word index");
        assert!(prompt.contains("foo.txt"), "missing file listing");
        assert!(
            prompt.contains("bar/"),
            "missing dir listing with trailing slash"
        );
        assert!(prompt.contains("OS: "), "missing OS field");
        assert!(prompt.contains("Shell: "), "missing shell field");
        assert!(
            prompt.contains("Recent commands:\nls -la"),
            "missing history"
        );
    }

    #[test]
    fn git_status_returns_none_outside_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            git_status(dir.path()).is_none(),
            "tempdir is not a git repo"
        );
    }

    #[test]
    fn dedup_rejects_buffer_echo() {
        let result = process_raw_texts(vec!["cp".into()], "cp", 3, "test");
        assert!(result.is_empty(), "echo of buffer should be rejected");
    }

    #[test]
    fn dedup_accepts_full_command() {
        let result = process_raw_texts(vec!["cp foo.txt".into()], "cp ", 3, "test");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].text.as_str(), "cp foo.txt");
    }

    #[test]
    fn dedup_prepends_buffer_for_suffix() {
        let result = process_raw_texts(vec!["foo.txt".into()], "cp ", 3, "test");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].text.as_str(), "cp foo.txt");
    }

    #[test]
    fn process_raw_texts_completes_current_word_without_doubling() {
        // Regression for "cargo b" -> LLM "build" doubling to "cargo bbuild".
        // The response completes the current word, so it must replace "b",
        // not be appended to the whole buffer.
        let result = process_raw_texts(vec!["build".into()], "cargo b", 3, "test");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].text.as_str(), "cargo build");
        // "cargo b" (7 chars) is the common prefix of the buffer and result.
        assert_eq!(result[0].match_indices, vec![0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn dedup_rejects_buffer_trailing_space_echo() {
        let result = process_raw_texts(vec!["cp".into()], "cp ", 3, "test");
        assert!(
            result.is_empty(),
            "trimmed echo should match trimmed buffer"
        );
    }

    #[test]
    fn process_raw_texts_match_indices_full_prefix() {
        // LLM returns the full command including the buffer prefix.
        let suggestions = process_raw_texts(
            vec!["supabase backups list".to_string()],
            "supabase",
            10,
            "llm",
        );
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text.as_str(), "supabase backups list");
        // "supabase" (8 chars) is the common prefix.
        assert_eq!(suggestions[0].match_indices, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn process_raw_texts_match_indices_suffix_only() {
        // LLM returns only the suffix — buffer (with trailing space) is prepended.
        let suggestions =
            process_raw_texts(vec!["backups list".to_string()], "supabase ", 10, "llm");
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text.as_str(), "supabase backups list");
        // full_text = "supabase " + "backups list" trimmed = "supabase backups list"
        // common prefix of "supabase" (trimmed_buffer) and full_text = 8
        assert_eq!(suggestions[0].match_indices, vec![0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn process_raw_texts_match_indices_empty_buffer() {
        let suggestions = process_raw_texts(vec!["git status".to_string()], "", 10, "llm");
        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].match_indices.is_empty());
    }

    #[test]
    fn process_raw_texts_caps_at_max_results() {
        let raw: Vec<String> = (0..5).map(|i| format!("cmd{i}")).collect();
        let suggestions = process_raw_texts(raw, "", 2, "test");
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].text, "cmd0");
        assert_eq!(suggestions[1].text, "cmd1");
    }

    #[test]
    fn system_prompt_is_used_verbatim() {
        let provider = LlmProvider::new(
            "http://x".into(),
            "k".into(),
            ApiFormat::OpenAiChat,
            "m".into(),
            "p".into(),
            Duration::from_secs(1),
            3,
            256,
            None,
            Thinking::Auto,
            "custom prompt".into(),
            None,
            true,
        )
        .expect("reqwest client builds in tests");
        assert_eq!(provider.system_prompt, "custom prompt");
    }

    #[test]
    fn default_system_prompt_substitutes_max_results() {
        let prompt = default_system_prompt(7);
        assert!(prompt.contains("Return exactly 7 completion candidates."));
        assert!(!prompt.contains("{max_results}"));
    }

    #[test]
    fn ask_ai_base_prompt_is_fixed_and_self_contained() {
        assert!(ASK_AI_BASE_PROMPT.contains("ONLY the command line"));
        assert!(ASK_AI_BASE_PROMPT.contains("no explanation"));
        assert!(ASK_AI_BASE_PROMPT.contains("no markdown"));
        assert!(ASK_AI_BASE_PROMPT.contains("no code fences"));
    }

    #[test]
    fn build_ask_ai_user_message_includes_context() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("foo.txt"), "").unwrap();
        let history = suggest::history::HistoryProvider::from_entries(vec!["ls -la".into()]);
        let msg = build_ask_ai_user_message("how do I list files", dir.path(), &history, false);
        assert!(msg.contains("Question: how do I list files"));
        assert!(msg.contains("foo.txt"));
        assert!(msg.contains("OS: "));
        assert!(msg.contains("Shell: "));
        assert!(msg.contains("Recent commands:\nls -la"));
    }

    #[test]
    fn clean_command_text_strips_fences() {
        assert_eq!(clean_command_text("```bash\nrm -rf x\n```"), "rm -rf x");
        assert_eq!(clean_command_text("  ls -la\n"), "ls -la");
    }
    #[test]
    fn process_raw_texts_multiline_splits_into_separate_suggestions() {
        let result = process_raw_texts(vec!["ls -la\ngit status".into()], "", 10, "test");
        assert_eq!(result.len(), 2, "each line must become its own suggestion");
        assert_eq!(result[0].text.as_str(), "ls -la");
        assert_eq!(result[1].text.as_str(), "git status");
    }

    #[test]
    fn process_raw_texts_skips_empty_lines() {
        let result = process_raw_texts(vec!["\n\ngit status\n\n".into()], "", 10, "test");
        assert_eq!(result.len(), 1, "blank lines must be skipped");
        assert_eq!(result[0].text.as_str(), "git status");
    }

    #[test]
    fn clean_command_text_multiline_joins() {
        assert_eq!(
            clean_command_text("```sh\nls\ngit status\n```"),
            "ls\ngit status"
        );
    }

    #[test]
    fn clean_command_text_empty_input() {
        assert_eq!(clean_command_text(""), "");
    }

    #[test]
    fn clean_command_text_fence_only() {
        assert_eq!(clean_command_text("```bash\n```"), "");
    }

    #[test]
    fn clean_command_text_dollar_prompt_unchanged() {
        // clean_command_text strips fences only; a leading "$" prompt glyph
        // is deliberately NOT removed.
        assert_eq!(clean_command_text("$ ls -la"), "$ ls -la");
    }

    #[test]
    fn git_status_inside_repo_with_changes() {
        // Skip when git is unavailable (e.g. minimal CI environments).
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init"]);
        std::fs::write(dir.path().join("tracked.txt"), "tracked\n").unwrap();
        git(&["add", "tracked.txt"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "init",
        ]);
        std::fs::write(dir.path().join("untracked.txt"), "new\n").unwrap();

        let s = git_status(dir.path()).expect("git_status must detect a repo");
        assert!(s.contains("Branch:"), "missing branch line in:\n{s}");
        assert!(s.contains("Changes:"), "missing changes section in:\n{s}");
        assert!(
            s.contains("untracked.txt"),
            "untracked file must appear in status:\n{s}"
        );
    }

    #[test]
    fn test_redact_secrets_aws_key() {
        let input = "export AWS_ACCESS_KEY_ID=AKIA1234567890ABCDEF";
        let out = redact_secrets(input);
        assert!(
            !out.contains("AKIA1234567890ABCDEF"),
            "AWS key must be redacted, got: {out}"
        );
        assert!(
            out.contains("<redacted>"),
            "redacted marker missing, got: {out}"
        );
        // Also via build_user_prompt with redact=true
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_ctx();
        let req = SuggestRequest {
            ctx: &ctx,
            cwd: dir.path(),
            buffer: input,
            cursor: input.len(),
        };
        let history = suggest::history::HistoryProvider::from_entries(vec![]);
        let prompt = build_user_prompt(&req, &history, true);
        assert!(
            !prompt.contains("AKIA1234567890ABCDEF"),
            "prompt must redact AWS key"
        );
        assert!(
            prompt.contains("<redacted>"),
            "prompt must contain redacted marker"
        );
    }

    #[test]
    fn test_redact_secrets_bearer_token() {
        let input = "curl -H \"Authorization: Bearer ghp_abc123\" https://api.github.com";
        let out = redact_secrets(input);
        assert!(
            !out.contains("ghp_abc123"),
            "Bearer token must be redacted, got: {out}"
        );
        assert!(
            out.contains("Bearer <redacted>"),
            "Bearer redaction incorrect, got: {out}"
        );
        let input2 = "Bearer ghp_abc123";
        let out2 = redact_secrets(input2);
        assert_eq!(
            out2, "Bearer <redacted>",
            "simple Bearer case failed, got: {out2}"
        );
    }

    #[test]
    fn test_redact_secrets_history() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_ctx();
        let req = SuggestRequest {
            ctx: &ctx,
            cwd: dir.path(),
            buffer: "echo hello",
            cursor: 3,
        };
        let history = suggest::history::HistoryProvider::from_entries(vec![
            "ls -la".into(),
            "export API_KEY=supersecret123".into(),
        ]);
        let prompt_redacted = build_user_prompt(&req, &history, true);
        assert!(
            !prompt_redacted.contains("supersecret123"),
            "history must be redacted, got: {prompt_redacted}"
        );
        assert!(
            prompt_redacted.contains("<redacted>"),
            "redacted marker missing in history, got: {prompt_redacted}"
        );
        // Without redaction, secret should appear
        let prompt_clean = build_user_prompt(&req, &history, false);
        assert!(
            prompt_clean.contains("supersecret123"),
            "without redact, history should be unchanged"
        );
        // Direct redact check
        let out = redact_secrets("export API_KEY=supersecret123");
        assert!(!out.contains("supersecret123"));
        assert!(out.contains("<redacted>"));
    }

    #[test]
    fn test_redact_secrets_clean_buffer() {
        let clean = "ls -la /tmp";
        let out = redact_secrets(clean);
        assert_eq!(out, clean, "clean buffer must be unchanged, got: {out}");
        let clean2 = "echo hello world";
        assert_eq!(redact_secrets(clean2), clean2);
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_ctx();
        let req = SuggestRequest {
            ctx: &ctx,
            cwd: dir.path(),
            buffer: clean,
            cursor: clean.len(),
        };
        let history = suggest::history::HistoryProvider::from_entries(vec![]);
        let prompt = build_user_prompt(&req, &history, true);
        assert!(
            prompt.contains(clean),
            "clean buffer must appear unchanged in prompt"
        );
    }
}
