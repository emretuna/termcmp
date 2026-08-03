//! Suggestion engine with multiple providers and fuzzy ranking.

pub mod alias;
pub(crate) mod alias_expand;
pub mod commands;
pub mod context;
mod engine;
mod env;
mod filesystem;
pub mod fuzzy;
pub mod history;
pub mod priority;
mod provider;
pub mod ssh;
pub mod types;
pub mod util;

pub use alias::{AliasEntry, ShellFamily};
pub use engine::{LiveSuggestConfig, SuggestionEngine, SyncResult};
pub use provider::{AsyncProvider, SuggestRequest};
pub use types::{SourceOrder, Suggestion, SuggestionKind, SuggestionSource};
pub use util::common_prefix_char_count;
