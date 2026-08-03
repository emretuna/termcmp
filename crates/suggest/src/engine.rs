use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use buffer::CommandContext;

use crate::alias::{AliasStore, ShellFamily};
use crate::alias_expand::expand_alias_head;
use crate::commands::CommandsProvider;
use crate::env::EnvProvider;
use crate::filesystem::FilesystemProvider;
use crate::fuzzy;
use crate::history::{HistoryProvider, DEFAULT_MAX_HISTORY_ENTRIES};
use crate::priority;
use crate::provider::Provider;
use crate::ssh::SshHostCache;
use crate::types::{SourceOrder, Suggestion, SuggestionKind, SuggestionSource};

/// Acquire the config read lock, recovering the guard from poisoning.
/// `LiveSuggestConfig` is plain data with no invariants a panic could
/// violate mid-mutation, so a poisoned lock still yields a consistent
/// value. Logs on the poison path so the originating panic is diagnosable.
fn config_lock<T>(lock: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| {
        tracing::warn!("suggest config RwLock poisoned on read, recovering: {e}");
        e.into_inner()
    })
}

/// Write-side counterpart of [`config_lock`].
fn config_lock_mut<T>(lock: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| {
        tracing::warn!("suggest config RwLock poisoned on write, recovering: {e}");
        e.into_inner()
    })
}

/// Result from `suggest_sync` — ranked suggestions for the current context.
#[derive(Debug)]
pub struct SyncResult {
    pub suggestions: Vec<Suggestion>,
}

impl SyncResult {
    /// Iterate over the ranked suggestions (convenience for callers and tests).
    pub fn iter(&self) -> std::slice::Iter<'_, Suggestion> {
        self.suggestions.iter()
    }

    /// True when there are ranked suggestions to display.
    pub fn has_suggestions(&self) -> bool {
        !self.suggestions.is_empty()
    }
}

/// Runtime-hot-swappable suggestion settings. Held behind a single
/// `RwLock` on the engine so the config watcher can update all of them
/// atomically; every rank call reads a cheap clone once.
#[derive(Debug, Clone)]
pub struct LiveSuggestConfig {
    pub max_results: usize,
    pub max_history_results: usize,
    pub match_mode: fuzzy::MatchMode,
    pub source_order: SourceOrder,
    pub providers_commands: bool,
    pub providers_filesystem: bool,
}

impl Default for LiveSuggestConfig {
    fn default() -> Self {
        Self {
            max_results: fuzzy::DEFAULT_MAX_RESULTS,
            max_history_results: 5,
            match_mode: fuzzy::MatchMode::Fuzzy,
            source_order: SourceOrder::default(),
            providers_commands: true,
            providers_filesystem: true,
        }
    }
}

pub struct SuggestionEngine {
    filesystem_provider: FilesystemProvider,
    history_provider: HistoryProvider,
    commands_provider: CommandsProvider,
    env_provider: EnvProvider,
    ssh_host_cache: Option<SshHostCache>,
    alias_map: AliasStore,
    /// Suggestion settings hot-swappable at runtime via [`Self::set_config`].
    config: std::sync::RwLock<LiveSuggestConfig>,
}

impl SuggestionEngine {
    pub fn new(shell: ShellFamily) -> Result<Self> {
        Ok(Self {
            filesystem_provider: FilesystemProvider::new(),
            history_provider: HistoryProvider::load(DEFAULT_MAX_HISTORY_ENTRIES),
            commands_provider: CommandsProvider::from_path_env(),
            env_provider: EnvProvider::new(),
            ssh_host_cache: SshHostCache::default_path(),
            alias_map: AliasStore::load_async(shell),
            config: std::sync::RwLock::new(LiveSuggestConfig::default()),
        })
    }

    pub fn with_suggest_config(
        self,
        max_results: usize,
        commands: bool,
        max_history_results: usize,
        filesystem: bool,
    ) -> Self {
        let mut cfg = config_lock_mut(&self.config);
        cfg.max_results = max_results;
        cfg.max_history_results = max_history_results;
        cfg.providers_commands = commands;
        cfg.providers_filesystem = filesystem;
        drop(cfg);
        self
    }

    /// Set the query match strategy (fuzzy subsequence vs contiguous
    /// substring). Hot-swappable at runtime via [`Self::set_config`].
    pub fn with_match_mode(self, mode: fuzzy::MatchMode) -> Self {
        config_lock_mut(&self.config).match_mode = mode;
        self
    }

    /// Set the source-group ordering. Hot-swappable at runtime via
    /// [`Self::set_config`].
    pub fn with_source_order(self, order: SourceOrder) -> Self {
        config_lock_mut(&self.config).source_order = order;
        self
    }

    /// Hot-swap the full suggestion settings. Safe to call at any time —
    /// the next rank call picks up the new values atomically.
    pub fn set_config(&self, cfg: LiveSuggestConfig) {
        *config_lock_mut(&self.config) = cfg;
    }

    /// Snapshot the live suggestion settings. Returns an owned clone so
    /// callers can hold it across a rank without keeping the read lock.
    pub fn config(&self) -> LiveSuggestConfig {
        config_lock(&self.config).clone()
    }

    /// The active query match strategy. Read by the PTY handler so its live
    /// re-rank on keystrokes uses the same mode as the engine's own ranking.
    pub fn match_mode(&self) -> fuzzy::MatchMode {
        config_lock(&self.config).match_mode
    }

    /// The active source-group ordering. Read by the PTY handler so its
    /// live re-rank uses the same order as the engine.
    pub fn source_order(&self) -> SourceOrder {
        config_lock(&self.config).source_order.clone()
    }

    #[doc(hidden)]
    pub fn with_aliases(
        self,
        map: std::collections::HashMap<String, crate::alias::AliasEntry>,
    ) -> Self {
        self.alias_map.install(map);
        self
    }

    #[doc(hidden)]
    pub fn with_ssh_host_cache_path(mut self, path: std::path::PathBuf) -> Self {
        self.ssh_host_cache = Some(SshHostCache::new(path));
        self
    }

    /// Test helper — set the history results cap without reloading from disk.
    #[cfg(test)]
    pub fn with_max_history_results(self, n: usize) -> Self {
        config_lock_mut(&self.config).max_history_results = n;
        self
    }

    /// Test helper — inject a custom SSH config path for deterministic tests.
    #[cfg(test)]
    pub fn with_ssh_config(mut self, path: std::path::PathBuf) -> Self {
        self.ssh_host_cache = Some(SshHostCache::new(path));
        self
    }

    /// Dispatcher for the synchronous suggestion pipeline. Each branch is
    /// handled by a focused helper; this method only picks the right one
    /// based on the cursor context.
    pub fn suggest_sync(
        &self,
        ctx: &CommandContext,
        cwd: &Path,
        buffer: &str,
    ) -> Result<SyncResult> {
        self.suggest_sync_with_env(ctx, cwd, buffer, None)
    }

    pub fn suggest_sync_with_env(
        &self,
        ctx: &CommandContext,
        cwd: &Path,
        buffer: &str,
        shell_env: Option<&HashMap<String, String>>,
    ) -> Result<SyncResult> {
        use crate::context::{classify, ClassifyInput, Context};

        let context = classify(ClassifyInput {
            current_word: &ctx.current_word,
            in_redirect: ctx.in_redirect,
            word_index: ctx.word_index,
        });

        match context {
            Context::CommandPosition => Ok(self.suggest_command_position(ctx, cwd, buffer)),
            Context::Redirect => Ok(self.suggest_redirect(ctx, cwd, buffer)),
            Context::PathPrefix => {
                // PathPrefix is the explicit user-typed escape hatch — only
                // filesystem candidates run. Env-var (`$VAR`) and ssh-host
                // injections are deliberately absent: PathPrefix words start
                // with `./`, `../`, `/`, or `~/` — none of those prefixes can
                // collide with `$VAR` or an SSH host token.
                Ok(self.suggest_filesystem_fallback(ctx, cwd, buffer, Vec::new(), "path"))
            }
            Context::FlagPrefix => Ok(self.suggest_flag_prefix()),
            Context::UnspeccedArg => {
                // No spec — fall back to filesystem + history + situational
                // injections.
                let mut candidates = Vec::new();
                self.extend_with_env_vars(ctx, cwd, shell_env, &mut candidates);
                self.extend_with_ssh_hosts(ctx, &mut candidates);
                Ok(self.suggest_filesystem_fallback(ctx, cwd, buffer, candidates, "fallback"))
            }
        }
    }

    /// Complete the command name (`ctx.word_index == 0`). Pulls candidates
    /// from the `$PATH` commands provider; history is injected by
    /// `rank_with_history`.
    fn suggest_command_position(
        &self,
        ctx: &CommandContext,
        cwd: &Path,
        buffer: &str,
    ) -> SyncResult {
        let cfg = self.config();
        let mut candidates = Vec::new();
        if cfg.providers_commands {
            match self.commands_provider.provide(ctx, cwd) {
                Ok(cmds) => candidates.extend(cmds),
                Err(e) => tracing::warn!("commands provider error: {e}"),
            }
        }
        SyncResult {
            suggestions: self.rank_with_history(ctx, cwd, buffer, candidates, true),
        }
    }

    /// Complete a flag-prefixed token (`-` or `--`). No spec flags are
    /// available, so this returns an empty result set.
    fn suggest_flag_prefix(&self) -> SyncResult {
        SyncResult {
            suggestions: Vec::new(),
        }
    }

    /// Complete after a redirect operator (e.g. `echo foo > <TAB>`). The
    /// shell will write to a file, so only filesystem candidates are
    /// relevant — not commands.
    fn suggest_redirect(&self, ctx: &CommandContext, cwd: &Path, buffer: &str) -> SyncResult {
        let cfg = self.config();
        let mut candidates = Vec::new();
        if cfg.providers_filesystem {
            match self.filesystem_provider.provide(ctx, cwd) {
                Ok(fs) => candidates.extend(fs),
                Err(e) => tracing::warn!("filesystem provider error (redirect): {e}"),
            }
        }
        SyncResult {
            suggestions: self.rank_with_history(ctx, cwd, buffer, candidates, true),
        }
    }

    /// Inject environment variable candidates when `current_word` starts
    /// with `$`. Augments the candidate set without short-circuiting
    /// filesystem resolution.
    fn extend_with_env_vars(
        &self,
        ctx: &CommandContext,
        cwd: &Path,
        shell_env: Option<&HashMap<String, String>>,
        candidates: &mut Vec<Suggestion>,
    ) {
        if !ctx.current_word.starts_with('$') {
            return;
        }
        let provided = match shell_env {
            Some(env) => self.env_provider.provide_from_snapshot(ctx, env),
            None => self.env_provider.provide(ctx, cwd),
        };
        match provided {
            Ok(env_vars) => candidates.extend(env_vars),
            Err(e) => tracing::warn!("env provider error: {e}"),
        }
    }

    /// Inject SSH host candidates when completing an argument to `ssh`
    /// (respecting alias resolution). Skips the command position and flag
    /// words so hosts don't appear for `ssh -p<TAB>` or unrelated commands.
    fn extend_with_ssh_hosts(&self, ctx: &CommandContext, candidates: &mut Vec<Suggestion>) {
        let Some(cache) = self.ssh_host_cache.as_ref() else {
            return;
        };
        if ctx.command.is_none() {
            return;
        }
        // Use the alias's resolved head so `alias dev=ssh` still triggers ssh-host injection.
        let resolved_cmd: String = match expand_alias_head(ctx, &self.alias_map) {
            Some(exp) => exp.into_owned(),
            None => return,
        };
        if resolved_cmd != "ssh" || ctx.word_index == 0 || ctx.is_flag {
            return;
        }
        candidates.extend(
            cache
                .hosts_matching(&ctx.current_word)
                .into_iter()
                .map(|host| Suggestion {
                    text: host,
                    description: Some("SSH host".to_string()),
                    kind: SuggestionKind::Command,
                    source: SuggestionSource::SshConfig,
                    ..Default::default()
                }),
        );
    }

    /// Extend `candidates` with filesystem results and rank. Used when no
    /// spec matches — either because `current_word` looks like a path or
    /// as a final fallback. `label` appears in the tracing log only.
    fn suggest_filesystem_fallback(
        &self,
        ctx: &CommandContext,
        cwd: &Path,
        buffer: &str,
        mut candidates: Vec<Suggestion>,
        label: &'static str,
    ) -> SyncResult {
        let cfg = self.config();
        if cfg.providers_filesystem {
            // When typing a `../`-prefixed word, offer one more level of parent
            // navigation (e.g. `../../`) so the user can chain upward without
            // switching context.
            if ctx.current_word.ends_with("../") {
                let parent_text = format!("{}../", ctx.current_word);
                let effective = cwd.join(&ctx.current_word);
                let at_boundary = effective.canonicalize().ok().is_none_or(|resolved| {
                    resolved == Path::new("/")
                        || std::env::var("HOME")
                            .ok()
                            .is_some_and(|h| resolved == Path::new(&h))
                });
                if !at_boundary {
                    candidates.push(Suggestion {
                        text: parent_text,
                        description: Some("Parent directory".to_string()),
                        kind: SuggestionKind::Directory,
                        source: SuggestionSource::Filesystem,
                        ..Default::default()
                    });
                }
            }
            match self.filesystem_provider.provide(ctx, cwd) {
                Ok(fs) => candidates.extend(fs),
                Err(e) => tracing::warn!("filesystem provider error ({label}): {e}"),
            }
        }
        SyncResult {
            suggestions: self.rank_with_history(ctx, cwd, buffer, candidates, true),
        }
    }

    /// Rank the full candidate pool against `ctx.current_word` in a single
    /// frizbee pass. History entries are appended to the pool (capped), and
    /// the input is pre-arranged by source-order then priority within group —
    /// frizbee's `ScoreThenIndexAsc` tiebreak preserves that arrangement for
    /// equal scores. Frecency no longer affects ordering; the DB still
    /// records acceptances.
    fn rank_with_history(
        &self,
        ctx: &CommandContext,
        cwd: &Path,
        _buffer: &str,
        mut candidates: Vec<Suggestion>,
        include_history: bool,
    ) -> Vec<Suggestion> {
        let cfg = self.config();

        // Append history to the pool (capped). Flag context (current_word
        // starts with '-') and redirect context both suppress history:
        // flags don't prefix-match command lines, and redirects expect
        // filenames.
        if include_history && cfg.max_history_results > 0 && !ctx.in_redirect && !ctx.is_flag {
            match self.history_provider.provide(ctx, cwd) {
                Ok(mut h) => {
                    h.truncate(cfg.max_history_results);
                    candidates.extend(h);
                }
                Err(e) => tracing::warn!("history provider error: {e}"),
            }
        }

        // Arrange input: source-order groups, then priority within group.
        // Frizbee's ScoreThenIndexAsc preserves this as tiebreak for equal
        // scores.
        let order = &cfg.source_order;
        candidates.sort_by(|a, b| {
            order
                .rank(a.source)
                .cmp(&order.rank(b.source))
                .then_with(|| priority::effective(b).cmp(&priority::effective(a)))
                .then_with(|| a.text.cmp(&b.text))
        });

        // Single frizbee pass on the full pool.
        fuzzy::rank_with_mode(
            &ctx.current_word,
            candidates,
            cfg.max_results,
            cfg.match_mode,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use buffer::QuoteState;

    fn make_engine() -> SuggestionEngine {
        let history = HistoryProvider::from_entries(vec![
            "git push".into(),
            "cargo build".into(),
            "ls -la".into(),
        ]);
        let commands = CommandsProvider::from_list(vec!["git".into(), "ls".into(), "cargo".into()]);
        make_test_engine(history, commands)
    }

    fn make_test_engine(history: HistoryProvider, commands: CommandsProvider) -> SuggestionEngine {
        SuggestionEngine {
            filesystem_provider: FilesystemProvider::new(),
            history_provider: history,
            commands_provider: commands,
            env_provider: EnvProvider::new(),
            ssh_host_cache: SshHostCache::default_path(),
            alias_map: AliasStore::empty(),
            config: std::sync::RwLock::new(LiveSuggestConfig::default()),
        }
    }

    fn make_ctx(
        command: Option<&str>,
        args: Vec<&str>,
        current_word: &str,
        word_index: usize,
    ) -> CommandContext {
        CommandContext {
            command: command.map(String::from),
            args: args.into_iter().map(String::from).collect(),
            current_word: current_word.to_string(),
            word_index,
            is_flag: current_word.starts_with('-'),
            is_long_flag: current_word.starts_with("--"),
            preceding_flag: None,
            in_pipe: false,
            in_redirect: false,
            quote_state: QuoteState::None,
            is_first_segment: true,
        }
    }

    #[test]
    fn test_command_position_returns_commands_and_history() {
        let engine = make_engine();
        let ctx = make_ctx(None, vec![], "gi", 0);
        let results = engine.suggest_sync(&ctx, Path::new("/tmp"), "gi").unwrap();
        // Should have "git" from both commands and history
        assert!(results.iter().any(|s| s.text == "git"));
    }

    #[test]
    fn test_source_order_groups_sources() {
        let engine = make_engine().with_source_order(SourceOrder::from_names(&[
            "filesystem".into(),
            "commands".into(),
        ]));
        let ctx = make_ctx(None, vec![], "git", 0);
        // Identical text → identical frizbee score. Source-order pre-sort
        // determines relative position via input index (ScoreThenIndexAsc).
        let candidates = vec![
            Suggestion {
                text: "git".to_string(),
                kind: SuggestionKind::Command,
                source: SuggestionSource::Commands,
                ..Default::default()
            },
            Suggestion {
                text: "git".to_string(),
                kind: SuggestionKind::FilePath,
                source: SuggestionSource::Filesystem,
                ..Default::default()
            },
        ];
        let results = engine.rank_with_history(&ctx, Path::new("/tmp"), "git", candidates, true);
        // Filesystem is listed first in the order, so its group must lead
        // the commands group among equal-score results.
        let fs_pos = results
            .iter()
            .position(|s| s.source == SuggestionSource::Filesystem)
            .expect("filesystem candidate present");
        let cmd_pos = results
            .iter()
            .position(|s| s.source == SuggestionSource::Commands)
            .expect("commands candidate present");
        assert!(
            fs_pos < cmd_pos,
            "filesystem group must precede commands group: {results:?}"
        );
    }

    #[test]
    fn providers_commands_false_excludes_command_source() {
        let engine = make_engine();
        let mut cfg = engine.config();
        cfg.providers_commands = false;
        engine.set_config(cfg);
        let ctx = make_ctx(None, vec![], "gi", 0);
        let results = engine.suggest_sync(&ctx, Path::new("/tmp"), "gi").unwrap();
        assert!(
            results
                .iter()
                .all(|s| s.source != SuggestionSource::Commands),
            "commands provider must be gated off: {results:?}"
        );
        // History injection is independent of the commands provider and
        // still surfaces "git push" for the same query.
        assert!(results
            .iter()
            .any(|s| s.source == SuggestionSource::History));
    }

    #[test]
    fn providers_filesystem_false_excludes_filesystem_source() {
        let engine = make_engine();
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("output.txt"), "").unwrap();
        let mut cfg = engine.config();
        cfg.providers_filesystem = false;
        engine.set_config(cfg);
        let mut ctx = make_ctx(Some("echo"), vec!["hello"], "", 2);
        ctx.in_redirect = true;
        let results = engine
            .suggest_sync(&ctx, tmp.path(), "echo hello ")
            .unwrap();
        assert!(
            results
                .iter()
                .all(|s| s.source != SuggestionSource::Filesystem),
            "filesystem provider must be gated off: {results:?}"
        );
        assert!(results.iter().all(|s| s.text != "output.txt"));
    }

    #[test]
    fn test_redirect_gives_filesystem() {
        let engine = make_engine();
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("output.txt"), "").unwrap();
        let mut ctx = make_ctx(Some("echo"), vec!["hello"], "", 2);
        ctx.in_redirect = true;
        let results = engine
            .suggest_sync(&ctx, tmp.path(), "echo hello ")
            .unwrap();
        assert!(results.iter().any(|s| s.text == "output.txt"));
    }

    #[test]
    fn test_path_prefix_triggers_filesystem() {
        let engine = make_engine();
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "").unwrap();
        let ctx = make_ctx(Some("cat"), vec![], "src/", 1);
        let results = engine.suggest_sync(&ctx, tmp.path(), "cat src/").unwrap();
        assert!(
            results.iter().any(|s| s.text == "src/main.rs"),
            "expected 'src/main.rs' in results: {results:?}"
        );
    }

    #[test]
    fn test_path_prefix_dispatches_via_classifier() {
        // Genuinely exercises the PathPrefix Context branch — `./foo` starts
        // with `./` so `has_path_prefix` returns true and the classifier
        // routes to PathPrefix instead of UnspeccedArg.
        let engine = make_engine();
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("foo")).unwrap();
        std::fs::write(tmp.path().join("foo/bar.txt"), "").unwrap();
        let ctx = make_ctx(Some("cat"), vec![], "./foo", 1);
        let results = engine.suggest_sync(&ctx, tmp.path(), "cat ./foo").unwrap();
        assert!(
            results.iter().any(|s| s.text.contains("foo")),
            "PathPrefix dispatch should yield filesystem entries: {results:?}"
        );
    }

    #[test]
    fn test_unknown_command_falls_back_to_filesystem() {
        let engine = make_engine();
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("data.csv"), "").unwrap();
        let ctx = make_ctx(Some("unknown_cmd"), vec![], "", 1);
        let results = engine
            .suggest_sync(&ctx, tmp.path(), "unknown_cmd_xyz ")
            .unwrap();
        assert!(results.iter().any(|s| s.text == "data.csv"));
    }

    #[test]
    fn test_empty_results_for_no_matches() {
        let engine = make_engine();
        let tmp = tempfile::TempDir::new().unwrap();
        let ctx = make_ctx(Some("git"), vec![], "zzzzzzz_no_match", 1);
        let results = engine
            .suggest_sync(&ctx, tmp.path(), "git zzzzzzz_no_match")
            .unwrap();
        assert!(results.suggestions.is_empty());
    }

    #[test]
    fn test_cd_chaining_offers_double_parent() {
        let engine = make_engine();
        let tmp = tempfile::TempDir::new().unwrap();
        let sub = tmp.path().join("aaa").join("bbb");
        std::fs::create_dir_all(&sub).unwrap();
        // Simulate: cd ../<TAB> from inside aaa/bbb
        let ctx = make_ctx(Some("cd"), vec![], "../", 1);
        let results = engine.suggest_sync(&ctx, &sub, "cd ../").unwrap();
        assert!(
            results.iter().any(|s| s.text == "../../"),
            "should offer ../../ when current_word is ../: {results:?}"
        );
    }

    #[test]
    fn test_path_prefix_chains_parent_dir_for_unspecced_command() {
        use crate::context::{classify, ClassifyInput, Context};
        // PathPrefix on an unspecced command should still offer the chained
        // `../../` when the user is one level deep into the working tree.
        let engine = make_engine();
        let tmp = tempfile::TempDir::new().unwrap();
        let sub = tmp.path().join("aaa").join("bbb");
        std::fs::create_dir_all(&sub).unwrap();
        let ctx = make_ctx(Some("unknown_cmd"), vec![], "../", 1);
        assert_eq!(
            classify(ClassifyInput {
                current_word: "../",
                in_redirect: false,
                word_index: 1,
            }),
            Context::PathPrefix
        );
        let results = engine.suggest_sync(&ctx, &sub, "unknown_cmd ../").unwrap();
        assert!(
            results.iter().any(|s| s.text == "../../"),
            "PathPrefix should chain parent dir on unspecced commands: {results:?}"
        );
    }

    #[test]
    fn test_unspecced_path_prefix_no_chain_at_root() {
        // Root has no parent — `../` chaining must not appear.
        let engine = make_engine();
        let ctx = make_ctx(Some("unknown_cmd"), vec![], "../", 1);
        let results = engine
            .suggest_sync(&ctx, Path::new("/"), "unknown_cmd ../")
            .unwrap();
        assert!(
            !results
                .iter()
                .any(|s| s.text == "../" || s.text == "../../"),
            "../ chaining should not appear at root: {results:?}"
        );
    }

    #[test]
    fn test_cd_parent_dir_absent_with_query() {
        let engine = make_engine();
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("mydir")).unwrap();
        // current_word = "my" — ../  doesn't match, should be filtered out
        let ctx = make_ctx(Some("cd"), vec![], "my", 1);
        let results = engine.suggest_sync(&ctx, tmp.path(), "cd my").unwrap();
        assert!(
            !results.iter().any(|s| s.text == "../"),
            "../ should be filtered out when current_word doesn't match: {results:?}"
        );
    }

    #[test]
    fn test_disabled_commands_provider() {
        let history = HistoryProvider::from_entries(vec![]);
        let commands = CommandsProvider::from_list(vec!["git".into(), "ls".into()]);
        let engine = make_test_engine(history, commands).with_suggest_config(50, false, 5, true);

        let ctx = make_ctx(None, vec![], "gi", 0);
        let results = engine.suggest_sync(&ctx, Path::new("/tmp"), "gi").unwrap();
        // Commands provider disabled — should not find "git" from commands
        assert!(
            !results
                .iter()
                .any(|s| s.source == crate::types::SuggestionSource::Commands),
            "should not have commands when provider disabled"
        );
    }

    #[test]
    fn suggest_sync_with_env_uses_shell_reported_env_vars() {
        let engine = make_engine();
        let ctx = make_ctx(Some("echo"), vec![], "$AWS", 1);
        let env = HashMap::from([("AWS_PROFILE".to_string(), "session".to_string())]);

        let results = engine
            .suggest_sync_with_env(&ctx, Path::new("/tmp"), "echo $AWS", Some(&env))
            .unwrap();

        assert!(
            results.suggestions.iter().any(|s| s.text == "$AWS_PROFILE"),
            "shell-reported env should drive $VAR suggestions, got {:?}",
            results
                .suggestions
                .iter()
                .map(|s| &s.text)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_history_capped_to_max_history_results() {
        let history = HistoryProvider::from_entries(vec![
            "git push origin main".into(),
            "git pull origin main".into(),
            "git fetch --all".into(),
            "git status".into(),
            "git log --oneline".into(),
        ]);
        let commands = CommandsProvider::from_list(vec!["git".into()]);
        let engine = make_test_engine(history, commands).with_max_history_results(3);

        let ctx = make_ctx(None, vec![], "git", 0);
        let results = engine.suggest_sync(&ctx, Path::new("/tmp"), "git").unwrap();
        let hist_count = results
            .iter()
            .filter(|s| s.source == crate::types::SuggestionSource::History)
            .count();
        assert_eq!(
            hist_count, 3,
            "history should be capped at 3, got {hist_count}"
        );
    }

    #[test]
    fn test_history_disabled_when_max_zero() {
        let history = HistoryProvider::from_entries(vec![
            "git push origin main".into(),
            "cargo build".into(),
        ]);
        let commands = CommandsProvider::from_list(vec!["git".into(), "cargo".into()]);
        let engine = make_test_engine(history, commands).with_max_history_results(0);

        let ctx = make_ctx(None, vec![], "git", 0);
        let results = engine.suggest_sync(&ctx, Path::new("/tmp"), "git").unwrap();
        let hist_count = results
            .iter()
            .filter(|s| s.source == crate::types::SuggestionSource::History)
            .count();
        assert_eq!(hist_count, 0, "history should be disabled when max is 0");
    }

    #[test]
    fn test_ssh_host_completion_injected() {
        let dir = tempfile::TempDir::new().unwrap();
        let ssh_config = dir.path().join("config");
        std::fs::write(&ssh_config, "Host prod\n    HostName prod.example.com\n\nHost staging\n    HostName staging.example.com\n").unwrap();
        // Use an empty temp dir as cwd so filesystem entries don't fill the
        // budget and truncate SSH hosts (single-pool ranking shares the cap).
        let cwd = tempfile::TempDir::new().unwrap();

        let engine = make_engine().with_ssh_config(ssh_config);
        let ctx = make_ctx(Some("ssh"), vec![], "", 1);
        let results = engine.suggest_sync(&ctx, cwd.path(), "ssh ").unwrap();
        let ssh_results: Vec<_> = results
            .iter()
            .filter(|s| s.source == crate::types::SuggestionSource::SshConfig)
            .collect();
        assert!(
            ssh_results.iter().any(|s| s.text == "prod"),
            "expected 'prod' in SSH results: {ssh_results:?}"
        );
        assert!(
            ssh_results.iter().any(|s| s.text == "staging"),
            "expected 'staging' in SSH results: {ssh_results:?}"
        );
    }

    #[test]
    fn test_ssh_host_completion_not_for_flags() {
        let dir = tempfile::TempDir::new().unwrap();
        let ssh_config = dir.path().join("config");
        std::fs::write(&ssh_config, "Host myhost\n").unwrap();

        let engine = make_engine().with_ssh_config(ssh_config);
        // Typing a flag: ssh -p  — should not inject hosts
        let ctx = make_ctx(Some("ssh"), vec![], "-p", 1);
        let results = engine
            .suggest_sync(&ctx, Path::new("/tmp"), "ssh -p")
            .unwrap();
        let ssh_results: Vec<_> = results
            .iter()
            .filter(|s| s.source == crate::types::SuggestionSource::SshConfig)
            .collect();
        assert!(
            ssh_results.is_empty(),
            "SSH hosts should not appear when typing a flag: {ssh_results:?}"
        );
    }

    #[test]
    fn test_ssh_host_completion_not_for_other_commands() {
        let dir = tempfile::TempDir::new().unwrap();
        let ssh_config = dir.path().join("config");
        std::fs::write(&ssh_config, "Host myhost\n").unwrap();

        let engine = make_engine().with_ssh_config(ssh_config);
        let ctx = make_ctx(Some("git"), vec![], "", 1);
        let results = engine
            .suggest_sync(&ctx, Path::new("/tmp"), "git ")
            .unwrap();
        let ssh_results: Vec<_> = results
            .iter()
            .filter(|s| s.source == crate::types::SuggestionSource::SshConfig)
            .collect();
        assert!(
            ssh_results.is_empty(),
            "SSH hosts should not appear for non-ssh commands: {ssh_results:?}"
        );
    }

    #[test]
    fn test_ssh_host_fuzzy_filtered() {
        let dir = tempfile::TempDir::new().unwrap();
        let ssh_config = dir.path().join("config");
        std::fs::write(&ssh_config, "Host prod staging dev\n").unwrap();

        let engine = make_engine().with_ssh_config(ssh_config);
        let ctx = make_ctx(Some("ssh"), vec![], "pro", 1);
        let results = engine
            .suggest_sync(&ctx, Path::new("/tmp"), "ssh pro")
            .unwrap();
        assert!(
            results.iter().any(|s| s.text == "prod"),
            "expected 'prod' to match fuzzy query 'pro': {results:?}"
        );
        // "staging" and "dev" should be filtered out by fuzzy ranking
        assert!(
            !results.iter().any(|s| s.text == "staging"),
            "'staging' should not match 'pro': {results:?}"
        );
    }

    // ---- helpers for Context-dispatch tests ----

    /// Synthesise a `CommandContext` from a raw buffer string.
    ///
    /// Splits on spaces; trailing space means `current_word` is `""` at
    /// `word_index == token_count`. Leading token is the command, remaining
    /// tokens before the last are `args`, last token is `current_word`.
    fn command_context_with(buffer: &str) -> CommandContext {
        // Tokenise, preserving trailing empty slot for "ends with space".
        let ends_with_space = buffer.ends_with(' ');
        let tokens: Vec<&str> = buffer.split_whitespace().collect();
        if tokens.is_empty() {
            return make_ctx(None, vec![], "", 0);
        }
        let command = tokens[0];
        if tokens.len() == 1 && !ends_with_space {
            // "git" — still typing the command
            return make_ctx(None, vec![], command, 0);
        }
        let (args_slice, current_word) = if ends_with_space {
            // All tokens are completed args; current_word is blank.
            (&tokens[1..], "")
        } else {
            // Last token is the word being typed.
            (&tokens[1..tokens.len() - 1], *tokens.last().unwrap())
        };
        let word_index = 1 + args_slice.len();
        make_ctx(Some(command), args_slice.to_vec(), current_word, word_index)
    }

    // ---- Context-dispatch contract tests ----

    #[test]
    fn suggest_sync_path_prefix_returns_filesystem_only() {
        let engine = make_engine();
        let ctx = command_context_with("git checkout ./");
        let result = engine
            .suggest_sync(&ctx, std::path::Path::new("/tmp"), "git checkout ./")
            .unwrap();
        assert!(
            result.suggestions.iter().all(|s| matches!(
                s.kind,
                crate::types::SuggestionKind::FilePath | crate::types::SuggestionKind::Directory
            )),
            "PathPrefix context should yield only filesystem suggestions, got {:?}",
            result
                .suggestions
                .iter()
                .map(|s| &s.kind)
                .collect::<Vec<_>>()
        );
    }

    // ---------------------------------------------------------------
    // rank_with_history tests
    // ---------------------------------------------------------------

    fn make_history_engine(history: Vec<String>) -> SuggestionEngine {
        let history = HistoryProvider::from_entries(history);
        let commands = CommandsProvider::from_list(vec!["git".into()]);
        let engine = make_test_engine(history, commands);
        let mut cfg = engine.config();
        cfg.max_results = 10;
        cfg.max_history_results = 5;
        engine.set_config(cfg);
        engine
    }

    fn flag_candidates(n: usize) -> Vec<Suggestion> {
        (0..n)
            .map(|i| Suggestion {
                text: format!("flag{i}"),
                kind: SuggestionKind::Flag,
                source: SuggestionSource::Provider,
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn engine_substring_mode_drops_subsequence_only_candidates() {
        // Issue #149 end-to-end: with match_mode = Substring the engine's
        // rank_with_history must drop a candidate that only matches the query
        // as a subsequence ("calendar" for "cl") while keeping the contiguous
        // match ("clone"). The default fuzzy engine keeps both.
        use crate::fuzzy::MatchMode;
        let engine = make_test_engine(
            HistoryProvider::from_entries(vec![]),
            CommandsProvider::from_list(vec![]),
        )
        .with_match_mode(MatchMode::Substring);
        assert_eq!(engine.match_mode(), MatchMode::Substring);

        let ctx = make_ctx(Some("git"), vec![], "cl", 1);
        let candidates = vec![
            Suggestion {
                text: "clone".into(),
                kind: SuggestionKind::Subcommand,
                source: SuggestionSource::Provider,
                ..Default::default()
            },
            Suggestion {
                text: "calendar".into(),
                kind: SuggestionKind::Subcommand,
                source: SuggestionSource::Provider,
                ..Default::default()
            },
        ];
        let results =
            engine.rank_with_history(&ctx, Path::new("/tmp"), "git cl", candidates, false);
        let texts: Vec<&str> = results.iter().map(|s| s.text.as_str()).collect();
        assert!(
            texts.contains(&"clone"),
            "contiguous 'cl' match must survive: {texts:?}"
        );
        assert!(
            !texts.contains(&"calendar"),
            "subsequence-only 'c..l' must be dropped in substring mode: {texts:?}"
        );
    }

    #[test]
    fn engine_history_skipped_in_redirect_context() {
        let engine = make_history_engine(vec!["echo redirected".into()]);
        let mut ctx = make_ctx(Some("echo"), vec![], "", 1);
        ctx.in_redirect = true;
        let candidates = vec![Suggestion {
            text: "foo.txt".to_string(),
            kind: SuggestionKind::FilePath,
            source: SuggestionSource::Filesystem,
            ..Default::default()
        }];
        let results =
            engine.rank_with_history(&ctx, Path::new("/tmp"), "echo > ", candidates, true);
        let history_count = results
            .iter()
            .filter(|s| s.source == SuggestionSource::History)
            .count();
        assert_eq!(
            history_count, 0,
            "redirect context expects filenames, not command history: {results:?}"
        );
    }

    #[test]
    fn engine_history_skipped_in_flag_context() {
        // ctx.is_flag = current_word.starts_with('-'). Flags don't
        // prefix-match command lines, so the lane is wasted here.
        let engine = make_history_engine(vec!["git --version".into()]);
        let ctx = make_ctx(Some("git"), vec![], "--", 1);
        let candidates = flag_candidates(10);
        let results = engine.rank_with_history(&ctx, Path::new("/tmp"), "git --", candidates, true);
        let history_count = results
            .iter()
            .filter(|s| s.source == SuggestionSource::History)
            .count();
        assert_eq!(
            history_count, 0,
            "flag context must not surface history rows: {results:?}"
        );
    }

    #[test]
    fn engine_history_no_match_preserves_full_candidate_budget() {
        // History is non-empty but NO entry matches the (empty) current_word,
        // so history rows sort after candidates (source-order pre-sort) and
        // are truncated by max_results. The full candidate budget is used.
        let engine = make_history_engine(vec!["docker build .".into(), "ls -la".into()]);
        let ctx = make_ctx(Some("git"), vec!["checkout"], "", 2);
        let results = engine.rank_with_history(
            &ctx,
            Path::new("/tmp"),
            "git checkout ",
            flag_candidates(10),
            true,
        );

        assert_eq!(
            results.len(),
            10,
            "candidates fill max_results; non-matching history is truncated: {results:?}"
        );
        let history_count = results
            .iter()
            .filter(|s| s.source == SuggestionSource::History)
            .count();
        assert_eq!(
            history_count, 0,
            "non-matching history displaced by candidates: {results:?}"
        );
    }

    #[test]
    fn engine_history_empty_provider_with_allowed_lane_preserves_budget() {
        // Empty history while the lane is ALLOWED (not redirect/flag): the
        // pool is just the candidates, and the full budget is available.
        let engine = make_history_engine(vec![]);
        let ctx = make_ctx(Some("git"), vec![], "", 1);
        let results =
            engine.rank_with_history(&ctx, Path::new("/tmp"), "git ", flag_candidates(3), true);

        assert_eq!(
            results.len(),
            3,
            "empty history adds nothing => full candidate budget: {results:?}"
        );
        let history_count = results
            .iter()
            .filter(|s| s.source == SuggestionSource::History)
            .count();
        assert_eq!(
            history_count, 0,
            "empty history provider yields no history rows: {results:?}"
        );
    }
}
