use crate::priority::Priority;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum SuggestionKind {
    Command,
    Subcommand,
    Flag,
    FilePath,
    Directory,
    History,
    EnvVar,
    /// Dynamic argument value produced by an async provider
    /// (e.g. shell completions, LLM suggestions). Grouped with other
    /// arg-position values for sort order.
    ProviderValue,
    /// AI-powered completion from an LLM provider.
    Llm,
    /// Sentinel action item: on-demand "Ask AI" trigger, pinned to the popup top.
    AskAi,
}

impl SuggestionKind {
    /// Base priority for this `SuggestionKind` when the suggestion does
    /// not declare its own. Numbers chosen so that provider
    /// output > flags > filesystem.
    pub fn base_priority(self) -> Priority {
        Priority::new(match self {
            Self::Subcommand => 70,
            Self::ProviderValue => 70,
            Self::EnvVar => 50,
            Self::Command => 40,
            Self::Llm => 60,
            Self::AskAi => 100,
            Self::Flag => 30,
            Self::Directory => 25,
            Self::FilePath => 20,
            Self::History => 10,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum SuggestionSource {
    Filesystem,
    History,
    Commands,
    Env,
    SshConfig,
    /// Async provider results (e.g. shell completions, LLM). Distinct
    /// from other sources for telemetry and downstream filtering.
    Provider,
    /// LLM-powered completions.
    Llm,
}

/// User-configured source-group ordering. Lower rank = earlier group in the
/// popup. Sources absent from the list get rank `usize::MAX` (sort last,
/// preserving relative order among themselves via the score/priority tiebreak).
#[derive(Debug, Clone)]
pub struct SourceOrder {
    order: Vec<SuggestionSource>,
}

impl SourceOrder {
    /// Default order preserving the pre-config grouping: commands, shell
    /// completions, AI, env, filesystem, ssh, history last.
    pub fn default_order() -> Self {
        Self {
            order: vec![
                SuggestionSource::Commands,
                SuggestionSource::Provider,
                SuggestionSource::Llm,
                SuggestionSource::Env,
                SuggestionSource::Filesystem,
                SuggestionSource::SshConfig,
                SuggestionSource::History,
            ],
        }
    }

    /// Parse user-facing config strings into a `SourceOrder`.
    /// Recognised names: `commands`, `filesystem`, `history`, `ai`, `env`,
    /// `shell`, `ssh`. Unrecognised names are silently skipped (validation
    /// and warnings happen in `config::TermcmpConfig::normalize`).
    pub fn from_names(names: &[String]) -> Self {
        let order = names
            .iter()
            .filter_map(|n| match n.as_str() {
                "commands" => Some(SuggestionSource::Commands),
                "filesystem" => Some(SuggestionSource::Filesystem),
                "history" => Some(SuggestionSource::History),
                "ai" => Some(SuggestionSource::Llm),
                "env" => Some(SuggestionSource::Env),
                "shell" => Some(SuggestionSource::Provider),
                "ssh" => Some(SuggestionSource::SshConfig),
                _ => None,
            })
            .collect();
        Self { order }
    }

    /// Group rank for a source. Lower = earlier in the popup.
    #[inline]
    pub fn rank(&self, source: SuggestionSource) -> usize {
        self.order
            .iter()
            .position(|s| *s == source)
            .unwrap_or(usize::MAX)
    }
}

impl Default for SourceOrder {
    fn default() -> Self {
        Self::default_order()
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Suggestion {
    pub text: String,
    pub description: Option<String>,
    pub kind: SuggestionKind,
    pub source: SuggestionSource,
    pub score: u32,
    pub match_indices: Vec<u32>,
    /// Optional rank hint, range 0..=100. When `None`, falls back to
    /// the kind's base priority (see `SuggestionKind::base_priority`).
    /// Higher values rank earlier in the popup.
    pub priority: Option<Priority>,
}

impl Default for Suggestion {
    fn default() -> Self {
        // Neutral default: `ProviderValue` + `Provider` is a kind/source
        // pair with no legacy overlap, so the default does not pretend to
        // be a shell command or a Commands-source entry. Every production
        // call site that builds a `Suggestion` via `..Default::default()`
        // sets `kind` and `source` explicitly; this default is only
        // observable when a caller forgets to, and picking a neutral
        // "dynamic arg-value" bucket is strictly better than defaulting
        // to Command (which would misclassify silently).
        Self {
            text: String::new(),
            description: None,
            kind: SuggestionKind::ProviderValue,
            source: SuggestionSource::Provider,
            score: 0,
            match_indices: Vec::new(),
            priority: None,
        }
    }
}

#[cfg(test)]
mod kind_invariants {
    use super::*;

    // Pin the behavioral contracts for `ProviderValue` + the neutral
    // `Suggestion::default()`. Silent drift in any of these values would
    // mis-rank the popup without being caught by the relative-ordering
    // tests in `engine.rs`.
    #[test]
    fn provider_value_contract() {
        assert_eq!(SuggestionKind::ProviderValue.base_priority().get(), 70);
        assert_eq!(Suggestion::default().kind, SuggestionKind::ProviderValue);
        assert_eq!(Suggestion::default().source, SuggestionSource::Provider);
    }

    #[test]
    fn suggestion_priority_defaults_to_none() {
        let s = Suggestion::default();
        assert_eq!(s.priority, None);
    }
}

#[cfg(test)]
mod source_order {
    use super::*;

    #[test]
    fn rank_returns_position() {
        let order = SourceOrder::from_names(&["history".into(), "commands".into()]);
        assert_eq!(order.rank(SuggestionSource::History), 0);
        assert_eq!(order.rank(SuggestionSource::Commands), 1);
        assert_eq!(order.rank(SuggestionSource::Env), usize::MAX);
    }

    #[test]
    fn from_names_skips_unknown() {
        let order = SourceOrder::from_names(&["commands".into(), "bogus".into(), "ai".into()]);
        assert_eq!(order.rank(SuggestionSource::Commands), 0);
        assert_eq!(order.rank(SuggestionSource::Llm), 1);
        assert_eq!(order.rank(SuggestionSource::History), usize::MAX);
    }

    #[test]
    fn default_has_all_sources() {
        let order = SourceOrder::default();
        for source in [
            SuggestionSource::Filesystem,
            SuggestionSource::History,
            SuggestionSource::Commands,
            SuggestionSource::Env,
            SuggestionSource::SshConfig,
            SuggestionSource::Provider,
            SuggestionSource::Llm,
        ] {
            assert!(
                order.rank(source) < usize::MAX,
                "default order missing {source:?}"
            );
        }
    }
}
