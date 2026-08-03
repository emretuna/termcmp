use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use buffer::{byte_to_char_offset, char_to_byte_offset, parse_command_context};
use config::DescriptionBoxMode;
use overlay::types::{
    OverlayState, PopupLayout, DEFAULT_MAX_POPUP_WIDTH, DEFAULT_MAX_VISIBLE,
    DEFAULT_MIN_POPUP_WIDTH,
};
use overlay::{
    clear_detail_box, clear_popup_unframed, compute_detail_layout,
    description_overflows_main_popup, popup_additional_scroll_deficit, render_detail_box,
    render_indicator_row, DetailLayout, FeedbackKind, PopupTheme,
};
use parser::TerminalParser;
use suggest::{AsyncProvider, Suggestion, SuggestionEngine, SuggestionKind};
use terminal::TerminalProfile;
use tokio::sync::{mpsc, Notify};

use crate::dynamic_result::{DynamicResult, ProviderTag};
use crate::feedback::AsyncFeedback;
use crate::input::KeyEvent;
use crate::predict::BufferModel;

/// Resolved keybindings — each action maps to a concrete `KeyEvent`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keybindings {
    pub accept: KeyEvent,
    pub accept_and_enter: KeyEvent,
    pub dismiss: KeyEvent,
    pub navigate_up: KeyEvent,
    pub navigate_down: KeyEvent,
    pub trigger: KeyEvent,
    pub toggle_match_mode: KeyEvent,
}

impl Default for Keybindings {
    fn default() -> Self {
        Self {
            accept: KeyEvent::Tab,
            accept_and_enter: KeyEvent::Enter,
            dismiss: KeyEvent::Escape,
            navigate_up: KeyEvent::ArrowUp,
            navigate_down: KeyEvent::ArrowDown,
            trigger: KeyEvent::CtrlSlash,
            toggle_match_mode: KeyEvent::Ctrl('r'),
        }
    }
}

/// Result of routing a key event through the popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// Raw bytes to forward to the PTY (empty = key was swallowed).
    Forward(Vec<u8>),
    /// The user accepted the "Ask AI" sentinel; the proxy must run the LLM
    /// on demand and inject the response. No bytes are forwarded yet.
    AskAiAccept,
}

impl KeyOutcome {
    /// The forward bytes, or `&[]` for `AskAiAccept`.
    pub fn forward_bytes(&self) -> &[u8] {
        match self {
            KeyOutcome::Forward(b) => b,
            KeyOutcome::AskAiAccept => &[],
        }
    }
}

impl Keybindings {
    pub fn from_config(config: &config::KeybindingsConfig) -> anyhow::Result<Self> {
        Ok(Self {
            accept: parse_key_name(&config.accept)?,
            accept_and_enter: parse_key_name(&config.accept_and_enter)?,
            dismiss: parse_key_name(&config.dismiss)?,
            navigate_up: parse_key_name(&config.navigate_up)?,
            navigate_down: parse_key_name(&config.navigate_down)?,
            trigger: parse_key_name(&config.trigger)?,
            toggle_match_mode: parse_key_name(&config.toggle_match_mode)?,
        })
    }
}

/// Parse a human-readable key name into a `KeyEvent`.
///
/// Supported names (case-insensitive):
/// `tab`, `enter`, `escape`, `backspace`, `ctrl+space`, `ctrl+/`,
/// `arrow_up`, `arrow_down`, `arrow_left`, `arrow_right`, `ctrl+a`-`ctrl+z`
pub fn parse_key_name(name: &str) -> anyhow::Result<KeyEvent> {
    match name.trim().to_lowercase().as_str() {
        "tab" => Ok(KeyEvent::Tab),
        "enter" => Ok(KeyEvent::Enter),
        "escape" => Ok(KeyEvent::Escape),
        "backspace" => Ok(KeyEvent::Backspace),
        "ctrl+space" => Ok(KeyEvent::CtrlSpace),
        "ctrl+/" => Ok(KeyEvent::CtrlSlash),
        "arrow_up" => Ok(KeyEvent::ArrowUp),
        "arrow_down" => Ok(KeyEvent::ArrowDown),
        "arrow_left" => Ok(KeyEvent::ArrowLeft),
        "arrow_right" => Ok(KeyEvent::ArrowRight),
        other => {
            if let Some(c) = other.strip_prefix("ctrl+") {
                if let Some(ch) = c.chars().next() {
                    if c.len() == 1 && ch.is_ascii_lowercase() {
                        match ch {
                            'c' => anyhow::bail!("ctrl+c is reserved for SIGINT — cannot be used as a keybinding"),
                            'd' => anyhow::bail!("ctrl+d is reserved for EOF — cannot be used as a keybinding"),
                            'z' => anyhow::bail!("ctrl+z is reserved for SIGTSTP — cannot be used as a keybinding"),
                            's' => anyhow::bail!("ctrl+s is reserved for flow control (XOFF) — cannot be used as a keybinding"),
                            'q' => anyhow::bail!("ctrl+q is reserved for flow control (XON) — cannot be used as a keybinding"),
                            'i' => anyhow::bail!("ctrl+i produces the same byte as Tab (0x09) — use 'tab' instead"),
                            'm' => anyhow::bail!("ctrl+m produces the same byte as Enter (0x0D) — use 'enter' instead"),
                            _ => return Ok(KeyEvent::Ctrl(ch)),
                        }
                    }
                }
                anyhow::bail!(
                    "ctrl+ must be followed by a single letter (a-z), got: {:?}",
                    c
                );
            }
            anyhow::bail!("unknown key name: {:?}", other)
        }
    }
}

/// Format a `KeyEvent` as a human-readable label for key hints.
fn format_key_event(key: &KeyEvent) -> String {
    match key {
        KeyEvent::Tab => "Tab".to_string(),
        KeyEvent::Enter => "Enter".to_string(),
        KeyEvent::Escape => "Esc".to_string(),
        KeyEvent::ArrowUp => "Up".to_string(),
        KeyEvent::ArrowDown => "Down".to_string(),
        KeyEvent::ArrowLeft => "Left".to_string(),
        KeyEvent::ArrowRight => "Right".to_string(),
        KeyEvent::CtrlSpace => "Ctrl+Space".to_string(),
        KeyEvent::CtrlSlash => "Ctrl+/".to_string(),
        KeyEvent::Ctrl(c) => format!("Ctrl+{}", c.to_ascii_uppercase()),
        KeyEvent::Printable(c) => c.to_string(),
        _ => format!("{key:?}"),
    }
}

/// Snapshot of command context at provider-spawn time, so merge-time can
/// decide whether in-flight results still match the user's current buffer.
#[derive(Debug, Clone)]
struct DynamicCtxSnapshot {
    command: Option<String>,
    args: Vec<String>,
    preceding_flag: Option<String>,
    word_index: usize,
}

impl DynamicCtxSnapshot {
    fn capture(ctx: &buffer::CommandContext) -> Self {
        Self {
            command: ctx.command.clone(),
            args: ctx.args.clone(),
            preceding_flag: ctx.preceding_flag.clone(),
            word_index: ctx.word_index,
        }
    }

    /// Returns true if `current` represents a different completion site than
    /// the site this snapshot was taken at — in which case in-flight results
    /// are stale and must not be merged.
    fn is_stale_against(&self, current: &buffer::CommandContext) -> bool {
        self.command != current.command
            || self.args != current.args
            || self.preceding_flag != current.preceding_flag
            || self.word_index != current.word_index
    }
}

/// Outcome of the staleness check shared by `try_merge_dynamic` and
/// `apply_block_result`.
enum MergeFreshness {
    /// Spawn-time context still matches the live buffer; merge with the
    /// returned live `current_word` and full `buffer`.
    Fresh {
        current_word: String,
        buffer: String,
    },
    /// Buffer drifted — drop the results and repaint.
    Stale,
    /// Parser lock was poisoned; caller should bail without rendering.
    PoisonedLock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OverlayRenderToken(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OverlayCleanupToken(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OverlayWriteTicket {
    pub(crate) epoch: u64,
    pub(crate) render_token: Option<OverlayRenderToken>,
    pub(crate) cleanup_token: Option<OverlayCleanupToken>,
}

#[derive(Debug, Clone)]
struct PendingOverlayRender {
    token: OverlayRenderToken,
    layout: PopupLayout,
    detail_layout: Option<DetailLayout>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupScope {
    /// Cleanup bytes clear both the main popup and the detail box.
    MainAndDetail,
    /// Cleanup bytes clear only the detail box; the main popup layout
    /// remains committed (e.g. runtime description-box disable).
    DetailOnly,
}

/// Runtime state for the detail-box debounce window, grouped so the
/// invariants ("clearing one of these fields without the others can leave a
/// stale displayed index resolving to a now-mismatched suggestion") are
/// reset together via `DetailDebounceState::reset`.
#[derive(Debug, Default)]
struct DetailDebounceState {
    /// Selected index whose description is currently displayed in the
    /// detail box. Diverges from `overlay.selected` during the debounce
    /// window; reconciled when the timer fires or on the next render past
    /// `detail_box_debounce_ms`.
    displayed_idx: Option<usize>,
    /// Monotonic timestamp of the first selection change in the current
    /// detail update window. Used together with `detail_box_debounce_ms` to
    /// throttle rapid arrow navigation into at most one detail repaint per
    /// window.
    last_change_at: Option<Instant>,
    /// Set to `true` while a debounce timer is in flight so we don't spawn
    /// duplicates while the user is hammering arrow keys.
    pending: bool,
}

impl DetailDebounceState {
    fn reset(&mut self) {
        self.displayed_idx = None;
        self.last_change_at = None;
        self.pending = false;
    }
}

#[derive(Debug, Clone)]
struct PendingOverlayCleanup {
    token: OverlayCleanupToken,
    scope: CleanupScope,
}

#[derive(Debug, Clone, Copy)]
struct OverlayRect {
    start_row: u16,
    start_col: u16,
    width: u16,
    height: u16,
}

impl OverlayRect {
    fn from_popup(layout: &PopupLayout) -> Self {
        Self {
            start_row: layout.start_row,
            start_col: layout.start_col,
            width: layout.width,
            height: layout.height,
        }
    }

    fn from_detail(layout: &DetailLayout) -> Self {
        Self {
            start_row: layout.start_row,
            start_col: layout.start_col,
            width: layout.width,
            height: layout.height,
        }
    }
}

fn detail_layout_after_scroll(layout: &DetailLayout, scroll: u16) -> Option<DetailLayout> {
    if layout.width == 0 || layout.height == 0 {
        return None;
    }
    if scroll == 0 {
        return Some(layout.clone());
    }

    let end_row = layout.start_row.saturating_add(layout.height);
    if scroll >= end_row {
        return None;
    }

    let clipped_rows = scroll.saturating_sub(layout.start_row);
    let height = layout.height.saturating_sub(clipped_rows);
    if height == 0 {
        return None;
    }

    Some(DetailLayout {
        start_row: layout.start_row.saturating_sub(scroll),
        height,
        ..layout.clone()
    })
}

fn clear_detail_box_uncovered_by(buf: &mut Vec<u8>, layout: &DetailLayout, covers: &[OverlayRect]) {
    if layout.width == 0 || layout.height == 0 {
        return;
    }

    overlay::ansi::save_cursor(buf);
    let target_col_end = layout.start_col.saturating_add(layout.width);
    for row_offset in 0..layout.height {
        let row = layout.start_row.saturating_add(row_offset);
        let mut spans = vec![(layout.start_col, target_col_end)];

        for cover in covers {
            if cover.width == 0 || cover.height == 0 {
                continue;
            }
            let cover_row_end = cover.start_row.saturating_add(cover.height);
            if row < cover.start_row || row >= cover_row_end {
                continue;
            }

            let cover_col_start = cover.start_col;
            let cover_col_end = cover.start_col.saturating_add(cover.width);
            if cover_col_end <= cover_col_start {
                continue;
            }

            let mut next_spans = Vec::with_capacity(spans.len() + 1);
            for (span_start, span_end) in spans {
                if cover_col_end <= span_start || cover_col_start >= span_end {
                    next_spans.push((span_start, span_end));
                    continue;
                }
                if cover_col_start > span_start {
                    next_spans.push((span_start, cover_col_start.min(span_end)));
                }
                if cover_col_end < span_end {
                    next_spans.push((cover_col_end.max(span_start), span_end));
                }
            }
            spans = next_spans;
            if spans.is_empty() {
                break;
            }
        }

        for (span_start, span_end) in spans {
            if span_end <= span_start {
                continue;
            }
            overlay::ansi::move_to(buf, row, span_start);
            for _ in span_start..span_end {
                buf.push(b' ');
            }
        }
    }
    overlay::ansi::restore_cursor(buf);
}

/// Shell-family detection for gating the keystroke buffer model.
///
/// Non-zsh shells (fish, bash, Other) fall back to the keystroke-driven
/// `BufferModel` because they don't self-report their command buffer via
/// OSC 7772. Zsh uses OSC 7772 as the sole buffer source and skips the
/// model entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Zsh,
    Fish,
    Bash,
    Other,
}

impl From<ShellKind> for suggest::ShellFamily {
    fn from(kind: ShellKind) -> Self {
        match kind {
            ShellKind::Zsh => suggest::ShellFamily::Zsh,
            ShellKind::Fish => suggest::ShellFamily::Fish,
            ShellKind::Bash => suggest::ShellFamily::Bash,
            ShellKind::Other => suggest::ShellFamily::Other,
        }
    }
}

/// Result of `InputHandler::prepare_trigger_with_block`.
///
/// When a high-priority async generator is pending and `render_block_ms > 0`,
/// the debounce loop receives `NeedsBlock` and awaits the bounded window
/// *outside* the `std::sync::Mutex` lock so Tokio can schedule other tasks
/// during the wait. On timeout or fast-completion the loop re-acquires the
/// lock to call `apply_block_result`.
pub enum TriggerPrepared {
    /// Sync-only suggestions were painted (or the trigger was a no-op). No
    /// further action needed from the caller.
    Painted,
    /// Sync suggestions were painted and a high-priority async generator is
    /// pending. The caller should await `rx` up to `block_ms`, then call
    /// `apply_block_result` under the handler lock.
    NeedsBlock {
        /// Receiver to `await` for the async generator's results.
        rx: mpsc::Receiver<DynamicResult>,
        /// Sync-only suggestions already painted. Used for merging.
        sync_suggestions: Vec<Suggestion>,
        /// Maximum wait duration.
        block_ms: u64,
        /// Cursor geometry for the follow-up render.
        cursor_row: u16,
        cursor_col: u16,
        screen_rows: u16,
        screen_cols: u16,
        /// Fingerprint to stamp on `last_trigger_fingerprint` after a
        /// successful merge render.
        fingerprint: TriggerFingerprint,
        /// Current word at trigger time. Passed to `apply_block_result` so
        /// the merge step filters/ranks the combined pool against the user's
        /// query (mirrors the empty-vs-non-empty branch in `try_merge_dynamic`).
        current_word: String,
    },
}

pub struct InputHandler {
    engine: Arc<SuggestionEngine>,
    overlay: OverlayState,
    suggestions: Vec<Suggestion>,
    last_layout: Option<PopupLayout>,
    visible: bool,
    trigger_requested: bool,
    last_repaint_at: Option<Instant>,
    max_visible: usize,
    debounce_suppressed: bool,
    auto_trigger: bool,
    keybindings: Keybindings,
    theme: PopupTheme,
    /// Typing-pause debounce window, shared with the debounce loop so a
    /// config change to `trigger.delay_ms` takes effect on the next cycle
    /// without restarting the loop task.
    delay_ms: Arc<std::sync::atomic::AtomicU64>,
    dynamic_rx: Option<mpsc::Receiver<DynamicResult>>,
    dynamic_task: Option<tokio::task::JoinHandle<()>>,
    dynamic_notify: Arc<Notify>,
    feedback_tick_notify: Arc<Notify>,
    feedback: AsyncFeedback,
    feedback_dismiss_ms: u16,
    pending_failed: Vec<String>,
    pending_empty_count: usize,
    /// Command context snapshot captured when generators were spawned.
    /// Consulted by `check_merge_staleness` (called from both
    /// `try_merge_dynamic` and `apply_block_result`) to drop stale results
    /// when the user's editing has changed WHICH generator would now apply.
    /// We compare command + args (subcommand path) + preceding_flag +
    /// word_index. `current_word` is also compared, but ONLY when a generator
    /// depends on it literally (script_template with `{current_token}`); for
    /// generators that treat it as a fuzzy-filter prefix, typing more
    /// characters still lets results merge and re-rank.
    /// See `DynamicCtxSnapshot::capture` and `is_stale_against`.
    dynamic_ctx: Option<DynamicCtxSnapshot>,
    terminal_profile: TerminalProfile,
    /// Accumulated viewport scroll caused by popup rendering. Persists across
    /// dismiss/re-trigger cycles because overlay-owned viewport scroll is permanent.
    /// Parser-observed shell scrolls are already reflected in `TerminalState`
    /// and must not be stored here. Reset when a CPR sync corrects the parser's
    /// cursor position (new prompt).
    overlay_scroll_deficit: u16,
    /// Fingerprint (buffer hash + cursor offset + shell env hash) of the last trigger that
    /// produced a visible popup. Used as an idempotency guard in the trigger
    /// paths (`InputHandler::trigger` and `prepare_trigger_with_block`/
    /// `apply_block_result`): when a new trigger arrives with an unchanged
    /// buffer AND the popup is still visible, we skip re-running
    /// `suggest_sync` because (1) it would produce the same suggestions —
    /// wasted work, and (2) an empty-sync + no-generators result would
    /// silently dismiss a popup populated by a prior trigger's async merge.
    /// See the bug-repro test `test_trigger_idempotent_when_buffer_unchanged`.
    /// Reset by `dismiss()` so ESC-then-retrigger on the same buffer still works.
    last_trigger_fingerprint: Option<TriggerFingerprint>,
    /// Monotonic counter ticked by both `trigger()` and
    /// `prepare_trigger_with_block()` before the new sync pass runs.
    /// `spawn_async_providers` snapshots it into `spawned_generation`;
    /// `check_merge_staleness` compares the two so async results spawned for
    /// an earlier buffer get dropped instead of merged.
    buffer_generation: u64,
    /// Generation counter snapshotted at `spawn_async_providers` time.
    /// `try_merge_dynamic` compares this against `buffer_generation` to drop
    /// results from a task spawned for an earlier buffer state.
    spawned_generation: u64,
    /// Maximum time (ms) to wait for a high-priority async generator before
    /// painting sync-only results. 0 disables bounded blocking (paint immediately).
    /// Set from `config.popup.render_block_ms` during the builder phase.
    render_block_ms: u64,
    /// Lower bound for popup width (display columns). Set from
    /// `config.popup.min_width`. Defaults to [`DEFAULT_MIN_POPUP_WIDTH`].
    min_popup_width: u16,
    /// Upper bound for popup width (display columns). Set from
    /// `config.popup.max_width`. Defaults to [`DEFAULT_MAX_POPUP_WIDTH`].
    max_popup_width: u16,
    /// Adjacent description-box mode (off / side). When `Side` an extra
    /// box is rendered next to the main popup with the selected suggestion's
    /// full wrapped description.
    detail_box_mode: DescriptionBoxMode,
    /// Maximum width (display columns) reserved for the detail box.
    detail_box_max_width: u16,
    /// Maximum number of wrapped lines in the detail box.
    detail_box_lines: u16,
    /// Debounce window (ms) for detail-box updates on selection change.
    detail_box_debounce_ms: u64,
    /// Layout of the most recently rendered detail box. Cleared with popup
    /// teardown or by detail-only cleanup when the description box is disabled;
    /// write acknowledgement controls when the committed layout is released.
    last_detail_layout: Option<DetailLayout>,
    /// Notify fired by a spawned timer when the detail-box debounce window
    /// has elapsed; the proxy listens to trigger a re-render.
    detail_redraw_notify: Arc<Notify>,
    /// Notify fired when a match-mode flash is armed; the proxy's flash
    /// timer waits on it, sleeps to the deadline, then triggers a re-render
    /// so the footer reverts from the mode label to the normal key hint.
    mode_flash_notify: Arc<Notify>,
    /// Detail-box debounce runtime state. Grouped so the three correlated
    /// fields reset together via `DetailDebounceState::reset` — clearing one
    /// without the others can leave a stale `displayed_idx` pointing at a
    /// reranked suggestion.
    detail_debounce: DetailDebounceState,
    /// Wrapping epoch stamp for overlay-owned bytes. Proxy tasks stamp render
    /// buffers with this value and drop them if shell output advances it before
    /// the buffer reaches stdout.
    output_epoch: u64,
    /// Monotonic token source for popup render buffers staged by `render_at`.
    overlay_render_generation: u64,
    /// Layout/scroll state for the latest render buffer. Committed only after
    /// the proxy writes that exact buffer to stdout.
    pending_overlay_render: Option<PendingOverlayRender>,
    /// Monotonic token source for popup cleanup buffers staged by teardown.
    overlay_cleanup_generation: u64,
    /// Pending acknowledgement for cleanup bytes that clear committed overlay layouts.
    pending_overlay_cleanup: Option<PendingOverlayCleanup>,
    /// When `true`, the accept key (Tab) accepts the top-ranked *completion*
    /// even when the user has not navigated yet (`overlay.selected == None`),
    /// instead of forwarding a literal tab to the shell. The un-navigated
    /// fallback skips the pinned "Ask AI" sentinel, so Tab never fires
    /// on-demand Ask AI implicitly. Opt-in via `config.popup.tab_accepts_top`;
    /// default `false` preserves the historical "navigate first, then accept"
    /// flow. See issue #150.
    ///
    /// Deliberately scoped to the `accept` action only. With the default
    /// bindings (`accept` = Tab, `accept_and_enter` = Enter) this means Enter
    /// still runs the command line — because it is a separate binding — so a
    /// stray Enter never silently accepts a suggestion the user meant to run
    /// verbatim. (Rebinding the `accept` action itself onto Enter makes Enter
    /// the accept key, which then accepts the top item; the dispatch checks
    /// `accept` before `accept_and_enter`.)
    tab_accepts_top: bool,
    /// Shell-family classification for gating the keystroke buffer model.
    /// Non-zsh shells fall back to the keystroke-driven `BufferModel`.
    shell_kind: ShellKind,
    /// Local keystroke model of the shell command line.
    /// Only populated for shells that don't self-report (non-zsh).
    input_model: BufferModel,
    /// One-shot: set when Ctrl+/ is pressed while `buffer_pending_display` is
    /// true. Tells Task B to fire `pending_trigger.resolve()` bypassing the
    /// `auto_trigger_enabled` gate. Drained + acted on in proxy.rs.
    manual_trigger_stashed: bool,
    async_providers: Vec<Arc<dyn AsyncProvider>>,
    /// On-demand "Ask AI" provider. `Some` iff `ai.ask_ai` is enabled and a
    /// usable provider is configured. Independent of the inline LLM provider.
    ask_ai_provider: Option<Arc<llm::LlmProvider>>,
    /// Persistent shell-completion tree cache. When present, the trigger path
    /// resolves cached completions synchronously and skips the backfill
    /// (fish/zsh) providers on a hit; live providers (LLM) still fire.
    completion_cache: Option<Arc<crate::shell_completion::CompletionTreeCache>>,
    /// When `Some`, the key-hint footer shows this label until the deadline
    /// passes, then reverts to the normal key hint. Set by `toggle_match_mode`.
    mode_flash: Option<(String, Instant)>,
}

impl InputHandler {
    pub fn new(terminal_profile: TerminalProfile, shell_kind: ShellKind) -> anyhow::Result<Self> {
        Ok(Self {
            engine: Arc::new(SuggestionEngine::new(shell_kind.into())?),
            overlay: OverlayState::new(),
            suggestions: Vec::new(),
            last_layout: None,
            visible: false,
            trigger_requested: false,
            last_repaint_at: None,
            max_visible: DEFAULT_MAX_VISIBLE,
            debounce_suppressed: false,
            auto_trigger: true,
            keybindings: Keybindings::default(),
            theme: PopupTheme::default(),
            delay_ms: Arc::new(std::sync::atomic::AtomicU64::new(150)),
            dynamic_rx: None,
            dynamic_task: None,
            dynamic_notify: Arc::new(Notify::new()),
            feedback_tick_notify: Arc::new(Notify::new()),
            feedback: AsyncFeedback::Idle,
            feedback_dismiss_ms: 1200,
            pending_failed: Vec::new(),
            pending_empty_count: 0,
            dynamic_ctx: None,
            terminal_profile,
            overlay_scroll_deficit: 0,
            last_trigger_fingerprint: None,
            buffer_generation: 0,
            spawned_generation: 0,
            render_block_ms: 80,
            min_popup_width: DEFAULT_MIN_POPUP_WIDTH,
            max_popup_width: DEFAULT_MAX_POPUP_WIDTH,
            detail_box_mode: DescriptionBoxMode::Off,
            detail_box_max_width: 60,
            detail_box_lines: 5,
            detail_box_debounce_ms: 80,
            last_detail_layout: None,
            detail_redraw_notify: Arc::new(Notify::new()),
            mode_flash_notify: Arc::new(Notify::new()),
            detail_debounce: DetailDebounceState::default(),
            output_epoch: 0,
            overlay_render_generation: 0,
            pending_overlay_render: None,
            overlay_cleanup_generation: 0,
            pending_overlay_cleanup: None,
            tab_accepts_top: false,
            shell_kind,
            input_model: BufferModel::default(),
            manual_trigger_stashed: false,
            async_providers: vec![],
            ask_ai_provider: None,
            completion_cache: None,
            mode_flash: None,
        })
    }

    pub fn with_async_provider(mut self, provider: Arc<dyn AsyncProvider>) -> Self {
        self.async_providers.push(provider);
        self
    }

    pub fn with_completion_cache(
        mut self,
        cache: Option<Arc<crate::shell_completion::CompletionTreeCache>>,
    ) -> Self {
        self.completion_cache = cache;
        self
    }

    pub fn with_ask_ai_provider(mut self, provider: Option<Arc<llm::LlmProvider>>) -> Self {
        self.ask_ai_provider = provider;
        self
    }

    /// Hot-reload hook: replace the Ask AI provider (None disables the item).
    pub fn set_ask_ai_provider(&mut self, provider: Option<Arc<llm::LlmProvider>>) {
        self.ask_ai_provider = provider;
    }

    /// Clone of the Ask AI provider for the proxy's on-demand spawn task.
    pub fn ask_ai_provider(&self) -> Option<Arc<llm::LlmProvider>> {
        self.ask_ai_provider.clone()
    }

    fn ask_ai_active(&self) -> bool {
        self.ask_ai_provider.is_some()
    }

    pub fn has_async_providers(&self) -> bool {
        !self.async_providers.is_empty()
    }

    pub fn dynamic_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.dynamic_notify)
    }

    pub fn feedback_tick_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.feedback_tick_notify)
    }

    pub fn with_popup_config(mut self, max_visible: usize) -> Self {
        self.max_visible = max_visible;
        self
    }

    pub fn with_feedback_dismiss_ms(mut self, ms: u16) -> Self {
        self.feedback_dismiss_ms = ms;
        self
    }

    /// Set the maximum time (ms) to block waiting for a high-priority async
    /// generator before painting sync-only results. 0 disables bounded
    /// blocking. Corresponds to `config.popup.render_block_ms`.
    pub fn with_render_block_ms(mut self, ms: u64) -> Self {
        self.render_block_ms = ms;
        self
    }

    /// Return the current render-block budget in milliseconds.
    pub fn render_block_ms(&self) -> u64 {
        self.render_block_ms
    }

    /// Whether the accept key accepts the top suggestion when nothing has been
    /// navigated. Observable for the config-reload propagation test.
    pub fn tab_accepts_top(&self) -> bool {
        self.tab_accepts_top
    }

    /// Enable/disable accepting the top suggestion on the accept key (Tab) when
    /// nothing has been navigated. Corresponds to `config.popup.tab_accepts_top`.
    pub fn with_tab_accepts_top(mut self, enabled: bool) -> Self {
        self.tab_accepts_top = enabled;
        self
    }
    /// Return the current shell-kind classification. Used by the proxy's
    /// stdout task to decide whether to reset the keystroke buffer model.
    pub fn shell_kind(&self) -> ShellKind {
        self.shell_kind
    }
    pub fn with_shell_kind(mut self, kind: ShellKind) -> Self {
        self.shell_kind = kind;
        self
    }

    /// Set popup min/max width bounds (display columns). Stored on the
    /// handler and read on every render; further clamping against the live
    /// `screen_cols` happens inside `compute_layout`.
    pub fn with_popup_widths(mut self, min_width: u16, max_width: u16) -> Self {
        self.min_popup_width = min_width;
        self.max_popup_width = max_width;
        self
    }

    /// Configure the adjacent description box. `mode` toggles the feature;
    /// `max_width`, `lines`, and `debounce_ms` are stored verbatim — callers
    /// must pass values already clamped at config load time by
    /// `config::TermcmpConfig::normalize`.
    pub fn with_description_box(
        mut self,
        mode: DescriptionBoxMode,
        max_width: u16,
        lines: u16,
        debounce_ms: u16,
    ) -> Self {
        self.detail_box_mode = mode;
        self.detail_box_max_width = max_width;
        self.detail_box_lines = lines;
        self.detail_box_debounce_ms = debounce_ms as u64;
        self
    }

    /// Notify fired when the detail-box debounce window expires; the proxy
    /// listens and triggers a re-render.
    pub fn detail_redraw_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.detail_redraw_notify)
    }

    /// Notify handle the proxy's flash timer waits on.
    pub fn mode_flash_notify(&self) -> Arc<Notify> {
        Arc::clone(&self.mode_flash_notify)
    }

    /// Deadline of the active mode flash, if any.
    pub fn mode_flash_deadline(&self) -> Option<Instant> {
        self.mode_flash.as_ref().map(|(_, deadline)| *deadline)
    }

    /// Clear the mode flash so the footer reverts to the normal key hint.
    pub fn clear_mode_flash(&mut self) {
        self.mode_flash = None;
    }

    pub fn with_auto_trigger(mut self, auto_trigger: bool) -> Self {
        self.auto_trigger = auto_trigger;
        self
    }

    pub fn with_keybindings(mut self, keybindings: Keybindings) -> Self {
        self.keybindings = keybindings;
        self
    }

    pub fn with_theme(mut self, theme: PopupTheme) -> Self {
        self.theme = theme;
        self
    }

    fn reset_detail_debounce_state(&mut self) {
        self.detail_debounce.reset();
    }
    /// Label for the pinned on-demand "Ask AI" sentinel row.
    const ASK_AI_LABEL: &'static str = "Ask AI";

    /// Build the pinned "Ask AI" sentinel suggestion.
    fn ask_ai_sentinel() -> Suggestion {
        Suggestion {
            text: Self::ASK_AI_LABEL.to_string(),
            description: Some(
                "Ask AI — answer with a command, then press Enter to run".to_string(),
            ),
            kind: suggest::SuggestionKind::AskAi,
            source: suggest::SuggestionSource::Llm,
            ..Default::default()
        }
    }

    /// Return `pool` with any AskAi sentinel removed, then re-inserted at index 0
    /// iff Ask AI is active. Call on EVERY path that assigns `self.suggestions`
    /// so the item stays pinned and survives fuzzy filtering/re-ranking.
    fn pin_ask_ai(&self, mut pool: Vec<Suggestion>) -> Vec<Suggestion> {
        pool.retain(|s| s.kind != suggest::SuggestionKind::AskAi);
        if self.ask_ai_active() {
            pool.insert(0, Self::ask_ai_sentinel());
        }
        pool
    }

    fn replace_suggestions_and_reset_overlay(&mut self, suggestions: Vec<Suggestion>) {
        self.suggestions = self.pin_ask_ai(suggestions);
        self.overlay.reset();
        self.reset_detail_debounce_state();
    }

    fn stage_overlay_cleanup(&mut self, scope: CleanupScope) {
        self.overlay_cleanup_generation = self.overlay_cleanup_generation.wrapping_add(1);
        self.pending_overlay_render = None;
        self.pending_overlay_cleanup = Some(PendingOverlayCleanup {
            token: OverlayCleanupToken(self.overlay_cleanup_generation),
            scope,
        });
    }

    /// Apply suggestion engine configuration during the builder phase.
    ///
    /// # Contract
    ///
    /// - **Must be called before the handler is shared.** Internally this
    ///   `try_unwrap`s the engine `Arc`, which only succeeds while the
    ///   refcount is exactly 1. Once the handler has been wrapped in
    ///   `Arc<Mutex<InputHandler>>` and handed off to the proxy tasks
    ///   (see `proxy.rs`), calling this method will panic with
    ///   `"with_suggest_config called after engine was shared"`.
    /// - **Builder phase only.** Call site convention is a single chained
    ///   `.with_suggest_config(...)` on the freshly constructed handler,
    ///   before any `handle_*` / `process_key` call.
    /// - **If never called**, the engine uses whatever defaults
    ///   `SuggestionEngine::new()` installs (all providers on,
    ///   `DEFAULT_MAX_RESULTS` for both main and history pools).
    /// - **Eager fields.** The provider / result-cap parameters are
    ///   baked into the engine at construction. None of them change
    ///   thereafter without going through [`InputHandler::update_config`]
    ///   / a runtime reload path.
    /// - **Repeated calls** are supported in theory (each call consumes
    ///   `self` and rebuilds the engine) but the current call path in
    ///   `proxy.rs` only invokes it once, so treat it as idempotent-by-replacement.
    pub fn with_suggest_config(
        self,
        max_results: usize,
        commands: bool,
        max_history_results: usize,
        filesystem: bool,
    ) -> Self {
        // During builder phase the Arc has exactly one reference, so try_unwrap succeeds.
        let engine = Arc::try_unwrap(self.engine)
            .unwrap_or_else(|_| {
                panic!("internal invariant: engine Arc was captured by shared reference")
            })
            .with_suggest_config(max_results, commands, max_history_results, filesystem);
        Self {
            engine: Arc::new(engine),
            ..self
        }
    }

    /// Set the query match strategy (fuzzy subsequence vs contiguous
    /// substring) on the underlying engine. Builder-time only — must run
    /// before the engine `Arc` is shared, like [`Self::with_suggest_config`].
    pub fn with_match_mode(self, mode: config::MatchMode) -> Self {
        let engine = Arc::try_unwrap(self.engine)
            .unwrap_or_else(|_| {
                panic!("internal invariant: engine Arc was captured by shared reference")
            })
            .with_match_mode(mode);
        Self {
            engine: Arc::new(engine),
            ..self
        }
    }

    /// Set the source-group ordering on the underlying engine. Builder-time
    /// convenience — delegates to [`Self::set_live_suggest_config`] which also
    /// works after the engine `Arc` is shared (hot-reload path).
    pub fn with_source_order(self, order: suggest::SourceOrder) -> Self {
        let mut cfg = self.engine.config();
        cfg.source_order = order;
        self.engine.set_config(cfg);
        self
    }

    /// Hot-swap the live suggestion settings (match mode, source order,
    /// result caps, provider toggles) on the underlying engine. Works through
    /// the shared `Arc` via interior mutability — called by the config watcher.
    pub fn set_live_suggest_config(&self, cfg: suggest::LiveSuggestConfig) {
        self.engine.set_config(cfg);
    }

    /// Set the typing-pause debounce window. Builder-time convenience that
    /// stores the value in the shared atomic; the debounce loop reads it
    /// every cycle, so later updates via [`Self::set_delay_ms`] also apply.
    pub fn with_delay_ms(self, ms: u64) -> Self {
        self.delay_ms
            .store(ms, std::sync::atomic::Ordering::Relaxed);
        self
    }

    /// Hot-swap the debounce window. The running debounce loop picks up the
    /// new value on its next cycle without a restart.
    pub fn set_delay_ms(&self, ms: u64) {
        self.delay_ms
            .store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// The shared debounce-window atomic, cloned for the debounce loop task.
    pub fn delay_ms_atomic(&self) -> Arc<std::sync::atomic::AtomicU64> {
        Arc::clone(&self.delay_ms)
    }

    /// Replace the async provider set (LLM + shell-native completions).
    /// Called by the config watcher when `[ai]` or
    /// `suggest.providers.shell_completions` changes. The next trigger
    /// spawns the new set.
    pub fn set_async_providers(&mut self, providers: Vec<Arc<dyn suggest::AsyncProvider>>) {
        self.async_providers = providers;
    }

    /// Hot-reload hook: replace the persistent completion tree cache.
    /// Called by the config watcher alongside [`set_async_providers`] so a
    /// `suggest.providers.shell_completions` toggle swaps both the providers
    /// and their shared cache in lockstep.
    pub fn set_completion_cache(
        &mut self,
        cache: Option<Arc<crate::shell_completion::CompletionTreeCache>>,
    ) {
        self.completion_cache = cache;
    }

    /// Re-rank a merged candidate `pool` against the live typed word.
    ///
    /// Shared by the two async merge paths (the bounded-block first paint and
    /// `try_merge_dynamic`) so both honor the engine's [`MatchMode`] identically
    /// — keeping them in lockstep instead of duplicating the branch at each
    /// call site.
    ///
    /// An empty `buffer` sorts by source-order, priority, then text — but
    /// does NOT truncate. Truncating on an empty query would evict high-value
    /// candidates from large dynamic pools before a later non-empty re-rank
    /// can recover them. A non-empty `buffer` (the current word at every call
    /// site) filters and ranks under the engine's configured match mode,
    /// capped at `max_visible * 5`. Frizbee's score order is final — no
    /// post-sort.
    fn rerank_live(&self, buffer: &str, mut pool: Vec<Suggestion>) -> Vec<Suggestion> {
        if buffer.is_empty() {
            let order = self.engine.source_order();
            pool.sort_by(|a, b| {
                order
                    .rank(a.source)
                    .cmp(&order.rank(b.source))
                    .then_with(|| {
                        suggest::priority::effective(b).cmp(&suggest::priority::effective(a))
                    })
                    .then_with(|| a.text.cmp(&b.text))
            });
            return pool;
        }
        suggest::fuzzy::rank_with_mode(buffer, pool, self.max_visible * 5, self.engine.match_mode())
    }

    /// Update runtime-configurable fields without restarting the proxy.
    /// Called by the config file watcher when config.toml changes on disk.
    /// Returns cleanup bytes to write to stdout (e.g. popup clear on
    /// auto_trigger toggle, or detail-box clear when description_box is
    /// disabled at runtime).
    #[allow(clippy::too_many_arguments)]
    pub fn update_config(
        &mut self,
        theme: PopupTheme,
        keybindings: Keybindings,
        max_visible: usize,
        feedback_dismiss_ms: u16,
        auto_trigger: bool,
        min_popup_width: u16,
        max_popup_width: u16,
        detail_box_mode: DescriptionBoxMode,
        detail_box_max_width: u16,
        detail_box_lines: u16,
        detail_box_debounce_ms: u16,
        render_block_ms: u64,
        tab_accepts_top: bool,
    ) -> Vec<u8> {
        let mut cleanup = Vec::new();
        let mut cleanup_scope: Option<CleanupScope> = None;
        let mut detail_cleanup_staged = false;

        // If auto_trigger is being disabled, tear down all pending state —
        // not just the visible popup.  A pending trigger_requested or in-flight
        // dynamic_task can survive without the popup being visible (e.g. the
        // debounce timer set trigger_requested but trigger() hasn't fired yet).
        if self.auto_trigger && !auto_trigger {
            if self.visible {
                if let Some(layout) = self.last_layout.clone() {
                    self.bump_output_epoch();
                    clear_popup_unframed(&mut cleanup, &layout);
                    if let Some(ref det) = self.last_detail_layout {
                        clear_detail_box(&mut cleanup, det);
                        detail_cleanup_staged = true;
                    }
                    cleanup_scope = Some(CleanupScope::MainAndDetail);
                } else if let Some(det) = self.last_detail_layout.clone() {
                    self.bump_output_epoch();
                    clear_detail_box(&mut cleanup, &det);
                    cleanup_scope = Some(CleanupScope::MainAndDetail);
                    detail_cleanup_staged = true;
                }
                self.visible = false;
                self.suggestions.clear();
                self.overlay.reset();
                self.reset_detail_debounce_state();
            }
            if let Some(handle) = self.dynamic_task.take() {
                handle.abort();
            }
            self.dynamic_rx = None;
            self.dynamic_ctx = None;
            self.feedback = AsyncFeedback::Idle;
            self.pending_failed.clear();
            self.pending_empty_count = 0;
            self.trigger_requested = false;
        }

        self.theme = theme;
        self.keybindings = keybindings;
        self.max_visible = max_visible;
        self.feedback_dismiss_ms = feedback_dismiss_ms;
        self.auto_trigger = auto_trigger;
        self.min_popup_width = min_popup_width;
        self.max_popup_width = max_popup_width;
        if self.detail_box_mode != DescriptionBoxMode::Off
            && detail_box_mode == DescriptionBoxMode::Off
        {
            if !detail_cleanup_staged {
                if let Some(det) = self.last_detail_layout.clone() {
                    self.bump_output_epoch();
                    clear_detail_box(&mut cleanup, &det);
                    cleanup_scope.get_or_insert(CleanupScope::DetailOnly);
                }
            }
            self.reset_detail_debounce_state();
        }
        if let Some(scope) = cleanup_scope {
            self.stage_overlay_cleanup(scope);
        }
        self.detail_box_mode = detail_box_mode;
        self.detail_box_max_width = detail_box_max_width;
        self.detail_box_lines = detail_box_lines;
        self.detail_box_debounce_ms = detail_box_debounce_ms as u64;
        self.render_block_ms = render_block_ms;
        self.tab_accepts_top = tab_accepts_top;

        cleanup
    }

    pub fn has_pending_trigger(&self) -> bool {
        self.trigger_requested
    }

    pub fn clear_trigger_request(&mut self) {
        self.trigger_requested = false;
    }
    pub fn take_manual_trigger_stashed(&mut self) -> bool {
        let v = self.manual_trigger_stashed;
        self.manual_trigger_stashed = false;
        v
    }
    pub fn set_manual_trigger_stashed(&mut self) {
        self.manual_trigger_stashed = true;
    }

    pub fn is_debounce_suppressed(&self) -> bool {
        self.debounce_suppressed
    }

    pub fn auto_trigger_enabled(&self) -> bool {
        self.auto_trigger
    }

    // The #[doc(hidden)] pub accessors below exist solely so integration tests
    // can simulate generator drift / drive the rx channel directly. They are
    // not part of the supported API.

    /// Restore a channel receiver that was taken out for an awaited bounded-block
    /// window but was not consumed (e.g. due to keystroke cancellation). This
    /// allows `dynamic_merge_loop` to pick up the result when the generator
    /// eventually completes.
    #[doc(hidden)]
    pub fn restore_dynamic_rx(&mut self, rx: mpsc::Receiver<DynamicResult>) {
        self.dynamic_rx = Some(rx);
    }

    /// Returns whether the `dynamic_rx` channel is set (a generator is pending).
    #[doc(hidden)]
    pub fn has_dynamic_rx(&self) -> bool {
        self.dynamic_rx.is_some()
    }

    /// Returns whether `dynamic_task` is set (a generator task handle is owned).
    #[doc(hidden)]
    pub fn has_dynamic_task(&self) -> bool {
        self.dynamic_task.is_some()
    }

    /// Takes the `dynamic_rx` channel out of the handler, leaving `None`.
    #[doc(hidden)]
    pub fn take_dynamic_rx(&mut self) -> Option<mpsc::Receiver<DynamicResult>> {
        self.dynamic_rx.take()
    }

    #[doc(hidden)]
    pub fn feedback_kind(&self) -> &AsyncFeedback {
        &self.feedback
    }

    #[doc(hidden)]
    pub fn output_epoch(&self) -> u64 {
        self.output_epoch
    }

    #[doc(hidden)]
    pub fn theme(&self) -> &PopupTheme {
        &self.theme
    }

    #[doc(hidden)]
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Test-only: force the popup visible with the given suggestions so
    /// hot-reload repaint paths can be exercised without a real trigger.
    #[doc(hidden)]
    pub fn force_visible_for_test(&mut self, suggestions: Vec<Suggestion>) {
        self.replace_suggestions_and_reset_overlay(suggestions);
        self.visible = true;
        self.last_layout = Some(PopupLayout {
            start_row: 5,
            start_col: 0,
            width: 20,
            height: 1,
            scroll_deficit: 0,
        });
    }

    pub(crate) fn overlay_write_ticket(&self) -> OverlayWriteTicket {
        OverlayWriteTicket {
            epoch: self.output_epoch,
            render_token: self
                .pending_overlay_render
                .as_ref()
                .map(|pending| pending.token),
            cleanup_token: self
                .pending_overlay_cleanup
                .as_ref()
                .map(|pending| pending.token),
        }
    }

    pub(crate) fn commit_overlay_write(&mut self, ticket: OverlayWriteTicket) {
        if let Some(token) = ticket.render_token {
            if let Some(pending) = self
                .pending_overlay_render
                .take_if(|pending| pending.token == token)
            {
                self.overlay_scroll_deficit = pending.layout.scroll_deficit;
                self.last_layout = Some(pending.layout);
                self.last_detail_layout = pending.detail_layout;
            }
        }

        if let Some(token) = ticket.cleanup_token {
            if let Some(pending) = self
                .pending_overlay_cleanup
                .take_if(|pending| pending.token == token)
            {
                if matches!(pending.scope, CleanupScope::MainAndDetail) {
                    self.last_layout = None;
                }
                self.last_detail_layout = None;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn has_overlay_ownership(&self) -> bool {
        self.visible || self.last_layout.is_some() || !matches!(self.feedback, AsyncFeedback::Idle)
    }

    pub(crate) fn discard_overlay_ownership_after_stale_write(
        &mut self,
        ticket: OverlayWriteTicket,
    ) {
        if let Some(token) = ticket.cleanup_token {
            if self
                .pending_overlay_cleanup
                .take_if(|pending| pending.token == token)
                .is_some()
            {
                return;
            }
        }

        let Some(token) = ticket.render_token else {
            return;
        };
        let Some(pending) = self.pending_overlay_render.as_ref() else {
            return;
        };
        if pending.token != token {
            return;
        }

        self.pending_overlay_render = None;
        self.visible = false;
        self.suggestions.clear();
        self.overlay.reset();
        self.last_layout = None;
        self.last_detail_layout = None;
        self.reset_detail_debounce_state();
        self.debounce_suppressed = false;
        if let Some(handle) = self.dynamic_task.take() {
            handle.abort();
        }
        self.dynamic_rx = None;
        self.dynamic_ctx = None;
        self.feedback = AsyncFeedback::Idle;
        self.pending_failed.clear();
        self.pending_empty_count = 0;
        self.last_trigger_fingerprint = None;
        self.bump_output_epoch();
    }

    /// Returns the current suggestions slice (read-only).
    #[doc(hidden)]
    pub fn current_suggestions(&self) -> &[Suggestion] {
        &self.suggestions
    }

    /// Set the `spawned_generation` field, simulating a `spawn_async_providers` run
    /// for the current `buffer_generation`.
    #[doc(hidden)]
    pub fn set_spawned_generation(&mut self, gen: u64) {
        self.spawned_generation = gen;
    }

    #[doc(hidden)]
    pub fn set_buffer_generation(&mut self, gen: u64) {
        self.buffer_generation = gen;
    }

    #[doc(hidden)]
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    #[cfg(test)]
    pub(crate) fn test_with_visible_suggestions(
        mut self,
        suggestions: Vec<Suggestion>,
        selected: usize,
    ) -> Self {
        self.suggestions = suggestions;
        self.visible = true;
        self.overlay.selected = Some(selected);
        self.last_layout = Some(PopupLayout {
            start_row: 5,
            start_col: 0,
            width: 20,
            height: 1,
            scroll_deficit: 0,
        });
        self
    }

    #[cfg(test)]
    pub(crate) fn displayed_detail_idx_for_test(&self) -> Option<usize> {
        self.detail_debounce.displayed_idx
    }

    #[cfg(test)]
    pub(crate) fn detail_debounce_pending_for_test(&self) -> bool {
        self.detail_debounce.pending
    }

    #[cfg(test)]
    pub(crate) fn set_detail_debounce_pending_for_test(&mut self, pending: bool) {
        self.detail_debounce.pending = pending;
    }

    #[cfg(test)]
    pub(crate) fn set_last_repaint_at_for_test(&mut self, at: Option<Instant>) {
        self.last_repaint_at = at;
    }

    /// Prime `dynamic_ctx` to the "no context" state that matches an empty
    /// buffer, bypassing `spawn_async_providers`.
    #[doc(hidden)]
    pub fn prime_dynamic_ctx_for_empty_buffer(&mut self) {
        let base_ctx = buffer::parse_command_context("", 0);
        self.dynamic_ctx = Some(DynamicCtxSnapshot::capture(&base_ctx));
    }

    /// Prime `dynamic_ctx` from an arbitrary buffer + cursor so tests can
    /// drive the staleness check against a non-empty live buffer.
    #[doc(hidden)]
    pub fn prime_dynamic_ctx_for_buffer(&mut self, buffer: &str, cursor: usize) {
        let base_ctx = buffer::parse_command_context(buffer, cursor);
        self.dynamic_ctx = Some(DynamicCtxSnapshot::capture(&base_ctx));
    }

    /// Seed `dynamic_task` with a no-op spawn handle so tests can verify
    /// abort/clear semantics without spinning up a real generator.
    #[doc(hidden)]
    pub fn seed_dynamic_task_noop(&mut self) {
        self.dynamic_task = Some(tokio::spawn(async {}));
    }

    /// Read the last trigger fingerprint so tests can assert `apply_block_result`
    /// stamped it on success / left it untouched on stale.
    #[doc(hidden)]
    pub fn last_trigger_fingerprint(&self) -> Option<TriggerFingerprint> {
        self.last_trigger_fingerprint
    }

    /// Process a single key event. Returns a `KeyOutcome`: bytes to forward
    /// to the PTY (`Forward`, empty if the key was intercepted by the popup),
    /// or `AskAiAccept` when the user accepted the on-demand "Ask AI" item.
    pub fn process_key(
        &mut self,
        key: &KeyEvent,
        parser: &Arc<Mutex<TerminalParser>>,
        stdout: &mut dyn Write,
    ) -> KeyOutcome {
        if self.visible {
            self.process_key_visible(key, parser, stdout)
        } else {
            self.process_key_hidden(key, parser, stdout)
        }
    }

    /// Apply a keystroke to the local buffer model and push the result into
    /// the parser's `command_buffer` if the model changed. Only active for
    /// non-zsh shells.
    ///
    /// Returns `true` if the model changed and was pushed to the parser
    /// (raising `buffer_dirty` + `buffer_pending_display`).
    fn maybe_apply_input_model(
        &mut self,
        key: &KeyEvent,
        parser: &Arc<Mutex<TerminalParser>>,
        _stdout: &mut dyn Write,
    ) -> bool {
        if self.shell_kind == ShellKind::Zsh {
            return false;
        }
        if !self.input_model.apply_key(key) {
            return false;
        }
        // Don't propagate drift resets (Escape, history recall, Tab completion,
        // paste) to the parser as an empty buffer — the model was cleared because
        // we lost track of the real line, and wiping command_buffer would prevent
        // manual Ctrl+/ from re-triggering. A genuine deletion to empty (Backspace,
        // Ctrl-U, Ctrl-W) IS the real line state, though: propagate it so the
        // trigger it raises dismisses the now-stale popup instead of leaving it
        // floating over an empty prompt.
        if self.input_model.buffer.is_empty()
            && !matches!(
                key,
                KeyEvent::Backspace | KeyEvent::Ctrl('u') | KeyEvent::Ctrl('w')
            )
        {
            return false;
        }
        match parser.lock() {
            Ok(mut p) => {
                p.state_mut()
                    .set_command_buffer(self.input_model.buffer.clone(), self.input_model.cursor);
                true
            }
            Err(e) => {
                tracing::warn!("parser mutex poisoned in maybe_apply_input_model: {e}");
                false
            }
        }
    }

    fn process_key_visible(
        &mut self,
        key: &KeyEvent,
        parser: &Arc<Mutex<TerminalParser>>,
        stdout: &mut dyn Write,
    ) -> KeyOutcome {
        // The toggle_match_mode key is a termcmp-internal action that must NOT
        // touch the keystroke buffer model. Check it before
        // maybe_apply_input_model, which would classify Ctrl('r') (the default
        // binding) as a non-modeled event and wipe the model — the same bug
        // process_key_hidden guards against for the trigger key.
        if key == &self.keybindings.toggle_match_mode {
            self.toggle_match_mode(parser, stdout);
            return KeyOutcome::Forward(Vec::new());
        }

        // Configurable actions checked first via if-chain. These keys are
        // intercepted by the popup and never forwarded to the shell, so the
        // input model must NOT see them — applying e.g. ArrowUp would trigger
        // a drift reset (history recall) that desyncs the model from the
        // shell's real buffer.
        let visible_rows = self.effective_navigation_visible_rows(parser);

        if key == &self.keybindings.navigate_up {
            self.overlay.move_up();
            self.render(parser, stdout);
            return KeyOutcome::Forward(Vec::new());
        }
        if key == &self.keybindings.navigate_down {
            self.overlay.move_down(self.suggestions.len(), visible_rows);
            self.render(parser, stdout);
            return KeyOutcome::Forward(Vec::new());
        }
        if key == &self.keybindings.accept {
            if self.effective_selection_is_ask_ai() {
                // On-demand Ask AI: keep popup visible (loading spinner renders in
                // the indicator row); the proxy runs the LLM and injects the answer.
                // No bytes are forwarded now; nothing is auto-executed.
                return KeyOutcome::AskAiAccept;
            }
            if self.effective_selected().is_none() {
                self.dismiss(stdout);
                return KeyOutcome::Forward(key_to_bytes(key));
            }
            return KeyOutcome::Forward(self.accept_with_chaining(parser, stdout));
        }
        if key == &self.keybindings.accept_and_enter {
            if self.effective_selection_is_ask_ai() {
                // Safety: never auto-run an AI answer. Divert to the same on-demand
                // path (fills buffer, user presses Enter themselves).
                return KeyOutcome::AskAiAccept;
            }
            if self.overlay.selected.is_some() {
                let mut forward = self.accept_suggestion(parser);
                self.dismiss(stdout);
                forward.push(0x0D);
                return KeyOutcome::Forward(forward);
            } else {
                self.dismiss(stdout);
                return KeyOutcome::Forward(vec![0x0D]);
            }
        }
        if key == &self.keybindings.dismiss {
            self.dismiss(stdout);
            return KeyOutcome::Forward(Vec::new());
        }

        match key {
            KeyEvent::PageUp => {
                self.overlay.move_page_up(visible_rows);
                self.render(parser, stdout);
                return KeyOutcome::Forward(Vec::new());
            }
            KeyEvent::PageDown => {
                self.overlay
                    .move_page_down(self.suggestions.len(), visible_rows);
                self.render(parser, stdout);
                return KeyOutcome::Forward(Vec::new());
            }
            KeyEvent::Home
            | KeyEvent::HomeCsiTilde
            | KeyEvent::HomeCsi7Tilde
            | KeyEvent::HomeSs3 => {
                self.overlay.move_home(self.suggestions.len());
                self.render(parser, stdout);
                return KeyOutcome::Forward(Vec::new());
            }
            KeyEvent::End | KeyEvent::EndCsiTilde | KeyEvent::EndCsi8Tilde | KeyEvent::EndSs3 => {
                self.overlay.move_end(self.suggestions.len(), visible_rows);
                self.render(parser, stdout);
                return KeyOutcome::Forward(Vec::new());
            }
            _ => {}
        }

        // All keys below are forwarded to the shell, so update the input
        // model to keep it in sync with what the shell will actually see.
        self.maybe_apply_input_model(key, parser, stdout);

        // Remaining structural keys/default visible-popup handling.
        match key {
            KeyEvent::ArrowLeft | KeyEvent::ArrowRight => {
                self.dismiss(stdout);
                KeyOutcome::Forward(key_to_bytes(key))
            }
            KeyEvent::Printable(_) | KeyEvent::Backspace => {
                let forward = key_to_bytes(key);
                // Keep the popup visible and re-filter it in place once the shell (or, for
                // non-zsh, the keystroke BufferModel via maybe_apply_input_model above)
                // updates the command buffer. The re-trigger pipeline (trigger_requested ->
                // buffer_dirty -> trigger()/prepare_trigger_with_block -> render_at) repaints
                // over the existing layout with no close/reopen gap.
                self.trigger_requested = true;
                KeyOutcome::Forward(forward)
            }
            _ => {
                self.dismiss(stdout);
                KeyOutcome::Forward(key_to_bytes(key))
            }
        }
    }

    fn effective_navigation_visible_rows(&self, parser: &Arc<Mutex<TerminalParser>>) -> usize {
        let border_pad: u16 = if self.theme.borders { 2 } else { 0 };
        let min_screen = 1 + border_pad;

        let screen_rows = match parser.lock() {
            Ok(p) => p.state().screen_dimensions().0,
            Err(e) => {
                tracing::warn!(
                    "parser mutex poisoned while computing popup navigation height: {e} — \
                     using configured max_visible"
                );
                return self.max_visible.max(1);
            }
        };

        if screen_rows > min_screen {
            self.max_visible
                .min((screen_rows - 1 - border_pad) as usize)
                .max(1)
        } else {
            self.max_visible.max(1)
        }
    }

    /// The suggestion index the accept path should act on.
    ///
    /// Normally this is the navigated selection (`overlay.selected`). When
    /// `tab_accepts_top` is enabled and the user has not navigated yet, it
    /// falls back to the first real completion — the first suggestion that
    /// is NOT the pinned "Ask AI" sentinel — so Tab accepts the top-ranked
    /// completion instead of triggering on-demand Ask AI (issue #150).
    /// Explicit navigation onto the sentinel still resolves via
    /// `overlay.selected`.
    ///
    /// Returns `None` when there is genuinely nothing to accept: no navigation
    /// and either the flag is off or the list has no real completion (empty,
    /// or holding only the sentinel).
    fn effective_selected(&self) -> Option<usize> {
        if let Some(selected) = self.overlay.selected {
            return Some(selected);
        }
        if !self.tab_accepts_top {
            return None;
        }
        self.suggestions
            .iter()
            .position(|s| s.kind != suggest::SuggestionKind::AskAi)
    }

    /// True when the effective selection is the pinned "Ask AI" sentinel.
    fn effective_selection_is_ask_ai(&self) -> bool {
        self.effective_selected()
            .and_then(|i| self.suggestions.get(i))
            .is_some_and(|s| s.kind == suggest::SuggestionKind::AskAi)
    }

    /// Accept the current suggestion, with directory chaining for paths ending in '/'.
    fn accept_with_chaining(
        &mut self,
        parser: &Arc<Mutex<TerminalParser>>,
        stdout: &mut dyn Write,
    ) -> Vec<u8> {
        let selected_idx = match self.effective_selected() {
            Some(idx) if idx < self.suggestions.len() => idx,
            _ => {
                self.dismiss(stdout);
                return Vec::new();
            }
        };

        let selected_text = self.suggestions[selected_idx].text.clone();
        let selected_kind = self.suggestions[selected_idx].kind;
        let is_dir = selected_text.ends_with('/');

        // Single parser lock for both the accept computation AND the
        // CD-chaining prediction. Prevents TOCTOU between the two reads and
        // mirrors the lock-ordering discipline established in proxy.rs.
        //
        // Poison handling mirrors render/accept_suggestion: if the parser
        // mutex is poisoned we can't read the buffer, so dismiss the popup
        // and return empty bytes. Failing here must not crash the proxy.
        let mut p = match parser.lock() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    "parser mutex poisoned in accept_with_chaining: {e} — \
                     dropping accept"
                );
                self.dismiss(stdout);
                return Vec::new();
            }
        };

        let AcceptLocked {
            forward,
            cwd,
            cursor,
            screen,
            escaped_replacement,
        } = match self.accept_suggestion_locked(&p) {
            Some(accepted) => accepted,
            None => {
                drop(p);
                self.dismiss(stdout);
                return Vec::new();
            }
        };

        // History entries never chain — they're full commands, not directory paths.
        if selected_kind == suggest::SuggestionKind::History {
            drop(p);
            self.dismiss(stdout);
            return forward;
        }

        if !is_dir {
            if self.shell_kind != ShellKind::Zsh {
                if selected_kind == suggest::SuggestionKind::ProviderValue
                    || selected_kind == suggest::SuggestionKind::Llm
                {
                    // Full-line accept replaced the entire buffer; sync the
                    // keystroke model to the post-accept state so the next
                    // keystroke doesn't replay against the stale pre-accept buffer.
                    let new_buf = if selected_text.ends_with('=') {
                        selected_text.clone()
                    } else {
                        format!("{selected_text} ")
                    };
                    self.input_model.buffer = new_buf.clone();
                    self.input_model.cursor = new_buf.chars().count();
                } else {
                    let state = p.state();
                    if let Some(buf) = state.command_buffer() {
                        self.input_model.buffer = buf.to_string();
                        self.input_model.cursor = state.buffer_cursor();
                    }
                }
            }
            drop(p);
            self.dismiss(stdout);
            if !selected_text.ends_with('=') {
                return [forward, vec![b' ']].concat();
            }
            return forward;
        }

        // CD chaining: predict the buffer after acceptance and immediately
        // show next-level suggestions. Avoids timing issues with the shell's
        // OSC 7770 roundtrip. Reuses the already-held parser lock for the
        // prediction read and the `predict_command_buffer` mutation.
        let state = p.state();
        let buffer = state.command_buffer().unwrap_or("").to_string();
        let char_cursor = state.buffer_cursor(); // character offset
        let byte_cursor = char_to_byte_offset(&buffer, char_cursor);
        // Use the RAW on-screen word start, not `byte_cursor -
        // old_ctx.current_word.len()`: the tokenizer-decoded `current_word`
        // strips backslashes/quotes, so on a double-chain into a
        // space-containing dir (buffer already `cd My\ Folder/`) the decoded
        // length is shorter than the on-screen span and would mis-slice the
        // predicted buffer, leaving a stray fragment of the previous word.
        let raw_word_start = current_word_raw_start(&buffer, byte_cursor);
        // Mirror the accept path: in a quoted context the opening quote is
        // structural and `escaped_replacement` is bare text that relies on it
        // surviving. Slice the predicted buffer so the opening quote is kept
        // (delete starts after it), otherwise the predicted buffer drifts from
        // the bytes the shell actually receives.
        let old_quote = parse_command_context(&buffer, char_cursor).quote_state;
        let word_start_bytes =
            current_word_delete_start(&buffer, raw_word_start, byte_cursor, old_quote);

        // The shell receives `escaped_replacement` from `forward`; the
        // predicted buffer must use the same text so the next suggestion
        // round sees the bytes the shell will see, not the raw display text.
        let mut predicted = String::with_capacity(buffer.len() + escaped_replacement.len());
        predicted.push_str(&buffer[..word_start_bytes]);
        predicted.push_str(&escaped_replacement);
        if byte_cursor < buffer.len() {
            predicted.push_str(&buffer[byte_cursor..]);
        }
        // new_cursor is a char offset for predict_command_buffer
        let word_start_chars = byte_to_char_offset(&buffer, word_start_bytes);
        let new_cursor = word_start_chars + escaped_replacement.chars().count();

        let predicted_ctx = parse_command_context(&predicted, new_cursor);
        let predicted_buffer = predicted.clone();

        // Update parser with predicted buffer so subsequent accept computes
        // correct current_word.
        p.state_mut().predict_command_buffer(predicted, new_cursor);
        if self.shell_kind != ShellKind::Zsh {
            self.input_model.buffer = predicted_buffer.clone();
            self.input_model.cursor = new_cursor;
        }
        drop(p);

        let (cr, cc) = (cursor.row, cursor.col);
        let (sr, sc) = (screen.rows, screen.cols);

        match self
            .engine
            .suggest_sync(&predicted_ctx, &cwd, &predicted_buffer)
        {
            Ok(result) if !result.suggestions.is_empty() => {
                self.replace_suggestions_and_reset_overlay(result.suggestions);
                self.visible = true;
                self.render_at(stdout, cr, cc, sr, sc);
            }
            _ => {
                self.dismiss(stdout);
            }
        }

        forward
    }

    fn process_key_hidden(
        &mut self,
        key: &KeyEvent,
        parser: &Arc<Mutex<TerminalParser>>,
        stdout: &mut dyn Write,
    ) -> KeyOutcome {
        // The manual trigger key must be checked BEFORE it is applied to the
        // keystroke buffer model. A trigger bound to a control letter (e.g.
        // ctrl+o) would otherwise be classified by BufferModel::apply_key as a
        // buffer-resetting control char, wiping the command line we're about to
        // complete. CtrlSlash (the default) is special-cased to not reset, but
        // any ctrl+letter trigger hits the reset arm. Handle the trigger first
        // so the model keeps the buffer the user typed.
        if key == &self.keybindings.trigger {
            self.debounce_suppressed = false;
            // If the most recent set_command_buffer hasn't been followed by a
            // display-changing op yet (the shell hasn't echoed the last typed
            // char), defer the trigger through Task B's pending_trigger stash so
            // the popup anchors to fresh cursor geometry.
            let pending_display = match parser.lock() {
                Ok(p) => p.state().buffer_pending_display(),
                Err(_) => false,
            };
            tracing::debug!(
                pending_display,
                buffer = %self.input_model.buffer,
                "manual trigger key pressed"
            );
            if pending_display {
                // Re-raise buffer_dirty with the current model contents so Task B
                // sees a fresh event and stashes a Trigger.
                if self.shell_kind != ShellKind::Zsh && !self.input_model.buffer.is_empty() {
                    if let Ok(mut p) = parser.lock() {
                        p.state_mut().set_command_buffer(
                            self.input_model.buffer.clone(),
                            self.input_model.cursor,
                        );
                    }
                }
                self.trigger_requested = true;
                self.manual_trigger_stashed = true;
            } else {
                self.trigger(parser, stdout);
            }
            return KeyOutcome::Forward(Vec::new());
        }
        self.maybe_apply_input_model(key, parser, stdout);
        match key {
            KeyEvent::Printable(c) => {
                self.debounce_suppressed = false;
                let mut buf = [0u8; 4];
                let forward = c.encode_utf8(&mut buf).as_bytes().to_vec();
                KeyOutcome::Forward(forward)
            }
            KeyEvent::ArrowUp | KeyEvent::ArrowDown => {
                // History navigation — suppress debounce so the popup doesn't
                // trigger on buffer changes from shell history recall.
                self.debounce_suppressed = true;
                KeyOutcome::Forward(key_to_bytes(key))
            }
            _ => KeyOutcome::Forward(key_to_bytes(key)),
        }
    }

    pub fn trigger(&mut self, parser: &Arc<Mutex<TerminalParser>>, stdout: &mut dyn Write) {
        // Suppress popups while a TUI app owns the alternate screen (nvim,
        // less, htop, tmux). The proxy drains `alt_screen_changed` to dismiss
        // any already-visible popup on transition; this gate stops new ones.
        {
            let in_alt = match parser.lock() {
                Ok(p) => p.state().in_alt_screen(),
                Err(_) => false,
            };
            if in_alt {
                tracing::debug!("suppressing popup: alt screen active");
                return;
            }
        }
        // Poison handling mirrors render/accept_suggestion: trigger() is the
        // main entry point of the suggestion pipeline (debounce loop, Task B
        // buffer_dirty/cwd_dirty, SIGWINCH). If the parser mutex is poisoned
        // we can't read the buffer or cursor, so log and bail out — the next
        // PTY input drives a retry. Propagating the poison here would crash
        // the proxy.
        let (buffer, cursor, cwd, shell_env, cursor_row, cursor_col, screen_rows, screen_cols) =
            match parser.lock() {
                Ok(mut p) => {
                    // CPR sync means the parser's cursor_row now reflects reality,
                    // so any accumulated scroll deficit from prior renders is stale.
                    if p.state_mut().take_cpr_synced() {
                        self.overlay_scroll_deficit = 0;
                    }
                    let state = p.state();
                    let buffer = state.command_buffer().unwrap_or("").to_string();
                    let cursor = state.buffer_cursor();
                    let cwd = state.cwd().cloned().unwrap_or_else(|| PathBuf::from("."));
                    let shell_env = state.shell_env().cloned();
                    let (cursor_row, cursor_col) = state.cursor_position();
                    let (screen_rows, screen_cols) = state.screen_dimensions();
                    (
                        buffer,
                        cursor,
                        cwd,
                        shell_env,
                        cursor_row,
                        cursor_col,
                        screen_rows,
                        screen_cols,
                    )
                }
                Err(e) => {
                    tracing::warn!("parser mutex poisoned in trigger: {e} — skipping trigger");
                    return;
                }
            };

        if buffer.is_empty() {
            if self.visible {
                self.dismiss(stdout);
            }
            return;
        }

        // Idempotency guard: if the buffer + cursor are unchanged since the
        // last trigger that populated a still-visible popup, skip the whole
        // trigger body. Two reasons:
        //   1. `suggest_sync` would return the same results — redundant work.
        //   2. The empty-sync + no-async branch below calls `dismiss()`,
        //      which would nuke a popup that had been populated by a prior
        //      trigger's async generator merge (the sync pass sees empty,
        //      but the visible content came from async). See the bug-repro
        //      test `test_trigger_idempotent_when_buffer_unchanged`.
        // `dismiss()` invalidates the fingerprint, and a genuine buffer
        // edit produces a different fingerprint — so ESC-dismiss and real
        // edits both take the full trigger path as before. The async
        // merge path (`try_merge_dynamic`) is separate and unaffected.
        let fingerprint = buffer_fingerprint(&buffer, cursor, shell_env.as_ref());
        if self.visible && self.last_trigger_fingerprint == Some(fingerprint) {
            return;
        }

        let ctx = parse_command_context(&buffer, cursor);

        // Synchronous tree-cache resolution. On a hit the cached shell
        // completions are merged into the sync suggestions below. Backfill
        // providers (fish/zsh) are skipped on a hit; live providers (LLM)
        // still fire. This is what makes repeat completions instant.
        let (cached_suggestions, cache_hit) = match self.completion_cache.as_ref() {
            Some(cache) => cache.resolve(&ctx, &buffer),
            None => (Vec::new(), false),
        };
        let (live_providers, backfill_providers) = self.partition_async_providers();
        // Live providers (LLM) fire on every trigger; backfill providers
        // (fish/zsh) only on a cache miss.
        let providers_to_spawn: Vec<Arc<dyn AsyncProvider>> = if cache_hit {
            live_providers
        } else {
            live_providers
                .into_iter()
                .chain(backfill_providers)
                .collect()
        };

        // Abort any in-flight generator task before dropping the receiver,
        // otherwise the spawned task leaks (tx.send blocks on dropped rx).
        if let Some(handle) = self.dynamic_task.take() {
            handle.abort();
        }
        self.dynamic_rx = None;
        self.dynamic_ctx = None;
        self.feedback = AsyncFeedback::Idle;
        self.pending_failed.clear();
        self.pending_empty_count = 0;

        self.buffer_generation = self.buffer_generation.wrapping_add(1);

        let sync_result =
            self.engine
                .suggest_sync_with_env(&ctx, &cwd, &buffer, shell_env.as_ref());
        // When Ask AI is active, guarantee the sentinel is present so the popup
        // shows even with zero sync matches; pin_ask_ai (in
        // replace_suggestions_and_reset_overlay) keeps it at index 0 after ranking.
        let sync_result = match sync_result {
            Ok(mut r) if self.ask_ai_active() => {
                if !r
                    .suggestions
                    .iter()
                    .any(|s| s.kind == suggest::SuggestionKind::AskAi)
                {
                    r.suggestions.push(Self::ask_ai_sentinel());
                }
                Ok(r)
            }
            other => other,
        };

        // Merge cached tree-completion rows into the sync pool (dedup by
        // text), then re-rank the combined pool so cached rows compete on
        // score rather than being appended raw after the sync results.
        let has_cached = !cached_suggestions.is_empty();
        let mut suggestions = match sync_result {
            Ok(r) => r.suggestions,
            Err(e) => {
                tracing::debug!("suggest_sync failed: {e}");
                Vec::new()
            }
        };
        suggestions = merge_cached_suggestions(suggestions, cached_suggestions);
        if has_cached {
            suggestions = self.rerank_live(&ctx.current_word, suggestions);
        }

        if !suggestions.is_empty() {
            self.replace_suggestions_and_reset_overlay(suggestions);
            self.visible = true;
            // Spawn the selected async providers. On a cache hit this is the
            // LLM only (backfill skipped so a warm cache stays instant); on a
            // miss it is every provider.
            if !providers_to_spawn.is_empty() {
                self.spawn_async_providers(
                    &ctx,
                    &cwd,
                    shell_env.clone(),
                    &buffer,
                    cursor,
                    providers_to_spawn,
                );
            }
            self.render_at(stdout, cursor_row, cursor_col, screen_rows, screen_cols);
            self.last_trigger_fingerprint = Some(fingerprint);
        } else if !providers_to_spawn.is_empty() {
            // No sync or cached matches, but async providers (shell
            // completions / LLM) may still produce some. Keep any visible
            // popup on screen — do NOT dismiss — and spawn the generators;
            // try_merge_dynamic merges their results in place. Dismissing here
            // is what caused the gap at a word boundary (e.g. `git `) before
            // async subcommands arrived.
            self.spawn_async_providers(
                &ctx,
                &cwd,
                shell_env.clone(),
                &buffer,
                cursor,
                providers_to_spawn,
            );
        } else if self.visible && !self.ask_ai_active() {
            // Nothing to show and nothing pending. ask_ai_active() guarantees
            // the sentinel was injected into a non-empty result above, so
            // reaching here means Ask AI is disabled and there is genuinely
            // nothing to display.
            self.dismiss(stdout);
        }
    }

    /// Synchronous phase of the bounded-block trigger for the debounce path.
    ///
    /// Runs `suggest_sync`, paints sync-only results, and spawns generators.
    /// If `render_block_ms > 0` and a high-priority generator is pending,
    /// returns `TriggerPrepared::NeedsBlock` with the channel receiver so the
    /// debounce loop can `await` the bounded window **outside** the
    /// `std::sync::Mutex` lock. Otherwise returns `TriggerPrepared::Painted`.
    ///
    /// Render bytes are appended to `stdout`. Caller writes them to stdout
    /// after releasing the lock.
    pub fn prepare_trigger_with_block(
        &mut self,
        parser: &Arc<Mutex<TerminalParser>>,
        stdout: &mut Vec<u8>,
    ) -> TriggerPrepared {
        // Same alt-screen gate as trigger(): suppress popups while a TUI owns
        // the alternate screen. The debounce loop calls this instead of trigger().
        {
            let in_alt = match parser.lock() {
                Ok(p) => p.state().in_alt_screen(),
                Err(_) => false,
            };
            if in_alt {
                return TriggerPrepared::Painted;
            }
        }
        let block_ms = self.render_block_ms;

        // Extract parser state.
        let (buffer, cursor, cwd, shell_env, cursor_row, cursor_col, screen_rows, screen_cols) =
            match parser.lock() {
                Ok(mut p) => {
                    if p.state_mut().take_cpr_synced() {
                        self.overlay_scroll_deficit = 0;
                    }
                    let state = p.state();
                    let buffer = state.command_buffer().unwrap_or("").to_string();
                    let cursor = state.buffer_cursor();
                    let cwd = state.cwd().cloned().unwrap_or_else(|| PathBuf::from("."));
                    let shell_env = state.shell_env().cloned();
                    let (cursor_row, cursor_col) = state.cursor_position();
                    let (screen_rows, screen_cols) = state.screen_dimensions();
                    (
                        buffer,
                        cursor,
                        cwd,
                        shell_env,
                        cursor_row,
                        cursor_col,
                        screen_rows,
                        screen_cols,
                    )
                }
                Err(e) => {
                    tracing::warn!(
                        "parser mutex poisoned in prepare_trigger_with_block: {e} — skipping"
                    );
                    return TriggerPrepared::Painted;
                }
            };

        if buffer.is_empty() {
            if self.visible {
                self.dismiss(stdout);
            }
            return TriggerPrepared::Painted;
        }

        let fingerprint = buffer_fingerprint(&buffer, cursor, shell_env.as_ref());
        if self.visible && self.last_trigger_fingerprint == Some(fingerprint) {
            return TriggerPrepared::Painted;
        }

        let ctx = parse_command_context(&buffer, cursor);

        // Synchronous tree-cache resolution (mirrors trigger()). On a hit the
        // cached rows merge into the sync pool; backfill providers (fish/zsh)
        // are skipped but live providers (LLM) still fire.
        let (cached_suggestions, cache_hit) = match self.completion_cache.as_ref() {
            Some(cache) => cache.resolve(&ctx, &buffer),
            None => (Vec::new(), false),
        };
        let (live_providers, backfill_providers) = self.partition_async_providers();
        // Live providers (LLM) fire on every trigger; backfill providers
        // (fish/zsh) only on a cache miss.
        let providers_to_spawn: Vec<Arc<dyn AsyncProvider>> = if cache_hit {
            live_providers
        } else {
            live_providers
                .into_iter()
                .chain(backfill_providers)
                .collect()
        };

        if let Some(handle) = self.dynamic_task.take() {
            handle.abort();
        }
        self.dynamic_rx = None;
        self.dynamic_ctx = None;
        self.feedback = AsyncFeedback::Idle;
        self.pending_failed.clear();
        self.pending_empty_count = 0;
        self.buffer_generation = self.buffer_generation.wrapping_add(1);

        let result =
            match self
                .engine
                .suggest_sync_with_env(&ctx, &cwd, &buffer, shell_env.as_ref())
            {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::debug!("suggest_sync failed in prepare_trigger_with_block: {e}");
                    None
                }
            };
        // Ask AI: guarantee the sentinel so the popup shows even with zero
        // sync matches (mirrors the normalization in trigger()).
        let result = match result {
            Some(mut r) if self.ask_ai_active() => {
                if !r
                    .suggestions
                    .iter()
                    .any(|s| s.kind == suggest::SuggestionKind::AskAi)
                {
                    r.suggestions.push(Self::ask_ai_sentinel());
                }
                Some(r)
            }
            other => other,
        };

        // Merge cached tree-completion rows into the sync pool (dedup by
        // text), then re-rank so cached rows compete on score rather than
        // being appended raw after the sync results.
        let has_cached = !cached_suggestions.is_empty();
        let mut sync_suggestions = merge_cached_suggestions(
            result.map(|r| r.suggestions).unwrap_or_default(),
            cached_suggestions,
        );
        if has_cached {
            sync_suggestions = self.rerank_live(&ctx.current_word, sync_suggestions);
        }

        // Live providers (LLM) fire on every trigger; backfill providers
        // (fish/zsh) only on a cache miss. Only block the first paint to
        // race providers that will actually spawn.
        let spawn_async = !providers_to_spawn.is_empty();
        let needs_block = block_ms > 0 && spawn_async;

        // Keep a visible popup alive while async providers may still produce matches;
        // dismissing here would reopen the word-boundary gap the immediate path (step 2)
        // already fixed. When nothing is pending and Ask AI is off there is
        // nothing left to show.
        if sync_suggestions.is_empty() && self.visible && !spawn_async && !self.ask_ai_active() {
            self.dismiss(stdout);
        }

        // Spawn the selected async providers.
        if spawn_async {
            self.spawn_async_providers(
                &ctx,
                &cwd,
                shell_env.clone(),
                &buffer,
                cursor,
                providers_to_spawn,
            );
        }

        if needs_block {
            // Take the rx out of self. The caller awaits it outside the lock,
            // then calls apply_block_result to merge and repaint.
            if let Some(rx) = self.dynamic_rx.take() {
                // Paint sync-only to give immediate feedback while waiting.
                if !sync_suggestions.is_empty() {
                    self.replace_suggestions_and_reset_overlay(sync_suggestions.clone());
                    self.visible = true;
                    self.render_at(stdout, cursor_row, cursor_col, screen_rows, screen_cols);
                }

                return TriggerPrepared::NeedsBlock {
                    rx,
                    sync_suggestions,
                    block_ms,
                    cursor_row,
                    cursor_col,
                    screen_rows,
                    screen_cols,
                    fingerprint,
                    current_word: ctx.current_word.clone(),
                };
            }
        }

        // No block needed — paint the merged pool and let dynamic_merge_loop
        // handle any async backfill.
        if !sync_suggestions.is_empty() {
            self.replace_suggestions_and_reset_overlay(sync_suggestions);
            self.visible = true;
            self.render_at(stdout, cursor_row, cursor_col, screen_rows, screen_cols);
            self.last_trigger_fingerprint = Some(fingerprint);
        }

        TriggerPrepared::Painted
    }

    /// Apply the result of the bounded-block window after the debounce loop
    /// awaited the async generator outside the mutex lock.
    #[allow(clippy::too_many_arguments)] // all args are genuinely independent
    pub fn apply_block_result(
        &mut self,
        parser: &Arc<Mutex<TerminalParser>>,
        stdout: &mut Vec<u8>,
        maybe_async: Option<DynamicResult>,
        rx_after_recv: Option<mpsc::Receiver<DynamicResult>>,
        rx_on_timeout: Option<mpsc::Receiver<DynamicResult>>,
        sync_suggestions: Vec<Suggestion>,
        _cursor_row: u16,
        _cursor_col: u16,
        _screen_rows: u16,
        _screen_cols: u16,
        fingerprint: TriggerFingerprint,
        _current_word: &str,
    ) {
        let was_timeout = rx_on_timeout.is_some();
        if let Some(rx) = rx_on_timeout {
            self.dynamic_rx = Some(rx);
        }

        if was_timeout {
            // Sync-only was already painted in prepare_trigger_with_block;
            // dynamic_merge_loop will merge the late result.
            self.last_trigger_fingerprint = Some(fingerprint);
            return;
        }

        let mut messages = maybe_async.into_iter().collect::<Vec<_>>();
        let mut disconnected = rx_after_recv.is_none();
        if let Some(mut rx) = rx_after_recv {
            loop {
                match rx.try_recv() {
                    Ok(message) => messages.push(message),
                    Err(mpsc::error::TryRecvError::Empty) => {
                        self.dynamic_rx = Some(rx);
                        break;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            self.dynamic_rx = None;
            self.dynamic_task = None;
        }
        let aggregation = AsyncFeedback::aggregate(messages);
        self.pending_failed.extend(aggregation.failed);
        self.pending_empty_count += aggregation.empty_count;
        if aggregation.loaded.is_empty() {
            if disconnected {
                self.dynamic_ctx = None;
            }
            let now = std::time::Instant::now();
            self.feedback = if !disconnected {
                self.feedback.clone()
            } else if !self.pending_failed.is_empty() && !sync_suggestions.is_empty() {
                AsyncFeedback::PartialError {
                    failed: std::mem::take(&mut self.pending_failed),
                    since: now,
                }
            } else if self.pending_empty_count > 0 && !sync_suggestions.is_empty() {
                AsyncFeedback::Idle
            } else {
                AsyncFeedback::terminal_for_outcome(
                    false,
                    &self.pending_failed,
                    self.pending_empty_count,
                    now,
                )
            };
            if disconnected {
                self.pending_failed.clear();
                self.pending_empty_count = 0;
            }
            self.feedback_tick_notify.notify_one();
            if self.visible || self.feedback.is_terminal() || self.feedback.is_loading() {
                self.render(parser, stdout);
            }
            self.last_trigger_fingerprint = Some(fingerprint);
            return;
        }
        let async_results = aggregation.loaded;
        let now = std::time::Instant::now();
        self.feedback = if disconnected {
            AsyncFeedback::terminal_for_outcome(
                !async_results.is_empty(),
                &self.pending_failed,
                self.pending_empty_count,
                now,
            )
        } else {
            self.feedback.clone()
        };
        if disconnected {
            self.pending_failed.clear();
            self.pending_empty_count = 0;
        }
        self.feedback_tick_notify.notify_one();

        // Re-check that the buffer hasn't drifted while we were awaiting
        // the generator. The captured `cursor_row` / `cursor_col` /
        // `current_word` / `fingerprint` are all from the spawn site
        // ~block_ms ago; merging against a freshly-typed buffer would rank
        // the wrong query and paint at the wrong cursor row.
        let (live_word, _live_buffer) = match self.check_merge_staleness(parser) {
            MergeFreshness::Fresh {
                current_word,
                buffer,
            } => (current_word, buffer),
            MergeFreshness::Stale => {
                self.dynamic_rx = None;
                self.dynamic_ctx = None;
                self.dynamic_task = None;
                self.feedback = AsyncFeedback::Idle;
                self.pending_failed.clear();
                self.pending_empty_count = 0;
                if self.visible {
                    self.render(parser, stdout);
                }
                return;
            }
            MergeFreshness::PoisonedLock => return,
        };

        let mut all = sync_suggestions;
        let extras = merge_dedup_against(&all, async_results);
        all.extend(extras);
        // Mirror try_merge_dynamic: rank against the LIVE query so a
        // user keystroke during the bounded wait re-filters the pool
        // instead of ranking against the stale spawn-time word.
        let all = self.rerank_live(&live_word, all);

        self.replace_suggestions_and_reset_overlay(all);
        self.visible = true;
        if disconnected {
            self.dynamic_ctx = None;
        }
        // Render against live cursor/screen geometry. The logical word can be
        // unchanged while shell output, wrapping, or viewport scroll moved the
        // visual cursor since the bounded-block trigger captured its geometry.
        self.render(parser, stdout);
        self.last_trigger_fingerprint = Some(fingerprint);
    }

    /// Split registered async providers into "live" providers that fire on
    /// every trigger (e.g. the LLM — never cached) and "backfill" providers
    /// (fish/zsh) that only run on a cache miss to populate the tree cache.
    #[allow(clippy::type_complexity)]
    fn partition_async_providers(
        &self,
    ) -> (Vec<Arc<dyn AsyncProvider>>, Vec<Arc<dyn AsyncProvider>>) {
        let mut live = Vec::new();
        let mut backfill = Vec::new();
        for p in &self.async_providers {
            if p.is_backfill_provider() {
                backfill.push(Arc::clone(p));
            } else {
                live.push(Arc::clone(p));
            }
        }
        (live, backfill)
    }

    /// Spawn an async task to run the caller-selected async providers.
    /// On a cache hit the caller passes live providers only (LLM); on a miss
    /// it passes all providers. Results arrive via `dynamic_rx` and Task E
    /// renders them via `dynamic_notify`.
    fn spawn_async_providers(
        &mut self,
        ctx: &buffer::CommandContext,
        cwd: &std::path::Path,
        shell_env: Option<HashMap<String, String>>,
        buffer: &str,
        cursor: usize,
        providers: Vec<Arc<dyn AsyncProvider>>,
    ) {
        if providers.is_empty() {
            return;
        }
        // Snapshot the command context so try_merge_dynamic can drop results
        // if the user typed a different command/subcommand/flag while
        // providers were running.
        self.dynamic_ctx = Some(DynamicCtxSnapshot::capture(ctx));
        self.spawned_generation = self.buffer_generation;
        let (tx, rx) = mpsc::channel::<DynamicResult>(8);
        self.dynamic_rx = Some(rx);
        self.feedback = AsyncFeedback::Loading {
            spawned_at: std::time::Instant::now(),
        };
        self.pending_failed.clear();
        self.pending_empty_count = 0;
        self.feedback_tick_notify.notify_one();
        let ctx = ctx.clone();
        let cwd = cwd.to_path_buf();
        let notify = Arc::clone(&self.dynamic_notify);
        let async_providers = providers;
        let async_buffer = buffer.to_string();
        let async_cursor = cursor;
        let _ = shell_env;
        let handle = tokio::spawn(async move {
            // Run async providers (LLM, fish/zsh completions) concurrently.
            // Each sends its own DynamicResult through the same channel.
            let req_ctx = ctx.clone();
            let req_cwd = cwd.clone();
            let req_buffer = async_buffer.clone();
            let req_cursor = async_cursor;
            let mut join_set = tokio::task::JoinSet::new();
            for provider in async_providers {
                let p_ctx = req_ctx.clone();
                let p_cwd = req_cwd.clone();
                let p_buf = req_buffer.clone();
                join_set.spawn(async move {
                    let req = suggest::SuggestRequest {
                        ctx: &p_ctx,
                        cwd: &p_cwd,
                        buffer: &p_buf,
                        cursor: req_cursor,
                    };
                    let name = provider.name().to_string();
                    match provider.suggest(&req).await {
                        Ok(suggestions) if suggestions.is_empty() => DynamicResult::Empty {
                            provider: ProviderTag::Async(name),
                        },
                        Ok(suggestions) => DynamicResult::Loaded {
                            provider: ProviderTag::Async(name),
                            suggestions,
                        },
                        Err(e) => DynamicResult::Error {
                            provider: ProviderTag::Async(name),
                            message: e.to_string(),
                        },
                    }
                });
            }
            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(dynamic_result) => {
                        if tx.send(dynamic_result).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("async provider task panicked: {e}");
                    }
                }
            }
            // Drop tx BEFORE notifying so Task E sees Disconnected on
            // the first try_recv after wake.
            drop(tx);
            // Always notify so Task E clears the loading indicator.
            notify.notify_one();
        });
        self.dynamic_task = Some(handle);
    }

    /// Re-acquire parser state and verify that an in-flight generator's
    /// spawn-time context still matches the user's current buffer.
    ///
    /// On `Fresh`, the caller proceeds with the merge and uses the returned
    /// live `current_word`. On `Stale`, the caller drops the results and
    /// repaints to clear the loading indicator. On `PoisonedLock`, the caller
    /// returns without re-rendering — the next `parser.lock()` would just
    /// log-and-skip again.
    fn check_merge_staleness(&mut self, parser: &Arc<Mutex<TerminalParser>>) -> MergeFreshness {
        let (current_ctx, buffer) = {
            let p = match parser.lock() {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(
                        "parser lock poisoned during dynamic merge re-rank: {e} — \
                         disabling dynamic_rx"
                    );
                    self.dynamic_rx = None;
                    self.dynamic_ctx = None;
                    self.dynamic_task = None;
                    return MergeFreshness::PoisonedLock;
                }
            };
            let state = p.state();
            let buffer = state.command_buffer().unwrap_or("").to_string();
            let cursor = state.buffer_cursor();
            (parse_command_context(&buffer, cursor), buffer)
        };
        let current_word = current_ctx.current_word.clone();

        if self.spawned_generation != self.buffer_generation {
            self.dynamic_ctx = None;
            return MergeFreshness::Stale;
        }

        let stale = match &self.dynamic_ctx {
            Some(spawned) => spawned.is_stale_against(&current_ctx),
            None => true,
        };
        if stale {
            return MergeFreshness::Stale;
        }
        MergeFreshness::Fresh {
            current_word,
            buffer,
        }
    }

    /// Check for pending dynamic (script generator) results and merge them
    /// into the current suggestions. Returns `true` if the popup was updated.
    pub fn try_merge_dynamic(
        &mut self,
        parser: &Arc<Mutex<TerminalParser>>,
        stdout: &mut dyn Write,
    ) -> bool {
        let mut messages = Vec::new();
        let mut disconnected = false;
        {
            let rx = match self.dynamic_rx.as_mut() {
                Some(rx) => rx,
                None => return false,
            };
            loop {
                match rx.try_recv() {
                    Ok(message) => messages.push(message),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        if messages.is_empty() {
            if disconnected {
                self.dynamic_rx = None;
                self.dynamic_ctx = None;
                self.dynamic_task = None;
                let now = std::time::Instant::now();
                self.feedback = if !self.pending_failed.is_empty() && !self.suggestions.is_empty() {
                    AsyncFeedback::PartialError {
                        failed: std::mem::take(&mut self.pending_failed),
                        since: now,
                    }
                } else if self.pending_empty_count > 0 && !self.suggestions.is_empty() {
                    AsyncFeedback::Idle
                } else {
                    AsyncFeedback::terminal_for_outcome(
                        false,
                        &self.pending_failed,
                        self.pending_empty_count,
                        now,
                    )
                };
                self.pending_failed.clear();
                self.pending_empty_count = 0;
                self.feedback_tick_notify.notify_one();
                if self.visible {
                    self.render(parser, stdout);
                }
            }
            return false;
        }

        let aggregation = AsyncFeedback::aggregate(messages);
        self.pending_failed.extend(aggregation.failed);
        self.pending_empty_count += aggregation.empty_count;

        if disconnected {
            self.dynamic_rx = None;
            self.dynamic_task = None;
        }

        if aggregation.loaded.is_empty() {
            if disconnected {
                self.dynamic_ctx = None;
            }
            let now = std::time::Instant::now();
            self.feedback = if !disconnected {
                self.feedback.clone()
            } else if !self.pending_failed.is_empty() && !self.suggestions.is_empty() {
                AsyncFeedback::PartialError {
                    failed: std::mem::take(&mut self.pending_failed),
                    since: now,
                }
            } else if self.pending_empty_count > 0 && !self.suggestions.is_empty() {
                AsyncFeedback::Idle
            } else {
                AsyncFeedback::terminal_for_outcome(
                    false,
                    &self.pending_failed,
                    self.pending_empty_count,
                    now,
                )
            };
            if disconnected {
                self.pending_failed.clear();
                self.pending_empty_count = 0;
            }
            self.feedback_tick_notify.notify_one();
            self.render(parser, stdout);
            return !matches!(self.feedback, AsyncFeedback::Idle);
        }

        {
            let dynamic_results = aggregation.loaded;
            let now = std::time::Instant::now();
            let previous_feedback = self.feedback.clone();
            self.feedback = if disconnected {
                AsyncFeedback::terminal_for_outcome(
                    !dynamic_results.is_empty(),
                    &self.pending_failed,
                    self.pending_empty_count,
                    now,
                )
            } else {
                previous_feedback
            };
            if disconnected {
                self.pending_failed.clear();
                self.pending_empty_count = 0;
            }
            self.feedback_tick_notify.notify_one();
            if disconnected {
                self.dynamic_rx = None;
                // The generator task has completed (it sent its results and
                // dropped tx). The JoinHandle is now a no-op for `.abort()`
                // but we still clear it so dismiss()/trigger() don't rely
                // on an already-completed handle for their orphan-task
                // cleanup guarantees.
                self.dynamic_task = None;
            }
            let (current_word, _current_buffer) = match self.check_merge_staleness(parser) {
                MergeFreshness::Fresh {
                    current_word,
                    buffer,
                } => (current_word, buffer),
                MergeFreshness::Stale => {
                    self.dynamic_rx = None;
                    self.dynamic_ctx = None;
                    self.dynamic_task = None;
                    self.feedback = AsyncFeedback::Idle;
                    self.pending_failed.clear();
                    self.pending_empty_count = 0;
                    if self.visible {
                        self.render(parser, stdout);
                    }
                    return false;
                }
                MergeFreshness::PoisonedLock => return false,
            };

            // Activate popup if it wasn't visible yet (async-only path:
            // no static suggestions, generators produced the results).
            if !self.visible {
                self.visible = true;
                self.overlay.reset();
                self.reset_detail_debounce_state();
            }

            let extras = merge_dedup_against(&self.suggestions, dynamic_results);
            self.suggestions.extend(extras);
            let merged = std::mem::take(&mut self.suggestions);
            self.suggestions = self.pin_ask_ai(self.rerank_live(&current_word, merged));
            // The rerank above reorders self.suggestions in place, so any
            // displayed_detail_idx captured pre-rerank now points at a
            // different suggestion. Clear the debounce state so a stale
            // index can't paint a mismatched description while the window
            // is still active.
            self.reset_detail_debounce_state();

            if self.suggestions.is_empty() {
                self.dismiss(stdout);
                return true;
            }
            if disconnected {
                self.dynamic_ctx = None;
            }

            self.render(parser, stdout);
            true
        }
    }

    fn render(&mut self, parser: &Arc<Mutex<TerminalParser>>, stdout: &mut dyn Write) {
        // Poison handling mirrors Task B in proxy.rs: if the parser mutex
        // is poisoned (another task panicked while holding it), log and
        // skip this render rather than propagating the panic. The popup
        // will simply not update on this tick; the next render attempt is
        // driven by further PTY input.
        let (cursor_row, cursor_col, screen_rows, screen_cols) = match parser.lock() {
            Ok(p) => {
                let state = p.state();
                let (cr, cc) = state.cursor_position();
                let (sr, sc) = state.screen_dimensions();
                (cr, cc, sr, sc)
            }
            Err(e) => {
                tracing::warn!("parser mutex poisoned in render: {e} — skipping render");
                return;
            }
        };
        self.render_at(stdout, cursor_row, cursor_col, screen_rows, screen_cols);
    }

    fn render_at(
        &mut self,
        stdout: &mut dyn Write,
        cursor_row: u16,
        cursor_col: u16,
        screen_rows: u16,
        screen_cols: u16,
    ) {
        // `bump_output_epoch` stays OUTSIDE the frame — exactly one bump per
        // render_at call regardless of strategy.
        self.bump_output_epoch();

        let feedback = self.current_feedback_kind();
        let hints = self.build_popup_hints();
        let additional_scroll = popup_additional_scroll_deficit(
            &self.suggestions,
            cursor_row,
            screen_rows,
            screen_cols,
            self.max_visible,
            self.min_popup_width,
            &self.theme,
            self.overlay_scroll_deficit,
            &feedback,
            &hints,
        );

        let feedback_only_repaint_after_scroll = self.suggestions.is_empty()
            && !matches!(&feedback, FeedbackKind::None)
            && self.overlay_scroll_deficit > 0;

        // Stage every overlay byte into inner_buf first.  All calls here are
        // normal method calls on &mut self — no borrow-checker issue.  The
        // resulting bytes are then handed to with_overlay_update_frame so the
        // entire update is enclosed in exactly ONE balanced DECSET 2026 pair
        // on Synchronized terminals.  On PreRenderBuffer terminals the frame
        // helper is a no-op and the single write_all below provides atomicity.
        let mut inner_buf = Vec::with_capacity(2048);

        // 1. Clear prior popup if no scroll is needed.
        let can_clear_old = additional_scroll == 0 && !feedback_only_repaint_after_scroll;
        if can_clear_old {
            if let Some(ref layout) = self.last_layout {
                overlay::clear_popup_unframed(&mut inner_buf, layout);
            }
            // Clear the previous detail box in lockstep so it can't survive a
            // popup repaint as a ghost rectangle.
            if let Some(ref det) = self.last_detail_layout {
                clear_detail_box(&mut inner_buf, det);
            }
        }

        // 2. Compute the post-scroll position of any old detail layout.
        let scrolled_old_detail_layout = if additional_scroll > 0 {
            self.last_detail_layout
                .as_ref()
                .and_then(|det| detail_layout_after_scroll(det, additional_scroll))
        } else {
            None
        };

        // 3. Render new popup (unframed — no sync markers).
        let layout = overlay::render_popup_unframed(
            &mut inner_buf,
            &self.suggestions,
            &self.overlay,
            cursor_row,
            cursor_col,
            screen_rows,
            screen_cols,
            self.max_visible,
            self.min_popup_width,
            self.max_popup_width,
            &self.theme,
            self.overlay_scroll_deficit,
            feedback,
            &hints,
        );

        // 4. Detail-box pass (render_detail_box is already unframed).
        let new_detail_layout =
            self.maybe_render_detail(&mut inner_buf, &layout, &hints, screen_rows, screen_cols);

        // 5. Clear scrolled-out portions of old detail (unframed).
        if let Some(ref old_detail) = scrolled_old_detail_layout {
            let mut covers = vec![OverlayRect::from_popup(&layout)];
            if let Some(ref new_detail) = new_detail_layout {
                covers.push(OverlayRect::from_detail(new_detail));
            }
            clear_detail_box_uncovered_by(&mut inner_buf, old_detail, &covers);
        }

        // Wrap the entire update in ONE sync frame.  On Synchronized profiles
        // this emits exactly one begin_sync / end_sync pair around all the
        // bytes staged above.  On PreRenderBuffer profiles the helper is a
        // transparent pass-through.  If inner_buf is empty (no-op render) the
        // helper short-circuits and buf stays empty — no 16-byte no-op pair.
        let mut buf = Vec::with_capacity(inner_buf.len() + 16);
        overlay::with_overlay_update_frame(&mut buf, &self.terminal_profile, |b| {
            b.extend_from_slice(&inner_buf);
        });

        if !buf.is_empty() {
            let _ = stdout.write_all(&buf);
            let _ = stdout.flush();
        }
        self.overlay_render_generation = self.overlay_render_generation.wrapping_add(1);
        self.last_repaint_at = Some(Instant::now());
        self.pending_overlay_cleanup = None;
        self.pending_overlay_render = Some(PendingOverlayRender {
            token: OverlayRenderToken(self.overlay_render_generation),
            layout,
            detail_layout: new_detail_layout,
        });
    }

    /// Render the adjacent description box when enabled and layout-fitting.
    /// While the debounce window is active, this may keep rendering the
    /// previously displayed suggestion's detail instead of the current
    /// selection. Returns the new `last_detail_layout` value.
    ///
    /// Within the throttle window, `displayed_detail_idx` is held fixed so
    /// rapid arrow navigation doesn't visually thrash the box; a one-shot
    /// timer is spawned to fire `detail_redraw_notify` after the window
    /// elapses, prompting the proxy to re-render.
    fn maybe_render_detail(
        &mut self,
        buf: &mut Vec<u8>,
        main_layout: &PopupLayout,
        hints: &overlay::PopupHints,
        screen_rows: u16,
        screen_cols: u16,
    ) -> Option<DetailLayout> {
        if self.detail_box_mode == DescriptionBoxMode::Off {
            self.detail_debounce.displayed_idx = None;
            return None;
        }
        if main_layout.height == 0 || self.suggestions.is_empty() {
            self.detail_debounce.displayed_idx = None;
            return None;
        }
        let Some(selected) = self.overlay.selected else {
            self.detail_debounce.displayed_idx = None;
            return None;
        };

        // Track selection changes for the detail-update throttle window.
        let selection_changed = self.detail_debounce.displayed_idx != Some(selected);
        let now = Instant::now();
        if selection_changed && self.detail_debounce.last_change_at.is_none() {
            self.detail_debounce.last_change_at = Some(now);
        }

        // Resolve which suggestion's description the box should display.
        let should_debounce = selection_changed
            && self.detail_debounce.displayed_idx.is_some()
            && self.detail_box_debounce_ms > 0;
        let target_idx = if should_debounce {
            let elapsed = self
                .detail_debounce
                .last_change_at
                .map(|t| now.saturating_duration_since(t).as_millis() as u64)
                .unwrap_or(u64::MAX);
            if elapsed < self.detail_box_debounce_ms {
                // Still within the debounce window — keep showing the prior
                // detail (or skip rendering on the very first paint when no
                // prior idx exists). Schedule a wake-up so the next render
                // happens after the window expires.
                self.schedule_detail_debounce_wakeup(self.detail_box_debounce_ms - elapsed);
                self.detail_debounce.displayed_idx
            } else {
                self.detail_debounce.last_change_at = None;
                Some(selected)
            }
        } else {
            self.detail_debounce.last_change_at = None;
            Some(selected)
        };

        let idx = target_idx?;
        let target_is_current_selection = idx == selected;
        let Some(suggestion) = self.suggestions.get(idx) else {
            if target_is_current_selection {
                self.detail_debounce.displayed_idx = Some(idx);
            }
            return None;
        };
        let Some(desc) = suggestion.description.as_deref() else {
            if target_is_current_selection {
                self.detail_debounce.displayed_idx = Some(idx);
            }
            return None;
        };
        if desc.is_empty() {
            if target_is_current_selection {
                self.detail_debounce.displayed_idx = Some(idx);
            }
            return None;
        }

        // Skip the detail box when the inline description already fits in
        // the main popup row — no truncation, no information to add. Saves
        // screen real estate for short descriptions like git's "branch" /
        // "current branch" labels.
        if !description_overflows_main_popup(
            suggestion,
            main_layout,
            self.suggestions.len(),
            self.max_visible,
            self.theme.borders,
            screen_rows,
        ) {
            self.detail_debounce.displayed_idx = Some(idx);
            return None;
        }

        let content_row_offset = if self.theme.borders {
            0
        } else {
            u16::from(hints.index.is_some())
        };

        let Some(layout) = compute_detail_layout(
            main_layout,
            screen_rows,
            screen_cols,
            self.detail_box_max_width,
            self.detail_box_lines,
            self.theme.borders,
            content_row_offset,
        ) else {
            if target_is_current_selection {
                self.detail_debounce.displayed_idx = Some(idx);
            }
            return None;
        };

        render_detail_box(buf, &layout, desc, &self.theme);
        self.detail_debounce.displayed_idx = Some(idx);
        Some(layout)
    }

    /// Spawn a one-shot tokio task that fires `detail_redraw_notify` after
    /// `delay_ms`. Idempotent — multiple in-flight timers are guarded by
    /// `detail_debounce.pending`. Silently no-ops outside an async runtime
    /// (used in unit tests).
    fn schedule_detail_debounce_wakeup(&mut self, delay_ms: u64) {
        if self.detail_debounce.pending {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        self.detail_debounce.pending = true;
        let notify = Arc::clone(&self.detail_redraw_notify);
        handle.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms.max(1))).await;
            notify.notify_one();
        });
    }

    /// Called by the proxy from the detail-redraw loop. Resets the in-flight
    /// debounce flag so a subsequent selection change will spawn a fresh
    /// timer.
    pub fn clear_detail_debounce_pending(&mut self) {
        self.detail_debounce.pending = false;
    }

    /// Re-render into `buf` in response to a detail-debounce wake-up. No-op
    /// when no popup is currently visible (the wake-up could correspond to a
    /// since-dismissed popup).
    pub fn render_for_detail_redraw(
        &mut self,
        parser: &Arc<Mutex<TerminalParser>>,
        buf: &mut Vec<u8>,
    ) {
        if !self.visible || self.suggestions.is_empty() {
            return;
        }
        if self.detail_box_mode == DescriptionBoxMode::Off {
            return;
        }
        // Only re-render if the displayed detail idx actually trails the
        // current selection — avoids needless repaints when the user
        // navigated and stopped within the same item.
        if self.detail_debounce.displayed_idx == self.overlay.selected {
            return;
        }
        self.repaint_visible_into(parser, buf);
    }

    /// Re-render into `buf` after the match-mode flash expired so the footer
    /// reverts from the mode label to the normal key hint. No-op when no
    /// popup is visible (the flash could outlive a dismissed popup).
    pub fn render_for_flash_expiry(
        &mut self,
        parser: &Arc<Mutex<TerminalParser>>,
        buf: &mut Vec<u8>,
    ) {
        if !self.visible || self.suggestions.is_empty() {
            return;
        }
        self.repaint_visible_into(parser, buf);
    }

    /// Repaint the visible popup into `buf` using the parser's current cursor
    /// geometry. Routes through the same `render_at` the main loop uses so
    /// generation/cleanup token bookkeeping stays consistent.
    pub(crate) fn repaint_visible_into(
        &mut self,
        parser: &Arc<Mutex<TerminalParser>>,
        buf: &mut Vec<u8>,
    ) {
        let (cursor_row, cursor_col, screen_rows, screen_cols) = match parser.lock() {
            Ok(p) => {
                let state = p.state();
                let (cr, cc) = state.cursor_position();
                let (sr, sc) = state.screen_dimensions();
                (cr, cc, sr, sc)
            }
            Err(e) => {
                tracing::warn!("parser mutex poisoned in overlay repaint: {e}");
                return;
            }
        };
        struct BufWriter<'a>(&'a mut Vec<u8>);
        impl<'a> Write for BufWriter<'a> {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut writer = BufWriter(buf);
        self.render_at(
            &mut writer,
            cursor_row,
            cursor_col,
            screen_rows,
            screen_cols,
        );
    }

    fn current_feedback_kind(&self) -> FeedbackKind {
        match &self.feedback {
            AsyncFeedback::Idle => FeedbackKind::None,
            AsyncFeedback::Loading { spawned_at } => {
                let elapsed_ms = spawned_at.elapsed().as_millis() as u64;
                FeedbackKind::Loading {
                    frame: ((elapsed_ms / 80) % 10) as u8,
                }
            }
            AsyncFeedback::Empty { .. } => FeedbackKind::Empty,
            AsyncFeedback::Error { failed, .. } => {
                if failed.len() > 1 {
                    FeedbackKind::PartialError {
                        providers: failed.len().min(u8::MAX as usize) as u8,
                    }
                } else {
                    FeedbackKind::Error {
                        provider: if self.theme.show_provider_errors {
                            failed.first().cloned().unwrap_or_default()
                        } else {
                            String::new()
                        },
                    }
                }
            }
            AsyncFeedback::PartialError { failed, .. } => FeedbackKind::PartialError {
                providers: failed.len().min(u8::MAX as usize) as u8,
            },
        }
    }

    /// Build header/footer hint content for the popup render.
    fn build_popup_hints(&self) -> overlay::PopupHints {
        let index = if self.theme.index_hints {
            let total = self
                .suggestions
                .iter()
                .filter(|s| s.kind != SuggestionKind::AskAi)
                .count();
            (total > 0).then(|| {
                let idx = self
                    .effective_selected()
                    .map(|sel| {
                        self.suggestions[..=sel]
                            .iter()
                            .filter(|s| s.kind != SuggestionKind::AskAi)
                            .count()
                    })
                    .unwrap_or(1)
                    .max(1);
                (idx, total)
            })
        } else {
            None
        };

        let key_label = self.theme.key_hints.then(|| {
            self.mode_flash
                .as_ref()
                .filter(|(_, deadline)| Instant::now() < *deadline)
                .map(|(label, _)| label.clone())
                .unwrap_or_else(|| {
                    let accept = format_key_event(&self.keybindings.accept);
                    let mode = format_key_event(&self.keybindings.toggle_match_mode);
                    format!("<{accept}> Accept · <{mode}> Mode")
                })
        });

        overlay::PopupHints { index, key_label }
    }

    /// Toggle the engine's match mode (Fuzzy ↔ Substring) and flash the new
    /// mode name in the key-hint footer for one second.
    fn toggle_match_mode(&mut self, parser: &Arc<Mutex<TerminalParser>>, stdout: &mut dyn Write) {
        let mut cfg = self.engine.config();
        cfg.match_mode = match cfg.match_mode {
            config::MatchMode::Fuzzy => config::MatchMode::Substring,
            config::MatchMode::Substring => config::MatchMode::Fuzzy,
        };
        let label = match cfg.match_mode {
            config::MatchMode::Fuzzy => "Fuzzy",
            config::MatchMode::Substring => "Substring",
        };
        self.engine.set_config(cfg);
        self.mode_flash = Some((label.to_string(), Instant::now() + Duration::from_secs(1)));
        self.mode_flash_notify.notify_one();
        // Re-run suggestions so the visible list reflects the new mode.
        self.last_trigger_fingerprint = None;
        self.trigger(parser, stdout);
    }

    pub fn render_indicator_only(&mut self, stdout: &mut dyn Write) {
        let Some(layout) = self.last_layout.clone() else {
            return;
        };
        let feedback = self.current_feedback_kind();
        let mut buf = Vec::new();
        self.bump_output_epoch();
        render_indicator_row(
            &mut buf,
            &layout,
            &self.theme,
            feedback,
            &self.build_popup_hints(),
        );
        let _ = stdout.write_all(&buf);
        let _ = stdout.flush();
    }

    pub fn clear_expired_feedback(&mut self, stdout: &mut dyn Write) -> bool {
        if self.feedback_dismiss_ms == 0 {
            return false;
        }
        if !self.feedback.is_terminal() {
            return false;
        }
        let Some(since) = self.feedback.since() else {
            return false;
        };
        if since.elapsed() < std::time::Duration::from_millis(self.feedback_dismiss_ms as u64) {
            return false;
        }
        // PartialError + suggestions present: drop only the indicator row, not the popup.
        if matches!(self.feedback, AsyncFeedback::PartialError { .. })
            && !self.suggestions.is_empty()
        {
            self.feedback = AsyncFeedback::Idle;
            if let Some(mut layout) = self.last_layout.clone() {
                if layout.height > 0 {
                    let borders = self.theme.borders;
                    let indicator_row = layout.start_row + layout.height - 1 - u16::from(borders);
                    let mut buf = Vec::with_capacity(layout.width as usize * 4 + 64);
                    self.bump_output_epoch();
                    buf.extend_from_slice(b"\x1b[s"); // save cursor
                    let _ = write!(
                        &mut buf,
                        "\x1b[{};{}H",
                        indicator_row + 1,
                        layout.start_col + 1
                    );
                    buf.extend(std::iter::repeat_n(b' ', layout.width as usize));
                    if borders {
                        let displaced_border_row = layout.start_row + layout.height - 1;
                        let _ = write!(
                            &mut buf,
                            "\x1b[{};{}H",
                            displaced_border_row + 1,
                            layout.start_col + 1
                        );
                        buf.extend(std::iter::repeat_n(b' ', layout.width as usize));
                        let _ = write!(
                            &mut buf,
                            "\x1b[{};{}H",
                            indicator_row + 1,
                            layout.start_col + 1
                        );
                        if !self.theme.border_on.is_empty() {
                            buf.extend_from_slice(&self.theme.border_on);
                        }
                        buf.extend_from_slice("╰".as_bytes());
                        let content_width = layout.width.saturating_sub(2);
                        for _ in 0..content_width {
                            buf.extend_from_slice("─".as_bytes());
                        }
                        buf.extend_from_slice("╯".as_bytes());
                        buf.extend_from_slice(b"\x1b[0m");
                    }
                    buf.extend_from_slice(b"\x1b[u"); // restore cursor
                    let _ = stdout.write_all(&buf);
                    let _ = stdout.flush();
                    // Shrink cached height so a later dismiss/clear_popup targets the right row.
                    layout.height -= 1;
                    self.last_layout = Some(layout);
                }
            }
            return true;
        }
        self.feedback = AsyncFeedback::Idle;
        self.dismiss(stdout);
        true
    }

    pub fn handle_terminal_output(
        &mut self,
        stdout: &mut dyn Write,
        display_dirty: bool,
        viewport_scrolls: u16,
    ) {
        if display_dirty || viewport_scrolls > 0 {
            self.bump_output_epoch();
        }

        if viewport_scrolls > 0 {
            self.last_trigger_fingerprint = None;
            // Shell-side viewport scrolls move the parser's cursor_row
            // independently of any overlay-induced scrolling. The cached
            // overlay_scroll_deficit was relative to the pre-scroll cursor
            // position; once the parser tracks the new shell scrolls, the
            // deficit no longer maps to anything meaningful. Drop it so the
            // next popup recomputes from a clean state. The next CPR sync at
            // a prompt boundary would do this anyway, but resetting eagerly
            // avoids one frame of misposition between the scroll and the
            // sync.
            self.overlay_scroll_deficit = 0;
        }

        // Within the repaint grace period, display-dirty output is the shell
        // echoing the keystroke we just rendered over — not genuine output.
        let within_grace = self
            .last_repaint_at
            .map(|t| t.elapsed().as_millis() < REPAINT_GRACE_MS)
            .unwrap_or(false);
        if display_dirty
            && !within_grace
            && (self.visible
                || self.last_layout.is_some()
                || !matches!(self.feedback, AsyncFeedback::Idle))
        {
            self.bump_output_epoch();
            self.teardown_popup(stdout, true);
            let cleanup_ticket = self.overlay_write_ticket();
            self.commit_overlay_write(cleanup_ticket);
        }
    }

    /// Drop the cached overlay scroll deficit. Called from the proxy's CPR
    /// dispatch when a response arrives whose coordinates we cannot reconcile
    /// with our cached screen dimensions even after re-querying the terminal
    /// size — at that point the deficit is meaningless and continuing to
    /// accumulate it would push subsequent popups further off the actual
    /// cursor row.
    pub fn invalidate_overlay_scroll_deficit(&mut self) {
        self.overlay_scroll_deficit = 0;
    }
    /// Reset the keystroke buffer model to empty. Called by the proxy's stdout
    /// task on prompt boundaries (OSC 133;A / 7771;A) for non-zsh shells.
    pub fn reset_input_model(&mut self) {
        self.input_model = BufferModel::default();
    }

    pub(crate) fn dismiss(&mut self, stdout: &mut dyn Write) {
        self.teardown_popup(stdout, false);
    }

    /// Enter the loading state for an on-demand Ask AI request and wake the
    /// feedback tick loop so the spinner animates in the indicator row.
    pub fn begin_ask_ai_loading(&mut self) {
        self.feedback = AsyncFeedback::Loading {
            spawned_at: Instant::now(),
        };
        self.feedback_tick_notify.notify_one();
    }

    /// Tear down the popup after an Ask AI request settles. Cleanup bytes
    /// (popup clear) are appended to `stdout_buf` for the proxy to write.
    pub fn finish_ask_ai(&mut self, stdout_buf: &mut Vec<u8>) {
        self.dismiss(stdout_buf);
    }

    /// Forward bytes that replace the whole current buffer with `response`
    /// (one 0x7F per char up to the cursor, then the response). Mirrors the
    /// History/Llm arm of `accept_suggestion_locked`. Returns empty on a
    /// poisoned parser lock.
    pub fn ask_ai_forward_bytes(
        &self,
        parser: &Arc<Mutex<TerminalParser>>,
        response: &str,
    ) -> Vec<u8> {
        let p = match parser.lock() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("parser poisoned in ask_ai_forward_bytes: {e}");
                return Vec::new();
            }
        };
        let state = p.state();
        let buffer = state.command_buffer().unwrap_or("");
        let cursor = state.buffer_cursor();
        let delete = cursor.min(buffer.chars().count());
        let mut bytes = vec![0x7F; delete];
        bytes.extend_from_slice(response.as_bytes());
        bytes
    }

    fn teardown_popup(&mut self, stdout: &mut dyn Write, preserve_trigger_request: bool) {
        let detail_layout = self.last_detail_layout.clone();
        if let Some(layout) = self.last_layout.clone() {
            let mut buf = Vec::new();
            self.bump_output_epoch();
            // Wrap popup-clear + detail-clear in ONE balanced DECSET 2026 pair
            // on Synchronized terminals so teardown doesn't flicker.
            overlay::with_overlay_update_frame(&mut buf, &self.terminal_profile, |b| {
                clear_popup_unframed(b, &layout);
                if let Some(ref det) = detail_layout {
                    clear_detail_box(b, det);
                }
            });
            let _ = stdout.write_all(&buf);
            let _ = stdout.flush();
            self.stage_overlay_cleanup(CleanupScope::MainAndDetail);
        } else if let Some(det) = detail_layout {
            // Defensive: detail layout exists without a main popup layout
            // (shouldn't happen, but clean it up if it does).
            let mut buf = Vec::new();
            self.bump_output_epoch();
            clear_detail_box(&mut buf, &det);
            let _ = stdout.write_all(&buf);
            let _ = stdout.flush();
            self.stage_overlay_cleanup(CleanupScope::MainAndDetail);
        } else {
            self.pending_overlay_cleanup = None;
        }
        self.pending_overlay_render = None;
        self.visible = false;
        self.suggestions.clear();
        self.overlay.reset();
        self.reset_detail_debounce_state();
        if !preserve_trigger_request {
            self.trigger_requested = false;
        }
        self.debounce_suppressed = false;
        if let Some(handle) = self.dynamic_task.take() {
            handle.abort();
        }
        self.dynamic_rx = None;
        self.dynamic_ctx = None;
        self.feedback = AsyncFeedback::Idle;
        self.pending_failed.clear();
        self.pending_empty_count = 0;
        // Invalidate the idempotency guard so the next trigger (e.g. after
        // ESC-then-retrigger on the same buffer) runs a fresh suggest_sync
        // instead of short-circuiting.
        self.last_trigger_fingerprint = None;
    }

    fn bump_output_epoch(&mut self) {
        self.output_epoch = self.output_epoch.wrapping_add(1);
    }

    /// Compute the accept bytes for the currently-selected suggestion using
    /// an already-locked parser. Caller owns the lock so additional reads
    /// (e.g. for CD chaining prediction) can happen under the same critical
    /// section without a second `parser.lock()` round-trip.
    ///
    /// Returns an [`AcceptLocked`]: `forward` is what the simple-accept path
    /// needs; `cwd`, `cursor`, `screen` and `escaped_replacement` are cheap to
    /// pull from the same `TerminalState` snapshot and are consumed by
    /// `accept_with_chaining` when the selection is a directory.
    ///
    /// Returns `None` when there is nothing to accept: no effective selection
    /// (no navigation and either `tab_accepts_top` is off or the list has no
    /// real completion — see [`Self::effective_selected`]), or the resolved
    /// index is out of range. With `tab_accepts_top` enabled, an un-navigated
    /// overlay resolves to the first non-Ask-AI completion rather than
    /// short-circuiting.
    fn accept_suggestion_locked(&self, p: &TerminalParser) -> Option<AcceptLocked> {
        let selected_idx = self.effective_selected()?;
        if selected_idx >= self.suggestions.len() {
            return None;
        }
        let selected = &self.suggestions[selected_idx];

        let state = p.state();
        let buffer = state.command_buffer().unwrap_or("");
        let cursor = state.buffer_cursor();
        let ctx = parse_command_context(buffer, cursor);
        let cwd = state.cwd().cloned().unwrap_or_else(|| PathBuf::from("."));
        // Construct the newtypes directly from the parser tuples at the read
        // site so the bare-`u16` window where a row/col (or rows/cols) swap is
        // compiler-undetectable shrinks to a single destructuring line per
        // tuple, rather than four free `u16` locals re-wired later.
        let cursor_pos = {
            let (row, col) = state.cursor_position();
            CursorPos { row, col }
        };
        let screen = {
            let (rows, cols) = state.screen_dimensions();
            ScreenSize { rows, cols }
        };

        let (delete_chars, replacement) = if selected.kind == suggest::SuggestionKind::History
            || selected.kind == suggest::SuggestionKind::ProviderValue
            || selected.kind == suggest::SuggestionKind::Llm
        {
            // History: delete the entire buffer up to cursor, then type
            // the full command. Cursor is always at buffer end when
            // popup is visible (arrow keys dismiss), but we use cursor
            // (not buffer.chars().count()) because over-deleting past
            // cursor into the prompt would be worse than leaving
            // trailing chars.
            //
            // Defense-in-depth: clamp cursor to buffer length even
            // though set_command_buffer already clamps, to prevent PTY
            // corruption if an unclamped value ever reaches here.
            let buf_len = buffer.chars().count();
            let safe_cursor = cursor.min(buf_len);
            if safe_cursor != buf_len {
                tracing::warn!(
                    cursor = safe_cursor,
                    buffer_len = buf_len,
                    "history accept: cursor not at end of buffer — \
                         using cursor position to avoid over-deleting into prompt"
                );
            }
            (safe_cursor, selected.text.clone())
        } else {
            // Delete one 0x7F per ON-SCREEN character of the current word, NOT
            // per character of the tokenizer-decoded `current_word`. The raw
            // buffer can hold backslash escapes / an opening quote (e.g.
            // `My\ Folder/` or `'My Fo`) that the tokenizer strips; counting
            // the decoded word would under-delete by exactly those bytes and
            // leave stray chars before the replacement. Compute the span from
            // the raw word start up to the cursor instead.
            let byte_cursor = char_to_byte_offset(buffer, cursor);
            let raw_word_start = current_word_raw_start(buffer, byte_cursor);
            // In a quoted context the raw word start points at the opening
            // quote, but that quote is structural and must survive the accept —
            // the quoted escape arms emit bare text assuming it does. Stop the
            // delete span after the opening quote so it is preserved; the
            // unquoted case deletes the whole raw word unchanged.
            let delete_start =
                current_word_delete_start(buffer, raw_word_start, byte_cursor, ctx.quote_state);
            let on_screen_word_len = buffer[delete_start..byte_cursor].chars().count();
            // Filesystem suggestions go through shell-escape so paths
            // containing spaces or other metacharacters survive
            // re-parsing by the shell. The chaining caller reuses the
            // same escaped string (returned below) so the predicted
            // buffer matches what the shell will actually see.
            let raw = selected.text.clone();
            let escaped = match selected.kind {
                suggest::SuggestionKind::FilePath | suggest::SuggestionKind::Directory => {
                    shell_escape_for_context(&raw, ctx.quote_state)
                }
                _ => raw,
            };
            (on_screen_word_len, escaped)
        };

        // One 0x7F (backspace) per CHARACTER — the shell deletes by character, not byte
        let mut bytes = vec![0x7F; delete_chars];
        bytes.extend_from_slice(replacement.as_bytes());

        Some(AcceptLocked {
            forward: bytes,
            cwd,
            cursor: cursor_pos,
            screen,
            escaped_replacement: replacement,
        })
    }

    fn accept_suggestion(&self, parser: &Arc<Mutex<TerminalParser>>) -> Vec<u8> {
        // Poison handling mirrors Task B in proxy.rs: if the parser
        // mutex is poisoned we can't safely read the buffer, so return
        // empty bytes (caller treats this as "no-op accept"). Failing
        // here must not crash the proxy.
        let p = match parser.lock() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("parser mutex poisoned in accept_suggestion: {e} — dropping accept");
                return Vec::new();
            }
        };
        match self.accept_suggestion_locked(&p) {
            Some(accepted) => accepted.forward,
            None => Vec::new(),
        }
    }

    /// Handle terminal resize while popup is visible.
    /// Dismisses popup instead of re-rendering — after a resize, screen dimensions
    /// change and prior scroll deficit is stale. Popup recomputes on next trigger.
    pub fn handle_resize(&mut self, _parser: &Arc<Mutex<TerminalParser>>, stdout: &mut dyn Write) {
        if self.visible {
            self.dismiss(stdout);
        }
        // Screen dimensions changed — prior scroll deficit is meaningless.
        self.overlay_scroll_deficit = 0;
    }

    /// Abort any in-flight dynamic generator task. Called during proxy
    /// shutdown to prevent orphaned background tasks.
    pub fn abort_dynamic_task(&mut self) {
        if let Some(handle) = self.dynamic_task.take() {
            handle.abort();
        }
        self.feedback = AsyncFeedback::Idle;
        self.pending_failed.clear();
        self.pending_empty_count = 0;
    }

    /// Abort any in-flight dynamic generator task and clear the spawn-time
    /// context snapshot. Used by the keystroke-cancel arm of the bounded-block
    /// debounce path: the rx was already dropped, so without aborting the task
    /// here it would burn CPU/IO and its eventual results would be silently
    /// discarded by `try_merge_dynamic` (rx is None).
    pub fn abort_dynamic_task_and_clear_ctx(&mut self) {
        if let Some(handle) = self.dynamic_task.take() {
            handle.abort();
        }
        self.dynamic_ctx = None;
        self.dynamic_rx = None;
        self.feedback = AsyncFeedback::Idle;
        self.pending_failed.clear();
        self.pending_empty_count = 0;
    }
}

/// Terminal cursor location (row, col), read under the parser lock.
///
/// A distinct newtype from [`ScreenSize`] so the two `(u16, u16)` pairs in
/// [`AcceptLocked`] can't be silently transposed at the call site — the
/// compiler rejects passing a cursor where a screen size is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CursorPos {
    row: u16,
    col: u16,
}

/// Terminal screen dimensions (rows, cols), read under the parser lock.
///
/// A distinct newtype from [`CursorPos`]; see that type's docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScreenSize {
    rows: u16,
    cols: u16,
}

/// Return value of `accept_suggestion_locked`.
///
/// - `forward`: the bytes to forward to the PTY — what the plain-accept path
///   needs.
/// - `cwd`, `cursor`, `screen`: terminal geometry and working directory read
///   under the same parser lock. Only consumed by the CD-chaining path in
///   `accept_with_chaining`; the plain accept path discards them.
/// - `escaped_replacement`: the (possibly-escaped) replacement text. Sharing
///   it here is what keeps the predicted post-accept buffer aligned with the
///   bytes the shell actually receives — re-escaping it in the chaining caller
///   would risk drift between the two sites.
struct AcceptLocked {
    forward: Vec<u8>,
    cwd: PathBuf,
    cursor: CursorPos,
    screen: ScreenSize,
    escaped_replacement: String,
}

/// Quote-context-aware shell escape for filesystem path insertions.
///
/// The shell parses words differently depending on quote state: unquoted
/// words split on whitespace and interpret `*?[]{}~$`<>|&;()#`, single quotes
/// only end on the next `'`, and double quotes interpret `$\`"\\`. Without
/// matching the user's current quote state, accepting a path with spaces
/// silently corrupts the next command line.
///
/// The escape is conservative — in unquoted context every word-splitting or
/// expansion-triggering metacharacter is backslashed. Within single quotes
/// only an embedded apostrophe needs the close-reopen dance (`'\''`). Within
/// double quotes the four-character set `"`, `\`, `$`, `` ` `` is escaped;
/// everything else (including spaces and globs) is literal.
///
/// Tilde is a special case in the unquoted arm: a **leading** `~` must be left
/// bare so tilde expansion (`~` -> `$HOME`, `~user` -> that user's home) still
/// fires — the filesystem provider preserves a leading `~/` in suggestion text
/// (e.g. `cd ~/Doc` -> `~/Documents/`), and `\~` is a literal tilde that the
/// shell will not expand. A `~` anywhere else in the word is not subject to
/// expansion, so it is escaped like any other safe-by-default char would be if
/// it were special — kept here for parity with the historical behavior.
fn shell_escape_for_context(text: &str, quote: buffer::QuoteState) -> String {
    match quote {
        buffer::QuoteState::None => {
            // `~` is intentionally absent: a leading `~` must stay unescaped for
            // tilde expansion (see fn docs); a non-leading `~` is escaped via the
            // index check below.
            let needs_escape = |c: char| {
                matches!(
                    c,
                    ' ' | '\t'
                        | '\n'
                        | ';'
                        | '&'
                        | '|'
                        | '<'
                        | '>'
                        | '('
                        | ')'
                        | '$'
                        | '`'
                        | '\\'
                        | '"'
                        | '\''
                        | '*'
                        | '?'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                        | '#'
                )
            };
            let mut out = String::with_capacity(text.len() + 4);
            for (i, ch) in text.chars().enumerate() {
                // Escape a `~` only when it is NOT the first character of the
                // token — only a leading `~` triggers tilde expansion.
                if needs_escape(ch) || (ch == '~' && i != 0) {
                    out.push('\\');
                }
                out.push(ch);
            }
            out
        }
        buffer::QuoteState::SingleQuoted => text.replace('\'', r"'\''"),
        buffer::QuoteState::DoubleQuoted => {
            let mut out = String::with_capacity(text.len() + 4);
            for ch in text.chars() {
                if matches!(ch, '"' | '\\' | '$' | '`') {
                    out.push('\\');
                }
                out.push(ch);
            }
            out
        }
    }
}

/// Byte offset where the word the cursor sits in *begins* in the **raw**
/// buffer, including any opening quote and backslash escapes.
///
/// The tokenizer (`parse_command_context`) decodes words — it drops the
/// backslashes of `My\ Folder/` and the opening `'` of `'My Fo`. Counting
/// backspaces from the *decoded* `current_word` length therefore under-deletes
/// the on-screen word by exactly the number of escape/quote bytes, leaving
/// stray characters before the replacement. The accept path needs the RAW
/// on-screen span instead, so it walks the raw prefix and records where the
/// current word started.
///
/// `byte_cursor` is a byte offset into `buffer` (already converted from the
/// char cursor by the caller). The returned offset is `<= byte_cursor`. When
/// the cursor sits at a word boundary (trailing space / empty current word)
/// the returned offset equals `byte_cursor`, so the on-screen span is empty —
/// matching the tokenizer's empty `current_word`.
///
/// This mirrors the tokenizer's unquoted word-boundary rules (split on ASCII
/// whitespace and on the pipe/redirect/control operators; `\` escapes the next
/// char; `'` / `"` open quote spans that suppress boundaries). It only needs
/// to be correct for the prefix up to the cursor, which is all the accept path
/// inspects.
fn current_word_raw_start(buffer: &str, byte_cursor: usize) -> usize {
    let prefix = &buffer[..byte_cursor.min(buffer.len())];
    // Byte offset where the in-progress word started; `None` while between
    // words (i.e. cursor would have an empty current_word here).
    let mut word_start: Option<usize> = None;
    let mut quote = buffer::QuoteState::None;
    let mut iter = prefix.char_indices().peekable();

    while let Some((idx, ch)) = iter.next() {
        match quote {
            buffer::QuoteState::SingleQuoted => {
                if ch == '\'' {
                    quote = buffer::QuoteState::None;
                }
                // Stays in the current word either way.
            }
            buffer::QuoteState::DoubleQuoted => {
                if ch == '"' {
                    quote = buffer::QuoteState::None;
                } else if ch == '\\' {
                    // Backslash + next char are both part of the word.
                    iter.next();
                }
            }
            buffer::QuoteState::None => {
                if ch == '\\' {
                    // Escaped char joins (or starts) the current word.
                    if word_start.is_none() {
                        word_start = Some(idx);
                    }
                    iter.next();
                } else if ch == '\'' {
                    if word_start.is_none() {
                        word_start = Some(idx);
                    }
                    quote = buffer::QuoteState::SingleQuoted;
                } else if ch == '"' {
                    if word_start.is_none() {
                        word_start = Some(idx);
                    }
                    quote = buffer::QuoteState::DoubleQuoted;
                } else if ch.is_ascii_whitespace() || matches!(ch, '|' | '&' | ';' | '<' | '>') {
                    // Word boundary — the next word (if any) starts later.
                    // `(`/`)` are intentionally NOT boundaries: the tokenizer
                    // keeps unquoted parens inside `current_word` (only `$(...)`
                    // is consumed specially), so treating them as boundaries
                    // here would split the word earlier than the tokenizer and
                    // under-delete a path containing a paren.
                    word_start = None;
                } else if word_start.is_none() {
                    word_start = Some(idx);
                }
            }
        }
    }

    word_start.unwrap_or(byte_cursor)
}

/// Byte offset where the accept path should **begin deleting** (and insert the
/// replacement), given the raw word start and the cursor's quote context.
///
/// In an unquoted context this is just `raw_word_start` — the whole on-screen
/// word is replaced. But when the cursor sits inside an *open* single/double
/// quote, the opening quote is structural: `shell_escape_for_context` emits bare
/// text for the quoted arms precisely because it assumes the surrounding quote
/// survives (it does not re-emit one). If we deleted back through the opening
/// quote and inserted that bare text, the quote would vanish and the now-
/// unquoted spaces/metacharacters would word-split the line (regression fixed
/// here: `cat 'My` accepting `My Folder/file.txt` previously produced
/// `cat My Folder/file.txt`). So in a quoted context we stop the delete span
/// *after* the unmatched opening quote, preserving it.
///
/// The opening quote is found by re-walking the raw word span `[raw_word_start,
/// byte_cursor)` and tracking which quote char opened the span that is still
/// open at the cursor — it is not always `buffer[raw_word_start]` (e.g.
/// `foo'My Fo`, where the word starts at `foo` and the quote opens mid-word).
/// `quote` is the tokenizer's quote state at the cursor; when it is
/// `QuoteState::None` no quote is preserved.
fn current_word_delete_start(
    buffer: &str,
    raw_word_start: usize,
    byte_cursor: usize,
    quote: buffer::QuoteState,
) -> usize {
    if quote == buffer::QuoteState::None {
        return raw_word_start;
    }
    // Re-walk the word span to locate the unmatched opening quote whose span is
    // still open at the cursor. The delete span starts just past it so the
    // structural quote survives the accept.
    let end = byte_cursor.min(buffer.len());
    let span = &buffer[raw_word_start..end];
    let mut state = buffer::QuoteState::None;
    // Byte offset (absolute into `buffer`) of the currently-open quote char.
    let mut open_quote_at: Option<usize> = None;
    let mut iter = span.char_indices().peekable();
    while let Some((rel, ch)) = iter.next() {
        let idx = raw_word_start + rel;
        match state {
            buffer::QuoteState::SingleQuoted => {
                if ch == '\'' {
                    state = buffer::QuoteState::None;
                    open_quote_at = None;
                }
            }
            buffer::QuoteState::DoubleQuoted => {
                if ch == '"' {
                    state = buffer::QuoteState::None;
                    open_quote_at = None;
                } else if ch == '\\' {
                    iter.next();
                }
            }
            buffer::QuoteState::None => {
                if ch == '\\' {
                    iter.next();
                } else if ch == '\'' {
                    state = buffer::QuoteState::SingleQuoted;
                    open_quote_at = Some(idx);
                } else if ch == '"' {
                    state = buffer::QuoteState::DoubleQuoted;
                    open_quote_at = Some(idx);
                }
            }
        }
    }
    match open_quote_at {
        // Preserve the opening quote: start deleting at the byte AFTER it.
        Some(q) => q + buffer[q..].chars().next().map_or(1, char::len_utf8),
        // Defensive: tokenizer reported a quote state but the walk found no open
        // quote in the word span (should not happen). Fall back to deleting the
        // whole raw word rather than risk leaving a stray fragment.
        None => raw_word_start,
    }
}

/// Grace period after a popup repaint during which display-dirty output
/// is treated as shell echo (not genuine output) and does NOT tear down
/// the popup. Covers fish's multi-batch redraw (syntax highlighting,
/// autosuggestions) which arrives in 2-3 PTY reads per keystroke.
const REPAINT_GRACE_MS: u128 = 150;

type TriggerFingerprint = (u64, usize, u64);

/// Compute a lightweight fingerprint of the current command-line buffer for
/// the trigger-idempotency guard on `InputHandler::last_trigger_fingerprint`.
/// Collision resistance doesn't need to be cryptographic — a same-content
/// match just short-circuits `trigger()` (saving work and avoiding the
/// stale-dismiss bug); a false collision would at worst miss one re-render,
/// which the next real buffer edit or environment update fixes.
fn buffer_fingerprint(
    buffer: &str,
    cursor: usize,
    shell_env: Option<&std::collections::HashMap<String, String>>,
) -> TriggerFingerprint {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    buffer.hash(&mut hasher);
    let env_hash = shell_env
        .map(|env| {
            let mut h = DefaultHasher::new();
            // HashMap has no stable iteration order, so hash entries in
            // sorted key order to keep the fingerprint deterministic.
            let mut entries: Vec<(&String, &String)> = env.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            for (k, v) in entries {
                k.hash(&mut h);
                v.hash(&mut h);
            }
            h.finish()
        })
        .unwrap_or(u64::MAX);
    (hasher.finish(), cursor, env_hash)
}

/// Merge cached tree-completion rows into the sync suggestion pool.
///
/// Cached rows are shell-completion provider results served synchronously
/// from the persistent tree cache. They are appended after the engine's sync
/// suggestions (history/commands/filesystem/env) and deduplicated by `text`
/// so a row the engine already produced is not shown twice. This does NOT
/// re-rank — callers that merge cached rows must run `rerank_live` on the
/// result so cached rows compete on score instead of sitting at the tail.
fn merge_cached_suggestions(sync: Vec<Suggestion>, cached: Vec<Suggestion>) -> Vec<Suggestion> {
    if cached.is_empty() {
        return sync;
    }
    let mut seen: std::collections::HashSet<String> = sync.iter().map(|s| s.text.clone()).collect();
    let mut out = sync;
    for s in cached {
        if seen.insert(s.text.clone()) {
            out.push(s);
        }
    }
    out
}

/// Two borrowed `HashSet<&str>`s (existing + per-batch) — keeping references
/// rather than owned `String` keys avoids the `s.text.clone()` per dupe check
/// that the previous owned-HashSet version paid on every dynamic merge.
fn merge_dedup_against(existing: &[Suggestion], incoming: Vec<Suggestion>) -> Vec<Suggestion> {
    let keep: Vec<bool> = {
        let existing_set: HashSet<&str> = existing.iter().map(|s| s.text.as_str()).collect();
        let mut batch_seen: HashSet<&str> = HashSet::with_capacity(incoming.len());
        incoming
            .iter()
            .map(|s| !existing_set.contains(s.text.as_str()) && batch_seen.insert(s.text.as_str()))
            .collect()
    };
    incoming
        .into_iter()
        .zip(keep)
        .filter_map(|(s, k)| if k { Some(s) } else { None })
        .collect()
}

/// Convert a KeyEvent back to raw bytes for forwarding to PTY.
pub fn key_to_bytes(key: &KeyEvent) -> Vec<u8> {
    match key {
        KeyEvent::Tab => vec![0x09],
        KeyEvent::Enter => vec![0x0D],
        KeyEvent::Escape => vec![0x1B],
        KeyEvent::ArrowUp => vec![0x1B, b'[', b'A'],
        KeyEvent::ArrowDown => vec![0x1B, b'[', b'B'],
        KeyEvent::ArrowRight => vec![0x1B, b'[', b'C'],
        KeyEvent::ArrowLeft => vec![0x1B, b'[', b'D'],
        KeyEvent::PageUp => vec![0x1B, b'[', b'5', b'~'],
        KeyEvent::PageDown => vec![0x1B, b'[', b'6', b'~'],
        KeyEvent::Home => vec![0x1B, b'[', b'H'],
        KeyEvent::HomeCsiTilde => vec![0x1B, b'[', b'1', b'~'],
        KeyEvent::HomeCsi7Tilde => vec![0x1B, b'[', b'7', b'~'],
        KeyEvent::HomeSs3 => vec![0x1B, b'O', b'H'],
        KeyEvent::End => vec![0x1B, b'[', b'F'],
        KeyEvent::EndCsiTilde => vec![0x1B, b'[', b'4', b'~'],
        KeyEvent::EndCsi8Tilde => vec![0x1B, b'[', b'8', b'~'],
        KeyEvent::EndSs3 => vec![0x1B, b'O', b'F'],
        KeyEvent::CtrlSpace => vec![0x00],
        KeyEvent::CtrlSlash => vec![0x1F],
        KeyEvent::Backspace => vec![0x7F],
        KeyEvent::Ctrl(c) => {
            if !c.is_ascii_lowercase() {
                tracing::error!(char = ?c, "Ctrl(char) contains non-lowercase ASCII — skipping");
                return Vec::new();
            }
            vec![*c as u8 - 0x60]
        }
        KeyEvent::Printable(c) => {
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        KeyEvent::CursorPositionReport(_, _) => Vec::new(), // intercepted in proxy
        KeyEvent::Raw(bytes) => bytes.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use overlay::types::DEFAULT_MAX_VISIBLE;
    use suggest::ShellFamily;
    use suggest::{SuggestionKind, SuggestionSource};

    #[test]
    fn test_key_to_bytes_tab() {
        assert_eq!(key_to_bytes(&KeyEvent::Tab), vec![0x09]);
    }

    #[test]
    fn test_key_to_bytes_arrow_up() {
        assert_eq!(key_to_bytes(&KeyEvent::ArrowUp), vec![0x1B, b'[', b'A']);
    }

    #[test]
    fn test_key_to_bytes_page_up_round_trip() {
        assert_eq!(key_to_bytes(&KeyEvent::PageUp), b"\x1B[5~");
    }

    #[test]
    fn test_key_to_bytes_page_down_round_trip() {
        assert_eq!(key_to_bytes(&KeyEvent::PageDown), b"\x1B[6~");
    }

    #[test]
    fn test_key_to_bytes_home_round_trip() {
        assert_eq!(key_to_bytes(&KeyEvent::Home), b"\x1B[H");
        assert_eq!(key_to_bytes(&KeyEvent::HomeCsiTilde), b"\x1B[1~");
        assert_eq!(key_to_bytes(&KeyEvent::HomeCsi7Tilde), b"\x1B[7~");
        assert_eq!(key_to_bytes(&KeyEvent::HomeSs3), b"\x1BOH");
    }

    #[test]
    fn test_key_to_bytes_end_round_trip() {
        assert_eq!(key_to_bytes(&KeyEvent::End), b"\x1B[F");
        assert_eq!(key_to_bytes(&KeyEvent::EndCsiTilde), b"\x1B[4~");
        assert_eq!(key_to_bytes(&KeyEvent::EndCsi8Tilde), b"\x1B[8~");
        assert_eq!(key_to_bytes(&KeyEvent::EndSs3), b"\x1BOF");
    }

    #[test]
    fn test_key_to_bytes_printable() {
        assert_eq!(key_to_bytes(&KeyEvent::Printable('x')), vec![b'x']);
    }

    #[test]
    fn test_key_to_bytes_raw() {
        let raw = vec![0x1B, b'[', b'1', b';', b'5', b'C'];
        assert_eq!(key_to_bytes(&KeyEvent::Raw(raw.clone())), raw);
    }

    #[test]
    fn test_key_to_bytes_roundtrip() {
        let keys = vec![
            KeyEvent::Tab,
            KeyEvent::Enter,
            KeyEvent::Escape,
            KeyEvent::ArrowUp,
            KeyEvent::ArrowDown,
            KeyEvent::ArrowLeft,
            KeyEvent::ArrowRight,
            KeyEvent::PageUp,
            KeyEvent::PageDown,
            KeyEvent::Home,
            KeyEvent::HomeCsiTilde,
            KeyEvent::HomeCsi7Tilde,
            KeyEvent::HomeSs3,
            KeyEvent::End,
            KeyEvent::EndCsiTilde,
            KeyEvent::EndCsi8Tilde,
            KeyEvent::EndSs3,
            KeyEvent::CtrlSpace,
            KeyEvent::CtrlSlash,
            KeyEvent::Backspace,
            KeyEvent::Printable('a'),
            KeyEvent::Raw(vec![0xFF]),
            KeyEvent::Ctrl('a'),
            KeyEvent::Ctrl('d'),
            KeyEvent::Ctrl('z'),
        ];
        for key in keys {
            let bytes = key_to_bytes(&key);
            assert!(!bytes.is_empty(), "key_to_bytes({:?}) was empty", key);
        }
    }

    #[test]
    fn test_key_to_bytes_ctrl() {
        assert_eq!(key_to_bytes(&KeyEvent::Ctrl('a')), vec![0x01]);
        assert_eq!(key_to_bytes(&KeyEvent::Ctrl('d')), vec![0x04]);
        assert_eq!(key_to_bytes(&KeyEvent::Ctrl('z')), vec![0x1A]);
    }

    #[test]
    fn test_try_merge_dynamic_empty_query_sorts_branches_before_history_and_files() {
        // On empty query, rerank_live sorts by source-order then priority.
        // Provider (branches/tags) precedes Filesystem and History in the
        // default source order, so dynamic arrivals sort to the top.
        use suggest::SuggestionKind;

        let mut handler = make_visible_handler(vec![
            Suggestion {
                text: "Makefile".to_string(),
                kind: SuggestionKind::FilePath,
                source: SuggestionSource::Filesystem,
                ..Default::default()
            },
            Suggestion {
                text: "git checkout demo".to_string(),
                kind: SuggestionKind::History,
                source: SuggestionSource::History,
                ..Default::default()
            },
        ]);

        // Prime the snapshot so the staleness check against a freshly-parsed
        // empty buffer passes (both ends resolve to command=None, args=[], word_index=0).
        let base_ctx = buffer::parse_command_context("", 0);
        handler.dynamic_ctx = Some(DynamicCtxSnapshot::capture(&base_ctx));

        let (tx, rx) = mpsc::channel::<DynamicResult>(1);
        tx.try_send(DynamicResult::Loaded {
            provider: ProviderTag::Async("git branches".into()),
            suggestions: vec![
                Suggestion {
                    text: "main".to_string(),
                    kind: SuggestionKind::Subcommand,
                    source: SuggestionSource::Provider,
                    ..Default::default()
                },
                Suggestion {
                    text: "v1.0".to_string(),
                    kind: SuggestionKind::Subcommand,
                    source: SuggestionSource::Provider,
                    ..Default::default()
                },
            ],
        })
        .unwrap();
        drop(tx);
        handler.dynamic_rx = Some(rx);

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let merged = handler.try_merge_dynamic(&parser, &mut buf);

        assert!(merged, "merge should have happened");
        let kinds: Vec<SuggestionKind> = handler.suggestions.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![
                SuggestionKind::Subcommand,
                SuggestionKind::Subcommand,
                SuggestionKind::FilePath,
                SuggestionKind::History,
            ],
            "branches and tags must land above files and history on empty query: {:?}",
            handler.suggestions,
        );
    }

    #[test]
    fn test_try_merge_dynamic_empty_query_stable_tiebreak_by_text() {
        // When two dynamic arrivals share the same effective priority (e.g. two
        // `Subcommand` entries), the comparator falls through to
        // `then_with(|| a.text.cmp(&b.text))` so the popup order is
        // alphabetic rather than channel-arrival order. Locks in both tiers
        // of the comparator: kind-priority first, text second.
        use suggest::SuggestionKind;

        let mut handler = make_visible_handler(vec![Suggestion {
            text: "Makefile".to_string(),
            kind: SuggestionKind::FilePath,
            source: SuggestionSource::Filesystem,
            ..Default::default()
        }]);

        let base_ctx = buffer::parse_command_context("", 0);
        handler.dynamic_ctx = Some(DynamicCtxSnapshot::capture(&base_ctx));

        let (tx, rx) = mpsc::channel::<DynamicResult>(1);
        tx.try_send(DynamicResult::Loaded {
            provider: ProviderTag::Async("git branches".into()),
            suggestions: vec![
                Suggestion {
                    text: "zeta".to_string(),
                    kind: SuggestionKind::Subcommand,
                    source: SuggestionSource::Provider,
                    ..Default::default()
                },
                Suggestion {
                    text: "alpha".to_string(),
                    kind: SuggestionKind::Subcommand,
                    source: SuggestionSource::Provider,
                    ..Default::default()
                },
            ],
        })
        .unwrap();
        drop(tx);
        handler.dynamic_rx = Some(rx);

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let merged = handler.try_merge_dynamic(&parser, &mut buf);

        assert!(merged, "merge should have happened");
        let ordered: Vec<(SuggestionKind, String)> = handler
            .suggestions
            .iter()
            .map(|s| (s.kind, s.text.clone()))
            .collect();
        assert_eq!(
            ordered,
            vec![
                (SuggestionKind::Subcommand, "alpha".to_string()),
                (SuggestionKind::Subcommand, "zeta".to_string()),
                (SuggestionKind::FilePath, "Makefile".to_string()),
            ],
            "same-priority subcommands must tiebreak alphabetically and both land above files: {:?}",
            handler.suggestions,
        );
    }

    #[test]
    fn test_try_merge_dynamic_keeps_open_receiver_for_later_batches() {
        let mut handler = make_visible_handler(Vec::new());
        let base_ctx = buffer::parse_command_context("", 0);
        handler.dynamic_ctx = Some(DynamicCtxSnapshot::capture(&base_ctx));
        handler.spawned_generation = handler.buffer_generation;

        let (tx, rx) = mpsc::channel::<DynamicResult>(2);
        tx.try_send(DynamicResult::Loaded {
            provider: ProviderTag::Async("git branches".into()),
            suggestions: vec![Suggestion {
                text: "main".to_string(),
                kind: SuggestionKind::Subcommand,
                source: SuggestionSource::Provider,
                ..Default::default()
            }],
        })
        .unwrap();
        handler.dynamic_rx = Some(rx);

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        assert!(handler.try_merge_dynamic(&parser, &mut buf));
        assert!(
            handler.dynamic_rx.is_some(),
            "open receiver must stay installed for later dynamic batches"
        );
        assert!(
            handler.dynamic_ctx.is_some(),
            "fresh context must survive until the dynamic channel disconnects"
        );

        tx.try_send(DynamicResult::Loaded {
            provider: ProviderTag::Async("git tags".into()),
            suggestions: vec![Suggestion {
                text: "v1.0".to_string(),
                kind: SuggestionKind::Subcommand,
                source: SuggestionSource::Provider,
                ..Default::default()
            }],
        })
        .unwrap();
        drop(tx);

        let mut buf = Vec::new();
        assert!(handler.try_merge_dynamic(&parser, &mut buf));
        let texts: Vec<&str> = handler
            .suggestions
            .iter()
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(texts, vec!["main", "v1.0"]);
        assert!(
            handler.dynamic_rx.is_none(),
            "receiver should clear after the final disconnected batch"
        );
        assert!(
            handler.dynamic_ctx.is_none(),
            "context should clear after the final disconnected batch"
        );
    }

    #[test]
    fn test_try_merge_dynamic_disconnected_rerenders_to_clear_loading() {
        // Regression: when the dynamic channel disconnects (generator task
        // finished without sending, or was aborted), `try_merge_dynamic`
        // previously cleared `dynamic_rx` but did NOT re-render. The popup
        // kept showing the loading indicator from its last paint because
        // render() reads `loading = self.dynamic_rx.is_some()` — without a
        // fresh render, the on-screen indicator is a stale snapshot. On an
        // idle shell this would stay stuck until the user typed or
        // dismissed manually.
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "static".to_string(),
            ..Default::default()
        }]);

        // Closed receiver: drop tx immediately so try_recv returns
        // Disconnected on the first call.
        let (tx, rx) = mpsc::channel::<DynamicResult>(1);
        drop(tx);
        handler.dynamic_rx = Some(rx);

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        handler.try_merge_dynamic(&parser, &mut buf);

        assert!(
            handler.dynamic_rx.is_none(),
            "dynamic_rx must be cleared on Disconnected"
        );
        assert!(
            !buf.is_empty(),
            "Disconnected path must re-render so the loading indicator clears"
        );
    }

    #[test]
    fn test_render_survives_poisoned_parser_mutex() {
        // Regression: previously render() called `parser.lock().unwrap()`,
        // which panics on poison. A poisoned parser mutex (from any prior
        // panic inside a parser lock in Task B or elsewhere) must not take
        // down Task B's render path.
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "poisoned".to_string(),
            ..Default::default()
        }]);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));

        // Poison the mutex by panicking inside a guard.
        let parser_clone = parser.clone();
        let _ = std::thread::spawn(move || {
            let _guard = parser_clone.lock().unwrap();
            panic!("intentional poison for test");
        })
        .join();
        assert!(parser.is_poisoned(), "setup: mutex must be poisoned");

        // Must not panic.
        let mut buf = Vec::new();
        handler.render(&parser, &mut buf);
    }

    #[test]
    fn test_accept_suggestion_survives_poisoned_parser_mutex() {
        // Regression: previously accept_suggestion() called
        // `parser.lock().unwrap()`, which panics on poison. Must return
        // an empty byte vec instead so the PTY proxy can continue cleanly.
        let handler = make_selected_handler(Suggestion {
            text: "poisoned".to_string(),
            kind: SuggestionKind::Subcommand,
            source: SuggestionSource::Commands,
            ..Default::default()
        });
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));

        let parser_clone = parser.clone();
        let _ = std::thread::spawn(move || {
            let _guard = parser_clone.lock().unwrap();
            panic!("intentional poison for test");
        })
        .join();
        assert!(parser.is_poisoned(), "setup: mutex must be poisoned");

        let bytes = handler.accept_suggestion(&parser);
        assert!(
            bytes.is_empty(),
            "accept_suggestion with poisoned mutex must return empty, got {bytes:?}"
        );
    }

    #[test]
    fn test_trigger_survives_poisoned_parser_mutex() {
        // Regression: previously trigger() called `parser.lock().unwrap()`,
        // which panics on poison. trigger() is the main entry point of the
        // suggestion pipeline — it runs from the debounce loop, Task B's
        // buffer_dirty/cwd_dirty branches, and the SIGWINCH handler — so a
        // poisoned parser (from any prior panic inside a parser lock) must
        // not propagate up through trigger().
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "poisoned".to_string(),
            ..Default::default()
        }]);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));

        // Poison the mutex by panicking inside a guard.
        let parser_clone = parser.clone();
        let _ = std::thread::spawn(move || {
            let _guard = parser_clone.lock().unwrap();
            panic!("intentional poison for test");
        })
        .join();
        assert!(parser.is_poisoned(), "setup: mutex must be poisoned");

        // Must not panic — trigger should log a warning and return without
        // touching the parser on the poisoned path.
        let mut buf = Vec::new();
        handler.trigger(&parser, &mut buf);
    }

    #[test]
    fn test_accept_with_chaining_survives_poisoned_parser_mutex() {
        // Regression: previously accept_with_chaining() called
        // `parser.lock().unwrap()` on the directory-chaining path, which
        // panics on poison. accept_with_chaining() runs every time the
        // user Tab-accepts a directory suggestion, so a poisoned parser
        // must not take down the proxy.
        let mut handler = make_selected_handler(Suggestion {
            // Trailing '/' makes is_dir=true, which is what hits the
            // parser.lock().unwrap() path inside accept_with_chaining.
            text: "Desktop/".to_string(),
            kind: SuggestionKind::Directory,
            source: SuggestionSource::Filesystem,
            ..Default::default()
        });
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));

        // Poison the mutex by panicking inside a guard.
        let parser_clone = parser.clone();
        let _ = std::thread::spawn(move || {
            let _guard = parser_clone.lock().unwrap();
            panic!("intentional poison for test");
        })
        .join();
        assert!(parser.is_poisoned(), "setup: mutex must be poisoned");

        // Must not panic — accept_with_chaining should log a warning and
        // return an empty byte vec so Task A forwards nothing to the PTY.
        let mut buf = Vec::new();
        let bytes = handler.accept_with_chaining(&parser, &mut buf);
        assert!(
            bytes.is_empty(),
            "accept_with_chaining with poisoned mutex must return empty, got {bytes:?}"
        );
    }

    #[test]
    fn test_dismiss_clears_state() {
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "test".to_string(),
            ..Default::default()
        }]);

        let mut stdout_buf = Vec::new();
        handler.dismiss(&mut stdout_buf);

        assert!(!handler.visible);
        assert!(handler.suggestions.is_empty());
        assert!(handler.last_layout.is_some());
        let cleanup_ticket = handler.overlay_write_ticket();
        assert!(cleanup_ticket.cleanup_token.is_some());
        handler.commit_overlay_write(cleanup_ticket);
        assert!(handler.last_layout.is_none());
        assert!(!stdout_buf.is_empty());
    }

    #[test]
    fn test_dismiss_clears_committed_detail_layout() {
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "test".to_string(),
            ..Default::default()
        }]);
        handler.last_detail_layout = Some(DetailLayout {
            start_row: 5,
            start_col: 24,
            width: 30,
            height: 3,
            position: overlay::DetailPosition::SideRight,
        });

        let mut stdout_buf = Vec::new();
        handler.dismiss(&mut stdout_buf);

        let output = String::from_utf8_lossy(&stdout_buf);
        assert!(
            output.contains("\x1b[6;25H"),
            "dismiss must clear the committed detail rectangle: {output:?}"
        );
        let cleanup_ticket = handler.overlay_write_ticket();
        assert!(
            cleanup_ticket.cleanup_token.is_some(),
            "dismiss cleanup must remain pending until stdout write ack"
        );
        assert!(handler.last_layout.is_some());
        assert!(handler.last_detail_layout.is_some());

        handler.commit_overlay_write(cleanup_ticket);

        assert!(handler.last_layout.is_none());
        assert!(handler.last_detail_layout.is_none());
    }

    #[test]
    fn test_trigger_idempotent_when_buffer_unchanged() {
        // Scenario:
        //   1. A prior `trigger()` populated the popup with static
        //      suggestions — visible=true, last_trigger_fingerprint is
        //      set for buffer B (fingerprint stamped on successful render
        //      in the `!result.suggestions.is_empty()` arm).
        //   2. A spurious re-trigger fires with buffer still at B (e.g.
        //      debounce loop tick, or SIGWINCH / Task B re-trigger without
        //      any intervening buffer edit).
        //   3. Without the idempotency guard, `suggest_sync` re-runs. If
        //      it returns empty with no async generators, the
        //      empty-no-generators arm calls `self.dismiss(stdout)`,
        //      emitting a clear-popup ANSI sequence and tearing down the
        //      popup — it disappears for no user-driven reason.
        //
        // `trigger()` fingerprints (buffer_hash, cursor_offset, env_hash)
        // and short-circuits when the fingerprint matches AND the popup is
        // still visible. ESC clears the fingerprint (via `dismiss()`), and a
        // genuine buffer or environment edit produces a different fingerprint.
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "prior-static".to_string(),
            ..Default::default()
        }]);

        // Drive the parser to report a non-empty buffer. OSC 7770 ;
        // <cursor> ; <buffer> BEL is the shell-integration buffer report
        // consumed by `parser` (see performer.rs OSC 7770 handler).
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let buffer = "xyzbogus";
        let cursor = buffer.chars().count();
        let osc = format!("\x1b]7770;{cursor};{buffer}\x07");
        parser.lock().unwrap().process_bytes(osc.as_bytes());
        assert_eq!(
            parser.lock().unwrap().state().command_buffer(),
            Some(buffer),
            "setup: OSC 7770 must land in command_buffer"
        );

        // Seed the fingerprint as if a prior trigger had populated this
        // popup for this exact buffer+cursor with no shell env snapshot.
        // This matches what the real code path sets on the
        // `!result.suggestions.is_empty()` arm.
        handler.last_trigger_fingerprint = Some(buffer_fingerprint(buffer, cursor, None));

        // First re-trigger: must be a full no-op (guard short-circuits
        // BEFORE suggest_sync runs, so no dismiss, no render, no writes).
        let mut buf1 = Vec::new();
        handler.trigger(&parser, &mut buf1);
        assert!(
            handler.visible,
            "popup must remain visible after idempotent re-trigger"
        );
        assert_eq!(
            handler.suggestions.len(),
            1,
            "prior static suggestion must survive idempotent re-trigger"
        );
        assert!(
            buf1.is_empty(),
            "idempotent re-trigger must not emit ANY bytes to stdout \
             (no clear-popup sequence, no re-render), got {:?}",
            String::from_utf8_lossy(&buf1)
        );

        // Second re-trigger with unchanged state: still a full no-op.
        let mut buf2 = Vec::new();
        handler.trigger(&parser, &mut buf2);
        assert!(
            handler.visible,
            "popup must remain visible after second idempotent re-trigger"
        );
        assert!(
            buf2.is_empty(),
            "second idempotent re-trigger must not emit ANY bytes, got {:?}",
            String::from_utf8_lossy(&buf2)
        );
    }

    #[test]
    fn test_trigger_suppressed_while_alt_screen_active() {
        // A TUI app (nvim, less, htop) owns the alternate screen via DECSET
        // 1049. trigger() must not render a popup over it — it early-returns
        // before touching any state.
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "prior".to_string(),
            ..Default::default()
        }]);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        parser.lock().unwrap().process_bytes(b"\x1b[?1049h");
        assert!(
            parser.lock().unwrap().state().in_alt_screen(),
            "setup: alt screen must be active"
        );

        let mut buf = Vec::new();
        handler.trigger(&parser, &mut buf);
        assert!(
            handler.visible,
            "popup must stay untouched while a TUI owns the alt screen"
        );
        assert!(buf.is_empty(), "no render bytes while alt screen active");
    }

    #[test]
    fn test_trigger_active_after_alt_screen_exit() {
        // Control: once the TUI exits the alt screen, trigger() resumes. With
        // an empty buffer it dismisses the stale popup — proof the gate lifted.
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "prior".to_string(),
            ..Default::default()
        }]);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        parser.lock().unwrap().process_bytes(b"\x1b[?1049h");
        parser.lock().unwrap().process_bytes(b"\x1b[?1049l");
        assert!(
            !parser.lock().unwrap().state().in_alt_screen(),
            "setup: back on the main screen"
        );

        let mut buf = Vec::new();
        handler.trigger(&parser, &mut buf);
        assert!(
            !handler.visible,
            "trigger() must act once the alt screen is gone"
        );
    }

    #[test]
    fn trigger_recomputes_when_shell_env_changes_for_same_buffer() {
        let mut handler = make_handler();
        handler.engine = Arc::new(
            SuggestionEngine::new(ShellFamily::Other)
                .unwrap()
                .with_suggest_config(50, false, 0, false),
        );
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        {
            let mut p = parser.lock().unwrap();
            p.process_bytes(b"\x1b]7773;AWS_REGION%3Dus-east-1\x07");
            p.state_mut()
                .predict_command_buffer("echo $AWS".to_string(), 9);
        }

        let mut first = Vec::new();
        handler.trigger(&parser, &mut first);
        handler.commit_overlay_write(handler.overlay_write_ticket());
        assert!(
            handler.suggestions.iter().any(|s| s.text == "$AWS_REGION"),
            "setup: first trigger should use initial shell env, got {:?}",
            handler
                .suggestions
                .iter()
                .map(|s| &s.text)
                .collect::<Vec<_>>()
        );

        {
            let mut p = parser.lock().unwrap();
            p.process_bytes(b"\x1b]7773;AWS_PROFILE%3Dloftyworks-pay-dev\x07");
        }

        let mut second = Vec::new();
        handler.trigger(&parser, &mut second);

        assert!(
            handler.suggestions.iter().any(|s| s.text == "$AWS_PROFILE"),
            "same-buffer re-trigger must observe updated shell env, got {:?}",
            handler
                .suggestions
                .iter()
                .map(|s| &s.text)
                .collect::<Vec<_>>()
        );
        assert!(
            handler.suggestions.iter().all(|s| s.text != "$AWS_REGION"),
            "updated shell env should replace prior snapshot, got {:?}",
            handler
                .suggestions
                .iter()
                .map(|s| &s.text)
                .collect::<Vec<_>>()
        );
    }

    fn make_handler() -> InputHandler {
        InputHandler {
            engine: Arc::new(SuggestionEngine::new(ShellFamily::Other).unwrap()),
            overlay: OverlayState::new(),
            suggestions: Vec::new(),
            last_layout: None,
            visible: false,
            trigger_requested: false,
            last_repaint_at: None,
            max_visible: DEFAULT_MAX_VISIBLE,
            debounce_suppressed: false,
            auto_trigger: true,
            keybindings: Keybindings::default(),
            theme: PopupTheme::default(),
            delay_ms: Arc::new(std::sync::atomic::AtomicU64::new(150)),
            dynamic_rx: None,
            dynamic_task: None,
            dynamic_notify: Arc::new(Notify::new()),
            feedback_tick_notify: Arc::new(Notify::new()),
            feedback: AsyncFeedback::Idle,
            feedback_dismiss_ms: 1200,
            pending_failed: Vec::new(),
            pending_empty_count: 0,
            dynamic_ctx: None,
            terminal_profile: TerminalProfile::for_ghostty(),
            overlay_scroll_deficit: 0,
            last_trigger_fingerprint: None,
            buffer_generation: 0,
            spawned_generation: 0,
            render_block_ms: 0,
            min_popup_width: DEFAULT_MIN_POPUP_WIDTH,
            max_popup_width: DEFAULT_MAX_POPUP_WIDTH,
            detail_box_mode: DescriptionBoxMode::Off,
            detail_box_max_width: 60,
            detail_box_lines: 5,
            detail_box_debounce_ms: 80,
            last_detail_layout: None,
            detail_redraw_notify: Arc::new(Notify::new()),
            mode_flash_notify: Arc::new(Notify::new()),
            detail_debounce: DetailDebounceState::default(),
            output_epoch: 0,
            overlay_render_generation: 0,
            pending_overlay_render: None,
            overlay_cleanup_generation: 0,
            pending_overlay_cleanup: None,
            tab_accepts_top: false,
            shell_kind: ShellKind::Other,
            input_model: BufferModel::default(),
            manual_trigger_stashed: false,
            async_providers: vec![],
            ask_ai_provider: None,
            completion_cache: None,
            mode_flash: None,
        }
    }

    /// Test builder: set up a visible popup with suggestions and a default layout.
    fn make_visible_handler(suggestions: Vec<Suggestion>) -> InputHandler {
        let mut h = make_handler();
        h.suggestions = suggestions;
        h.visible = true;
        h.last_layout = Some(PopupLayout {
            start_row: 5,
            start_col: 0,
            width: 20,
            height: 1,
            scroll_deficit: 0,
        });
        h
    }

    fn numbered_suggestions(count: usize) -> Vec<Suggestion> {
        (0..count)
            .map(|n| Suggestion {
                text: format!("item-{n}"),
                kind: SuggestionKind::Command,
                source: SuggestionSource::Commands,
                ..Default::default()
            })
            .collect()
    }

    fn command_suggestion(text: &str, description: Option<&str>) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            description: description.map(str::to_string),
            kind: SuggestionKind::Command,
            source: SuggestionSource::Commands,
            ..Default::default()
        }
    }

    /// Test builder: visible handler with a single selected suggestion.
    fn make_selected_handler(suggestion: Suggestion) -> InputHandler {
        let mut h = make_visible_handler(vec![suggestion]);
        h.overlay.selected = Some(0);
        h
    }

    #[test]
    fn rerank_live_honors_substring_match_mode() {
        // The live keystroke filter (issue #149) runs through rerank_live,
        // shared by both async merge paths. In Substring mode a candidate that
        // only matches as a subsequence ("calendar" for "cl") must be dropped;
        // the contiguous match ("clone") survives. This guards against either
        // merge site silently reverting to fuzzy-only ranking.
        use config::MatchMode;

        let substring = make_handler().with_match_mode(MatchMode::Substring);
        let ranked = substring.rerank_live(
            "cl",
            vec![
                command_suggestion("clone", None),
                command_suggestion("calendar", None),
            ],
        );
        let texts: Vec<&str> = ranked.iter().map(|s| s.text.as_str()).collect();
        assert!(
            texts.contains(&"clone"),
            "contiguous 'cl' must survive substring mode: {texts:?}"
        );
        assert!(
            !texts.contains(&"calendar"),
            "subsequence-only 'c..l' must be dropped in substring mode: {texts:?}"
        );

        // Contrast: the default fuzzy handler keeps the subsequence match.
        let fuzzy = make_handler();
        let ranked = fuzzy.rerank_live(
            "cl",
            vec![
                command_suggestion("clone", None),
                command_suggestion("calendar", None),
            ],
        );
        let texts: Vec<&str> = ranked.iter().map(|s| s.text.as_str()).collect();
        assert!(
            texts.contains(&"calendar"),
            "default fuzzy mode keeps the c..l subsequence: {texts:?}"
        );
    }

    #[test]
    fn rerank_live_fuzzy_subsequence_full_line() {
        // Fuzzy mode: "supababack" is a subsequence of "supabase backups"
        // (s-u-p-a-b-a-b-a-c-k ⊂ supabase backups). The full-line lane
        // ranks against the buffer, so the candidate must survive.
        let handler = make_handler(); // default = Fuzzy
        let ranked = handler.rerank_live(
            "supababack",
            vec![command_suggestion("supabase backups", None)],
        );
        assert!(
            ranked.iter().any(|s| s.text == "supabase backups"),
            "fuzzy subsequence must match full-line candidate: {ranked:?}"
        );
    }

    #[test]
    fn rerank_live_substring_contiguous_full_line() {
        // Substring mode: "base" is a contiguous run inside "supabase backups".
        use config::MatchMode;
        let handler = make_handler().with_match_mode(MatchMode::Substring);
        let ranked =
            handler.rerank_live("base", vec![command_suggestion("supabase backups", None)]);
        assert!(
            ranked.iter().any(|s| s.text == "supabase backups"),
            "contiguous substring must match full-line candidate: {ranked:?}"
        );
    }

    #[test]
    fn rerank_live_substring_drops_non_contiguous_full_line() {
        // Substring mode: "supababack" is NOT contiguous in "supabase backups"
        // (the chars are spread out). Must be dropped.
        use config::MatchMode;
        let handler = make_handler().with_match_mode(MatchMode::Substring);
        let ranked = handler.rerank_live(
            "supababack",
            vec![command_suggestion("supabase backups", None)],
        );
        assert!(
            !ranked.iter().any(|s| s.text == "supabase backups"),
            "non-contiguous query must be dropped in substring mode: {ranked:?}"
        );
    }

    #[test]
    fn rerank_live_empty_buffer_sorts_by_priority() {
        // Empty buffer sorts by source-order, priority, then text — but does
        // NOT truncate. Guards the empty-query branch both merge paths share.
        let handler = make_handler();
        let ranked = handler.rerank_live(
            "",
            vec![
                command_suggestion("zebra", None),
                command_suggestion("alpha", None),
            ],
        );
        assert_eq!(ranked.len(), 2, "empty buffer keeps every candidate");
        assert_eq!(
            ranked[0].text, "alpha",
            "equal-priority candidates fall back to alphabetical order"
        );
    }

    #[test]
    fn test_page_down_when_visible_advances_selection() {
        let mut handler = make_visible_handler(numbered_suggestions(50));
        handler.overlay.selected = Some(5);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::PageDown, &parser, &mut buf);

        assert!(result.forward_bytes().is_empty());
        assert_eq!(handler.overlay.selected, Some(15));
    }

    #[test]
    fn test_page_down_uses_effective_popup_height_on_short_terminal() {
        let mut handler = make_visible_handler(numbered_suggestions(50));
        handler.overlay.selected = Some(5);
        handler.overlay.scroll_offset = 0;
        handler.last_layout = Some(PopupLayout {
            start_row: 1,
            start_col: 0,
            width: 20,
            height: 3,
            scroll_deficit: 0,
        });
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(4, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::PageDown, &parser, &mut buf);

        assert!(result.forward_bytes().is_empty());
        assert_eq!(handler.overlay.selected, Some(8));
        assert_eq!(handler.overlay.scroll_offset, 6);
        let selected = handler.overlay.selected.unwrap();
        assert!(handler.overlay.scroll_offset <= selected);
        assert!(selected < handler.overlay.scroll_offset + 3);
    }

    #[test]
    fn test_page_up_uses_effective_popup_height_on_short_terminal() {
        let mut handler = make_visible_handler(numbered_suggestions(50));
        handler.overlay.selected = Some(8);
        handler.overlay.scroll_offset = 6;
        handler.last_layout = Some(PopupLayout {
            start_row: 1,
            start_col: 0,
            width: 20,
            height: 3,
            scroll_deficit: 0,
        });
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(4, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::PageUp, &parser, &mut buf);

        assert!(result.forward_bytes().is_empty());
        assert_eq!(handler.overlay.selected, Some(5));
        assert_eq!(handler.overlay.scroll_offset, 5);
        let selected = handler.overlay.selected.unwrap();
        assert!(handler.overlay.scroll_offset <= selected);
        assert!(selected < handler.overlay.scroll_offset + 3);
    }

    #[test]
    fn test_end_uses_effective_popup_height_with_bordered_short_terminal() {
        let mut handler = make_visible_handler(numbered_suggestions(50));
        handler.theme = PopupTheme {
            borders: true,
            ..PopupTheme::default()
        };
        handler.last_layout = Some(PopupLayout {
            start_row: 1,
            start_col: 0,
            width: 20,
            height: 5,
            scroll_deficit: 0,
        });
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(6, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::End, &parser, &mut buf);

        assert!(result.forward_bytes().is_empty());
        assert_eq!(handler.overlay.selected, Some(49));
        assert_eq!(handler.overlay.scroll_offset, 47);
        let selected = handler.overlay.selected.unwrap();
        assert!(handler.overlay.scroll_offset <= selected);
        assert!(selected < handler.overlay.scroll_offset + 3);
    }

    #[test]
    fn test_page_down_uses_configured_height_when_borderless_popup_suppressed() {
        let mut handler = make_visible_handler(numbered_suggestions(50));
        handler.overlay.selected = Some(5);
        handler.overlay.scroll_offset = 0;
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(1, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::PageDown, &parser, &mut buf);

        assert!(result.forward_bytes().is_empty());
        assert!(handler.visible);
        assert_eq!(handler.overlay.selected, Some(15));
        assert_eq!(handler.overlay.scroll_offset, 6);
    }

    #[test]
    fn test_end_uses_configured_height_when_bordered_popup_suppressed() {
        let mut handler = make_visible_handler(numbered_suggestions(50));
        handler.theme = PopupTheme {
            borders: true,
            ..PopupTheme::default()
        };
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(3, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::End, &parser, &mut buf);

        assert!(result.forward_bytes().is_empty());
        assert!(handler.visible);
        assert_eq!(handler.overlay.selected, Some(49));
        assert_eq!(handler.overlay.scroll_offset, 40);
    }

    #[test]
    fn test_page_up_when_visible_retreats_selection() {
        let mut handler = make_visible_handler(numbered_suggestions(50));
        handler.overlay.selected = Some(15);
        handler.overlay.scroll_offset = 6;
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::PageUp, &parser, &mut buf);

        assert!(result.forward_bytes().is_empty());
        assert_eq!(handler.overlay.selected, Some(5));
    }

    #[test]
    fn test_home_when_visible_jumps_to_zero() {
        let mut handler = make_visible_handler(numbered_suggestions(50));
        handler.overlay.selected = Some(20);
        handler.overlay.scroll_offset = 11;
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::Home, &parser, &mut buf);

        assert!(result.forward_bytes().is_empty());
        assert_eq!(handler.overlay.selected, Some(0));
        assert_eq!(handler.overlay.scroll_offset, 0);
    }

    #[test]
    fn test_end_when_visible_jumps_to_last() {
        let mut handler = make_visible_handler(numbered_suggestions(50));
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::End, &parser, &mut buf);

        assert!(result.forward_bytes().is_empty());
        assert_eq!(handler.overlay.selected, Some(49));
        assert_eq!(handler.overlay.scroll_offset, 40);
    }

    #[test]
    fn test_page_down_intercepted_when_visible_returns_empty_bytes() {
        let mut handler = make_visible_handler(numbered_suggestions(50));
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::PageDown, &parser, &mut buf);

        assert!(result.forward_bytes().is_empty());
    }

    #[test]
    fn test_split_page_up_sequence_is_buffered_until_visible_popup_can_intercept() {
        let mut key_parser = crate::input::KeyParser::new();
        let mut handler = make_visible_handler(numbered_suggestions(50));
        handler.overlay.selected = Some(15);
        handler.overlay.scroll_offset = 6;
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let first_keys = key_parser.parse(b"\x1B[5");
        assert!(first_keys.is_empty());
        assert!(handler.visible);

        let second_keys = key_parser.parse(b"~");
        assert_eq!(second_keys, vec![KeyEvent::PageUp]);
        let result = handler.process_key(&second_keys[0], &parser, &mut buf);

        assert!(result.forward_bytes().is_empty());
        assert!(handler.visible);
        assert_eq!(handler.overlay.selected, Some(5));
    }

    #[test]
    fn test_page_down_when_hidden_forwards_bytes() {
        let mut handler = make_handler();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::PageDown, &parser, &mut buf);

        assert_eq!(result.forward_bytes(), b"\x1B[6~");
    }

    #[test]
    fn test_page_up_when_hidden_forwards_bytes() {
        let mut handler = make_handler();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::PageUp, &parser, &mut buf);

        assert_eq!(result.forward_bytes(), b"\x1B[5~");
    }

    #[test]
    fn test_home_when_hidden_forwards_bytes() {
        let mut handler = make_handler();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::Home, &parser, &mut buf);

        assert_eq!(result.forward_bytes(), b"\x1B[H");
    }

    #[test]
    fn test_end_when_hidden_forwards_bytes() {
        let mut handler = make_handler();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::End, &parser, &mut buf);

        assert_eq!(result.forward_bytes(), b"\x1B[F");
    }

    #[test]
    fn test_hidden_home_end_alternate_encodings_forward_verbatim() {
        for raw in [
            b"\x1B[1~".as_slice(),
            b"\x1B[4~",
            b"\x1B[7~",
            b"\x1B[8~",
            b"\x1BOH",
            b"\x1BOF",
        ] {
            let events = crate::input::parse_keys(raw);
            assert_eq!(events.len(), 1, "expected one parsed event for {raw:?}");

            let mut handler = make_handler();
            let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
            let mut buf = Vec::new();

            let result = handler.process_key(&events[0], &parser, &mut buf);

            assert_eq!(
                result.forward_bytes(),
                raw,
                "hidden popup must forward {raw:?} unchanged"
            );
        }
    }

    #[test]
    fn test_visible_home_end_csi_7_8_tilde_navigate() {
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let mut home_handler = make_visible_handler(numbered_suggestions(50));
        home_handler.overlay.selected = Some(20);
        home_handler.overlay.scroll_offset = 11;
        let home_result = home_handler.process_key(&KeyEvent::HomeCsi7Tilde, &parser, &mut buf);

        assert!(home_result.forward_bytes().is_empty());
        assert!(home_handler.visible);
        assert_eq!(home_handler.overlay.selected, Some(0));
        assert_eq!(home_handler.overlay.scroll_offset, 0);

        let mut end_handler = make_visible_handler(numbered_suggestions(50));
        let end_result = end_handler.process_key(&KeyEvent::EndCsi8Tilde, &parser, &mut buf);

        assert!(end_result.forward_bytes().is_empty());
        assert!(end_handler.visible);
        assert_eq!(end_handler.overlay.selected, Some(49));
        assert_eq!(end_handler.overlay.scroll_offset, 40);
    }

    #[test]
    fn test_visible_home_variants_jump_to_zero() {
        for key in [
            KeyEvent::Home,
            KeyEvent::HomeCsiTilde,
            KeyEvent::HomeCsi7Tilde,
            KeyEvent::HomeSs3,
        ] {
            let mut handler = make_visible_handler(numbered_suggestions(50));
            handler.overlay.selected = Some(20);
            handler.overlay.scroll_offset = 11;
            let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
            let mut buf = Vec::new();

            let result = handler.process_key(&key, &parser, &mut buf);

            assert!(
                result.forward_bytes().is_empty(),
                "{key:?} should be intercepted"
            );
            assert!(handler.visible, "{key:?} should not dismiss popup");
            assert_eq!(handler.overlay.selected, Some(0), "{key:?}");
            assert_eq!(handler.overlay.scroll_offset, 0, "{key:?}");
        }
    }

    #[test]
    fn test_visible_end_variants_jump_to_last() {
        for key in [
            KeyEvent::End,
            KeyEvent::EndCsiTilde,
            KeyEvent::EndCsi8Tilde,
            KeyEvent::EndSs3,
        ] {
            let mut handler = make_visible_handler(numbered_suggestions(50));
            let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
            let mut buf = Vec::new();

            let result = handler.process_key(&key, &parser, &mut buf);

            assert!(
                result.forward_bytes().is_empty(),
                "{key:?} should be intercepted"
            );
            assert!(handler.visible, "{key:?} should not dismiss popup");
            assert_eq!(handler.overlay.selected, Some(49), "{key:?}");
            assert_eq!(handler.overlay.scroll_offset, 40, "{key:?}");
        }
    }

    #[test]
    fn test_page_navigation_does_not_dismiss_popup() {
        for key in [
            KeyEvent::PageUp,
            KeyEvent::PageDown,
            KeyEvent::Home,
            KeyEvent::HomeCsiTilde,
            KeyEvent::HomeCsi7Tilde,
            KeyEvent::HomeSs3,
            KeyEvent::End,
            KeyEvent::EndCsiTilde,
            KeyEvent::EndCsi8Tilde,
            KeyEvent::EndSs3,
        ] {
            let mut handler = make_visible_handler(numbered_suggestions(50));
            handler.overlay.selected = Some(5);
            let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
            let mut buf = Vec::new();

            handler.process_key(&key, &parser, &mut buf);

            assert!(handler.visible, "{key:?} should not dismiss popup");
        }
    }

    #[test]
    fn test_page_down_then_accept_uses_new_selection() {
        let mut handler = make_visible_handler(numbered_suggestions(50));
        handler.overlay.selected = Some(5);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let page_result = handler.process_key(&KeyEvent::PageDown, &parser, &mut buf);
        assert!(page_result.forward_bytes().is_empty());

        let accept_result = handler.process_key(&KeyEvent::Tab, &parser, &mut buf);
        let accepted = String::from_utf8_lossy(accept_result.forward_bytes());

        assert!(
            accepted.contains("item-15"),
            "expected accept to use paged selection, got {accepted:?}"
        );
    }

    #[test]
    fn test_visible_printable_keeps_popup_open_and_forwards() {
        let mut handler = make_visible_handler(numbered_suggestions(3));
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::Printable('a'), &parser, &mut buf);

        assert_eq!(result.forward_bytes(), b"a");
        assert!(
            handler.visible,
            "popup must stay open while typing so it can re-filter in place"
        );
        assert!(handler.has_pending_trigger());
    }

    #[test]
    fn test_visible_backspace_keeps_popup_open_and_forwards() {
        let mut handler = make_visible_handler(numbered_suggestions(3));
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::Backspace, &parser, &mut buf);

        assert_eq!(result.forward_bytes(), vec![0x7f]);
        assert!(
            handler.visible,
            "popup must stay open across backspace so it can re-filter in place"
        );
        assert!(handler.has_pending_trigger());
    }

    #[tokio::test]
    async fn test_trigger_keeps_visible_popup_when_sync_empty_and_async_pending() {
        // The `git ` word-boundary case: sync pass is empty (FlagPrefix context)
        // but an async provider is pending. The popup must stay open so the
        // async results can merge in place without a close/reopen gap.
        use std::future::Future;
        struct PendingProvider;
        impl AsyncProvider for PendingProvider {
            fn name(&self) -> &'static str {
                "pending"
            }
            fn suggest<'a>(
                &'a self,
                _req: &'a suggest::SuggestRequest<'a>,
            ) -> std::pin::Pin<Box<dyn Future<Output = anyhow::Result<Vec<Suggestion>>> + Send + 'a>>
            {
                Box::pin(std::future::pending())
            }
        }
        let mut handler = make_visible_handler(numbered_suggestions(3))
            .with_async_provider(Arc::new(PendingProvider));
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        // "git -" classifies as FlagPrefix → deterministic empty sync result.
        parser
            .lock()
            .unwrap()
            .state_mut()
            .predict_command_buffer("git -".to_string(), 5);
        let mut buf = Vec::new();
        handler.trigger(&parser, &mut buf);
        assert!(
            handler.visible,
            "popup must stay open while async providers are pending"
        );
    }

    #[tokio::test]
    async fn test_cache_hit_fires_llm_but_skips_backfill_provider() {
        use std::future::Future;
        use std::sync::atomic::{AtomicBool, Ordering};

        fn flag_provider(name: &'static str, flag: Arc<AtomicBool>) -> Arc<dyn AsyncProvider> {
            struct FlagProvider {
                name: &'static str,
                flag: Arc<AtomicBool>,
            }
            impl AsyncProvider for FlagProvider {
                fn name(&self) -> &'static str {
                    self.name
                }
                // Mirror the real split: fish is backfill, llm is live.
                fn is_backfill_provider(&self) -> bool {
                    self.name == "fish"
                }
                fn suggest<'a>(
                    &'a self,
                    _req: &'a suggest::SuggestRequest<'a>,
                ) -> std::pin::Pin<
                    Box<dyn Future<Output = anyhow::Result<Vec<Suggestion>>> + Send + 'a>,
                > {
                    Box::pin(async move {
                        self.flag.store(true, Ordering::SeqCst);
                        Ok(vec![])
                    })
                }
            }
            Arc::new(FlagProvider { name, flag })
        }

        let llm_polled = Arc::new(AtomicBool::new(false));
        let fish_polled = Arc::new(AtomicBool::new(false));

        // Seed a cache hit for `git ` (subcommand position).
        let cache = Arc::new(crate::shell_completion::CompletionTreeCache::new("fish"));
        cache.seed_for_test("git", vec![("add".to_string(), None)]);

        let mut handler = make_visible_handler(numbered_suggestions(3))
            .with_async_provider(flag_provider("llm", Arc::clone(&llm_polled)))
            .with_async_provider(flag_provider("fish", Arc::clone(&fish_polled)))
            .with_completion_cache(Some(cache));

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        parser
            .lock()
            .unwrap()
            .state_mut()
            .predict_command_buffer("git ".to_string(), 4);

        let mut buf = Vec::new();
        handler.trigger(&parser, &mut buf);
        // Let the spawned provider tasks run their futures (bounded poll).
        for _ in 0..50 {
            if llm_polled.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }

        assert!(
            llm_polled.load(Ordering::SeqCst),
            "LLM provider must fire on a cache hit"
        );
        assert!(
            !fish_polled.load(Ordering::SeqCst),
            "backfill (fish) provider must NOT fire on a cache hit"
        );
    }

    #[test]
    fn test_trigger_dismisses_visible_popup_when_empty_no_async_no_askai() {
        // No async providers, Ask AI disabled, sync pass empty → nothing can
        // populate the popup, so it must close.
        let mut handler = make_visible_handler(numbered_suggestions(3));
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        parser
            .lock()
            .unwrap()
            .state_mut()
            .predict_command_buffer("git -".to_string(), 5);
        let mut buf = Vec::new();
        handler.trigger(&parser, &mut buf);
        assert!(
            !handler.visible,
            "popup must close when sync is empty, no async pending, and Ask AI is off"
        );
    }

    #[test]
    fn test_trigger_keeps_visible_popup_when_empty_but_askai_active() {
        // Ask AI active → sentinel is injected into the sync result, making it
        // non-empty, so the popup stays open showing the pinned "Ask AI" row.
        let mut handler = make_visible_handler(numbered_suggestions(3))
            .with_ask_ai_provider(Some(dummy_ask_ai_provider()));
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        parser
            .lock()
            .unwrap()
            .state_mut()
            .predict_command_buffer("git -".to_string(), 5);
        let mut buf = Vec::new();
        handler.trigger(&parser, &mut buf);
        assert!(
            handler.visible,
            "popup must stay open when Ask AI sentinel is pinned"
        );
    }

    #[test]
    fn test_trigger_not_requested_on_alpha() {
        let mut handler = make_handler();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.process_key(&KeyEvent::Printable('a'), &parser, &mut buf);
        assert!(!handler.has_pending_trigger());
    }

    #[test]
    fn test_ctrl_space_triggers_immediately() {
        let kb = Keybindings {
            trigger: KeyEvent::CtrlSpace,
            ..Keybindings::default()
        };
        let mut handler = make_handler().with_keybindings(kb);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.process_key(&KeyEvent::CtrlSpace, &parser, &mut buf);
        // CtrlSpace triggers immediately — does NOT set trigger_requested
        assert!(!handler.has_pending_trigger());
    }

    #[test]
    fn test_ctrl_slash_triggers_immediately() {
        let mut handler = make_handler();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.process_key(&KeyEvent::CtrlSlash, &parser, &mut buf);
        // CtrlSlash is the default trigger — fires immediately
        assert!(!handler.has_pending_trigger());
    }

    #[test]
    fn test_handler_starts_not_visible() {
        let handler = make_handler();
        // Accessing the private field directly — the public `is_visible()`
        // accessor was removed as dead API.
        assert!(!handler.visible);
        assert!(!handler.has_pending_trigger());
    }

    #[test]
    fn test_tab_accept_directory_predicts_buffer() {
        let mut handler = make_selected_handler(Suggestion {
            text: "Desktop/".to_string(),
            kind: SuggestionKind::Directory,
            source: SuggestionSource::Filesystem,
            ..Default::default()
        });

        // Simulate buffer "cd " with cursor at 3
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        {
            let mut p = parser.lock().unwrap();
            p.state_mut().predict_command_buffer("cd ".to_string(), 3);
        }

        let mut buf = Vec::new();
        handler.process_key(&KeyEvent::Tab, &parser, &mut buf);

        // Should NOT use deferred trigger — triggers immediately
        assert!(
            !handler.has_pending_trigger(),
            "directory Tab should trigger immediately, not defer"
        );
        // Parser buffer should be updated to predicted state
        {
            let p = parser.lock().unwrap();
            assert_eq!(p.state().command_buffer(), Some("cd Desktop/"));
            assert_eq!(p.state().buffer_cursor(), 11);
        }
    }

    #[test]
    fn test_tab_accept_file_dismisses() {
        let mut handler = make_selected_handler(Suggestion {
            text: "README.md".to_string(),
            kind: SuggestionKind::FilePath,
            source: SuggestionSource::Filesystem,
            ..Default::default()
        });

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let result = handler.process_key(&KeyEvent::Tab, &parser, &mut buf);
        assert!(
            !handler.visible,
            "popup should dismiss after accepting a file"
        );
        assert!(
            result.forward_bytes().ends_with(b" "),
            "accepting a file should append trailing space, got: {result:?}"
        );
    }

    #[test]
    fn test_tab_accept_flag_ending_with_equals_no_space() {
        let mut handler = make_selected_handler(Suggestion {
            text: "--output=".to_string(),
            kind: SuggestionKind::Flag,
            source: SuggestionSource::Commands,
            ..Default::default()
        });

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let result = handler.process_key(&KeyEvent::Tab, &parser, &mut buf);
        assert!(
            !result.forward_bytes().ends_with(b" "),
            "flags ending with = should NOT get trailing space, got: {result:?}"
        );
    }

    #[test]
    fn test_enter_no_selection_forwards_enter() {
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "test".to_string(),
            ..Default::default()
        }]);

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let result = handler.process_key(&KeyEvent::Enter, &parser, &mut buf);

        assert_eq!(
            result.forward_bytes(),
            vec![0x0D],
            "should forward Enter when nothing selected"
        );
        assert!(!handler.visible, "popup should be dismissed");
    }

    #[test]
    fn test_tab_no_selection_forwards_tab() {
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "test".to_string(),
            ..Default::default()
        }]);

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let result = handler.process_key(&KeyEvent::Tab, &parser, &mut buf);

        assert_eq!(
            result.forward_bytes(),
            vec![0x09],
            "should forward Tab when nothing selected"
        );
        assert!(!handler.visible, "popup should be dismissed");
    }

    // --- tab_accepts_top (issue #150) ---

    #[test]
    fn test_tab_accepts_top_accepts_first_when_enabled() {
        let mut handler = make_visible_handler(vec![
            command_suggestion("status", None),
            command_suggestion("stash", None),
        ])
        .with_tab_accepts_top(true);
        assert_eq!(
            handler.overlay.selected, None,
            "precondition: no manual navigation"
        );

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let result = handler.process_key(&KeyEvent::Tab, &parser, &mut buf);

        assert_ne!(
            result.forward_bytes(),
            vec![0x09],
            "Tab should accept the top item, not forward a literal tab"
        );
        assert!(
            result.forward_bytes().ends_with(b"status "),
            "accepting the top command inserts it with a trailing space, got {result:?}"
        );
        assert!(!handler.visible, "popup should dismiss after accepting");
    }

    #[test]
    fn test_tab_accepts_top_disabled_still_forwards_tab() {
        // Default (flag off) preserves the historical "navigate first" flow.
        let mut handler = make_visible_handler(vec![command_suggestion("status", None)]);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let result = handler.process_key(&KeyEvent::Tab, &parser, &mut buf);

        assert_eq!(
            result.forward_bytes(),
            vec![0x09],
            "default behavior: forward literal tab when nothing navigated"
        );
        assert!(!handler.visible);
    }

    #[test]
    fn test_tab_accepts_top_does_not_hijack_enter() {
        // Enter must keep running the command line even with the flag on, so a
        // stray Enter never silently accepts a suggestion into a command the
        // user meant to run verbatim.
        let mut handler = make_visible_handler(vec![command_suggestion("status", None)])
            .with_tab_accepts_top(true);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let result = handler.process_key(&KeyEvent::Enter, &parser, &mut buf);

        assert_eq!(
            result.forward_bytes(),
            vec![0x0D],
            "Enter forwards CR even with tab_accepts_top enabled"
        );
        assert!(!handler.visible);
    }

    #[test]
    fn test_tab_accepts_top_with_no_suggestions_forwards_tab() {
        // A feedback-only popup (Loading/Empty/Error, zero suggestions) has no
        // top item to accept, so Tab still forwards a literal tab.
        let mut handler = make_visible_handler(vec![]).with_tab_accepts_top(true);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let result = handler.process_key(&KeyEvent::Tab, &parser, &mut buf);

        assert_eq!(result.forward_bytes(), vec![0x09]);
    }

    #[test]
    fn test_tab_accepts_top_respects_existing_navigation() {
        // A real selection wins over the preselect fallback: Tab accepts the
        // navigated item, not the top.
        let mut handler = make_visible_handler(vec![
            command_suggestion("status", None),
            command_suggestion("stash", None),
        ])
        .with_tab_accepts_top(true);
        handler.overlay.selected = Some(1); // navigated to "stash"

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let result = handler.process_key(&KeyEvent::Tab, &parser, &mut buf);

        assert!(
            result.forward_bytes().ends_with(b"stash "),
            "should accept the navigated item, got {result:?}"
        );
    }

    #[test]
    fn test_tab_accepts_top_accept_rebound_to_enter_accepts_top() {
        // The documented "contradictory double-opt-in": both `accept` and
        // `accept_and_enter` are bound to Enter (accept_and_enter's default),
        // while tab_accepts_top is on and the user has not navigated. Because
        // process_key_visible checks `accept` before `accept_and_enter`, Enter
        // hits the `accept` branch — whose effective_selected() resolves the
        // preselect fallback to Some(0) — and accepts the top item ("status ").
        // Reversing the two checks would instead hit `accept_and_enter`, which
        // reads the raw overlay.selected (None here) and returns a bare carriage
        // return (vec![0x0D]) that runs the line WITHOUT accepting. This test
        // therefore pins the dispatch ordering: a reorder flips the result from
        // "accept top" to "bare CR" and trips the assertions below.
        let mut handler = make_visible_handler(vec![
            command_suggestion("status", None),
            command_suggestion("stash", None),
        ])
        .with_tab_accepts_top(true);
        handler.keybindings.accept = KeyEvent::Enter;
        handler.keybindings.accept_and_enter = KeyEvent::Enter;
        assert_eq!(
            handler.overlay.selected, None,
            "precondition: no manual navigation"
        );

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let result = handler.process_key(&KeyEvent::Enter, &parser, &mut buf);

        assert!(
            result.forward_bytes().ends_with(b"status "),
            "Enter (accept checked before accept_and_enter) should accept the top item, got {result:?}"
        );
        assert_ne!(
            result.forward_bytes(),
            vec![0x0D],
            "must not forward a bare carriage return — that is the reversed-dispatch behavior"
        );
        assert!(!handler.visible, "popup should dismiss after accepting");
    }

    #[test]
    fn test_tab_accepts_top_chains_into_top_directory() {
        // Accepting a directory at the top via the preselect fallback must still
        // drive cd-chaining (predict the buffer + re-trigger) exactly as an
        // explicitly-selected directory does, even though overlay.selected was
        // never set.
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "Desktop/".to_string(),
            kind: SuggestionKind::Directory,
            source: SuggestionSource::Filesystem,
            ..Default::default()
        }])
        .with_tab_accepts_top(true);
        assert_eq!(
            handler.overlay.selected, None,
            "precondition: no navigation"
        );

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        {
            let mut p = parser.lock().unwrap();
            p.state_mut().predict_command_buffer("cd ".to_string(), 3);
        }

        let mut buf = Vec::new();
        handler.process_key(&KeyEvent::Tab, &parser, &mut buf);

        let p = parser.lock().unwrap();
        assert_eq!(
            p.state().command_buffer(),
            Some("cd Desktop/"),
            "preselected directory should chain like a navigated one"
        );
    }

    #[test]
    fn test_update_config_toggles_tab_accepts_top() {
        // The flag hot-reloads through update_config like the other popup fields.
        let mut handler = make_visible_handler(vec![command_suggestion("status", None)]);
        assert_eq!(
            handler.effective_selected(),
            None,
            "default: no preselect fallback"
        );

        handler.update_config(
            PopupTheme::default(),
            Keybindings::default(),
            DEFAULT_MAX_VISIBLE,
            1200,
            true,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            DescriptionBoxMode::Off,
            60,
            5,
            80,
            80,
            true, // tab_accepts_top
        );

        assert_eq!(
            handler.effective_selected(),
            Some(0),
            "hot-reload should enable the top-item preselect fallback"
        );
    }

    #[test]
    fn test_update_config_disables_tab_accepts_top() {
        // The on->off direction is the riskier reload: a stale `true` left
        // behind would keep silently hijacking Tab. update_config must clear
        // the field, not just set it.
        let mut handler = make_visible_handler(vec![command_suggestion("status", None)])
            .with_tab_accepts_top(true);
        assert_eq!(
            handler.effective_selected(),
            Some(0),
            "precondition: flag on yields the top-item preselect fallback"
        );

        handler.update_config(
            PopupTheme::default(),
            Keybindings::default(),
            DEFAULT_MAX_VISIBLE,
            1200,
            true,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            DescriptionBoxMode::Off,
            60,
            5,
            80,
            80,
            false, // tab_accepts_top
        );

        assert_eq!(
            handler.effective_selected(),
            None,
            "hot-reload should clear the top-item preselect fallback"
        );
    }

    // --- parse_key_name tests ---

    #[test]
    fn test_parse_key_name_all_supported() {
        assert_eq!(parse_key_name("tab").unwrap(), KeyEvent::Tab);
        assert_eq!(parse_key_name("enter").unwrap(), KeyEvent::Enter);
        assert_eq!(parse_key_name("escape").unwrap(), KeyEvent::Escape);
        assert_eq!(parse_key_name("backspace").unwrap(), KeyEvent::Backspace);
        assert_eq!(parse_key_name("ctrl+space").unwrap(), KeyEvent::CtrlSpace);
        assert_eq!(parse_key_name("ctrl+/").unwrap(), KeyEvent::CtrlSlash);
        assert_eq!(parse_key_name("arrow_up").unwrap(), KeyEvent::ArrowUp);
        assert_eq!(parse_key_name("arrow_down").unwrap(), KeyEvent::ArrowDown);
        assert_eq!(parse_key_name("arrow_left").unwrap(), KeyEvent::ArrowLeft);
        assert_eq!(parse_key_name("arrow_right").unwrap(), KeyEvent::ArrowRight);
    }

    #[test]
    fn test_parse_key_name_case_insensitive() {
        assert_eq!(parse_key_name("Tab").unwrap(), KeyEvent::Tab);
        assert_eq!(parse_key_name("TAB").unwrap(), KeyEvent::Tab);
        assert_eq!(parse_key_name("CTRL+SPACE").unwrap(), KeyEvent::CtrlSpace);
        assert_eq!(parse_key_name("CTRL+/").unwrap(), KeyEvent::CtrlSlash);
        assert_eq!(parse_key_name("Arrow_Up").unwrap(), KeyEvent::ArrowUp);
        assert_eq!(parse_key_name("ESCAPE").unwrap(), KeyEvent::Escape);
    }

    #[test]
    fn test_parse_key_name_trims_whitespace() {
        assert_eq!(parse_key_name("  tab  ").unwrap(), KeyEvent::Tab);
        assert_eq!(parse_key_name(" ctrl+space ").unwrap(), KeyEvent::CtrlSpace);
    }

    #[test]
    fn test_parse_key_name_unknown_errors() {
        assert!(parse_key_name("f1").is_err());
        assert!(parse_key_name("").is_err());
        assert!(parse_key_name("banana").is_err());
        assert!(parse_key_name("ctrl+1").is_err());
        assert!(parse_key_name("ctrl+").is_err());
    }

    #[test]
    fn test_parse_key_name_ctrl_letters() {
        assert_eq!(parse_key_name("ctrl+a").unwrap(), KeyEvent::Ctrl('a'));
        assert_eq!(parse_key_name("ctrl+e").unwrap(), KeyEvent::Ctrl('e'));
        assert_eq!(parse_key_name("ctrl+n").unwrap(), KeyEvent::Ctrl('n'));
        assert_eq!(parse_key_name("ctrl+p").unwrap(), KeyEvent::Ctrl('p'));
        assert_eq!(parse_key_name("Ctrl+X").unwrap(), KeyEvent::Ctrl('x'));
    }

    #[test]
    fn test_parse_key_name_rejects_signal_keys() {
        assert!(parse_key_name("ctrl+c").is_err());
        assert!(parse_key_name("ctrl+d").is_err());
        assert!(parse_key_name("ctrl+z").is_err());
        assert!(parse_key_name("ctrl+s").is_err());
        assert!(parse_key_name("ctrl+q").is_err());
        // Case-insensitive: uppercase input hits same deny-list
        assert!(parse_key_name("CTRL+C").is_err());
        assert!(parse_key_name("Ctrl+Z").is_err());
    }

    #[test]
    fn test_parse_key_name_rejects_aliased_keys() {
        assert!(parse_key_name("ctrl+i").is_err());
        assert!(parse_key_name("ctrl+m").is_err());
        assert!(parse_key_name("CTRL+I").is_err());
    }

    #[test]
    fn test_parse_key_name_ctrl_multi_char_error() {
        let err = parse_key_name("ctrl+ab").unwrap_err();
        assert!(
            err.to_string().contains("single letter"),
            "should mention 'single letter': {err}"
        );
        let err = parse_key_name("ctrl+1").unwrap_err();
        assert!(
            err.to_string().contains("single letter"),
            "should mention 'single letter' for digits: {err}"
        );
    }

    // --- Keybindings tests ---

    #[test]
    fn test_keybindings_from_default_config() {
        let config = config::KeybindingsConfig::default();
        let kb = Keybindings::from_config(&config).unwrap();
        assert_eq!(kb, Keybindings::default());
    }

    #[test]
    fn test_keybindings_from_custom_config() {
        let config = config::KeybindingsConfig {
            accept: "enter".to_string(),
            accept_and_enter: "tab".to_string(),
            dismiss: "backspace".to_string(),
            navigate_up: "ctrl+space".to_string(),
            navigate_down: "arrow_right".to_string(),
            trigger: "tab".to_string(),
            toggle_match_mode: "ctrl+r".to_string(),
        };
        let kb = Keybindings::from_config(&config).unwrap();
        assert_eq!(kb.accept, KeyEvent::Enter);
        assert_eq!(kb.accept_and_enter, KeyEvent::Tab);
        assert_eq!(kb.dismiss, KeyEvent::Backspace);
        assert_eq!(kb.navigate_up, KeyEvent::CtrlSpace);
        assert_eq!(kb.navigate_down, KeyEvent::ArrowRight);
        assert_eq!(kb.trigger, KeyEvent::Tab);
        assert_eq!(kb.toggle_match_mode, KeyEvent::Ctrl('r'));
    }

    #[test]
    fn toggle_match_mode_flips_engine_config() {
        let mut handler = make_visible_handler(numbered_suggestions(5));
        let parser = parser_with_buffer("l");
        let mut buf = Vec::new();
        assert_eq!(handler.engine.match_mode(), config::MatchMode::Fuzzy);

        let result = handler.process_key(&KeyEvent::Ctrl('r'), &parser, &mut buf);

        assert!(result.forward_bytes().is_empty(), "toggle key is swallowed");
        assert_eq!(
            handler.engine.match_mode(),
            config::MatchMode::Substring,
            "Ctrl+R must flip the engine to substring matching"
        );

        let result = handler.process_key(&KeyEvent::Ctrl('r'), &parser, &mut buf);
        assert!(result.forward_bytes().is_empty());
        assert_eq!(
            handler.engine.match_mode(),
            config::MatchMode::Fuzzy,
            "second toggle must flip back to fuzzy"
        );
    }

    #[test]
    fn toggle_match_mode_sets_flash() {
        let mut handler = make_visible_handler(numbered_suggestions(5));
        let parser = parser_with_buffer("l");
        let mut buf = Vec::new();

        handler.process_key(&KeyEvent::Ctrl('r'), &parser, &mut buf);

        let (label, deadline) = handler
            .mode_flash
            .as_ref()
            .expect("toggle must arm a mode flash");
        assert_eq!(label, "Substring", "flash shows the newly-active mode");
        assert!(
            *deadline > Instant::now(),
            "flash deadline must be in the future"
        );
    }

    #[test]
    fn toggle_match_mode_hidden_forwards_to_shell() {
        let mut handler = make_handler();
        assert!(!handler.visible);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        let result = handler.process_key(&KeyEvent::Ctrl('r'), &parser, &mut buf);

        assert_eq!(
            result.forward_bytes(),
            vec![0x12],
            "with the popup hidden, Ctrl+R must reach the shell (reverse-i-search)"
        );
        assert_eq!(
            handler.engine.match_mode(),
            config::MatchMode::Fuzzy,
            "hidden toggle must not change the engine mode"
        );
    }

    #[test]
    fn build_popup_hints_excludes_ask_ai() {
        let mut handler = make_visible_handler(vec![
            command_suggestion("file-a", None),
            Suggestion {
                text: "Ask AI".to_string(),
                kind: SuggestionKind::AskAi,
                ..Default::default()
            },
            command_suggestion("cmd-b", None),
        ]);
        handler.overlay.selected = Some(2);

        let hints = handler.build_popup_hints();

        assert_eq!(
            hints.index,
            Some((2, 2)),
            "Ask AI sentinel must not count toward index or total"
        );
    }

    #[test]
    fn build_popup_hints_no_selection() {
        let handler = make_visible_handler(numbered_suggestions(5));
        assert_eq!(handler.overlay.selected, None);

        let hints = handler.build_popup_hints();

        assert_eq!(
            hints.index,
            Some((1, 5)),
            "no selection reports 1-based index 1 of the full total"
        );
    }

    #[test]
    fn build_popup_hints_key_label_uses_configured_keys() {
        let handler = make_visible_handler(numbered_suggestions(3));
        let hints = handler.build_popup_hints();
        let label = hints.key_label.expect("key hints default on");
        assert!(
            label.contains("<Tab> Accept"),
            "accept binding must be formatted: {label}"
        );
        assert!(
            label.contains("<Ctrl+R> Mode"),
            "toggle binding must be formatted: {label}"
        );
    }

    #[test]
    fn build_popup_hints_disabled_by_theme() {
        let mut handler = make_visible_handler(numbered_suggestions(3));
        handler.theme.index_hints = false;
        handler.theme.key_hints = false;

        let hints = handler.build_popup_hints();

        assert!(hints.index.is_none(), "index hint disabled by theme");
        assert!(hints.key_label.is_none(), "key hint disabled by theme");
    }

    #[test]
    fn render_for_flash_expiry_repaints_with_detail_box_off() {
        let mut handler = make_visible_handler(numbered_suggestions(5));
        // detail_box_mode defaults to Off — the gate that makes
        // render_for_detail_redraw a no-op. The flash-expiry repaint must not
        // share that gate or the footer never reverts to the key hint.
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        handler.render_for_flash_expiry(&parser, &mut buf);

        assert!(
            !buf.is_empty(),
            "flash expiry must repaint even when the detail box is off"
        );
    }

    #[test]
    fn test_keybindings_from_config_invalid_key() {
        let config = config::KeybindingsConfig {
            accept: "nonexistent".to_string(),
            ..config::KeybindingsConfig::default()
        };
        assert!(Keybindings::from_config(&config).is_err());
    }

    // --- Custom keybinding behavior test ---

    #[test]
    fn test_custom_keybinding_trigger() {
        let kb = Keybindings {
            trigger: KeyEvent::Tab, // Tab triggers instead of Ctrl+Space
            ..Keybindings::default()
        };
        let mut handler = make_handler().with_keybindings(kb);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        // Tab should now act as trigger when popup is hidden
        handler.process_key(&KeyEvent::Tab, &parser, &mut buf);
        // Tab triggers immediately (like CtrlSpace normally does)
        assert!(!handler.has_pending_trigger());

        // CtrlSpace should pass through as raw bytes since it's no longer trigger
        let result = handler.process_key(&KeyEvent::CtrlSpace, &parser, &mut buf);
        assert_eq!(result.forward_bytes(), vec![0x00]);
    }

    // --- on-demand "Ask AI" ---

    /// Dummy Ask AI provider — these tests never touch the network.
    fn dummy_ask_ai_provider() -> Arc<llm::LlmProvider> {
        Arc::new(
            llm::LlmProvider::new(
                "http://127.0.0.1:9/v1".to_string(),
                "unused".to_string(),
                llm::ApiFormat::OpenAiChat,
                "m".to_string(),
                "p".to_string(),
                std::time::Duration::from_millis(100),
                3,
                256,
                None,
                llm::Thinking::Auto,
                String::new(),
                None,
            )
            .expect("reqwest client builds in tests"),
        )
    }

    #[test]
    fn ask_ai_sentinel_pinned_when_active() {
        let mut handler = make_handler().with_ask_ai_provider(Some(dummy_ask_ai_provider()));
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        parser
            .lock()
            .unwrap()
            .state_mut()
            .predict_command_buffer("git ".to_string(), 4);
        let mut buf = Vec::new();
        handler.trigger(&parser, &mut buf);
        assert!(
            handler
                .suggestions
                .first()
                .is_some_and(|s| s.kind == SuggestionKind::AskAi),
            "sentinel must be pinned at index 0, got {:?}",
            handler
                .suggestions
                .iter()
                .map(|s| &s.text)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn ask_ai_absent_when_inactive() {
        let mut handler = make_handler();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        parser
            .lock()
            .unwrap()
            .state_mut()
            .predict_command_buffer("git ".to_string(), 4);
        let mut buf = Vec::new();
        handler.trigger(&parser, &mut buf);
        assert!(
            !handler
                .suggestions
                .iter()
                .any(|s| s.kind == SuggestionKind::AskAi),
            "no AskAi sentinel when provider is None"
        );
    }

    #[test]
    fn tab_accepts_top_accepts_first_completion_not_ask_ai() {
        let mut handler = make_visible_handler(vec![
            InputHandler::ask_ai_sentinel(),
            command_suggestion("status", None),
        ])
        .with_tab_accepts_top(true)
        .with_ask_ai_provider(Some(dummy_ask_ai_provider()));
        assert_eq!(
            handler.overlay.selected, None,
            "precondition: no navigation"
        );
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let outcome = handler.process_key(&KeyEvent::Tab, &parser, &mut buf);
        assert_ne!(
            outcome,
            KeyOutcome::AskAiAccept,
            "un-navigated Tab must not trigger Ask AI when a real completion exists"
        );
        assert!(
            outcome.forward_bytes().ends_with(b"status "),
            "Tab should accept the first completion, got {outcome:?}"
        );
        assert!(!handler.visible, "popup should dismiss after accepting");
    }

    #[test]
    fn tab_accepts_top_forwards_tab_when_only_ask_ai_present() {
        // Popup holds ONLY the Ask AI sentinel (zero real completions): nothing to
        // accept, so Tab falls through to a literal tab instead of firing Ask AI.
        let mut handler = make_visible_handler(vec![InputHandler::ask_ai_sentinel()])
            .with_tab_accepts_top(true)
            .with_ask_ai_provider(Some(dummy_ask_ai_provider()));
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let outcome = handler.process_key(&KeyEvent::Tab, &parser, &mut buf);
        assert_eq!(
            outcome,
            KeyOutcome::Forward(vec![0x09]),
            "no real completion: Tab forwards a literal tab"
        );
    }

    #[test]
    fn ask_ai_accept_requires_selection_when_tab_accepts_top_off() {
        let mut handler = make_visible_handler(vec![
            InputHandler::ask_ai_sentinel(),
            command_suggestion("status", None),
        ])
        .with_ask_ai_provider(Some(dummy_ask_ai_provider()));
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let outcome = handler.process_key(&KeyEvent::Tab, &parser, &mut buf);
        assert_eq!(
            outcome,
            KeyOutcome::Forward(vec![0x09]),
            "flag off + no navigation: effective_selected() is None, so a literal tab is forwarded"
        );
    }

    #[test]
    fn navigated_accept_of_ask_ai_returns_ask_ai_accept() {
        let mut handler = make_visible_handler(vec![
            InputHandler::ask_ai_sentinel(),
            command_suggestion("status", None),
        ])
        .with_ask_ai_provider(Some(dummy_ask_ai_provider()));
        handler.overlay.selected = Some(0);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        let outcome = handler.process_key(&KeyEvent::Tab, &parser, &mut buf);
        assert_eq!(outcome, KeyOutcome::AskAiAccept);
    }

    #[test]
    fn ask_ai_forward_bytes_replaces_buffer() {
        let handler = make_handler();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        parser
            .lock()
            .unwrap()
            .state_mut()
            .predict_command_buffer("how do I list files".to_string(), 19);
        let bytes = handler.ask_ai_forward_bytes(&parser, "ls -la");
        let mut expected = vec![0x7F; 19];
        expected.extend_from_slice(b"ls -la");
        assert_eq!(bytes, expected);
    }

    #[test]
    fn pin_ask_ai_survives_rerank() {
        let handler = make_handler().with_ask_ai_provider(Some(dummy_ask_ai_provider()));
        // A pool that a fuzzy filter against a non-matching query would empty.
        let pinned = handler.pin_ask_ai(vec![command_suggestion("git status", None)]);
        assert_eq!(pinned[0].kind, SuggestionKind::AskAi);
        // An inactive handler strips the sentinel instead of pinning it.
        let inactive = make_handler();
        let stripped = inactive.pin_ask_ai(vec![
            InputHandler::ask_ai_sentinel(),
            command_suggestion("git status", None),
        ]);
        assert!(!stripped.iter().any(|s| s.kind == SuggestionKind::AskAi));
    }

    // --- update_config tests ---

    #[test]
    fn test_update_config_changes_theme() {
        let mut handler = make_handler();
        // Default theme uses \x1b[7m for selected (reverse video)
        assert_eq!(handler.theme.selected_on, b"\x1b[7m".to_vec());

        let new_theme = PopupTheme {
            selected_on: vec![0x1B, b'[', b'1', b'm'],
            description_on: vec![0x1B, b'[', b'2', b'm'],
            feedback_loading_on: vec![0x1B, b'[', b'2', b'm'],
            feedback_empty_on: vec![0x1B, b'[', b'2', b'm'],
            feedback_error_on: vec![0x1B, b'[', b'3', b'1', b'm'],
            match_highlight_on: vec![0x1B, b'[', b'4', b'm'],
            item_text_on: vec![],
            scrollbar_on: vec![0x1B, b'[', b'2', b'm'],
            border_on: vec![0x1B, b'[', b'2', b'm'],
            borders: true,
            border_radius: true,
            spinner: true,
            show_provider_errors: false,
            background_on: vec![],
            description_box_background_on: vec![],
            kind_icon_on: vec![],
            index_hints: true,
            key_hints: true,
            nerd_icons: true,
        };

        handler.update_config(
            new_theme,
            Keybindings::default(),
            15,
            1200,
            true,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            DescriptionBoxMode::Off,
            60,
            5,
            80,
            80,
            false,
        );

        assert_eq!(handler.theme.selected_on, vec![0x1B, b'[', b'1', b'm']);
        assert_eq!(handler.theme.description_on, vec![0x1B, b'[', b'2', b'm']);
    }

    #[test]
    fn test_update_config_changes_keybindings() {
        let mut handler = make_handler();

        let new_kb = Keybindings {
            accept: KeyEvent::Enter,
            accept_and_enter: KeyEvent::Tab,
            dismiss: KeyEvent::Backspace,
            navigate_up: KeyEvent::CtrlSpace,
            navigate_down: KeyEvent::ArrowRight,
            trigger: KeyEvent::Tab,
            toggle_match_mode: KeyEvent::Ctrl('r'),
        };

        handler.update_config(
            PopupTheme::default(),
            new_kb.clone(),
            10,
            1200,
            true,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            DescriptionBoxMode::Off,
            60,
            5,
            80,
            80,
            false,
        );

        assert_eq!(handler.keybindings, new_kb);
    }

    #[test]
    fn test_update_config_changes_max_visible() {
        let mut handler = make_handler();

        handler.update_config(
            PopupTheme::default(),
            Keybindings::default(),
            20,
            1200,
            true,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            DescriptionBoxMode::Off,
            60,
            5,
            80,
            80,
            false,
        );

        assert_eq!(handler.max_visible, 20);
    }

    #[test]
    fn test_update_config_changes_popup_and_detail_knobs() {
        let mut handler = make_handler();

        handler.update_config(
            PopupTheme::default(),
            Keybindings::default(),
            12,
            1200,
            true,
            35,
            120,
            DescriptionBoxMode::Side,
            90,
            7,
            0,
            80,
            false,
        );

        assert_eq!(handler.min_popup_width, 35);
        assert_eq!(handler.max_popup_width, 120);
        assert_eq!(handler.detail_box_mode, DescriptionBoxMode::Side);
        assert_eq!(handler.detail_box_max_width, 90);
        assert_eq!(handler.detail_box_lines, 7);
        assert_eq!(handler.detail_box_debounce_ms, 0);
    }

    /// Side→Side reloads (e.g. shrinking `detail_box_max_width` or
    /// `detail_box_lines`) must NOT stage a detail-box cleanup. Only the
    /// Side→Off transition tears down the existing rectangle; otherwise the
    /// next render naturally redraws the box at its new size and any old
    /// stray cells get overwritten there. This test guards that the
    /// non-cleanup branch in `update_config` stays put.
    #[test]
    fn test_update_config_side_to_side_with_new_size_does_not_stage_cleanup() {
        let mut handler = make_selected_handler(command_suggestion(
            "checkout",
            Some("long description already visible in the detail box"),
        ))
        .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        handler.last_detail_layout = Some(DetailLayout {
            start_row: 5,
            start_col: 24,
            width: 30,
            height: 3,
            position: overlay::DetailPosition::SideRight,
        });

        let cleanup = handler.update_config(
            PopupTheme::default(),
            Keybindings::default(),
            10,
            1200,
            true,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            DescriptionBoxMode::Side,
            30,
            3,
            0,
            80,
            false,
        );

        assert!(
            cleanup.is_empty(),
            "Side→Side reload must not emit any cleanup bytes: {:?}",
            String::from_utf8_lossy(&cleanup)
        );
        assert!(
            handler.last_detail_layout.is_some(),
            "Side→Side must keep the committed detail layout — no clear is needed",
        );
        let ticket = handler.overlay_write_ticket();
        assert!(
            ticket.cleanup_token.is_none(),
            "Side→Side must not stage an overlay cleanup token",
        );

        // Field updates still landed.
        assert_eq!(handler.detail_box_mode, DescriptionBoxMode::Side);
        assert_eq!(handler.detail_box_max_width, 30);
        assert_eq!(handler.detail_box_lines, 3);
        assert_eq!(handler.detail_box_debounce_ms, 0);
    }

    #[test]
    fn test_update_config_clears_visible_detail_box_when_disabled() {
        let mut handler = make_selected_handler(command_suggestion(
            "checkout",
            Some("long description already visible in the detail box"),
        ))
        .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        handler.last_detail_layout = Some(DetailLayout {
            start_row: 5,
            start_col: 24,
            width: 30,
            height: 3,
            position: overlay::DetailPosition::SideRight,
        });
        handler.detail_debounce.displayed_idx = Some(0);
        handler.detail_debounce.last_change_at = Some(Instant::now());
        handler.detail_debounce.pending = true;
        let before_epoch = handler.output_epoch();

        let cleanup = handler.update_config(
            PopupTheme::default(),
            Keybindings::default(),
            10,
            1200,
            true,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            DescriptionBoxMode::Off,
            60,
            5,
            80,
            80,
            false,
        );

        let output = String::from_utf8_lossy(&cleanup);
        assert!(
            output.contains("\x1b[6;25H"),
            "disabling the detail box must clear its old rectangle: {output:?}"
        );
        handler.commit_overlay_write(handler.overlay_write_ticket());
        assert!(handler.last_detail_layout.is_none());
        assert_eq!(handler.detail_debounce.displayed_idx, None);
        assert_eq!(handler.detail_debounce.last_change_at, None);
        assert!(!handler.detail_debounce.pending);
        assert!(handler.output_epoch() > before_epoch);
    }

    #[test]
    fn test_update_config_stages_detail_cleanup_until_overlay_write_ack() {
        let mut handler = make_selected_handler(command_suggestion(
            "checkout",
            Some("long description already visible in the detail box"),
        ))
        .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        handler.last_detail_layout = Some(DetailLayout {
            start_row: 5,
            start_col: 24,
            width: 30,
            height: 3,
            position: overlay::DetailPosition::SideRight,
        });
        handler.detail_debounce.displayed_idx = Some(0);
        handler.detail_debounce.last_change_at = Some(Instant::now());
        handler.detail_debounce.pending = true;

        let cleanup = handler.update_config(
            PopupTheme::default(),
            Keybindings::default(),
            10,
            1200,
            true,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            DescriptionBoxMode::Off,
            60,
            5,
            80,
            80,
            false,
        );

        let output = String::from_utf8_lossy(&cleanup);
        assert!(
            output.contains("\x1b[6;25H"),
            "disabling the detail box must stage a clear for its old rectangle: {output:?}"
        );
        assert!(
            handler.last_detail_layout.is_some(),
            "detail layout ownership must remain committed until cleanup bytes are written"
        );
        assert_eq!(handler.detail_debounce.displayed_idx, None);
        assert_eq!(handler.detail_debounce.last_change_at, None);
        assert!(!handler.detail_debounce.pending);

        let ticket = handler.overlay_write_ticket();
        assert!(
            ticket.cleanup_token.is_some(),
            "config cleanup must be acknowledged through the overlay write token"
        );
        handler.commit_overlay_write(ticket);
        assert!(
            handler.last_detail_layout.is_none(),
            "acknowledged cleanup should release the committed detail layout"
        );
        assert!(
            handler.last_layout.is_some(),
            "detail-only cleanup must not release the still-visible main popup layout"
        );

        let mut dismiss = Vec::new();
        handler.dismiss(&mut dismiss);
        let dismiss_output = String::from_utf8_lossy(&dismiss);
        assert!(
            dismiss_output.contains("\x1b[6;1H"),
            "main popup must remain clearable after detail-only cleanup: {dismiss_output:?}"
        );
    }

    // --- auto_trigger tests ---

    #[test]
    fn test_auto_trigger_false_suppresses_trigger_on_space() {
        let mut handler = make_handler().with_auto_trigger(false);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.process_key(&KeyEvent::Printable(' '), &parser, &mut buf);
        assert!(!handler.has_pending_trigger());
    }

    #[test]
    fn test_auto_trigger_false_allows_manual_trigger() {
        let mut handler = make_handler().with_auto_trigger(false);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.process_key(&KeyEvent::CtrlSlash, &parser, &mut buf);
        // Manual trigger fires immediately — not gated by auto_trigger
        assert!(!handler.has_pending_trigger());
    }

    #[test]
    fn test_ctrl_letter_trigger_preserves_buffer() {
        // Regression: a trigger bound to a control letter (ctrl+o) must not
        // wipe the typed buffer. Before the fix, process_key_hidden applied the
        // key to the keystroke model FIRST; BufferModel::apply_key classifies
        // unlisted Ctrl(_) as a buffer reset, so the command line was cleared
        // before trigger() read it — no popup for non-zsh shells (fish/bash).
        // CtrlSlash (the default) is special-cased to not reset, which is why
        // the default binding never hit this. The trigger check now runs before
        // the model apply, so the buffer survives.
        let kb = Keybindings {
            trigger: KeyEvent::Ctrl('o'),
            ..Keybindings::default()
        };
        let mut handler = make_handler()
            .with_shell_kind(ShellKind::Fish)
            .with_keybindings(kb);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        // Type "git" — the keystroke model tracks it for non-zsh shells.
        for c in ['g', 'i', 't'] {
            handler.process_key(&KeyEvent::Printable(c), &parser, &mut buf);
        }
        assert_eq!(
            handler.input_model.buffer, "git",
            "setup: model tracks typing"
        );

        // Press the ctrl+o trigger.
        let mut trigger_buf = Vec::new();
        handler.process_key(&KeyEvent::Ctrl('o'), &parser, &mut trigger_buf);

        // The typed buffer must survive the trigger keypress.
        assert_eq!(
            handler.input_model.buffer, "git",
            "ctrl+letter trigger must not reset the keystroke buffer"
        );
        assert_eq!(
            parser.lock().unwrap().state().command_buffer(),
            Some("git"),
            "parser command_buffer must retain the typed text"
        );
        // The trigger must be recognized as a trigger, not forwarded to the
        // shell as a literal byte.
        assert!(
            trigger_buf.is_empty(),
            "trigger key must be consumed, got {:?}",
            String::from_utf8_lossy(&trigger_buf)
        );
    }

    #[test]
    fn test_backspace_to_empty_propagates_empty_buffer_to_parser() {
        // Regression: backspacing the command line down to nothing must push
        // the empty buffer to the parser so the trigger it raises dismisses
        // the popup. Previously every empty model was dropped (to avoid
        // clobbering the buffer on drift resets), so a popup survived an
        // emptied prompt.
        let mut handler = make_handler().with_shell_kind(ShellKind::Fish);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        for c in ['g', 'i', 't'] {
            handler.process_key(&KeyEvent::Printable(c), &parser, &mut buf);
        }
        assert_eq!(parser.lock().unwrap().state().command_buffer(), Some("git"));

        for _ in 0..3 {
            handler.process_key(&KeyEvent::Backspace, &parser, &mut buf);
        }
        assert_eq!(
            parser.lock().unwrap().state().command_buffer(),
            Some(""),
            "genuine deletion to empty must propagate so the popup can dismiss"
        );
    }

    #[test]
    fn test_drift_reset_to_empty_does_not_propagate() {
        // Counterpart guard: a non-modeled reset (Escape = drift) clears the
        // model but must NOT push an empty buffer to the parser — that would
        // wipe the last known command line and break manual Ctrl+/ re-trigger.
        let mut handler = make_handler().with_shell_kind(ShellKind::Fish);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();

        for c in ['g', 'i', 't'] {
            handler.process_key(&KeyEvent::Printable(c), &parser, &mut buf);
        }
        handler.process_key(&KeyEvent::Escape, &parser, &mut buf);

        assert!(
            handler.input_model.buffer.is_empty(),
            "model reset on Escape"
        );
        assert_eq!(
            parser.lock().unwrap().state().command_buffer(),
            Some("git"),
            "drift reset must not clobber the parser's last known buffer"
        );
    }

    #[test]
    fn test_update_config_sets_auto_trigger_false() {
        let mut handler = make_handler();
        assert!(handler.auto_trigger_enabled());

        handler.update_config(
            PopupTheme::default(),
            Keybindings::default(),
            10,
            1200,
            false,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            DescriptionBoxMode::Off,
            60,
            5,
            80,
            80,
            false,
        );

        assert!(!handler.auto_trigger_enabled());
    }

    #[test]
    fn test_update_config_dismisses_popup_on_auto_trigger_disable() {
        let suggestion = Suggestion {
            text: "test".into(),
            ..Default::default()
        };
        let mut handler = make_visible_handler(vec![suggestion]);
        assert!(handler.visible);
        assert!(handler.auto_trigger_enabled());

        let cleanup = handler.update_config(
            PopupTheme::default(),
            Keybindings::default(),
            10,
            1200,
            false,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            DescriptionBoxMode::Off,
            60,
            5,
            80,
            80,
            false,
        );

        assert!(!handler.visible);
        assert!(!handler.auto_trigger_enabled());
        assert!(!cleanup.is_empty(), "should emit popup clear sequences");
        assert!(handler.dynamic_rx.is_none(), "dynamic_rx must be cleared");
        assert!(handler.dynamic_ctx.is_none(), "dynamic_ctx must be cleared");
        assert!(
            handler.dynamic_task.is_none(),
            "dynamic_task must be cleared"
        );
    }

    #[test]
    fn test_update_config_clears_pending_trigger_even_when_not_visible() {
        let mut handler = make_handler();
        // Simulate a pending trigger (debounce timer fired, trigger() hasn't
        // run yet) while the popup is NOT visible.
        handler.trigger_requested = true;
        assert!(!handler.visible);

        let cleanup = handler.update_config(
            PopupTheme::default(),
            Keybindings::default(),
            10,
            1200,
            false,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            DescriptionBoxMode::Off,
            60,
            5,
            80,
            80,
            false,
        );

        assert!(
            !handler.has_pending_trigger(),
            "pending trigger must be cancelled"
        );
        assert!(handler.dynamic_task.is_none());
        assert!(handler.dynamic_rx.is_none());
        assert!(handler.dynamic_ctx.is_none());
        // No popup was visible, so no visual cleanup needed.
        assert!(cleanup.is_empty());
    }

    #[test]
    fn test_update_config_keeps_popup_when_auto_trigger_stays_true() {
        let suggestion = Suggestion {
            text: "test".into(),
            ..Default::default()
        };
        let mut handler = make_visible_handler(vec![suggestion]);
        assert!(handler.visible);

        let cleanup = handler.update_config(
            PopupTheme::default(),
            Keybindings::default(),
            10,
            1200,
            true,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            DescriptionBoxMode::Off,
            60,
            5,
            80,
            80,
            false,
        );

        assert!(handler.visible);
        assert!(cleanup.is_empty(), "no cleanup when auto_trigger unchanged");
    }

    // --- Debounce suppression tests ---

    #[test]
    fn test_arrow_up_suppresses_debounce() {
        let mut handler = make_handler();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        assert!(!handler.is_debounce_suppressed());
        handler.process_key(&KeyEvent::ArrowUp, &parser, &mut buf);
        assert!(handler.is_debounce_suppressed());
    }

    #[test]
    fn test_arrow_down_suppresses_debounce() {
        let mut handler = make_handler();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.process_key(&KeyEvent::ArrowDown, &parser, &mut buf);
        assert!(handler.is_debounce_suppressed());
    }

    #[test]
    fn test_printable_clears_debounce_suppression() {
        let mut handler = make_handler();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        // Suppress via arrow
        handler.process_key(&KeyEvent::ArrowUp, &parser, &mut buf);
        assert!(handler.is_debounce_suppressed());
        // Clear via typing
        handler.process_key(&KeyEvent::Printable('a'), &parser, &mut buf);
        assert!(!handler.is_debounce_suppressed());
    }

    #[test]
    fn test_terminal_output_dismisses_owned_popup_before_shell_bytes() {
        let mut handler = make_visible_handler(numbered_suggestions(3));
        let mut buf = Vec::new();

        handler.handle_terminal_output(&mut buf, true, 0);

        assert!(!handler.visible);
        assert!(handler.last_layout.is_none());
        let output = String::from_utf8_lossy(&buf);
        assert!(
            output.contains("\x1b[6;1H"),
            "terminal output cleanup should clear the owned popup: {output:?}"
        );
    }

    #[test]
    fn test_terminal_output_preserves_popup_within_grace_period() {
        // Within the repaint grace period, display-changing output is the
        // shell redrawing the line just typed (fish repaints on every
        // keystroke). The popup must stay up — tearing it down here is
        // what caused the open/close flicker.
        let mut handler = make_visible_handler(numbered_suggestions(3));
        handler.last_repaint_at = Some(Instant::now());
        let mut buf = Vec::new();
        let before_epoch = handler.output_epoch();

        handler.handle_terminal_output(&mut buf, true, 0);

        assert!(handler.visible, "grace period must keep the popup up");
        assert!(handler.last_layout.is_some());
        assert!(
            buf.is_empty(),
            "no teardown cleanup within the repaint grace period"
        );
        assert!(handler.output_epoch() > before_epoch);
    }

    #[test]
    fn test_terminal_output_tears_down_after_grace_expires() {
        // Once the grace period expires, display-dirty output is genuine
        // shell output and must tear down the popup.
        let mut handler = make_visible_handler(numbered_suggestions(3));
        handler.last_repaint_at = Some(Instant::now() - Duration::from_millis(200));
        let mut buf = Vec::new();

        handler.handle_terminal_output(&mut buf, true, 0);

        assert!(!handler.visible, "expired grace must tear down the popup");
        assert!(handler.last_layout.is_none());
        assert!(!buf.is_empty(), "teardown must emit cleanup bytes");
    }

    #[test]
    fn test_display_dirty_terminal_output_bumps_epoch_without_owned_popup() {
        let mut handler = make_handler();
        let before_epoch = handler.output_epoch();
        let mut buf = Vec::new();

        handler.handle_terminal_output(&mut buf, true, 0);

        assert!(buf.is_empty());
        assert!(
            handler.output_epoch() > before_epoch,
            "display-changing PTY output must invalidate pending overlay buffers"
        );
    }

    #[test]
    fn test_terminal_output_clears_feedback_only_layout_when_not_visible() {
        let mut handler = make_handler();
        handler.feedback = AsyncFeedback::Loading {
            spawned_at: std::time::Instant::now(),
        };
        handler.last_layout = Some(PopupLayout {
            start_row: 5,
            start_col: 0,
            width: 20,
            height: 1,
            scroll_deficit: 0,
        });
        let mut buf = Vec::new();

        handler.handle_terminal_output(&mut buf, true, 0);

        assert!(!buf.is_empty());
        assert!(handler.last_layout.is_none());
        assert!(matches!(handler.feedback, AsyncFeedback::Idle));
    }

    #[test]
    fn test_terminal_scroll_resets_overlay_scroll_deficit_when_hidden() {
        // Shell-side viewport scrolls move the parser's cursor independently
        // of the overlay's bookkeeping. Once a shell scroll lands, the
        // cached deficit no longer corresponds to any real cursor offset
        // (parser saturates at the bottom while the real cursor advances),
        // and carrying it forward causes the next popup to render above the
        // actual cursor row — the bug captured in the
        // `codex/fix-terminal-rendering-corruption` log evidence
        // (`row=79 col=1 screen_rows=35`). Resetting eagerly is the
        // recovery path; a real CPR sync at the next prompt boundary
        // would do the same thing one frame later.
        let mut handler = make_handler();
        handler.overlay_scroll_deficit = 3;
        let mut buf = Vec::new();

        handler.handle_terminal_output(&mut buf, false, 2);

        assert!(buf.is_empty());
        assert_eq!(handler.overlay_scroll_deficit, 0);
    }

    #[test]
    fn test_invalidate_overlay_scroll_deficit_clears_cached_value() {
        let mut handler = make_handler();
        handler.overlay_scroll_deficit = 7;
        handler.invalidate_overlay_scroll_deficit();
        assert_eq!(handler.overlay_scroll_deficit, 0);
    }

    #[test]
    fn test_hidden_terminal_scroll_does_not_create_overlay_scroll_deficit() {
        let mut handler = make_handler();
        let mut buf = Vec::new();

        handler.handle_terminal_output(&mut buf, false, 2);

        assert!(buf.is_empty());
        assert_eq!(handler.overlay_scroll_deficit, 0);
    }

    #[test]
    fn test_render_at_skips_old_clear_when_new_render_scrolls() {
        let mut handler = make_visible_handler(numbered_suggestions(8));
        handler.overlay_scroll_deficit = 4;
        handler.last_layout = Some(PopupLayout {
            start_row: 19,
            start_col: 70,
            width: 10,
            height: 2,
            scroll_deficit: 4,
        });
        let mut buf = Vec::new();

        handler.render_at(&mut buf, 22, 0, 24, 80);

        let output = String::from_utf8_lossy(&buf);
        assert!(
            !output.contains("\x1b[20;71H"),
            "stale clear at old popup coordinates must be skipped before viewport scroll: {output:?}"
        );
        assert!(
            output.contains("\x1b[24;1H"),
            "new render should still scroll from the bottom row: {output:?}"
        );
    }

    #[test]
    fn test_render_at_skips_old_detail_clear_when_new_render_scrolls() {
        let mut handler = make_visible_handler(numbered_suggestions(8));
        handler.overlay_scroll_deficit = 4;
        handler.last_layout = Some(PopupLayout {
            start_row: 19,
            start_col: 70,
            width: 10,
            height: 2,
            scroll_deficit: 4,
        });
        handler.last_detail_layout = Some(DetailLayout {
            start_row: 19,
            start_col: 30,
            width: 10,
            height: 2,
            position: overlay::DetailPosition::SideLeft,
        });
        let mut buf = Vec::new();

        handler.render_at(&mut buf, 22, 0, 24, 80);

        let output = String::from_utf8_lossy(&buf);
        assert!(
            !output.contains("\x1b[20;31H"),
            "stale clear at old detail coordinates must be skipped before viewport scroll: {output:?}"
        );
        assert!(
            output.contains("\x1b[24;1H"),
            "new render should still scroll from the bottom row: {output:?}"
        );
    }

    #[test]
    fn test_render_at_clears_scrolled_old_detail_when_new_selection_has_no_detail() {
        let mut suggestions = numbered_suggestions(8);
        suggestions[0] = command_suggestion("checkout", Some("short"));
        let mut handler = make_visible_handler(suggestions)
            .with_popup_widths(20, 80)
            .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        handler.overlay.selected = Some(0);
        handler.overlay_scroll_deficit = 4;
        handler.last_layout = Some(PopupLayout {
            start_row: 19,
            start_col: 70,
            width: 10,
            height: 2,
            scroll_deficit: 4,
        });
        let old_detail = DetailLayout {
            start_row: 19,
            start_col: 30,
            width: 10,
            height: 2,
            position: overlay::DetailPosition::SideLeft,
        };
        handler.last_detail_layout = Some(old_detail.clone());
        let additional_scroll = popup_additional_scroll_deficit(
            &handler.suggestions,
            22,
            24,
            80,
            handler.max_visible,
            handler.min_popup_width,
            &handler.theme,
            handler.overlay_scroll_deficit,
            &handler.current_feedback_kind(),
            &handler.build_popup_hints(),
        );
        assert!(
            additional_scroll > 0,
            "setup must force a scrolling repaint"
        );
        let shifted_row = old_detail.start_row - additional_scroll;
        let mut buf = Vec::new();

        handler.render_at(&mut buf, 22, 0, 24, 80);

        let output = String::from_utf8_lossy(&buf);
        assert!(
            !output.contains("\x1b[20;31H"),
            "stale clear at pre-scroll detail coordinates must still be skipped: {output:?}"
        );
        let shifted_clear = format!("\x1b[{};{}H", shifted_row + 1, old_detail.start_col + 1);
        assert!(
            output.contains(&shifted_clear),
            "old detail rectangle must be cleared at its post-scroll coordinates: {output:?}"
        );
        handler.commit_overlay_write(handler.overlay_write_ticket());
        assert!(handler.last_detail_layout.is_none());
    }

    /// Cover guard: when `render_at` triggers a viewport scroll AND the
    /// new selection produces a fresh detail layout, the
    /// `clear_detail_box_uncovered_by` pass that erases the *scrolled* old
    /// detail rectangle must spare the freshly-painted new detail. A
    /// regression that drops the conditional `covers.push(new_detail)` would
    /// emit space-fills inside the new detail's columns and visibly clobber
    /// the just-painted box during the same render frame.
    #[test]
    fn test_render_at_scroll_clears_old_detail_but_preserves_new_detail() {
        // Eight suggestions, the first two carrying long descriptions that
        // both overflow the 40-col popup. Eight items beats the available
        // 5 rows below the prior cursor, so the render forces a viewport
        // scroll — the precondition for the cover branch under test.
        let long_desc_a = "MARKER_ALPHA alpha beta gamma delta epsilon zeta eta theta iota kappa \
                           lambda mu nu xi omicron pi rho sigma tau";
        let long_desc_b = "MARKER_BRAVO alpha beta gamma delta epsilon zeta eta theta iota kappa \
                           lambda mu nu xi omicron pi rho sigma tau";
        let mut suggestions = numbered_suggestions(8);
        suggestions[0] = command_suggestion("checkout", Some(long_desc_a));
        suggestions[1] = command_suggestion("commit", Some(long_desc_b));
        let mut handler = make_visible_handler(suggestions)
            .with_popup_widths(20, 40)
            .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        handler.overlay.selected = Some(0);
        handler.overlay_scroll_deficit = 4;
        // Place the prior popup + detail near the bottom so the new render
        // forces a scroll. The old detail spans a wide column range so the
        // shifted clear will straddle the new detail's columns.
        handler.last_layout = Some(PopupLayout {
            start_row: 22,
            start_col: 70,
            width: 40,
            height: 2,
            scroll_deficit: 4,
        });
        let old_detail = DetailLayout {
            start_row: 22,
            start_col: 10,
            width: 60,
            height: 2,
            position: overlay::DetailPosition::SideLeft,
        };
        handler.last_detail_layout = Some(old_detail.clone());

        let additional_scroll = popup_additional_scroll_deficit(
            &handler.suggestions,
            22,
            24,
            120,
            handler.max_visible,
            handler.min_popup_width,
            &handler.theme,
            handler.overlay_scroll_deficit,
            &handler.current_feedback_kind(),
            &handler.build_popup_hints(),
        );
        assert!(
            additional_scroll > 0,
            "setup must force a scrolling repaint so the cover branch runs"
        );
        let shifted_row = old_detail.start_row - additional_scroll;
        let mut buf = Vec::new();

        handler.render_at(&mut buf, 22, 0, 24, 120);

        let output = String::from_utf8_lossy(&buf);
        // (1) Pre-scroll old detail coordinates must not be cleared.
        assert!(
            !output.contains("\x1b[23;11H"),
            "pre-scroll detail coordinates must remain untouched: {output:?}"
        );
        // (2) The new detail content (marker word) must be emitted.
        assert!(
            output.contains("MARKER_ALPHA"),
            "new selection's detail must render before the clear-pass spares it: {output:?}"
        );

        // Commit the staged write so `last_detail_layout` reflects the
        // freshly-painted new detail rectangle.
        handler.commit_overlay_write(handler.overlay_write_ticket());
        let new_detail = handler
            .last_detail_layout
            .as_ref()
            .expect("new selection's detail layout must be retained")
            .clone();

        // The clear pass for the scrolled old detail is the LAST
        // save/restore-cursor block in the buffer (DECSAVE `\x1b7` …
        // DECRESTORE `\x1b8`). Slice it out so the assertions below see
        // only the clear-pass cursor moves, not the new detail's PAINT
        // moves which target the same row.
        let clear_block_start = output
            .rfind("\u{1b}7")
            .expect("buffer must contain a save_cursor at the start of the clear pass");
        let clear_block_end = output[clear_block_start..]
            .find("\u{1b}8")
            .map(|rel| clear_block_start + rel)
            .expect("clear pass must end with a restore_cursor");
        let clear_block = &output[clear_block_start..clear_block_end];

        // (3) The shifted old-detail row IS cleared in at least one column
        // outside both the new popup and the new detail. Locate cursor
        // moves on the shifted row inside the clear block.
        let new_detail_end_col = new_detail.start_col.saturating_add(new_detail.width);
        let shifted_prefix = format!("\x1b[{};", shifted_row + 1);
        let mut shifted_clears: Vec<u16> = Vec::new();
        let mut search_from = 0usize;
        while let Some(idx) = clear_block[search_from..].find(&shifted_prefix) {
            let abs_idx = search_from + idx;
            let col_start = abs_idx + shifted_prefix.len();
            let col_end_off = clear_block[col_start..].find('H');
            if let Some(rel_end) = col_end_off {
                if let Ok(col_one_idx) = clear_block[col_start..col_start + rel_end].parse::<u16>()
                {
                    if col_one_idx > 0 {
                        shifted_clears.push(col_one_idx - 1);
                    }
                }
                search_from = col_start + rel_end + 1;
            } else {
                break;
            }
        }
        assert!(
            !shifted_clears.is_empty(),
            "expected at least one cursor-move on shifted row {shifted_row} (1-indexed \
             {}) inside the clear pass: {clear_block:?}",
            shifted_row + 1
        );
        assert!(
            shifted_clears
                .iter()
                .any(|&c| c < new_detail.start_col || c >= new_detail_end_col),
            "shifted old-detail row must be cleared in at least one column outside \
             the new detail's range [{}, {}); cursor moves seen at cols (0-indexed) \
             {:?}: {clear_block:?}",
            new_detail.start_col,
            new_detail_end_col,
            shifted_clears
        );

        // (4) The clear pass must not emit any cursor move INSIDE the new
        // detail's column range on the shifted old-detail row — that is the
        // exact regression the cover branch protects against.
        for &col in &shifted_clears {
            assert!(
                col < new_detail.start_col || col >= new_detail_end_col,
                "clear pass must not move cursor inside the new detail column range \
                 [{}, {}); found cursor move at 0-indexed col {col} on shifted row \
                 {shifted_row}: {clear_block:?}",
                new_detail.start_col,
                new_detail_end_col
            );
        }
    }

    #[test]
    fn test_render_at_renders_side_description_box_for_long_description() {
        let description = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let mut handler = make_selected_handler(command_suggestion("checkout", Some(description)))
            .with_popup_widths(20, 40)
            .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        let mut buf = Vec::new();

        handler.render_at(&mut buf, 5, 0, 24, 120);

        let output = String::from_utf8_lossy(&buf);
        assert!(
            output.contains("lambda mu"),
            "long description should be emitted in the detail box: {output:?}"
        );
        handler.commit_overlay_write(handler.overlay_write_ticket());
        let layout = handler
            .last_detail_layout
            .as_ref()
            .expect("detail layout should be retained");
        assert_eq!(layout.position, overlay::DetailPosition::SideRight);
    }

    #[test]
    fn test_render_at_stages_detail_layout_until_overlay_write_ack() {
        let description = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let mut handler = make_selected_handler(command_suggestion("checkout", Some(description)))
            .with_popup_widths(20, 40)
            .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        let mut buf = Vec::new();

        handler.render_at(&mut buf, 5, 0, 24, 120);

        assert!(!buf.is_empty());
        assert!(
            handler.last_detail_layout.is_none(),
            "staged render must not commit detail layout before stdout write succeeds"
        );

        handler.commit_overlay_write(handler.overlay_write_ticket());

        let layout = handler
            .last_detail_layout
            .as_ref()
            .expect("acknowledged render should retain detail layout");
        assert_eq!(layout.position, overlay::DetailPosition::SideRight);
    }

    #[tokio::test]
    async fn test_detail_box_selection_debounce_holds_then_catches_up() {
        let desc0 =
            "ALPHADETAIL alpha beta gamma delta epsilon zeta eta theta iota kappa ALPHATAIL";
        let desc1 =
            "BRAVODETAIL alpha beta gamma delta epsilon zeta eta theta iota kappa BRAVOTAIL";
        let mut handler = make_visible_handler(vec![
            command_suggestion("alpha", Some(desc0)),
            command_suggestion("bravo", Some(desc1)),
        ])
        .with_popup_widths(20, 40)
        .with_description_box(DescriptionBoxMode::Side, 60, 5, 200);
        handler.overlay.selected = Some(0);

        let mut first = Vec::new();
        handler.render_at(&mut first, 5, 0, 24, 120);
        handler.commit_overlay_write(handler.overlay_write_ticket());
        let first_output = String::from_utf8_lossy(&first);
        assert!(
            first_output.contains("ALPHADETAIL"),
            "setup should render suggestion 0 detail: {first_output:?}"
        );
        assert_eq!(handler.detail_debounce.displayed_idx, Some(0));

        handler.overlay.selected = Some(1);
        let mut immediate = Vec::new();
        handler.render_at(&mut immediate, 5, 0, 24, 120);
        let immediate_output = String::from_utf8_lossy(&immediate);
        assert!(
            immediate_output.contains("ALPHADETAIL"),
            "in-window render should keep the old detail: {immediate_output:?}"
        );
        assert!(
            !immediate_output.contains("BRAVOTAIL"),
            "in-window render must not emit the new detail yet: {immediate_output:?}"
        );
        assert!(
            handler.detail_debounce.pending,
            "in-window render should schedule a detail redraw wakeup"
        );
        handler.commit_overlay_write(handler.overlay_write_ticket());

        handler.detail_debounce.last_change_at =
            Some(Instant::now() - std::time::Duration::from_millis(250));
        handler.clear_detail_debounce_pending();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 120)));
        {
            let mut p = parser.lock().unwrap();
            p.process_bytes(b"\x1b[6;1H");
        }
        let mut redraw = Vec::new();
        handler.render_for_detail_redraw(&parser, &mut redraw);
        handler.commit_overlay_write(handler.overlay_write_ticket());

        let redraw_output = String::from_utf8_lossy(&redraw);
        assert!(
            redraw_output.contains("BRAVOTAIL"),
            "detail redraw should emit the settled selection detail: {redraw_output:?}"
        );
        assert_eq!(handler.detail_debounce.displayed_idx, Some(1));
    }

    #[tokio::test]
    async fn test_detail_box_debounce_wakeup_notifies_and_redraws_selection() {
        let desc0 = "ALPHAWAKE alpha beta gamma delta epsilon zeta eta theta iota kappa ALPHADONE";
        let desc1 = "BRAVOWAKE alpha beta gamma delta epsilon zeta eta theta iota kappa BRAVODONE";
        let mut handler = make_visible_handler(vec![
            command_suggestion("alpha", Some(desc0)),
            command_suggestion("bravo", Some(desc1)),
        ])
        .with_popup_widths(20, 40)
        .with_description_box(DescriptionBoxMode::Side, 60, 5, 5);
        handler.overlay.selected = Some(0);

        let mut first = Vec::new();
        handler.render_at(&mut first, 5, 0, 24, 120);
        handler.commit_overlay_write(handler.overlay_write_ticket());
        assert_eq!(handler.detail_debounce.displayed_idx, Some(0));

        let notify = handler.detail_redraw_notify();
        let notified = notify.notified();
        handler.overlay.selected = Some(1);
        let mut immediate = Vec::new();
        handler.render_at(&mut immediate, 5, 0, 24, 120);
        handler.commit_overlay_write(handler.overlay_write_ticket());
        assert!(
            handler.detail_debounce.pending,
            "selection-change render should arm the debounce wakeup"
        );

        tokio::time::timeout(std::time::Duration::from_millis(200), notified)
            .await
            .expect("debounce wakeup should notify without a manual state age");

        handler.clear_detail_debounce_pending();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 120)));
        {
            let mut p = parser.lock().unwrap();
            p.process_bytes(b"\x1b[6;1H");
        }
        let mut redraw = Vec::new();
        handler.render_for_detail_redraw(&parser, &mut redraw);
        handler.commit_overlay_write(handler.overlay_write_ticket());

        let redraw_output = String::from_utf8_lossy(&redraw);
        assert!(
            redraw_output.contains("BRAVODONE"),
            "debounce wakeup redraw should emit the settled selection detail: {redraw_output:?}"
        );
        assert_eq!(handler.detail_debounce.displayed_idx, Some(1));
    }

    #[test]
    fn test_detail_box_debounce_zero_repaints_selection_immediately() {
        let desc0 = "ALPHAZERO alpha beta gamma delta epsilon zeta eta theta iota kappa ALPHADONE";
        let desc1 = "BRAVOZERO alpha beta gamma delta epsilon zeta eta theta iota kappa BRAVODONE";
        let mut handler = make_visible_handler(vec![
            command_suggestion("alpha", Some(desc0)),
            command_suggestion("bravo", Some(desc1)),
        ])
        .with_popup_widths(20, 40)
        .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        handler.overlay.selected = Some(0);

        let mut first = Vec::new();
        handler.render_at(&mut first, 5, 0, 24, 120);
        handler.commit_overlay_write(handler.overlay_write_ticket());
        let first_output = String::from_utf8_lossy(&first);
        assert!(
            first_output.contains("ALPHAZERO"),
            "setup should render item 0 detail: {first_output:?}"
        );

        handler.overlay.selected = Some(1);
        let mut second = Vec::new();
        handler.render_at(&mut second, 5, 0, 24, 120);
        handler.commit_overlay_write(handler.overlay_write_ticket());

        let second_output = String::from_utf8_lossy(&second);
        assert!(
            second_output.contains("BRAVOZERO"),
            "debounce disabled should render item 1 detail immediately: {second_output:?}"
        );
        assert!(
            !second_output.contains("ALPHADONE"),
            "debounce disabled must not keep rendering item 0 detail: {second_output:?}"
        );
        assert!(
            !handler.detail_debounce.pending,
            "debounce disabled must not arm a debounce wakeup"
        );
    }

    #[test]
    fn test_render_at_description_box_off_skips_detail_box() {
        let description = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let mut handler = make_selected_handler(command_suggestion("checkout", Some(description)))
            .with_popup_widths(20, 40)
            .with_description_box(DescriptionBoxMode::Off, 60, 5, 0);
        let mut buf = Vec::new();

        handler.render_at(&mut buf, 5, 0, 24, 120);

        assert!(handler.last_detail_layout.is_none());
        let output = String::from_utf8_lossy(&buf);
        assert!(
            !output.contains("lambda mu"),
            "off mode must not emit detail-only wrapped text: {output:?}"
        );
    }

    #[test]
    fn test_render_at_skips_detail_box_when_inline_description_fits() {
        let mut handler = make_selected_handler(command_suggestion("checkout", Some("short")))
            .with_popup_widths(20, 80)
            .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        let mut buf = Vec::new();

        handler.render_at(&mut buf, 5, 0, 24, 120);

        assert!(handler.last_detail_layout.is_none());
        assert_eq!(handler.detail_debounce.displayed_idx, Some(0));
    }

    #[test]
    fn test_render_at_clears_previous_detail_when_selection_no_longer_needs_box() {
        let mut handler = make_selected_handler(command_suggestion("checkout", Some("short")))
            .with_popup_widths(20, 80)
            .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        handler.last_detail_layout = Some(DetailLayout {
            start_row: 5,
            start_col: 50,
            width: 20,
            height: 2,
            position: overlay::DetailPosition::SideRight,
        });
        let mut buf = Vec::new();

        handler.render_at(&mut buf, 5, 0, 24, 120);

        let output = String::from_utf8_lossy(&buf);
        assert!(
            output.contains("\x1b[6;51H"),
            "old detail layout should be cleared when no new detail is needed: {output:?}"
        );
        handler.commit_overlay_write(handler.overlay_write_ticket());
        assert!(handler.last_detail_layout.is_none());
    }

    #[test]
    fn test_settled_selection_without_description_does_not_repaint_old_detail() {
        let old_desc = format!("{}UNIQUEDETAILMARKER", "alpha ".repeat(20));
        let mut handler = make_visible_handler(vec![
            command_suggestion("old", Some(&old_desc)),
            command_suggestion("new", None),
        ])
        .with_popup_widths(20, 40)
        .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        handler.overlay.selected = Some(0);

        let mut first = Vec::new();
        handler.render_at(&mut first, 5, 0, 24, 120);
        assert!(
            String::from_utf8_lossy(&first).contains("UNIQUEDETAILMARKER"),
            "setup must render the first item's detail text"
        );
        assert_eq!(handler.detail_debounce.displayed_idx, Some(0));

        handler.detail_box_debounce_ms = 80;
        handler.overlay.selected = Some(1);
        handler.detail_debounce.last_change_at =
            Some(Instant::now() - std::time::Duration::from_millis(100));
        let mut settled = Vec::new();
        handler.render_at(&mut settled, 5, 0, 24, 120);
        assert!(
            !String::from_utf8_lossy(&settled).contains("UNIQUEDETAILMARKER"),
            "settled no-description selection must not render old detail text"
        );

        let mut repaint = Vec::new();
        handler.render_at(&mut repaint, 5, 0, 24, 120);

        let output = String::from_utf8_lossy(&repaint);
        assert!(
            !output.contains("UNIQUEDETAILMARKER"),
            "later repaint must not debounce back to the previous detail text: {output:?}"
        );
        assert_eq!(handler.detail_debounce.displayed_idx, Some(1));
    }

    #[test]
    fn test_render_at_uses_runtime_popup_and_detail_size_knobs() {
        let mut min_width_handler =
            make_visible_handler(vec![command_suggestion("sh", None)]).with_popup_widths(35, 120);
        let mut min_buf = Vec::new();

        min_width_handler.render_at(&mut min_buf, 5, 0, 24, 120);
        min_width_handler.commit_overlay_write(min_width_handler.overlay_write_ticket());

        assert_eq!(
            min_width_handler
                .last_layout
                .as_ref()
                .expect("min-width render should commit a layout")
                .width,
            35
        );

        let long_text = "x".repeat(200);
        let mut max_width_handler =
            make_visible_handler(vec![command_suggestion(&long_text, None)])
                .with_popup_widths(10, 30);
        let mut max_buf = Vec::new();

        max_width_handler.render_at(&mut max_buf, 5, 0, 24, 120);
        max_width_handler.commit_overlay_write(max_width_handler.overlay_write_ticket());

        assert_eq!(
            max_width_handler
                .last_layout
                .as_ref()
                .expect("max-width render should commit a layout")
                .width,
            30
        );

        let detail_desc =
            "DETAILSIZING alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu";
        let mut detail_handler =
            make_selected_handler(command_suggestion("checkout", Some(detail_desc)))
                .with_popup_widths(20, 40)
                .with_description_box(DescriptionBoxMode::Side, 30, 2, 0);
        let mut detail_buf = Vec::new();

        detail_handler.render_at(&mut detail_buf, 5, 0, 24, 120);
        detail_handler.commit_overlay_write(detail_handler.overlay_write_ticket());

        let detail_layout = detail_handler
            .last_detail_layout
            .as_ref()
            .expect("detail sizing render should commit a detail layout");
        assert_eq!(detail_layout.width, 30);
        assert_eq!(detail_layout.height, 2);
    }

    #[test]
    fn test_render_at_skips_old_clear_when_feedback_only_render_scrolls() {
        let mut handler = make_handler();
        handler.overlay_scroll_deficit = 4;
        handler.last_layout = Some(PopupLayout {
            start_row: 19,
            start_col: 70,
            width: 10,
            height: 1,
            scroll_deficit: 4,
        });
        handler.feedback = AsyncFeedback::Loading {
            spawned_at: std::time::Instant::now(),
        };
        let mut buf = Vec::new();

        handler.render_at(&mut buf, 23, 0, 24, 80);
        handler.commit_overlay_write(handler.overlay_write_ticket());

        let output = String::from_utf8_lossy(&buf);
        assert!(
            !output.contains("\x1b[20;71H"),
            "stale feedback-only clear at old popup coordinates must be skipped: {output:?}"
        );
        assert_eq!(
            handler
                .last_layout
                .as_ref()
                .expect("feedback layout")
                .scroll_deficit,
            handler.overlay_scroll_deficit
        );
    }

    #[test]
    fn test_render_at_bumps_output_epoch_for_proxy_stale_write_gate() {
        let mut handler = make_visible_handler(numbered_suggestions(3));
        let before_epoch = handler.output_epoch();
        let mut buf = Vec::new();

        handler.render_at(&mut buf, 5, 0, 24, 80);

        assert!(!buf.is_empty());
        assert!(handler.output_epoch() > before_epoch);
    }

    #[test]
    fn test_render_at_commits_layout_only_after_overlay_write_ack() {
        let mut handler = make_visible_handler(numbered_suggestions(8));
        handler.overlay_scroll_deficit = 4;
        handler.last_layout = Some(PopupLayout {
            start_row: 19,
            start_col: 70,
            width: 10,
            height: 2,
            scroll_deficit: 4,
        });
        let mut buf = Vec::new();

        handler.render_at(&mut buf, 22, 0, 24, 80);

        assert!(!buf.is_empty());
        assert_eq!(
            handler.overlay_scroll_deficit, 4,
            "staged render must not commit scroll deficit before stdout write succeeds"
        );
        let layout = handler.last_layout.as_ref().expect("committed old layout");
        assert_eq!(layout.start_row, 19);
        assert_eq!(layout.start_col, 70);
        assert_eq!(layout.scroll_deficit, 4);

        handler.commit_overlay_write(handler.overlay_write_ticket());

        let layout = handler.last_layout.as_ref().expect("committed new layout");
        assert_eq!(handler.overlay_scroll_deficit, layout.scroll_deficit);
        assert_ne!(
            layout.start_col, 70,
            "acknowledged render should replace the previous committed layout"
        );
    }

    #[test]
    fn test_apply_block_result_uses_live_geometry_when_word_unchanged() {
        let mut handler = make_handler();
        handler.prime_dynamic_ctx_for_buffer("git checkout main", 17);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        {
            let mut p = parser.lock().unwrap();
            p.process_bytes(b"\x1b[11;1H");
            p.state_mut()
                .predict_command_buffer("git checkout main".to_string(), 17);
        }
        let mut buf = Vec::new();

        handler.apply_block_result(
            &parser,
            &mut buf,
            Some(DynamicResult::Loaded {
                provider: ProviderTag::Async("git branches".into()),
                suggestions: vec![Suggestion {
                    text: "main".into(),
                    kind: SuggestionKind::Subcommand,
                    source: SuggestionSource::Provider,
                    ..Default::default()
                }],
            }),
            None,
            None,
            Vec::new(),
            0,
            0,
            24,
            80,
            (0, 0, u64::MAX),
            "main",
        );

        let output = String::from_utf8_lossy(&buf);
        assert!(
            output.contains("\x1b[12;1H"),
            "async block merge must render at live cursor row, not spawn-time row: {output:?}"
        );
    }

    #[test]
    fn test_apply_block_result_replacement_resets_detail_debounce_state() {
        let mut handler = make_handler().with_description_box(DescriptionBoxMode::Side, 60, 5, 200);
        handler.visible = true;
        handler.detail_debounce.displayed_idx = Some(1);
        handler.detail_debounce.last_change_at = Some(Instant::now());
        handler.detail_debounce.pending = true;
        handler.prime_dynamic_ctx_for_buffer("git checkout main", 17);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 120)));
        {
            let mut p = parser.lock().unwrap();
            p.process_bytes(b"\x1b[6;1H");
            p.state_mut()
                .predict_command_buffer("git checkout main".to_string(), 17);
        }
        let mut buf = Vec::new();

        handler.apply_block_result(
            &parser,
            &mut buf,
            Some(DynamicResult::Loaded {
                provider: ProviderTag::Async("git branches".into()),
                suggestions: vec![command_suggestion(
                    "main",
                    Some("MAINDETAIL alpha beta gamma delta epsilon zeta eta theta"),
                )],
            }),
            None,
            None,
            Vec::new(),
            0,
            0,
            24,
            120,
            (0, 0, u64::MAX),
            "main",
        );

        assert_eq!(handler.detail_debounce.displayed_idx, None);
        assert_eq!(handler.detail_debounce.last_change_at, None);
        assert!(
            !handler.detail_debounce.pending,
            "replacing the result set must not keep an old detail redraw timer armed"
        );
    }

    #[test]
    fn test_manual_trigger_clears_debounce_suppression() {
        let mut handler = make_handler();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        // Suppress via arrow
        handler.process_key(&KeyEvent::ArrowUp, &parser, &mut buf);
        assert!(handler.is_debounce_suppressed());
        // Clear via manual trigger (Ctrl+/)
        handler.process_key(&KeyEvent::CtrlSlash, &parser, &mut buf);
        assert!(!handler.is_debounce_suppressed());
    }

    // --- DynamicCtxSnapshot staleness truth table ---

    /// Test-only helper: build a `CommandContext` with the minimum field
    /// set the staleness tests care about. Everything else defaults to the
    /// "unquoted, first segment, not a flag" configuration.
    fn ctx(
        cmd: &str,
        args: &[&str],
        preceding_flag: Option<&str>,
        word_idx: usize,
        current_word: &str,
    ) -> buffer::CommandContext {
        buffer::CommandContext {
            command: Some(cmd.to_string()),
            args: args.iter().map(|s| s.to_string()).collect(),
            current_word: current_word.to_string(),
            word_index: word_idx,
            is_flag: false,
            is_long_flag: false,
            preceding_flag: preceding_flag.map(|s| s.to_string()),
            in_pipe: false,
            in_redirect: false,
            quote_state: buffer::QuoteState::None,
            is_first_segment: true,
        }
    }

    #[test]
    fn dynamic_ctx_identical_context_is_not_stale() {
        let base = ctx("git", &["checkout"], None, 2, "");
        let snap = DynamicCtxSnapshot::capture(&base);
        assert!(
            !snap.is_stale_against(&base),
            "identical context must not be stale"
        );
    }

    #[test]
    fn dynamic_ctx_different_command_is_stale() {
        let base = ctx("git", &["checkout"], None, 2, "");
        let snap = DynamicCtxSnapshot::capture(&base);
        let changed = ctx("docker", &["checkout"], None, 2, "");
        assert!(
            snap.is_stale_against(&changed),
            "different command must be stale"
        );
    }

    #[test]
    fn dynamic_ctx_different_args_is_stale() {
        let base = ctx("git", &["checkout"], None, 2, "");
        let snap = DynamicCtxSnapshot::capture(&base);
        let changed = ctx("git", &["branch"], None, 2, "");
        assert!(
            snap.is_stale_against(&changed),
            "different args must be stale"
        );
    }

    #[test]
    fn dynamic_ctx_different_preceding_flag_is_stale() {
        let base = ctx("git", &["checkout"], None, 2, "");
        let snap = DynamicCtxSnapshot::capture(&base);
        let changed = ctx("git", &["checkout"], Some("-b"), 2, "");
        assert!(
            snap.is_stale_against(&changed),
            "different preceding_flag must be stale"
        );
    }

    #[test]
    fn dynamic_ctx_different_word_index_is_stale() {
        let base = ctx("git", &["checkout"], None, 2, "");
        let snap = DynamicCtxSnapshot::capture(&base);
        let changed = ctx("git", &["checkout"], None, 3, "");
        assert!(
            snap.is_stale_against(&changed),
            "different word_index must be stale"
        );
    }

    #[test]
    fn dynamic_ctx_prefix_extension_not_stale() {
        // Async providers use current_word only as a fuzzy-filter prefix,
        // so typing more characters of the prefix is not a staleness trigger.
        let base = ctx("git", &["checkout"], None, 2, "ma");
        let snap = DynamicCtxSnapshot::capture(&base);
        let extended = ctx("git", &["checkout"], None, 2, "main");
        assert!(
            !snap.is_stale_against(&extended),
            "prefix extension must not be stale"
        );
    }

    // --- dismiss/trigger dynamic_task abort verification ---

    #[tokio::test]
    async fn test_dismiss_clears_dynamic_task_and_rx() {
        // Regression: dismiss() must abort any in-flight generator task
        // AND clear dynamic_rx/dynamic_ctx so a subsequent trigger can
        // start fresh without merging stale results.
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "test".to_string(),
            ..Default::default()
        }]);

        // Populate dynamic state as if generators were in flight.
        let (_tx, rx) = mpsc::channel::<DynamicResult>(1);
        handler.dynamic_rx = Some(rx);
        handler.dynamic_ctx = Some(DynamicCtxSnapshot::capture(&ctx(
            "git",
            &["checkout"],
            None,
            2,
            "",
        )));
        handler.dynamic_task = Some(tokio::spawn(async {
            // Long-running task that must be aborted.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }));

        let mut stdout_buf = Vec::new();
        handler.dismiss(&mut stdout_buf);

        assert!(
            handler.dynamic_task.is_none(),
            "dismiss must clear dynamic_task"
        );
        assert!(
            handler.dynamic_rx.is_none(),
            "dismiss must clear dynamic_rx"
        );
        assert!(
            handler.dynamic_ctx.is_none(),
            "dismiss must clear dynamic_ctx"
        );
    }

    #[tokio::test]
    async fn test_trigger_aborts_in_flight_generators() {
        // Regression: when trigger() fires with a new context, any
        // in-flight generator task from a previous trigger must be
        // aborted and dynamic_rx/dynamic_ctx cleared before the new
        // generators are spawned. Otherwise stale generator results
        // could be merged into an unrelated completion site.
        let mut handler = make_handler();
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));

        // Set buffer state so trigger() doesn't early-return on empty.
        {
            let mut p = parser.lock().unwrap();
            p.state_mut().predict_command_buffer("git ".to_string(), 4);
        }

        // Populate in-flight dynamic state mimicking a prior trigger that
        // spawned generators against a different command.
        let (_tx, rx) = mpsc::channel::<DynamicResult>(1);
        handler.dynamic_rx = Some(rx);
        handler.dynamic_ctx = Some(DynamicCtxSnapshot::capture(&ctx(
            "old-cmd",
            &[],
            None,
            0,
            "",
        )));
        handler.dynamic_task = Some(tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }));

        let mut stdout = Vec::new();
        handler.trigger(&parser, &mut stdout);

        // trigger() may re-populate dynamic_rx/ctx/task if the new buffer
        // produced new async generators. What matters is that the OLD
        // values were replaced, not their specific new state.
        if let Some(ref snapshot) = handler.dynamic_ctx {
            assert_ne!(
                snapshot.command.as_deref(),
                Some("old-cmd"),
                "trigger() must clear or replace stale dynamic_ctx"
            );
        }
    }

    #[tokio::test]
    async fn test_abort_dynamic_task_and_clear_ctx_clears_both_fields() {
        let mut handler = make_handler();
        handler.dynamic_task = Some(tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }));
        handler.dynamic_ctx = Some(DynamicCtxSnapshot::capture(&ctx(
            "git",
            &["checkout"],
            None,
            2,
            "",
        )));

        handler.abort_dynamic_task_and_clear_ctx();

        assert!(handler.dynamic_task.is_none());
        assert!(handler.dynamic_ctx.is_none());
    }

    fn dedup_suggestion(text: &str) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn merge_dedup_against_drops_text_duplicates_from_existing_pool() {
        let existing = vec![dedup_suggestion("main")];
        let incoming = vec![dedup_suggestion("main"), dedup_suggestion("dev")];
        let kept = merge_dedup_against(&existing, incoming);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].text, "dev");
    }

    #[test]
    fn merge_dedup_against_drops_duplicates_within_same_batch() {
        let incoming = vec![
            dedup_suggestion("main"),
            dedup_suggestion("main"),
            dedup_suggestion("dev"),
        ];
        let kept = merge_dedup_against(&[], incoming);
        let texts: Vec<&str> = kept.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["main", "dev"]);
    }

    // --- clear_expired_feedback state machine ---

    #[test]
    fn test_clear_expired_feedback_returns_false_when_idle() {
        let mut handler = make_visible_handler(Vec::new()).with_feedback_dismiss_ms(1200);
        handler.feedback = AsyncFeedback::Idle;
        let mut buf: Vec<u8> = Vec::new();
        assert!(!handler.clear_expired_feedback(&mut buf));
        assert!(buf.is_empty());
    }

    #[test]
    fn test_clear_expired_feedback_returns_false_when_loading() {
        let mut handler = make_visible_handler(Vec::new()).with_feedback_dismiss_ms(1200);
        handler.feedback = AsyncFeedback::Loading {
            spawned_at: std::time::Instant::now(),
        };
        let mut buf: Vec<u8> = Vec::new();
        assert!(!handler.clear_expired_feedback(&mut buf));
        assert!(buf.is_empty());
    }

    #[test]
    fn test_clear_expired_feedback_returns_false_when_dismiss_disabled() {
        let mut handler = make_visible_handler(Vec::new()).with_feedback_dismiss_ms(0);
        handler.feedback = AsyncFeedback::Empty {
            since: std::time::Instant::now() - std::time::Duration::from_secs(10),
        };
        let mut buf: Vec<u8> = Vec::new();
        assert!(!handler.clear_expired_feedback(&mut buf));
        assert!(buf.is_empty());
    }

    #[test]
    fn test_clear_expired_feedback_returns_false_when_not_yet_expired() {
        let mut handler = make_visible_handler(Vec::new()).with_feedback_dismiss_ms(10_000);
        handler.feedback = AsyncFeedback::Empty {
            since: std::time::Instant::now(),
        };
        let mut buf: Vec<u8> = Vec::new();
        assert!(!handler.clear_expired_feedback(&mut buf));
        assert!(handler.visible);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_clear_expired_feedback_dismisses_on_expired_empty() {
        let mut handler = make_visible_handler(Vec::new()).with_feedback_dismiss_ms(1200);
        handler.feedback = AsyncFeedback::Empty {
            since: std::time::Instant::now() - std::time::Duration::from_millis(2000),
        };
        let mut buf: Vec<u8> = Vec::new();
        assert!(handler.clear_expired_feedback(&mut buf));
        assert!(!handler.visible);
        assert!(matches!(handler.feedback, AsyncFeedback::Idle));
    }

    #[test]
    fn test_clear_expired_feedback_partial_error_with_suggestions_demotes_to_idle() {
        // Regression: PartialError expiry with merged suggestions must keep popup visible.
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "main".into(),
            kind: SuggestionKind::Subcommand,
            ..Default::default()
        }])
        .with_feedback_dismiss_ms(1200);
        handler.feedback = AsyncFeedback::PartialError {
            failed: vec!["git script".into()],
            since: std::time::Instant::now() - std::time::Duration::from_millis(2000),
        };
        let mut buf: Vec<u8> = Vec::new();
        assert!(handler.clear_expired_feedback(&mut buf));
        assert!(handler.visible, "popup must stay visible");
        assert_eq!(handler.suggestions.len(), 1, "suggestions must survive");
        assert!(matches!(handler.feedback, AsyncFeedback::Idle));
    }

    #[test]
    fn test_clear_expired_feedback_bordered_partial_error_paints_displaced_border_row() {
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "main".into(),
            kind: SuggestionKind::Subcommand,
            ..Default::default()
        }])
        .with_feedback_dismiss_ms(1200);
        handler.theme = PopupTheme {
            borders: true,
            ..PopupTheme::default()
        };
        handler.last_layout = Some(PopupLayout {
            start_row: 5,
            start_col: 10,
            width: 20,
            height: 4,
            scroll_deficit: 0,
        });
        handler.feedback = AsyncFeedback::PartialError {
            failed: vec!["git script".into()],
            since: std::time::Instant::now() - std::time::Duration::from_millis(2000),
        };

        let mut buf: Vec<u8> = Vec::new();
        assert!(handler.clear_expired_feedback(&mut buf));
        assert!(matches!(handler.feedback, AsyncFeedback::Idle));

        let painted = String::from_utf8_lossy(&buf).into_owned();
        assert!(
            painted.contains("\x1b[8;11H"),
            "indicator row must be addressed: {painted:?}"
        );
        assert!(
            painted.contains("\x1b[9;11H"),
            "displaced bottom-border row must be cleared: {painted:?}"
        );
        // A new bottom border must be drawn at the shrunk-bottom position.
        assert!(
            painted.contains('╰') && painted.contains('╯'),
            "bottom border must be redrawn at the new position: {painted:?}"
        );

        let shrunk = handler.last_layout.clone().expect("layout retained");
        assert_eq!(shrunk.height, 3);
        let mut clear_buf: Vec<u8> = Vec::new();
        overlay::clear_popup(&mut clear_buf, &shrunk, &handler.terminal_profile);
        let clear_text = String::from_utf8_lossy(&clear_buf).into_owned();
        // Rows 5,6,7 must be addressed by clear_popup (1-based 6,7,8).
        for row_1based in [6_u16, 7, 8] {
            let needle = format!("\x1b[{row_1based};11H");
            assert!(
                clear_text.contains(&needle),
                "clear_popup must address row {row_1based}: {clear_text:?}"
            );
        }
    }

    #[test]
    fn test_pending_failed_accumulates_across_two_try_merge_dynamic_calls() {
        // Cross-batch accumulation: an Error in batch 1 must survive into
        // the disconnect-time terminal feedback computed in batch 2.
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "--flag".into(),
            kind: SuggestionKind::Flag,
            source: SuggestionSource::Commands,
            ..Default::default()
        }]);
        let base_ctx = buffer::parse_command_context("", 0);
        handler.dynamic_ctx = Some(DynamicCtxSnapshot::capture(&base_ctx));
        handler.feedback = AsyncFeedback::Loading {
            spawned_at: std::time::Instant::now(),
        };

        let (tx, rx) = mpsc::channel::<DynamicResult>(2);
        // Batch 1: send Error only, do NOT drop tx — channel is still open.
        tx.try_send(DynamicResult::Error {
            provider: ProviderTag::Async("npm".into()),
            message: "oops".into(),
        })
        .unwrap();
        handler.dynamic_rx = Some(rx);

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.try_merge_dynamic(&parser, &mut buf);

        // After batch 1: pending_failed has the npm error, feedback still Loading.
        assert_eq!(handler.pending_failed.len(), 1);
        assert!(handler.feedback.is_loading());

        // Batch 2: send Loaded then drop tx so the channel disconnects.
        tx.try_send(DynamicResult::Loaded {
            provider: ProviderTag::Async("git branches".into()),
            suggestions: vec![Suggestion {
                text: "main".into(),
                kind: SuggestionKind::Subcommand,
                source: SuggestionSource::Provider,
                ..Default::default()
            }],
        })
        .unwrap();
        drop(tx);
        handler.try_merge_dynamic(&parser, &mut buf);

        // After batch 2 + disconnect: PartialError with the npm error from
        // batch 1 must have survived.
        match handler.feedback_kind() {
            AsyncFeedback::PartialError { failed, .. } => {
                assert_eq!(failed.len(), 1, "batch-1 error must survive batch-2");
            }
            other => panic!("expected PartialError feedback, got {other:?}"),
        }
    }

    #[test]
    fn test_pending_empty_count_accumulates_across_two_try_merge_dynamic_calls() {
        // Symmetric variant of the cross-batch accumulation test, for empty.
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "--flag".into(),
            kind: SuggestionKind::Flag,
            source: SuggestionSource::Commands,
            ..Default::default()
        }]);
        let base_ctx = buffer::parse_command_context("", 0);
        handler.dynamic_ctx = Some(DynamicCtxSnapshot::capture(&base_ctx));
        handler.feedback = AsyncFeedback::Loading {
            spawned_at: std::time::Instant::now(),
        };

        let (tx, rx) = mpsc::channel::<DynamicResult>(2);
        // Batch 1: Empty only, channel still open.
        tx.try_send(DynamicResult::Empty {
            provider: ProviderTag::Async("npm".into()),
        })
        .unwrap();
        handler.dynamic_rx = Some(rx);

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.try_merge_dynamic(&parser, &mut buf);

        assert_eq!(handler.pending_empty_count, 1);
        assert!(handler.feedback.is_loading());

        // Batch 2: Loaded then drop tx.
        tx.try_send(DynamicResult::Loaded {
            provider: ProviderTag::Async("git branches".into()),
            suggestions: vec![Suggestion {
                text: "main".into(),
                kind: SuggestionKind::Subcommand,
                source: SuggestionSource::Provider,
                ..Default::default()
            }],
        })
        .unwrap();
        drop(tx);
        handler.try_merge_dynamic(&parser, &mut buf);

        assert_eq!(handler.pending_empty_count, 0, "drained on disconnect");
        assert_eq!(handler.pending_failed.len(), 0, "no errors accumulated");
    }

    // --- try_merge_dynamic disconnect branches ---

    #[test]
    fn test_try_merge_dynamic_error_only_disconnect_yields_error() {
        let mut handler = make_visible_handler(Vec::new());
        let base_ctx = buffer::parse_command_context("", 0);
        handler.dynamic_ctx = Some(DynamicCtxSnapshot::capture(&base_ctx));

        let (tx, rx) = mpsc::channel::<DynamicResult>(1);
        tx.try_send(DynamicResult::Error {
            provider: ProviderTag::Async("git branches".into()),
            message: "boom".into(),
        })
        .unwrap();
        drop(tx);
        handler.dynamic_rx = Some(rx);

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.try_merge_dynamic(&parser, &mut buf);

        match handler.feedback_kind() {
            AsyncFeedback::Error { failed, .. } => {
                assert_eq!(failed.len(), 1, "single failed provider expected");
            }
            other => panic!("expected Error feedback, got {other:?}"),
        }
    }

    #[test]
    fn test_try_merge_dynamic_partial_error_with_static_present() {
        // Pre-seed a static suggestion, then deliver one Loaded + one Error
        // with a final disconnect. PartialError must result.
        let mut handler = make_visible_handler(vec![Suggestion {
            text: "--flag".into(),
            kind: SuggestionKind::Flag,
            source: SuggestionSource::Commands,
            ..Default::default()
        }]);
        let base_ctx = buffer::parse_command_context("", 0);
        handler.dynamic_ctx = Some(DynamicCtxSnapshot::capture(&base_ctx));

        let (tx, rx) = mpsc::channel::<DynamicResult>(2);
        tx.try_send(DynamicResult::Loaded {
            provider: ProviderTag::Async("git branches".into()),
            suggestions: vec![Suggestion {
                text: "main".into(),
                kind: SuggestionKind::Subcommand,
                source: SuggestionSource::Provider,
                ..Default::default()
            }],
        })
        .unwrap();
        tx.try_send(DynamicResult::Error {
            provider: ProviderTag::Async("npm".into()),
            message: "oops".into(),
        })
        .unwrap();
        drop(tx);
        handler.dynamic_rx = Some(rx);

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.try_merge_dynamic(&parser, &mut buf);

        match handler.feedback_kind() {
            AsyncFeedback::PartialError { failed, .. } => {
                assert_eq!(failed.len(), 1);
            }
            other => panic!("expected PartialError feedback, got {other:?}"),
        }
    }

    #[test]
    fn test_try_merge_dynamic_empty_only_with_no_static_yields_empty() {
        let mut handler = make_visible_handler(Vec::new());
        let base_ctx = buffer::parse_command_context("", 0);
        handler.dynamic_ctx = Some(DynamicCtxSnapshot::capture(&base_ctx));

        let (tx, rx) = mpsc::channel::<DynamicResult>(1);
        tx.try_send(DynamicResult::Empty {
            provider: ProviderTag::Async("git branches".into()),
        })
        .unwrap();
        drop(tx);
        handler.dynamic_rx = Some(rx);

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.try_merge_dynamic(&parser, &mut buf);

        assert!(matches!(
            handler.feedback_kind(),
            AsyncFeedback::Empty { .. }
        ));
    }

    // --- current_feedback_kind redaction and Error → PartialError fallthrough ---

    #[test]
    fn test_current_feedback_kind_redacts_provider_when_disabled() {
        let mut handler = make_handler();
        handler.theme.show_provider_errors = false;
        handler.feedback = AsyncFeedback::Error {
            failed: vec!["git script".into()],
            since: std::time::Instant::now(),
        };
        match handler.current_feedback_kind() {
            overlay::FeedbackKind::Error { provider } => {
                assert_eq!(provider, "", "provider must be redacted");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn test_current_feedback_kind_surfaces_provider_when_enabled() {
        let mut handler = make_handler();
        handler.theme.show_provider_errors = true;
        handler.feedback = AsyncFeedback::Error {
            failed: vec!["git script".into()],
            since: std::time::Instant::now(),
        };
        match handler.current_feedback_kind() {
            overlay::FeedbackKind::Error { provider } => {
                assert_eq!(provider, "git script");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn test_current_feedback_kind_multi_failed_falls_through_to_partial_error() {
        let mut handler = make_handler();
        handler.feedback = AsyncFeedback::Error {
            failed: vec!["a".into(), "b".into(), "c".into()],
            since: std::time::Instant::now(),
        };
        match handler.current_feedback_kind() {
            overlay::FeedbackKind::PartialError { providers } => {
                assert_eq!(providers, 3);
            }
            other => panic!("expected PartialError, got {other:?}"),
        }
    }

    #[test]
    fn test_current_feedback_kind_partial_error_clamps_at_u8_max() {
        let mut handler = make_handler();
        handler.feedback = AsyncFeedback::PartialError {
            failed: (0..300).map(|i| format!("p{i}")).collect(),
            since: std::time::Instant::now(),
        };
        match handler.current_feedback_kind() {
            overlay::FeedbackKind::PartialError { providers } => {
                assert_eq!(providers, u8::MAX);
            }
            other => panic!("expected PartialError, got {other:?}"),
        }
    }

    fn detail_layout_at(start_row: u16, start_col: u16, width: u16, height: u16) -> DetailLayout {
        DetailLayout {
            start_row,
            start_col,
            width,
            height,
            position: overlay::DetailPosition::SideRight,
        }
    }

    fn count_cursor_moves(buf: &[u8]) -> usize {
        let mut count = 0;
        let mut i = 0;
        while i + 1 < buf.len() {
            if buf[i] == 0x1b && buf[i + 1] == b'[' {
                let mut j = i + 2;
                while j < buf.len() && buf[j] != b'H' && buf[j] != b'l' && buf[j] != b'h' {
                    j += 1;
                }
                if j < buf.len() && buf[j] == b'H' {
                    count += 1;
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
        count
    }

    #[test]
    fn test_clear_detail_box_uncovered_by_no_covers_clears_full_rect() {
        let layout = detail_layout_at(5, 10, 4, 3);
        let mut buf = Vec::new();
        clear_detail_box_uncovered_by(&mut buf, &layout, &[]);

        assert!(buf.starts_with(b"\x1b7"), "must save cursor at start");
        assert!(buf.ends_with(b"\x1b8"), "must restore cursor at end");

        let space_count = buf.iter().filter(|&&b| b == b' ').count();
        assert_eq!(
            space_count,
            (layout.width as usize) * (layout.height as usize),
            "no covers should fully wipe the rect with width*height spaces",
        );
    }

    #[test]
    fn test_clear_detail_box_uncovered_by_full_cover_emits_no_spaces() {
        let layout = detail_layout_at(5, 10, 4, 3);
        let cover = OverlayRect {
            start_row: 4,
            start_col: 8,
            width: 12,
            height: 5,
        };
        let mut buf = Vec::new();
        clear_detail_box_uncovered_by(&mut buf, &layout, &[cover]);

        assert!(buf.starts_with(b"\x1b7"));
        assert!(buf.ends_with(b"\x1b8"));
        let space_count = buf.iter().filter(|&&b| b == b' ').count();
        assert_eq!(
            space_count, 0,
            "cover that fully contains the layout should leave no spans to clear",
        );
    }

    #[test]
    fn test_clear_detail_box_uncovered_by_split_into_left_right_slivers() {
        let layout = detail_layout_at(5, 10, 10, 2);
        let cover = OverlayRect {
            start_row: 5,
            start_col: 13,
            width: 4,
            height: 2,
        };
        let mut buf = Vec::new();
        clear_detail_box_uncovered_by(&mut buf, &layout, &[cover]);

        assert!(buf.starts_with(b"\x1b7"));
        assert!(buf.ends_with(b"\x1b8"));
        let cursor_moves = count_cursor_moves(&buf);
        assert_eq!(
            cursor_moves, 4,
            "splitting each row into two slivers should emit 2 moves * 2 rows = 4 moves",
        );
        let space_count = buf.iter().filter(|&&b| b == b' ').count();
        let expected_per_row = (layout.width - cover.width) as usize;
        assert_eq!(
            space_count,
            expected_per_row * layout.height as usize,
            "surviving widths per row must sum to (layout.width - cover.width)",
        );
    }

    #[test]
    fn test_clear_detail_box_uncovered_by_multiple_covers_disjoint() {
        let layout = detail_layout_at(5, 10, 14, 1);
        let cover_a = OverlayRect {
            start_row: 5,
            start_col: 13,
            width: 3,
            height: 1,
        };
        let cover_b = OverlayRect {
            start_row: 5,
            start_col: 19,
            width: 2,
            height: 1,
        };
        let mut buf = Vec::new();
        clear_detail_box_uncovered_by(&mut buf, &layout, &[cover_a, cover_b]);

        assert!(buf.starts_with(b"\x1b7"));
        assert!(buf.ends_with(b"\x1b8"));
        let cursor_moves = count_cursor_moves(&buf);
        assert_eq!(
            cursor_moves, 3,
            "two disjoint covers on one row should leave three slivers (3 moves)",
        );
    }

    /// Guards the zero-size early-return at the top of
    /// `clear_detail_box_uncovered_by`. A regression that flips the `||` to
    /// `&&` would let a zero-width layout slip through to the row loop and
    /// emit a save/restore-cursor pair (\x1b7\x1b8) for nothing — a low-grade
    /// scrollback corruption risk.
    #[test]
    fn test_clear_detail_box_uncovered_by_zero_width_layout_emits_nothing() {
        let layout = DetailLayout {
            start_row: 5,
            start_col: 10,
            width: 0,
            height: 3,
            position: overlay::DetailPosition::SideRight,
        };
        let mut buf = Vec::new();
        clear_detail_box_uncovered_by(&mut buf, &layout, &[]);
        assert!(
            buf.is_empty(),
            "zero-width layout must short-circuit before emitting save/restore cursor: {buf:?}"
        );
    }

    /// Sibling guard for the zero-height arm of the same early-return.
    #[test]
    fn test_clear_detail_box_uncovered_by_zero_height_layout_emits_nothing() {
        let layout = DetailLayout {
            start_row: 5,
            start_col: 10,
            width: 4,
            height: 0,
            position: overlay::DetailPosition::SideRight,
        };
        let mut buf = Vec::new();
        clear_detail_box_uncovered_by(&mut buf, &layout, &[]);
        assert!(
            buf.is_empty(),
            "zero-height layout must short-circuit before emitting save/restore cursor: {buf:?}"
        );
    }

    #[test]
    fn test_detail_layout_after_scroll_zero_size_returns_none() {
        let layout = DetailLayout {
            start_row: 5,
            start_col: 10,
            width: 0,
            height: 0,
            position: overlay::DetailPosition::SideRight,
        };
        assert!(detail_layout_after_scroll(&layout, 1).is_none());
    }

    #[test]
    fn test_detail_layout_after_scroll_zero_scroll_clones() {
        let layout = detail_layout_at(5, 10, 12, 4);
        let result = detail_layout_after_scroll(&layout, 0).expect("zero scroll must clone");
        assert_eq!(result.start_row, layout.start_row);
        assert_eq!(result.start_col, layout.start_col);
        assert_eq!(result.width, layout.width);
        assert_eq!(result.height, layout.height);
    }

    #[test]
    fn test_detail_layout_after_scroll_fully_consumed_returns_none() {
        let layout = detail_layout_at(5, 10, 12, 3);
        assert!(
            detail_layout_after_scroll(&layout, 10).is_none(),
            "scroll that meets or exceeds end_row must drop the layout",
        );
    }

    #[test]
    fn test_detail_layout_after_scroll_partial_clip_adjusts_height() {
        let layout = detail_layout_at(2, 10, 12, 5);
        let result = detail_layout_after_scroll(&layout, 4).expect("partial clip must keep layout");
        assert_eq!(result.start_row, 0);
        assert_eq!(result.height, 3);
    }

    /// Boundary: layout starts at row 0, so `clipped_rows = scroll - 0 = scroll`
    /// directly, with no `saturating_sub` underflow involved. Guards the
    /// branch where the layout is anchored at the top of the screen and a
    /// partial scroll trims it from above.
    #[test]
    fn test_detail_layout_after_scroll_layout_at_origin_clips_full_scroll() {
        let layout = detail_layout_at(0, 0, 10, 5);
        let result = detail_layout_after_scroll(&layout, 2)
            .expect("partial clip at origin must keep layout");
        assert_eq!(result.start_row, 0);
        assert_eq!(result.height, 3);
    }

    #[test]
    fn test_detail_layout_after_scroll_scroll_below_start_row_no_clip() {
        let layout = detail_layout_at(5, 10, 12, 3);
        let result =
            detail_layout_after_scroll(&layout, 2).expect("scroll above start must keep layout");
        assert_eq!(result.start_row, 3);
        assert_eq!(result.height, 3);
    }

    #[test]
    fn test_render_for_detail_redraw_noop_when_invisible() {
        let mut handler = make_handler().with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.render_for_detail_redraw(&parser, &mut buf);
        assert!(buf.is_empty(), "no-op when handler is not visible");
    }

    #[test]
    fn test_render_for_detail_redraw_noop_when_mode_off() {
        let mut handler = make_visible_handler(vec![command_suggestion(
            "alpha",
            Some("ALPHADESC alpha beta gamma delta epsilon"),
        )]);
        handler.overlay.selected = Some(0);
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.render_for_detail_redraw(&parser, &mut buf);
        assert!(buf.is_empty(), "no-op when description_box mode is Off");
    }

    #[test]
    fn test_render_for_detail_redraw_noop_when_selection_unchanged() {
        let mut handler = make_visible_handler(vec![command_suggestion(
            "alpha",
            Some("ALPHADESC alpha beta gamma delta epsilon"),
        )])
        .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        handler.overlay.selected = Some(0);
        handler.detail_debounce.displayed_idx = Some(0);

        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let mut buf = Vec::new();
        handler.render_for_detail_redraw(&parser, &mut buf);
        assert!(
            buf.is_empty(),
            "no-op when displayed idx already matches selected"
        );
    }

    #[test]
    fn test_update_config_disable_auto_trigger_clears_orphaned_detail_layout() {
        let mut handler = make_visible_handler(vec![command_suggestion(
            "alpha",
            Some("ALPHADESC alpha beta gamma delta epsilon zeta eta"),
        )])
        .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        handler.overlay.selected = Some(0);
        handler.last_detail_layout = Some(DetailLayout {
            start_row: 7,
            start_col: 30,
            width: 25,
            height: 4,
            position: overlay::DetailPosition::SideRight,
        });
        handler.last_layout = None;

        let cleanup = handler.update_config(
            PopupTheme::default(),
            Keybindings::default(),
            10,
            1200,
            false,
            DEFAULT_MIN_POPUP_WIDTH,
            DEFAULT_MAX_POPUP_WIDTH,
            DescriptionBoxMode::Side,
            60,
            5,
            80,
            80,
            false,
        );

        let output = String::from_utf8_lossy(&cleanup);
        assert!(
            output.contains("\x1b[8;31H"),
            "cleanup must move to detail layout's start_row+1, start_col+1: {output:?}",
        );
        let space_count = cleanup.iter().filter(|&&b| b == b' ').count();
        assert!(
            space_count >= 25,
            "cleanup must include at least width spaces per detail row: got {space_count}",
        );

        let ticket = handler.overlay_write_ticket();
        assert!(
            ticket.cleanup_token.is_some(),
            "orphaned detail cleanup must be staged through the overlay write token",
        );
        handler.commit_overlay_write(ticket);
        assert!(
            handler.last_detail_layout.is_none(),
            "detail layout must be released after the cleanup write is acknowledged",
        );
    }

    // ------------------------------------------------------------------
    // Task 4: one DECSET 2026 frame per overlay update
    // ------------------------------------------------------------------

    /// Returns the byte position of a known detail-box content marker within
    /// `buf`, or panics with a message if not found.
    fn find_detail_marker(buf: &[u8], marker: &[u8]) -> usize {
        buf.windows(marker.len())
            .position(|w| w == marker)
            .unwrap_or_else(|| {
                panic!(
                    "detail marker {:?} not found in buf (len={})",
                    String::from_utf8_lossy(marker),
                    buf.len()
                )
            })
    }

    #[test]
    fn render_at_emits_exactly_one_sync_frame_on_synchronized_profile() {
        // Ghostty → Synchronized → DECSET 2026 frames expected.
        // Long description so the detail box is actually rendered.
        let description =
            "DETAIL_MARKER alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu";
        let mut handler = make_selected_handler(command_suggestion("checkout", Some(description)))
            .with_popup_widths(20, 40)
            .with_description_box(DescriptionBoxMode::Side, 60, 5, 0);
        // make_handler() already uses for_ghostty() (Synchronized), but be explicit.
        handler.terminal_profile = TerminalProfile::for_ghostty();
        let mut stdout = Vec::<u8>::new();

        handler.render_at(&mut stdout, 10, 0, 24, 120);

        let begin_count = stdout.windows(8).filter(|w| *w == b"\x1b[?2026h").count();
        let end_count = stdout.windows(8).filter(|w| *w == b"\x1b[?2026l").count();
        assert_eq!(
            begin_count, 1,
            "expected exactly one begin_sync; got {begin_count}"
        );
        assert_eq!(
            end_count, 1,
            "expected exactly one end_sync; got {end_count}"
        );

        // The detail-box bytes must be INSIDE the sync window.
        let begin_pos = stdout.windows(8).position(|w| w == b"\x1b[?2026h").unwrap();
        let end_pos = stdout.windows(8).position(|w| w == b"\x1b[?2026l").unwrap();
        let detail_pos = find_detail_marker(&stdout, b"DETAIL_MARKER");
        assert!(
            detail_pos > begin_pos && detail_pos < end_pos,
            "detail bytes must be inside the sync window \
             (begin={begin_pos}, detail={detail_pos}, end={end_pos})"
        );
    }

    #[test]
    fn render_at_pre_render_buffer_emits_no_sync_markers() {
        // iTerm2 → PreRenderBuffer → no DECSET 2026 markers.
        let mut handler = make_visible_handler(numbered_suggestions(5));
        handler.terminal_profile = TerminalProfile::for_iterm2();
        let mut stdout = Vec::<u8>::new();

        handler.render_at(&mut stdout, 10, 0, 24, 80);

        assert!(
            !stdout.windows(8).any(|w| w == b"\x1b[?2026h"),
            "PreRenderBuffer profile must not emit begin_sync"
        );
        assert!(
            !stdout.windows(8).any(|w| w == b"\x1b[?2026l"),
            "PreRenderBuffer profile must not emit end_sync"
        );
    }

    #[test]
    fn render_at_noop_renders_emit_no_bytes() {
        // Empty suggestions + no feedback → no-op render → zero bytes.
        let mut handler = make_handler();
        // make_handler() uses Ghostty (Synchronized) — sync markers would appear if not no-op.
        let mut stdout = Vec::<u8>::new();

        handler.render_at(&mut stdout, 10, 0, 24, 80);

        assert!(
            stdout.is_empty(),
            "no-op render must emit zero bytes; got {} bytes",
            stdout.len()
        );
    }

    #[test]
    fn render_at_emits_one_sync_frame_when_only_clear_runs() {
        // Boundary case: last_layout=Some(non-zero) AND suggestions=vec![] —
        // the popup was visible last frame but the user has typed a query that
        // filtered out all matches. clear_popup_unframed writes bytes (DECSC
        // \x1b7 + spaces) but render_popup_unframed early-exits because
        // suggestions.is_empty() and feedback is None. The frame helper must
        // still emit exactly one balanced begin_sync/end_sync pair around the
        // clear bytes — a future re-order of the clear/render calls could
        // regress this and only fail in live use.
        let mut handler = make_handler();
        handler.terminal_profile = TerminalProfile::for_ghostty();
        handler.last_layout = Some(PopupLayout {
            start_row: 5,
            start_col: 0,
            width: 20,
            height: 1,
            scroll_deficit: 0,
        });
        // suggestions stays empty (default); feedback stays Idle (None).
        let mut stdout = Vec::<u8>::new();

        handler.render_at(&mut stdout, 10, 0, 24, 80);

        let begin_count = stdout.windows(8).filter(|w| *w == b"\x1b[?2026h").count();
        let end_count = stdout.windows(8).filter(|w| *w == b"\x1b[?2026l").count();
        assert_eq!(
            begin_count, 1,
            "expected exactly one begin_sync; got {begin_count}"
        );
        assert_eq!(
            end_count, 1,
            "expected exactly one end_sync; got {end_count}"
        );

        // The clear bytes (DECSC \x1b7 emitted by clear_popup_unframed) must
        // sit inside the sync window.
        let begin_pos = stdout.windows(8).position(|w| w == b"\x1b[?2026h").unwrap();
        let end_pos = stdout.windows(8).position(|w| w == b"\x1b[?2026l").unwrap();
        let decsc_pos = stdout
            .windows(2)
            .position(|w| w == b"\x1b7")
            .expect("clear_popup_unframed must emit DECSC \\x1b7");
        assert!(
            decsc_pos > begin_pos && decsc_pos < end_pos,
            "clear bytes must be inside the sync window \
             (begin={begin_pos}, decsc={decsc_pos}, end={end_pos})"
        );
    }
    #[test]
    fn teardown_popup_emits_one_balanced_sync_pair_around_popup_and_detail() {
        let mut handler = make_handler();
        handler.terminal_profile = TerminalProfile::for_ghostty();
        handler.last_layout = Some(PopupLayout {
            start_row: 5,
            start_col: 0,
            width: 20,
            height: 3,
            scroll_deficit: 0,
        });
        handler.last_detail_layout = Some(detail_layout_at(5, 22, 30, 3));
        let mut stdout = Vec::<u8>::new();

        handler.teardown_popup(&mut stdout, false);

        let begin_count = stdout.windows(8).filter(|w| *w == b"\x1b[?2026h").count();
        let end_count = stdout.windows(8).filter(|w| *w == b"\x1b[?2026l").count();
        assert_eq!(
            begin_count, 1,
            "expected exactly one begin_sync; got {begin_count}"
        );
        assert_eq!(
            end_count, 1,
            "expected exactly one end_sync; got {end_count}"
        );

        // Both the popup-clear (DECSC \x1b7) and the detail-clear bytes must
        // sit inside the sync window.
        let begin_pos = stdout.windows(8).position(|w| w == b"\x1b[?2026h").unwrap();
        let end_pos = stdout.windows(8).position(|w| w == b"\x1b[?2026l").unwrap();
        let decsc_pos = stdout
            .windows(2)
            .position(|w| w == b"\x1b7")
            .expect("clear_popup_unframed must emit DECSC \\x1b7");
        assert!(
            decsc_pos > begin_pos && decsc_pos < end_pos,
            "popup-clear bytes must be inside the sync window \
             (begin={begin_pos}, decsc={decsc_pos}, end={end_pos})"
        );
    }

    // ---- shell_escape_for_context unit tests ----

    #[test]
    fn shell_escape_unquoted_escapes_whitespace_and_metacharacters() {
        let out = shell_escape_for_context("My Folder/file.txt", buffer::QuoteState::None);
        assert_eq!(out, r"My\ Folder/file.txt");

        let out = shell_escape_for_context("a$b`c|d", buffer::QuoteState::None);
        assert_eq!(out, r"a\$b\`c\|d");
    }

    #[test]
    fn shell_escape_unquoted_identity_for_safe_chars() {
        let out = shell_escape_for_context("plain_file.txt", buffer::QuoteState::None);
        assert_eq!(out, "plain_file.txt");
    }

    #[test]
    fn shell_escape_unquoted_leading_tilde_is_not_escaped() {
        // A leading `~` must survive bare so the shell still expands it to
        // $HOME. Escaping it (`\~`) makes the shell treat it as a literal
        // tilde and the `cd` fails. `/` is always left unescaped.
        let out = shell_escape_for_context("~/Documents/file", buffer::QuoteState::None);
        assert_eq!(out, "~/Documents/file");
    }

    #[test]
    fn shell_escape_unquoted_mid_word_tilde_is_escaped() {
        // A non-leading `~` is not subject to tilde expansion, so it stays
        // escaped (parity with the historical metacharacter handling).
        let out = shell_escape_for_context("a~b", buffer::QuoteState::None);
        assert_eq!(out, r"a\~b");
    }

    #[test]
    fn shell_escape_unquoted_escapes_glob_brace_and_hash() {
        // Glob (* ? [ ]), brace ({ }), and comment (#) chars must all be
        // backslashed so an accepted path is taken literally, not expanded.
        // Avoids a leading `~` per the tilde rule above.
        let out = shell_escape_for_context("a*b?c[d]e{f}g#i", buffer::QuoteState::None);
        assert_eq!(out, r"a\*b\?c\[d\]e\{f\}g\#i");
    }

    #[test]
    fn shell_escape_empty_input_is_unchanged_for_all_quote_states() {
        assert_eq!(shell_escape_for_context("", buffer::QuoteState::None), "");
        assert_eq!(
            shell_escape_for_context("", buffer::QuoteState::SingleQuoted),
            ""
        );
        assert_eq!(
            shell_escape_for_context("", buffer::QuoteState::DoubleQuoted),
            ""
        );
    }

    // ---- current_word_raw_start unit tests ----

    #[test]
    fn raw_word_start_plain_word() {
        // "cat My" — current word "My" starts at byte 4.
        let buf = "cat My";
        assert_eq!(current_word_raw_start(buf, buf.len()), 4);
    }

    #[test]
    fn raw_word_start_includes_backslash_escapes() {
        // "cd My\ Folder/" — the on-screen word starts at byte 3 and the
        // whole `My\ Folder/` span (backslash included) is one word.
        let buf = r"cd My\ Folder/";
        assert_eq!(current_word_raw_start(buf, buf.len()), 3);
        // On-screen char span = 11 (`My\ Folder/`), NOT the decoded 10.
        let start = current_word_raw_start(buf, buf.len());
        assert_eq!(buf[start..].chars().count(), 11);
    }

    #[test]
    fn raw_word_start_includes_opening_single_quote() {
        // "cat 'My Fo" — open single quote suppresses the space boundary, so
        // the raw word starts at the opening quote (byte 4).
        let buf = "cat 'My Fo";
        assert_eq!(current_word_raw_start(buf, buf.len()), 4);
        let start = current_word_raw_start(buf, buf.len());
        // On-screen span `'My Fo` = 6 chars (opening quote + "My Fo").
        assert_eq!(buf[start..].chars().count(), 6);
    }

    #[test]
    fn raw_word_start_at_word_boundary_is_cursor() {
        // Trailing space => empty current word => span start == cursor.
        let buf = "cat ";
        assert_eq!(current_word_raw_start(buf, buf.len()), buf.len());
    }

    #[test]
    fn shell_escape_single_quoted_leaves_spaces_alone() {
        let out = shell_escape_for_context("My Folder/file.txt", buffer::QuoteState::SingleQuoted);
        assert_eq!(out, "My Folder/file.txt");
    }

    #[test]
    fn shell_escape_single_quoted_close_reopens_internal_apostrophe() {
        let out = shell_escape_for_context("don't.txt", buffer::QuoteState::SingleQuoted);
        assert_eq!(out, r"don'\''t.txt");
    }

    #[test]
    fn shell_escape_double_quoted_escapes_only_special_quad() {
        // Inside double quotes, only " \ $ ` are special; spaces stay literal.
        let out = shell_escape_for_context(
            "My Folder/$VAR-`cmd`-\"q\"-\\b",
            buffer::QuoteState::DoubleQuoted,
        );
        assert_eq!(out, r#"My Folder/\$VAR-\`cmd\`-\"q\"-\\b"#);
    }

    // ---- shell escape applied at the accept call site ----

    /// Drive a parser to report a known command buffer via OSC 7770 so the
    /// accept-time `parse_command_context` sees the expected state. Returns
    /// the parser ready for `handler.accept_suggestion(&parser)`.
    fn parser_with_buffer(buffer: &str) -> Arc<Mutex<parser::TerminalParser>> {
        let parser = Arc::new(Mutex::new(parser::TerminalParser::new(24, 80)));
        let cursor = buffer.chars().count();
        let osc = format!("\x1b]7770;{cursor};{buffer}\x07");
        parser.lock().unwrap().process_bytes(osc.as_bytes());
        parser
    }

    fn path_suggestion(text: &str, kind: SuggestionKind) -> Suggestion {
        Suggestion {
            text: text.to_string(),
            kind,
            source: SuggestionSource::Filesystem,
            ..Default::default()
        }
    }

    #[test]
    fn accept_path_with_spaces_is_shell_escaped() {
        let handler = make_selected_handler(path_suggestion(
            "My Folder/file.txt",
            SuggestionKind::FilePath,
        ));
        let parser = parser_with_buffer("cat My");

        let bytes = handler.accept_suggestion(&parser);
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains(r"My\ Folder/file.txt"),
            "unquoted accept must backslash-escape spaces; got bytes={:?}",
            s
        );
        // Defensive: the un-escaped form is NOT present (otherwise the
        // shell would word-split into 'My' + 'Folder/file.txt').
        assert!(
            !s.contains("My Folder/file.txt"),
            "unescaped path must not appear in accept bytes: {:?}",
            s
        );
    }

    #[test]
    fn accept_path_in_single_quotes_is_not_backslash_escaped() {
        // Buffer ends inside an unclosed single quote: tokenizer reports
        // quote_state = SingleQuoted, so spaces are already preserved as
        // literal — backslash-escaping them would inject a stray '\'.
        let handler = make_selected_handler(path_suggestion(
            "My Folder/file.txt",
            SuggestionKind::FilePath,
        ));
        let parser = parser_with_buffer("cat 'My");

        let bytes = handler.accept_suggestion(&parser);
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            !s.contains(r"\ "),
            "single-quoted accept must NOT backslash-escape spaces; got bytes={:?}",
            s
        );
        assert!(
            s.contains("My Folder/file.txt"),
            "raw path text expected inside single quotes; got bytes={:?}",
            s
        );
        // Full reconstruction: only the in-quote partial `My` (2 chars) is
        // deleted; the opening `'` is preserved so the bare path stays quoted.
        // Pre-fix this deleted the quote too -> `cat My Folder/file.txt`.
        assert_eq!(
            s,
            format!("{}My Folder/file.txt", "\u{7f}".repeat(2)),
            "single-quoted accept must preserve the opening quote and insert bare text"
        );
    }

    #[test]
    fn chaining_uses_escaped_buffer() {
        // After accepting a directory, the chaining path predicts the
        // post-acceptance buffer state so the next suggestion round sees
        // the SAME bytes the shell will see. Without the escape applied
        // here, the engine reads "cd My Folder/" (3 tokens) while the
        // shell sees "cd My\ Folder/" (2 tokens) — the next completion
        // resolves the wrong directory.
        let mut handler =
            make_selected_handler(path_suggestion("My Folder/", SuggestionKind::Directory));
        let parser = parser_with_buffer("cd My");

        let mut stdout = Vec::new();
        let _ = handler.accept_with_chaining(&parser, &mut stdout);

        let predicted = parser
            .lock()
            .unwrap()
            .state()
            .command_buffer()
            .map(|s| s.to_string())
            .unwrap_or_default();
        assert!(
            predicted.contains(r"My\ Folder/"),
            "chaining predicted buffer must use escaped path; got {:?}",
            predicted
        );
    }

    /// Count leading 0x7F (backspace) bytes at the front of an accept payload.
    fn leading_backspaces(bytes: &[u8]) -> usize {
        bytes.iter().take_while(|&&b| b == 0x7F).count()
    }

    #[test]
    fn accept_deletes_on_screen_width_of_escaped_word_not_decoded() {
        // Regression (code-reviewer-2): when the on-screen buffer already
        // contains backslash escapes, the backspace count must match the RAW
        // on-screen span, not the tokenizer-decoded current_word. Here the
        // on-screen word `My\ Folder/` is 11 chars while the decoded word
        // `My Folder/` is 10 — using 10 would leave a stray char.
        let handler =
            make_selected_handler(path_suggestion("My Folder/sub/", SuggestionKind::Directory));
        let parser = parser_with_buffer(r"cd My\ Folder/");

        let bytes = handler.accept_suggestion(&parser);
        assert_eq!(
            leading_backspaces(&bytes),
            r"My\ Folder/".chars().count(),
            "must delete the full on-screen (escaped) word width, got bytes={:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    #[test]
    fn accept_deletes_partial_word_inside_open_single_quote_preserves_quote() {
        // Regression (code-reviewer-1): the on-screen word `'My Fo` includes the
        // OPENING quote, but that quote is structural — the single-quoted escape
        // arm emits bare text assuming the quote survives. The delete span must
        // therefore stop AFTER the opening quote, covering only the in-quote
        // partial word `My Fo` (5 chars), and the opening `'` must be preserved.
        // (Previously this deleted all 6 chars including the quote, dropping it
        // and unquoting the space -> `cat My Folder/file.txt`, which the shell
        // word-splits into 3 args.)
        let handler = make_selected_handler(path_suggestion(
            "My Folder/file.txt",
            SuggestionKind::FilePath,
        ));
        let parser = parser_with_buffer("cat 'My Fo");

        let bytes = handler.accept_suggestion(&parser);
        assert_eq!(
            leading_backspaces(&bytes),
            "My Fo".chars().count(),
            "must delete only the in-quote partial word, preserving the opening quote; got bytes={:?}",
            String::from_utf8_lossy(&bytes)
        );
        // Full reconstruction: 5 backspaces erase `My Fo`, leaving `cat '`, then
        // the bare (un-backslashed) path is typed inside the surviving quote.
        let s = String::from_utf8_lossy(&bytes);
        assert_eq!(
            s,
            format!("{}My Folder/file.txt", "\u{7f}".repeat(5)),
            "resulting accept bytes must keep the opening quote and insert bare text"
        );
    }

    #[test]
    fn double_chain_into_space_dir_deletes_full_escaped_word() {
        // Full chaining happy path: accept `My Folder/` (dir) so the predicted
        // buffer becomes `cd My\ Folder/`, then accept a child. The second
        // accept's leading-backspace count must equal the on-screen width of
        // the escaped word `My\ Folder/`, not the decoded `My Folder/`.
        let mut handler =
            make_selected_handler(path_suggestion("My Folder/", SuggestionKind::Directory));
        let parser = parser_with_buffer("cd My");

        let mut stdout = Vec::new();
        let _ = handler.accept_with_chaining(&parser, &mut stdout);

        let predicted = parser
            .lock()
            .unwrap()
            .state()
            .command_buffer()
            .map(|s| s.to_string())
            .unwrap_or_default();
        assert!(
            predicted.contains(r"My\ Folder/"),
            "precondition: chaining must predict escaped buffer; got {:?}",
            predicted
        );

        // Simulate the user selecting a child on the now-visible popup, then
        // accepting it. The second accept reads the predicted (escaped) buffer.
        handler.suggestions = vec![path_suggestion("My Folder/sub/", SuggestionKind::Directory)];
        handler.overlay.selected = Some(0);
        let bytes = handler.accept_suggestion(&parser);
        assert_eq!(
            leading_backspaces(&bytes),
            r"My\ Folder/".chars().count(),
            "second accept must delete the full on-screen escaped word; got bytes={:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    #[test]
    fn chaining_inside_open_double_quote_preserves_opening_quote() {
        // Regression (pr-test-analyzer-1): the chaining predicted-buffer
        // reconstruction (accept_with_chaining) was only exercised by UNQUOTED
        // directory chains. A quoted-directory chain routes a non-None
        // old_quote through `current_word_delete_start` at the predict slice
        // (`predicted.push_str(&buffer[..word_start_bytes])`). The opening `"`
        // is structural and `escaped_replacement` is bare text relying on it
        // surviving, so the predicted buffer must KEEP the opening quote and
        // leave the space literal inside the quote — no backslash inserted.
        // If the predict path dropped the quote, the next suggestion round
        // would read an unquoted buffer and resolve the wrong directory.
        let mut handler =
            make_selected_handler(path_suggestion("My Folder/", SuggestionKind::Directory));
        let parser = parser_with_buffer("cd \"My");

        let mut stdout = Vec::new();
        let _ = handler.accept_with_chaining(&parser, &mut stdout);

        let predicted = parser
            .lock()
            .unwrap()
            .state()
            .command_buffer()
            .map(|s| s.to_string())
            .unwrap_or_default();
        // Exact reconstruction: opening `"` preserved, space left literal
        // inside the quote, no backslash inserted. NOT a `.contains` check.
        assert_eq!(
            predicted, "cd \"My Folder/",
            "double-quoted chaining predicted buffer must keep the opening quote \
             and leave the space literal (no backslash); got {:?}",
            predicted
        );
    }

    #[test]
    fn chaining_inside_open_single_quote_preserves_opening_quote() {
        // Single-quote variant of the above (pr-test-analyzer-1). Inside an
        // open single quote spaces are already literal, so the bare path is
        // inserted unescaped and the opening `'` survives in the predicted
        // buffer used to drive the next suggestion round.
        let mut handler =
            make_selected_handler(path_suggestion("My Folder/", SuggestionKind::Directory));
        let parser = parser_with_buffer("cd 'My");

        let mut stdout = Vec::new();
        let _ = handler.accept_with_chaining(&parser, &mut stdout);

        let predicted = parser
            .lock()
            .unwrap()
            .state()
            .command_buffer()
            .map(|s| s.to_string())
            .unwrap_or_default();
        assert_eq!(
            predicted, "cd 'My Folder/",
            "single-quoted chaining predicted buffer must keep the opening quote \
             and leave the space literal (no backslash); got {:?}",
            predicted
        );
    }

    #[test]
    fn accept_inside_open_double_quote_escapes_special_quad_only() {
        // Integration (pr-test-analyzer-6): buffer ends inside an unclosed
        // double quote. A path containing `$` must have the `$` backslashed
        // (double-quote-special) while spaces stay literal (no `\ `).
        let handler =
            make_selected_handler(path_suggestion("My $VAR Dir/", SuggestionKind::Directory));
        let parser = parser_with_buffer("cd \"My $");

        let bytes = handler.accept_suggestion(&parser);
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains(r"\$VAR"),
            "double-quoted accept must escape `$`; got bytes={:?}",
            s
        );
        assert!(
            !s.contains(r"\ "),
            "double-quoted accept must leave spaces literal (no `\\ `); got bytes={:?}",
            s
        );
        // Full reconstruction (code-reviewer-1): the opening `"` is structural
        // and must survive. Only the in-quote partial `My $` (4 chars) is
        // deleted; the `$` is escaped but spaces stay literal under the quote.
        // Pre-fix the `"` was deleted too -> `cd My \$VAR Dir/` (quote gone).
        assert_eq!(
            s,
            format!("{}My \\$VAR Dir/", "\u{7f}".repeat(4)),
            "double-quoted accept must preserve the opening quote and escape only the special quad"
        );
    }

    // ---- current_word_delete_start unit tests ----

    #[test]
    fn delete_start_unquoted_is_raw_word_start() {
        // Unquoted context: delete span begins at the raw word start (whole
        // on-screen word replaced), identical to pre-fix behavior.
        let buf = "cat My";
        let raw = current_word_raw_start(buf, buf.len());
        assert_eq!(raw, 4);
        assert_eq!(
            current_word_delete_start(buf, raw, buf.len(), buffer::QuoteState::None),
            4
        );
    }

    #[test]
    fn delete_start_open_single_quote_skips_opening_quote() {
        // `cat 'My Fo`: raw word starts at the opening quote (byte 4); the
        // delete span must begin AFTER it (byte 5) so the quote is preserved.
        let buf = "cat 'My Fo";
        let raw = current_word_raw_start(buf, buf.len());
        assert_eq!(raw, 4);
        let start =
            current_word_delete_start(buf, raw, buf.len(), buffer::QuoteState::SingleQuoted);
        assert_eq!(start, 5);
        assert_eq!(&buf[start..], "My Fo");
    }

    #[test]
    fn delete_start_open_double_quote_skips_opening_quote() {
        // `cd "My $`: raw word starts at the opening quote (byte 3); delete
        // span begins after it (byte 4).
        let buf = "cd \"My $";
        let raw = current_word_raw_start(buf, buf.len());
        assert_eq!(raw, 3);
        let start =
            current_word_delete_start(buf, raw, buf.len(), buffer::QuoteState::DoubleQuoted);
        assert_eq!(start, 4);
        assert_eq!(&buf[start..], "My $");
    }

    #[test]
    fn delete_start_quote_opens_mid_word_preserves_only_up_to_quote() {
        // `cat foo'My Fo`: the word starts at `foo` (byte 4) but the quote opens
        // mid-word at byte 7. The delete span must begin after that quote (byte
        // 8), preserving the structural `foo'` prefix, not just the bare quote.
        let buf = "cat foo'My Fo";
        let raw = current_word_raw_start(buf, buf.len());
        assert_eq!(raw, 4);
        let start =
            current_word_delete_start(buf, raw, buf.len(), buffer::QuoteState::SingleQuoted);
        assert_eq!(start, 8);
        assert_eq!(&buf[start..], "My Fo");
    }

    // ---- current_word_raw_start: closing-quote and edge coverage ----

    #[test]
    fn raw_word_start_closed_single_quote_then_more_of_word() {
        // pr-test-analyzer-1: a CLOSED single quote followed by more of the same
        // word. `cd 'a'/My` — the quote opens at byte 3, closes at byte 5, and
        // `/My` continues the same word, so the word start stays at byte 3.
        let buf = "cd 'a'/My";
        let start = current_word_raw_start(buf, buf.len());
        assert_eq!(start, 3);
        assert_eq!(&buf[start..], "'a'/My");
    }

    #[test]
    fn raw_word_start_closed_double_quote_then_new_word() {
        // pr-test-analyzer-1: a CLOSED double quote, then whitespace, then a new
        // word. `cd "x" My` — after the quote closes (byte 5) the space at byte
        // 6 is a boundary, so the current word `My` starts at byte 7.
        let buf = "cd \"x\" My";
        let start = current_word_raw_start(buf, buf.len());
        assert_eq!(start, 7);
        assert_eq!(&buf[start..], "My");
    }

    #[test]
    fn raw_word_start_backslash_escaped_quote_inside_double_quote() {
        // pr-test-analyzer-1: inside double quotes, `\"` is an escaped literal
        // quote that does NOT close the span. `cd "a\"b` stays one open word
        // beginning at the opening quote (byte 3).
        let buf = "cd \"a\\\"b";
        let start = current_word_raw_start(buf, buf.len());
        assert_eq!(start, 3);
        assert_eq!(&buf[start..], "\"a\\\"b");
    }

    #[test]
    fn raw_word_start_multibyte_prefix() {
        // pr-test-analyzer-2: the helper returns a BYTE offset. With a multibyte
        // char before the current word, the offset must land on a char boundary
        // and slice correctly. `café My` — `é` is 2 bytes so `My` starts at
        // byte 6 (c,a,f = 3, é = 2, space = 1).
        let buf = "café My";
        let start = current_word_raw_start(buf, buf.len());
        assert_eq!(start, 6);
        assert_eq!(&buf[start..], "My");
    }

    #[test]
    fn accept_with_multibyte_before_word_counts_char_width_not_bytes() {
        // pr-test-analyzer-2: leading-backspace count is the on-screen CHAR
        // width of the current word, independent of multibyte bytes elsewhere.
        // Buffer `café My` (cursor at end) — current word `My` is 2 chars, so 2
        // backspaces regardless of the 2-byte `é` earlier in the line.
        let handler =
            make_selected_handler(path_suggestion("My Folder/", SuggestionKind::Directory));
        let parser = parser_with_buffer("café My");
        let bytes = handler.accept_suggestion(&parser);
        assert_eq!(
            leading_backspaces(&bytes),
            "My".chars().count(),
            "delete width must be the on-screen char width of the current word; got bytes={:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    #[test]
    fn raw_word_start_operator_boundary() {
        // pr-test-analyzer-4: the operator word-boundary arm. `ls foo|cat ba`
        // — the pipe at byte 6 is a boundary, then `cat` and a space, so the
        // current word `ba` starts at byte 11.
        let buf = "ls foo|cat ba";
        let start = current_word_raw_start(buf, buf.len());
        assert_eq!(start, 11);
        assert_eq!(&buf[start..], "ba");
    }

    #[test]
    fn raw_word_start_paren_is_not_a_boundary() {
        // code-reviewer-2: `(`/`)` are NOT boundaries (the tokenizer keeps them
        // in current_word). `cat (a)b` — the word `(a)b` begins at byte 4, the
        // paren is part of it, not a split point.
        let buf = "cat (a)b";
        let start = current_word_raw_start(buf, buf.len());
        assert_eq!(start, 4);
        assert_eq!(&buf[start..], "(a)b");
    }

    #[test]
    fn raw_word_start_trailing_backslash_survives() {
        // pr-test-analyzer-5: a dangling trailing `\` (iter.next() returns None)
        // must not panic and the word `foo\` survives. `cat foo\` — word starts
        // at byte 4.
        let buf = "cat foo\\";
        let start = current_word_raw_start(buf, buf.len());
        assert_eq!(start, 4);
        assert_eq!(&buf[start..], "foo\\");
    }

    // ---- full-line accept (ProviderValue / Llm) delete-span tests ----

    #[test]
    fn full_line_provider_accept_deletes_entire_buffer() {
        // Buffer "supabase " (9 chars), accept ProviderValue "supabase backups".
        // The entire buffer must be deleted (9 backspaces) and replaced with
        // the full suggestion text — no prefix duplication.
        let suggestion = Suggestion {
            text: "supabase backups".to_string(),
            kind: SuggestionKind::ProviderValue,
            source: SuggestionSource::Provider,
            ..Default::default()
        };
        let handler = make_selected_handler(suggestion);
        let parser = parser_with_buffer("supabase ");
        let bytes = handler.accept_suggestion(&parser);
        assert_eq!(
            leading_backspaces(&bytes),
            "supabase ".chars().count(),
            "full-line ProviderValue accept must delete the entire buffer"
        );
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains("supabase backups"),
            "replacement must be the full suggestion text, got: {s:?}"
        );
        // Ensure no duplication: the payload after backspaces should NOT
        // contain "supabase supabase".
        assert!(
            !s.contains("supabase supabase"),
            "buffer prefix must not be duplicated, got: {s:?}"
        );
    }

    #[test]
    fn full_line_llm_accept_deletes_entire_buffer() {
        // Buffer "git st" (6 chars), accept Llm "git status".
        let suggestion = Suggestion {
            text: "git status".to_string(),
            kind: SuggestionKind::Llm,
            source: SuggestionSource::Llm,
            ..Default::default()
        };
        let handler = make_selected_handler(suggestion);
        let parser = parser_with_buffer("git st");
        let bytes = handler.accept_suggestion(&parser);
        assert_eq!(
            leading_backspaces(&bytes),
            "git st".chars().count(),
            "full-line Llm accept must delete the entire buffer"
        );
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains("git status"),
            "replacement must be the full suggestion text, got: {s:?}"
        );
        assert!(
            !s.contains("git git"),
            "buffer prefix must not be duplicated, got: {s:?}"
        );
    }

    #[test]
    fn word_level_command_accept_deletes_only_current_word() {
        // Buffer "git st" (6 chars), accept Command "status".
        // Only the current word "st" (2 chars) should be deleted.
        let suggestion = Suggestion {
            text: "status".to_string(),
            kind: SuggestionKind::Command,
            source: SuggestionSource::Commands,
            ..Default::default()
        };
        let handler = make_selected_handler(suggestion);
        let parser = parser_with_buffer("git st");
        let bytes = handler.accept_suggestion(&parser);
        assert_eq!(
            leading_backspaces(&bytes),
            "st".chars().count(),
            "word-level Command accept must delete only the current word"
        );
        let s = String::from_utf8_lossy(&bytes);
        assert!(
            s.contains("status"),
            "replacement must be the suggestion text, got: {s:?}"
        );
    }
}
