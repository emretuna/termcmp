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

    fn suggest<'a>(
        &'a self,
        req: &'a SuggestRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Suggestion>>> + Send + 'a>>;
}
