use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use anyhow::Result;
use buffer::CommandContext;

use crate::types::Suggestion;

pub trait Provider: Send + Sync {
    fn provide(&self, ctx: &CommandContext, cwd: &Path) -> Result<Vec<Suggestion>>;
}

/// Snapshot handed to async providers. `cursor` is a CHAR offset into `buffer`.
#[derive(Debug)]
pub struct SuggestRequest<'a> {
    pub ctx: &'a CommandContext,
    pub cwd: &'a Path,
    pub buffer: &'a str,
    pub cursor: usize,
}

/// Networked/IPC providers (LLM, shell-native completions). Results flow
/// through the existing DynamicResult pipeline in pty.
/// Implementations MUST self-impose a timeout.
pub trait AsyncProvider: Send + Sync {
    fn name(&self) -> &'static str;

    /// Whether this provider only backfills the shell-completion tree cache
    /// and should be skipped on a cache hit. `false` (the default) means the
    /// provider is "live" and fires on every trigger — e.g. the LLM, whose
    /// results are never cached. The `fish`/`zsh` providers override this to
    /// `true` so a warm cache serves instantly without a PTY spawn.
    fn is_backfill_provider(&self) -> bool {
        false
    }
    fn suggest<'a>(
        &'a self,
        req: &'a SuggestRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Suggestion>>> + Send + 'a>>;
}
