//! TOML configuration, keybinding definitions, and color themes.
//!
//! Reads from `~/.config/termcmp/config.toml` with serde deserialization
//! and sensible defaults for all fields.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{de, Deserialize, Deserializer, Serialize};

fn deserialize_saturating_u16<'de, D>(deserializer: D) -> std::result::Result<u16, D::Error>
where
    D: Deserializer<'de>,
{
    let value = i64::deserialize(deserializer)?;
    if value < 0 {
        return Err(de::Error::invalid_value(
            de::Unexpected::Signed(value),
            &"a nonnegative integer",
        ));
    }
    if value > i64::from(u16::MAX) {
        // The post-clamp value would otherwise show up in normalize()'s warning
        // (always 65535), losing the user's original magnitude. Surface the
        // raw value here so the operator can spot the typo.
        tracing::warn!(
            "config value {} exceeds u16::MAX ({}); saturating before normalization",
            value,
            u16::MAX,
        );
    }

    Ok(value.min(i64::from(u16::MAX)) as u16)
}

/// Returns `~/.config/termcmp`, ignoring macOS `~/Library/Application Support/`.
pub fn config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".config").join("termcmp"))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TermcmpConfig {
    pub trigger: TriggerConfig,
    pub popup: PopupConfig,
    pub suggest: SuggestConfig,
    pub keybindings: KeybindingsConfig,
    pub theme: ThemeConfig,
    pub experimental: ExperimentalConfig,
    pub ai: AiConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExperimentalConfig {
    pub multi_terminal: bool,
}

/// Per-feature thinking/reasoning toggle. Maps to wire-format fields:
/// `chat_template_kwargs.enable_thinking` (openai-chat) or `reasoning.effort` (openai-responses).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiThinking {
    /// Send no thinking-related field; let the server decide.
    Auto,
    /// Force thinking on.
    On,
    /// Force thinking off (fast, deterministic — the default).
    #[default]
    Off,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    /// Inline autocompletion feature config.
    pub completion: AiCompletionConfig,
    /// On-demand "Ask AI" feature config.
    pub ask: AiAskConfig,
    /// User-defined LLM providers keyed by name (shared by both features).
    pub providers: std::collections::HashMap<String, AiProviderConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiCompletionConfig {
    /// Master switch for LLM-based inline autocompletion.
    pub enabled: bool,
    /// Key into `providers` map selecting the active provider.
    pub provider: String,
    /// Model id from the selected provider's model list.
    pub model: String,
    /// Timeout in milliseconds per LLM request. Clamped to [200, 30000].
    pub timeout_ms: u64,
    /// Maximum LLM suggestions per trigger. Clamped to [1, 10].
    pub max_results: usize,
    /// Max tokens for the completion response. Clamped to [16, 4096].
    pub max_tokens: u32,
    /// Thinking/reasoning toggle for thinking models (e.g. Qwen3).
    pub thinking: AiThinking,
}

impl Default for AiCompletionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: String::new(),
            model: String::new(),
            timeout_ms: 2000,
            max_results: 3,
            max_tokens: 256,
            thinking: AiThinking::Off,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiAskConfig {
    /// Show an on-demand "Ask AI" item at the top of the popup.
    pub enabled: bool,
    /// Key into `providers` map selecting the active provider.
    pub provider: String,
    /// Model id from the selected provider's model list.
    pub model: String,
    /// Timeout in milliseconds per LLM request. Clamped to [200, 30000].
    pub timeout_ms: u64,
    /// Max tokens for the Ask AI response. Clamped to [16, 4096].
    pub max_tokens: u32,
    /// Thinking/reasoning toggle for thinking models (e.g. Qwen3).
    pub thinking: AiThinking,
}

impl Default for AiAskConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: String::new(),
            model: String::new(),
            timeout_ms: 15000,
            max_tokens: 512,
            thinking: AiThinking::Off,
        }
    }
}

/// Common accessor for per-feature AI config, so the proxy can resolve both
/// features through one code path.
pub trait AiFeatureConfig {
    fn enabled(&self) -> bool;
    fn provider(&self) -> &str;
    fn model(&self) -> &str;
    fn timeout_ms(&self) -> u64;
    fn max_tokens(&self) -> u32;
    fn thinking(&self) -> AiThinking;
    /// Completion returns its configured cap; Ask returns 1 (single command).
    fn max_results(&self) -> usize;
}

impl AiFeatureConfig for AiCompletionConfig {
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn provider(&self) -> &str {
        &self.provider
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }
    fn max_tokens(&self) -> u32 {
        self.max_tokens
    }
    fn thinking(&self) -> AiThinking {
        self.thinking
    }
    fn max_results(&self) -> usize {
        self.max_results
    }
}

impl AiFeatureConfig for AiAskConfig {
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn provider(&self) -> &str {
        &self.provider
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }
    fn max_tokens(&self) -> u32 {
        self.max_tokens
    }
    fn thinking(&self) -> AiThinking {
        self.thinking
    }
    fn max_results(&self) -> usize {
        1
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiProviderConfig {
    /// Base URL of the OpenAI-compatible API (e.g. "https://api.openai.com/v1").
    pub base_url: String,
    /// API key — resolved as env-var name first, then treated as a literal token.
    /// Empty means no `Authorization` header is sent (local servers like llama.cpp).
    pub api_key: String,
    /// Wire format: "openai-chat" (POST /chat/completions) or "openai-responses" (POST /responses).
    pub api: String,
    /// Cap on reasoning tokens for thinking models (e.g. Qwen). 0 = model default.
    pub thinking_budget: u32,
    /// Extra fields merged verbatim into the request body. Use for
    /// server-specific options the typed fields don't cover — e.g.
    /// `chat_template_kwargs = { enable_thinking = false }` for Qwen3 on
    /// llama.cpp. termcmp's own fields (model, messages, max_tokens, …)
    /// always win on key collision. `None` = send nothing extra.
    pub extra_body: Option<toml::Value>,
    /// Available models for this provider.
    pub models: Vec<AiModelConfig>,
}

impl Default for AiProviderConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            api: "openai-chat".into(),
            thinking_budget: 0,
            extra_body: None,
            models: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AiModelConfig {
    /// Upstream model id used in the wire request.
    pub id: String,
    /// Display label.
    pub name: String,
    /// Max tokens override for this model. Falls back to `ai.max_tokens` if None.
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KeybindingsConfig {
    pub accept: String,
    pub accept_and_enter: String,
    pub dismiss: String,
    pub navigate_up: String,
    pub navigate_down: String,
    pub trigger: String,
    pub toggle_match_mode: String,
}

impl Default for KeybindingsConfig {
    fn default() -> Self {
        Self {
            accept: "tab".to_string(),
            accept_and_enter: "enter".to_string(),
            dismiss: "escape".to_string(),
            navigate_up: "arrow_up".to_string(),
            navigate_down: "arrow_down".to_string(),
            trigger: "ctrl+/".to_string(),
            toggle_match_mode: "ctrl+r".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TriggerConfig {
    /// Typing-pause debounce window (milliseconds) before suggestions are
    /// computed on regular printable keystrokes.
    ///
    /// - `delay_ms > 0`: Task D in `pty/src/proxy.rs` waits for this many
    ///   ms of inactivity after the last keystroke before firing a trigger.
    ///   This is the recommended behavior — it avoids re-ranking on every
    ///   character during fast typing.
    /// - `delay_ms = 0`: the debounce task is not spawned. Every printable
    ///   key and backspace fires a trigger immediately via
    ///   `handler.trigger_requested`, without any wait. Explicit triggers
    ///   (the `trigger` keybinding) still fire instantly regardless of this
    ///   value — `delay_ms` only gates the passive typing-pause path.
    ///
    /// Default: 150ms.
    ///
    /// **Hot-reload:** Yes — the debounce loop reads the window from a shared
    /// atomic each cycle, so a `config.toml` edit takes effect on the next
    /// typing pause without restarting the proxy.
    pub delay_ms: u64,
    pub auto_trigger: bool,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            delay_ms: 150,
            auto_trigger: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PopupConfig {
    pub max_visible: usize,
    pub borders: bool,
    /// When `true` (default), popup borders use rounded corners (`╭╮╰╯`).
    /// When `false`, square corners (`┌┐└┘`) are used. Applies to both the
    /// main popup and the description box.
    pub border_radius: bool,
    /// Empty/Error feedback dismiss delay (ms); 0 disables. Clamped to [0, 10000]. Default 1200.
    #[serde(deserialize_with = "deserialize_saturating_u16")]
    pub feedback_dismiss_ms: u16,
    /// Animate Loading feedback with a spinner; narrow popups fall back to ellipsis. Default true.
    pub spinner: bool,
    /// Show provider names in error feedback; default false to avoid leaking on shared screens.
    pub show_provider_errors: bool,
    /// Maximum time (ms) the popup will block waiting for a higher-priority
    /// async generator before painting whatever sync results we have. Set
    /// to `0` to disable blocking entirely (paint immediately, merge async
    /// later). Clamped to `[0, 300]` during normalization. Default: 80 ms,
    /// chosen to stay below the human perception threshold for "instant".
    #[serde(deserialize_with = "deserialize_saturating_u16")]
    pub render_block_ms: u16,
    /// Minimum popup width in display columns. Clamped to `[10, 500]`
    /// during normalization. Default 20.
    #[serde(deserialize_with = "deserialize_saturating_u16")]
    pub min_width: u16,
    /// Maximum popup width in display columns. Clamped to `[min_width, 500]`
    /// (or to `screen_cols` at render time, whichever is smaller).
    /// Increase this on wide terminals to give descriptions more room before
    /// the truncation ellipsis kicks in. Default 60.
    #[serde(deserialize_with = "deserialize_saturating_u16")]
    pub max_width: u16,
    /// Description box mode. When `"side"`, an adjacent box is rendered next
    /// to the main popup with the selected suggestion's full description,
    /// wrapped to multiple lines. `"off"` keeps the legacy inline-truncated
    /// behavior. Default `"off"`.
    ///
    /// The runtime stores the sibling tuning fields
    /// (`description_box_max_width`, `description_box_lines`,
    /// `description_box_debounce_ms`) regardless of mode and gates actual
    /// rendering on `description_box == Side`.
    pub description_box: DescriptionBoxMode,
    /// Maximum width (display columns) for the description box. Clamped to
    /// `[20, 200]` during normalization. The actual rendered width is
    /// `min(this, remaining columns next to main popup)`. Default 60.
    #[serde(deserialize_with = "deserialize_saturating_u16")]
    pub description_box_max_width: u16,
    /// Maximum number of wrapped lines in the description box. Long
    /// descriptions are hard-truncated with an ellipsis on the final line.
    /// `0` resets to default 5; values above 20 clamp to 20. Default 5.
    #[serde(deserialize_with = "deserialize_saturating_u16")]
    pub description_box_lines: u16,
    /// Debounce window (ms) for description-box updates on selection change.
    /// Holding arrow keys causes the box to update at most once per window,
    /// avoiding flicker. `0` disables debounce. Clamped to `[0, 500]`.
    /// Default 80, matching `render_block_ms`.
    #[serde(deserialize_with = "deserialize_saturating_u16")]
    pub description_box_debounce_ms: u16,
    /// When `true`, the accept key (Tab) accepts the top-ranked suggestion even
    /// when nothing has been navigated yet, instead of forwarding a literal tab
    /// to the shell. Lets users coming from Fig/Kiro keep a "type, glance, Tab"
    /// flow without an extra arrow-key press. Default `false` preserves the
    /// historical "navigate first, then accept" behavior. Only the `accept`
    /// action is affected: with the default bindings the `accept_and_enter`
    /// action (Enter) is a separate binding and still runs the command line, so
    /// a stray Enter never silently accepts the top suggestion. See issue #150.
    pub tab_accepts_top: bool,
    /// Show `selected/total` index in the popup header row. Default `true`.
    pub index_hints: bool,
    /// Show keybinding hints in the popup footer row. Default `true`.
    pub key_hints: bool,
    /// Use Nerd Font glyphs for kind icons in the popup gutter. When `false`,
    /// plain ASCII fallbacks are used. Default `true`.
    pub nerd_icons: bool,
}

impl Default for PopupConfig {
    fn default() -> Self {
        Self {
            max_visible: 10,
            borders: false,
            border_radius: true,
            feedback_dismiss_ms: 1200,
            spinner: true,
            show_provider_errors: false,
            render_block_ms: 80,
            min_width: 40,
            max_width: 60,
            description_box: DescriptionBoxMode::Off,
            description_box_max_width: 60,
            description_box_lines: 5,
            description_box_debounce_ms: 80,
            tab_accepts_top: false,
            index_hints: true,
            key_hints: true,
            nerd_icons: true,
        }
    }
}

/// How the typed query filters and ranks candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    /// Subsequence fuzzy matching: the typed characters must appear in order
    /// but need not be adjacent — `gco` matches `git checkout`. This is the
    /// default and preserves the historical behavior.
    #[default]
    Fuzzy,
    /// Contiguous substring matching: the typed characters must appear
    /// together, in order — `cl` matches `clone` and `include`, but not
    /// `calendar`. Space-separated words are matched as independent
    /// substrings (every word must be present). Less noisy and marginally
    /// faster than fuzzy on large candidate pools.
    Substring,
}

/// Behavior for the optional adjacent description box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DescriptionBoxMode {
    /// Legacy inline-truncated description in the main popup row only.
    #[default]
    Off,
    /// Adjacent box rendered to the side of (or below) the main popup, with
    /// wrapped multi-line description for the selected suggestion.
    Side,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SuggestConfig {
    /// Maximum number of ranked suggestions shown in the popup after
    /// fuzzy matching. Clamped to `[1, 10_000]` by [`TermcmpConfig::normalize`].
    ///
    /// - Upper bound `10_000`: values above are clamped with a warning to
    ///   avoid pathological memory / render cost.
    /// - Lower bound `1`: a literal `max_results = 0` is clamped to the
    ///   default (`50`) with a warning, because a zero cap would truncate
    ///   every result set to empty and render the popup permanently blank —
    ///   there is no legitimate user-facing reason to request that.
    /// - Default: `50`.
    ///
    /// **Hot-reload:** Yes — the config watcher swaps the value into the
    /// engine's live config on file change.
    pub max_results: usize,
    pub max_history_results: usize,
    /// How the typed query filters candidates: `fuzzy` (subsequence, default)
    /// or `substring` (contiguous). See [`MatchMode`].
    ///
    /// **Hot-reload:** Yes — swapped into the engine's live config on file
    /// change, like `max_results`.
    pub match_mode: MatchMode,
    pub providers: ProvidersConfig,
    /// Source-group ordering for the popup. Each name maps to a completion
    /// source; all items from an earlier-listed source appear before all
    /// items from a later one. Recognised names: `commands`, `filesystem`,
    /// `history`, `ai`, `env`, `shell`, `ssh`.
    ///
    /// **Hot-reload:** Yes — the config watcher swaps the order into the
    /// engine's live config via `SuggestionEngine::set_config` on file change.
    pub order: Vec<String>,
}

impl Default for SuggestConfig {
    fn default() -> Self {
        Self {
            max_results: 50,
            max_history_results: 5,
            match_mode: MatchMode::default(),
            providers: ProvidersConfig::default(),
            order: vec![
                "ai".into(),
                "history".into(),
                "shell".into(),
                "filesystem".into(),
                "commands".into(),
                "env".into(),
                "ssh".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    pub commands: bool,
    pub filesystem: bool,
    /// Enable shell-native completion providers (fish/zsh completions).
    /// When `true`, the shell's own completion system is queried for
    /// additional candidates. Default `true`.
    pub shell_completions: bool,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            commands: true,
            filesystem: true,
            shell_completions: true,
        }
    }
}

/// Theme-file schema — the TOML format used by built-in themes and
/// `<config_dir>/themes/<name>.toml` files.
///
/// Each field is `Option<String>`:
///
/// * `None` — key omitted. Inherits from the built-in `dark` fallback.
/// * `Some("")` — explicitly no styling (zero ANSI bytes).
/// * `Some("bold fg:196")` — explicit style, used verbatim.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct ThemeFile {
    // `skip_serializing_if` kept on every Option for consistency with the
    // TOML format even though ThemeFile is never serialized back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub match_highlight: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scrollbar: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_loading: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_empty: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_box_background: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind_icon: Option<String>,
}

/// User-facing theme config — a name selector, deserialized from `config.toml`.
///
/// All styling lives in theme files (built-in or `<config_dir>/themes/<name>.toml`).
/// Call [`ThemeConfig::resolve`] to collapse the name into a [`ResolvedTheme`]
/// where every field is a concrete `String`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Theme name: a built-in (`dark`, `light`, `catppuccin`, `material-darker`,
    /// `gruvbox`, `nord`, `dracula`, `tokyo-night`)
    /// or a `<config_dir>/themes/<name>.toml` file. Empty string resolves to `"dark"`.
    pub name: String,
    /// When true, the popup and description box backgrounds are cleared so
    /// the terminal's own background (including transparency/blur) shows through.
    pub transparency: bool,
}

/// Fully resolved theme — every field is a concrete style string (possibly
/// empty, meaning "no styling"). Produced by [`ThemeConfig::resolve`]; this
/// is what consumers (pty, overlay) should read.
///
/// Unlike [`ThemeFile`], there is no optionality: the resolver has already
/// merged the theme-file values over the built-in fallback.
#[derive(Debug, Clone, Default)]
pub struct ResolvedTheme {
    pub selected: String,
    pub description: String,
    pub match_highlight: String,
    pub item_text: String,
    pub scrollbar: String,
    pub border: String,
    pub feedback_loading: String,
    pub feedback_empty: String,
    pub feedback_error: String,
    pub background: String,
    pub description_box_background: String,
    pub kind_icon: String,
}

impl ThemeConfig {
    /// Resolve the theme name to a fully merged theme.
    ///
    /// Two-layer merge:
    /// 1. `<base_dir>/themes/<name>.toml` if it exists — omitted fields fall
    ///    back to the built-in `dark` theme.
    /// 2. Otherwise the embedded built-in theme matching `name`.
    pub fn resolve(&self, base_dir: Option<&Path>) -> Result<ResolvedTheme> {
        let name = if self.name.is_empty() {
            "dark"
        } else {
            self.name.as_str()
        };

        // 1. User theme file overrides the built-in of the same name.
        let user_file = base_dir.map(|d| d.join("themes").join(format!("{name}.toml")));
        let base = if let Some(path) = &user_file.filter(|p| p.is_file()) {
            let contents = std::fs::read_to_string(path)
                .with_context(|| format!("failed to read theme file: {}", path.display()))?;
            let file_theme: ThemeFile = toml::from_str(&contents)
                .with_context(|| format!("failed to parse theme file: {}", path.display()))?;
            // Fields omitted from a theme file fall back to built-in dark.
            apply_overrides(
                &file_theme,
                built_in_theme("dark").expect("dark built-in exists"),
            )
        } else if let Some(builtin) = built_in_theme(name) {
            // 2. Embedded built-in.
            builtin
        } else {
            bail!(
                "unknown theme {name:?} (built-ins: dark, light, catppuccin, material-darker, gruvbox, nord, dracula, tokyo-night; \
                 or add themes/{name}.toml)",
            );
        };

        let mut resolved = base;
        if self.transparency {
            resolved.background.clear();
            resolved.description_box_background.clear();
        }
        Ok(resolved)
    }
}

/// Layer the style overrides from `overrides` on top of `base`.
/// `Some(v)` (including `Some("")`) wins; `None` keeps `base`'s value.
fn apply_overrides(overrides: &ThemeFile, base: ResolvedTheme) -> ResolvedTheme {
    ResolvedTheme {
        selected: overrides.selected.clone().unwrap_or(base.selected),
        description: overrides.description.clone().unwrap_or(base.description),
        match_highlight: overrides
            .match_highlight
            .clone()
            .unwrap_or(base.match_highlight),
        item_text: overrides.item_text.clone().unwrap_or(base.item_text),
        scrollbar: overrides.scrollbar.clone().unwrap_or(base.scrollbar),
        border: overrides.border.clone().unwrap_or(base.border),
        feedback_loading: overrides
            .feedback_loading
            .clone()
            .unwrap_or(base.feedback_loading),
        feedback_empty: overrides
            .feedback_empty
            .clone()
            .unwrap_or(base.feedback_empty),
        feedback_error: overrides
            .feedback_error
            .clone()
            .unwrap_or(base.feedback_error),
        background: overrides.background.clone().unwrap_or(base.background),
        description_box_background: overrides
            .description_box_background
            .clone()
            .unwrap_or(base.description_box_background),
        kind_icon: overrides.kind_icon.clone().unwrap_or(base.kind_icon),
    }
}

/// Parse an embedded built-in theme file into a ResolvedTheme.
fn built_in_theme(name: &str) -> Option<ResolvedTheme> {
    let toml_str = match name {
        "dark" => include_str!("../themes/dark.toml"),
        "light" => include_str!("../themes/light.toml"),
        "catppuccin" => include_str!("../themes/catppuccin.toml"),
        "material-darker" => include_str!("../themes/material-darker.toml"),
        "gruvbox" => include_str!("../themes/gruvbox.toml"),
        "nord" => include_str!("../themes/nord.toml"),
        "dracula" => include_str!("../themes/dracula.toml"),
        "tokyo-night" => include_str!("../themes/tokyo-night.toml"),
        _ => return None,
    };
    let theme: ThemeFile = toml::from_str(toml_str)
        .unwrap_or_else(|e| panic!("built-in theme {name:?} malformed: {e}"));
    Some(apply_overrides(&theme, ResolvedTheme::default()))
}

const MAX_VISIBLE_DEFAULT: usize = 10;
const MAX_VISIBLE_UPPER: usize = 50;
const MAX_RESULTS_UPPER: usize = 10_000;
const MAX_RESULTS_DEFAULT: usize = 50;
const RENDER_BLOCK_MS_UPPER: u16 = 300;
const FEEDBACK_DISMISS_MS_UPPER: u16 = 10_000;
const POPUP_MIN_WIDTH_FLOOR: u16 = 10;
const POPUP_MAX_WIDTH_CEILING: u16 = 500;
const DESC_BOX_MAX_WIDTH_FLOOR: u16 = 20;
const DESC_BOX_MAX_WIDTH_CEILING: u16 = 200;
const DESC_BOX_LINES_CEILING: u16 = 20;
const DESC_BOX_DEBOUNCE_CEILING: u16 = 500;

/// Returns every leaf field path in dotted form, e.g. `popup.render_block_ms`.
/// Used by drift tests in `termcmp` to verify the install template and
/// `docs/CONFIGURATION.md` hot-reload table stay in sync with the schema.
///
/// New schema fields MUST appear here. Adding via copy-paste from
/// `TermcmpConfig` is acceptable; the cost of forgetting is a failing
/// drift test, not a runtime bug.
pub fn all_field_paths() -> Vec<&'static str> {
    vec![
        // [trigger]
        "trigger.delay_ms",
        "trigger.auto_trigger",
        // [popup]
        "popup.max_visible",
        "popup.borders",
        "popup.border_radius",
        "popup.feedback_dismiss_ms",
        "popup.spinner",
        "popup.show_provider_errors",
        "popup.render_block_ms",
        "popup.min_width",
        "popup.max_width",
        "popup.description_box",
        "popup.description_box_max_width",
        "popup.description_box_lines",
        "popup.description_box_debounce_ms",
        "popup.tab_accepts_top",
        "popup.index_hints",
        "popup.key_hints",
        // [suggest]
        "suggest.max_results",
        "suggest.max_history_results",
        "suggest.match_mode",
        "suggest.order",
        // [suggest.providers]
        "suggest.providers.commands",
        "suggest.providers.filesystem",
        "suggest.providers.shell_completions",
        // [keybindings] — 7 fields
        "keybindings.accept",
        "keybindings.accept_and_enter",
        "keybindings.dismiss",
        "keybindings.navigate_up",
        "keybindings.navigate_down",
        "keybindings.trigger",
        "keybindings.toggle_match_mode",
        // [theme] — 2 fields
        "theme.name",
        "theme.transparency",
        // [experimental]
        "experimental.multi_terminal",
        // [ai.completion]
        "ai.completion.enabled",
        "ai.completion.provider",
        "ai.completion.model",
        "ai.completion.timeout_ms",
        "ai.completion.max_results",
        "ai.completion.max_tokens",
        "ai.completion.thinking",
        // [ai.ask]
        "ai.ask.enabled",
        "ai.ask.provider",
        "ai.ask.model",
        "ai.ask.timeout_ms",
        "ai.ask.max_tokens",
        "ai.ask.thinking",
    ]
}

impl TermcmpConfig {
    /// Clamp config values to sane bounds, logging warnings when clamping.
    ///
    /// Exposed for TUI editor validation: callers can clone, normalize, and
    /// compare to detect out-of-range values without mutating the original.
    pub fn normalize(&mut self) {
        if self.popup.max_visible == 0 {
            tracing::warn!(
                "popup.max_visible=0 is invalid (would break popup scrolling), clamping to default {}",
                MAX_VISIBLE_DEFAULT,
            );
            self.popup.max_visible = MAX_VISIBLE_DEFAULT;
        }
        if self.popup.max_visible > MAX_VISIBLE_UPPER {
            tracing::warn!(
                "popup.max_visible={} exceeds maximum {}, clamping",
                self.popup.max_visible,
                MAX_VISIBLE_UPPER,
            );
            self.popup.max_visible = MAX_VISIBLE_UPPER;
        }
        if self.suggest.max_results > MAX_RESULTS_UPPER {
            tracing::warn!(
                "suggest.max_results={} exceeds maximum {}, clamping",
                self.suggest.max_results,
                MAX_RESULTS_UPPER,
            );
            self.suggest.max_results = MAX_RESULTS_UPPER;
        }
        // max_results=0 would truncate all ranked output to empty, leaving
        // the popup permanently blank. Clamp to the default and warn.
        if self.suggest.max_results == 0 {
            tracing::warn!(
                "suggest.max_results=0 is invalid (would hide all suggestions), \
                 clamping to default {}",
                MAX_RESULTS_DEFAULT,
            );
            self.suggest.max_results = MAX_RESULTS_DEFAULT;
        }
        // max_history_results upper bound: the popup can't show more than
        // ~50 rows, so 100 is generous headroom. Zero is valid (disables
        // history) and is intentionally NOT clamped.
        if self.suggest.max_history_results > 100 {
            tracing::warn!(
                "suggest.max_history_results={} exceeds maximum 100, clamping",
                self.suggest.max_history_results
            );
            self.suggest.max_history_results = 100;
        }
        // suggest.order: drop unknown names, deduplicate, warn on both.
        {
            const VALID_ORDER_NAMES: &[&str] = &[
                "commands",
                "filesystem",
                "history",
                "ai",
                "env",
                "shell",
                "ssh",
            ];
            let mut seen = std::collections::HashSet::new();
            let mut cleaned = Vec::with_capacity(self.suggest.order.len());
            for name in &self.suggest.order {
                if !VALID_ORDER_NAMES.contains(&name.as_str()) {
                    tracing::warn!("suggest.order: unknown source {name:?}, ignoring");
                    continue;
                }
                if !seen.insert(name.as_str()) {
                    tracing::warn!("suggest.order: duplicate source {name:?}, ignoring");
                    continue;
                }
                cleaned.push(name.clone());
            }
            if cleaned.is_empty() {
                tracing::warn!("suggest.order: no valid entries, resetting to default");
                cleaned = SuggestConfig::default().order;
            }
            self.suggest.order = cleaned;
        }
        if self.popup.render_block_ms > RENDER_BLOCK_MS_UPPER {
            tracing::warn!(
                "popup.render_block_ms={} exceeds maximum {}, clamping",
                self.popup.render_block_ms,
                RENDER_BLOCK_MS_UPPER,
            );
            self.popup.render_block_ms = RENDER_BLOCK_MS_UPPER;
        }
        if self.popup.feedback_dismiss_ms > FEEDBACK_DISMISS_MS_UPPER {
            tracing::warn!(
                "popup.feedback_dismiss_ms={} exceeds maximum {}, clamping",
                self.popup.feedback_dismiss_ms,
                FEEDBACK_DISMISS_MS_UPPER,
            );
            self.popup.feedback_dismiss_ms = FEEDBACK_DISMISS_MS_UPPER;
        }
        // Popup width sanity. min_width is clamped first so the max_width
        // clamp can rely on a valid lower bound.
        if self.popup.min_width < POPUP_MIN_WIDTH_FLOOR {
            tracing::warn!(
                "popup.min_width={} below floor {}, clamping",
                self.popup.min_width,
                POPUP_MIN_WIDTH_FLOOR,
            );
            self.popup.min_width = POPUP_MIN_WIDTH_FLOOR;
        }
        if self.popup.min_width > POPUP_MAX_WIDTH_CEILING {
            tracing::warn!(
                "popup.min_width={} exceeds ceiling {}, clamping",
                self.popup.min_width,
                POPUP_MAX_WIDTH_CEILING,
            );
            self.popup.min_width = POPUP_MAX_WIDTH_CEILING;
        }
        if self.popup.max_width > POPUP_MAX_WIDTH_CEILING {
            tracing::warn!(
                "popup.max_width={} exceeds ceiling {}, clamping",
                self.popup.max_width,
                POPUP_MAX_WIDTH_CEILING,
            );
            self.popup.max_width = POPUP_MAX_WIDTH_CEILING;
        }
        if self.popup.max_width < self.popup.min_width {
            tracing::warn!(
                "popup.max_width={} < popup.min_width={}, raising max to min",
                self.popup.max_width,
                self.popup.min_width,
            );
            self.popup.max_width = self.popup.min_width;
        }
        // Description box knobs.
        if self.popup.description_box_max_width < DESC_BOX_MAX_WIDTH_FLOOR {
            tracing::warn!(
                "popup.description_box_max_width={} below floor {}, clamping",
                self.popup.description_box_max_width,
                DESC_BOX_MAX_WIDTH_FLOOR,
            );
            self.popup.description_box_max_width = DESC_BOX_MAX_WIDTH_FLOOR;
        }
        if self.popup.description_box_max_width > DESC_BOX_MAX_WIDTH_CEILING {
            tracing::warn!(
                "popup.description_box_max_width={} exceeds ceiling {}, clamping",
                self.popup.description_box_max_width,
                DESC_BOX_MAX_WIDTH_CEILING,
            );
            self.popup.description_box_max_width = DESC_BOX_MAX_WIDTH_CEILING;
        }
        if self.popup.description_box_lines == 0 {
            tracing::warn!(
                "popup.description_box_lines=0 is invalid (would render an empty box), \
                 clamping to default 5",
            );
            self.popup.description_box_lines = 5;
        }
        if self.popup.description_box_lines > DESC_BOX_LINES_CEILING {
            tracing::warn!(
                "popup.description_box_lines={} exceeds ceiling {}, clamping",
                self.popup.description_box_lines,
                DESC_BOX_LINES_CEILING,
            );
            self.popup.description_box_lines = DESC_BOX_LINES_CEILING;
        }
        if self.popup.description_box_debounce_ms > DESC_BOX_DEBOUNCE_CEILING {
            tracing::warn!(
                "popup.description_box_debounce_ms={} exceeds ceiling {}, clamping",
                self.popup.description_box_debounce_ms,
                DESC_BOX_DEBOUNCE_CEILING,
            );
            self.popup.description_box_debounce_ms = DESC_BOX_DEBOUNCE_CEILING;
        }
        // AI feature sanity.
        fn clamp_feature(
            feature: &str,
            timeout_ms: &mut u64,
            max_tokens: &mut u32,
            max_results: Option<&mut usize>,
        ) {
            if *timeout_ms < 200 {
                tracing::warn!(
                    "ai.{feature}.timeout_ms={} below floor 200, clamping",
                    *timeout_ms
                );
                *timeout_ms = 200;
            }
            if *timeout_ms > 30000 {
                tracing::warn!(
                    "ai.{feature}.timeout_ms={} exceeds ceiling 30000, clamping",
                    *timeout_ms
                );
                *timeout_ms = 30000;
            }
            if *max_tokens < 16 {
                tracing::warn!(
                    "ai.{feature}.max_tokens={} below floor 16, clamping",
                    *max_tokens
                );
                *max_tokens = 16;
            }
            if *max_tokens > 4096 {
                tracing::warn!(
                    "ai.{feature}.max_tokens={} exceeds ceiling 4096, clamping",
                    *max_tokens
                );
                *max_tokens = 4096;
            }
            if let Some(mr) = max_results {
                if *mr == 0 {
                    tracing::warn!("ai.{feature}.max_results=0 is invalid, clamping to 1");
                    *mr = 1;
                }
                if *mr > 10 {
                    tracing::warn!(
                        "ai.{feature}.max_results={} exceeds ceiling 10, clamping",
                        *mr
                    );
                    *mr = 10;
                }
            }
        }
        clamp_feature(
            "completion",
            &mut self.ai.completion.timeout_ms,
            &mut self.ai.completion.max_tokens,
            Some(&mut self.ai.completion.max_results),
        );
        clamp_feature(
            "ask",
            &mut self.ai.ask.timeout_ms,
            &mut self.ai.ask.max_tokens,
            None,
        );
    }

    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => {
                let Some(dir) = config_dir() else {
                    // HOME unset — refuse to load from CWD (could be attacker-controlled).
                    return Ok(Self::default());
                };
                dir.join("config.toml")
            }
        };

        let contents = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "failed to read config file: {}",
                    config_path.display()
                )));
            }
        };

        let mut config: TermcmpConfig = toml::from_str(&contents)
            .with_context(|| format!("failed to parse config file: {}", config_path.display()))?;

        // Two-pass unknown-key detection: re-parse the source as a
        // permissive `toml::Value`, serialize the strictly-typed `TermcmpConfig`
        // back to `toml::Value`, and diff the two trees. Any key present in
        // the loose tree but absent in the typed tree is a typo / removed
        // field / unknown field — warn (not error) so a bad config.toml edit
        // can never take the proxy down.
        if let Ok(loose) = toml::from_str::<toml::Value>(&contents) {
            if let Ok(strict) = toml::Value::try_from(&config) {
                let mut unknown = Vec::new();
                let mut path: Vec<String> = Vec::new();
                diff_unknown_keys(&loose, &strict, &mut path, &mut unknown);
                for key in unknown {
                    tracing::warn!(
                        "unknown config key in {}: {} (typo? removed field?)",
                        config_path.display(),
                        key,
                    );
                }
            }
            // Legacy flat [ai] detection: warn loudly so users know to migrate.
            if let Some(toml::Value::Table(ai)) = loose.get("ai") {
                let legacy_keys = [
                    "enabled",
                    "provider",
                    "model",
                    "timeout_ms",
                    "max_results",
                    "max_tokens",
                    "ask_ai",
                ];
                if legacy_keys.iter().any(|k| ai.contains_key(*k)) {
                    tracing::warn!("ai: flat [ai] config is no longer supported; move settings into [ai.completion] and [ai.ask] — AI disabled until migrated");
                }
            }
        }

        config.normalize();

        Ok(config)
    }
}

/// Walk `loose` (a permissive `toml::Value` parsed from the source file) and
/// `strict` (the same config serialized back from the typed `TermcmpConfig`) in
/// parallel, collecting dotted-path keys that exist only on the loose side.
///
/// Both sides are expected to be `Table`s at the root. Nested tables recurse.
/// Arrays-of-tables recurse element-wise. Leaf / scalar values are ignored —
/// value-level mismatches aren't unknown-key diagnostics.
fn diff_unknown_keys(
    loose: &toml::Value,
    strict: &toml::Value,
    path: &mut Vec<String>,
    out: &mut Vec<String>,
) {
    match (loose, strict) {
        (toml::Value::Table(loose_tbl), toml::Value::Table(strict_tbl)) => {
            for (key, loose_val) in loose_tbl {
                path.push(key.clone());
                match strict_tbl.get(key) {
                    Some(strict_val) => diff_unknown_keys(loose_val, strict_val, path, out),
                    None => out.push(path.join(".")),
                }
                path.pop();
            }
        }
        (toml::Value::Array(loose_arr), toml::Value::Array(strict_arr)) => {
            // Recurse into array-of-tables elements; scalar arrays bottom out
            // because their elements have no inner keys to diff.
            for (idx, loose_item) in loose_arr.iter().enumerate() {
                if let Some(strict_item) = strict_arr.get(idx) {
                    path.push(format!("[{idx}]"));
                    diff_unknown_keys(loose_item, strict_item, path, out);
                    path.pop();
                }
            }
        }
        _ => {
            // Leaves (scalar values) — nothing to diff key-wise.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn all_field_paths_covers_every_section() {
        let paths = all_field_paths();
        let expected_sections = &[
            "trigger.",
            "popup.",
            "suggest.",
            "suggest.providers.",
            "keybindings.",
            "theme.",
            "experimental.",
        ];
        for prefix in expected_sections {
            assert!(
                paths.iter().any(|p| p.starts_with(prefix)),
                "missing section: {}",
                prefix,
            );
        }
    }

    #[test]
    fn all_field_paths_includes_render_block_ms() {
        let paths = all_field_paths();
        assert!(paths.contains(&"popup.render_block_ms"));
    }

    #[test]
    fn test_default_config_matches_hardcoded() {
        let config = TermcmpConfig::default();
        assert_eq!(config.trigger.delay_ms, 150);
        assert!(config.trigger.auto_trigger);
        assert_eq!(config.popup.max_visible, 10);
        assert_eq!(config.popup.feedback_dismiss_ms, 1200);
        assert!(config.popup.spinner);
        assert!(!config.popup.show_provider_errors);
        assert_eq!(config.suggest.max_results, 50);
        assert_eq!(config.suggest.max_history_results, 5);
        assert!(config.suggest.providers.commands);
        assert!(config.suggest.providers.filesystem);
        assert_eq!(config.keybindings.accept, "tab");
        assert_eq!(config.keybindings.accept_and_enter, "enter");
        assert_eq!(config.keybindings.dismiss, "escape");
        assert_eq!(config.keybindings.navigate_up, "arrow_up");
        assert_eq!(config.keybindings.navigate_down, "arrow_down");
        assert_eq!(config.keybindings.trigger, "ctrl+/");
        assert_eq!(config.theme.name, "");
        assert!(!config.experimental.multi_terminal);
    }

    #[test]
    fn test_parse_partial_toml() {
        let toml_str = r#"
[popup]
max_visible = 5
"#;
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.popup.max_visible, 5);
        // Everything else should be default
        assert_eq!(config.suggest.max_results, 50);
    }

    #[test]
    fn match_mode_defaults_to_fuzzy() {
        let config = SuggestConfig::default();
        assert_eq!(config.match_mode, MatchMode::Fuzzy);
    }

    #[test]
    fn match_mode_deserializes_from_toml() {
        let toml = r#"
[suggest]
match_mode = "substring"
"#;
        let parsed: TermcmpConfig = toml::from_str(toml).unwrap();
        assert_eq!(parsed.suggest.match_mode, MatchMode::Substring);
    }

    #[test]
    fn match_mode_round_trips_through_serialization() {
        // The two-pass loader serializes the strict view back to TOML; a
        // non-default match_mode must survive that round-trip without being
        // flagged as an unknown key.
        let config = TermcmpConfig {
            suggest: SuggestConfig {
                match_mode: MatchMode::Substring,
                ..Default::default()
            },
            ..Default::default()
        };
        let serialized = toml::to_string(&config).unwrap();
        assert!(serialized.contains("match_mode = \"substring\""));
        let reparsed: TermcmpConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.suggest.match_mode, MatchMode::Substring);
    }

    #[test]
    fn suggest_order_round_trips_through_serialization() {
        // A custom suggest.order must survive the two-pass loader's
        // serialize-then-reparse without being flagged as an unknown key.
        let toml_str = "[suggest]\norder = [\"history\", \"commands\"]\n";
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.suggest.order,
            vec!["history".to_string(), "commands".to_string()]
        );
        let serialized = toml::to_string(&config).unwrap();
        let reparsed: TermcmpConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.suggest.order, config.suggest.order);
    }

    #[test]
    fn suggest_order_defaults_to_documented_list() {
        assert_eq!(
            TermcmpConfig::default().suggest.order,
            vec![
                "ai".to_string(),
                "history".to_string(),
                "shell".to_string(),
                "filesystem".to_string(),
                "commands".to_string(),
                "env".to_string(),
                "ssh".to_string(),
            ]
        );
    }

    #[test]
    fn match_mode_rejects_unknown_variant() {
        // An invalid match_mode must fail deserialization (an `unknown variant`
        // serde error) rather than being silently defaulted — this pins the
        // rejection contract so a future `#[serde(other)]` or variant rename
        // can't quietly swallow a typo and take the proxy down on load.
        let toml = r#"
[suggest]
match_mode = "contiguous"
"#;
        let result = toml::from_str::<TermcmpConfig>(toml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("unknown variant"),
            "expected an `unknown variant` error, got: {err}"
        );
        assert!(
            err.contains("fuzzy") && err.contains("substring"),
            "error should list the valid variants, got: {err}"
        );
    }

    #[test]
    fn test_missing_file_returns_default() {
        let config = TermcmpConfig::load(Some(Path::new("/nonexistent/path/config.toml"))).unwrap();
        assert_eq!(config.popup.max_visible, 10);
    }

    #[test]
    fn test_malformed_toml_returns_error() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "this is not [valid toml = {{}}").unwrap();
        let result = TermcmpConfig::load(Some(tmp.path()));
        assert!(result.is_err());
    }

    #[test]
    fn test_partial_keybindings_override() {
        let toml_str = r#"
[keybindings]
accept = "enter"
navigate_up = "ctrl+space"
"#;
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.keybindings.accept, "enter");
        assert_eq!(config.keybindings.navigate_up, "ctrl+space");
        // Unset fields keep defaults
        assert_eq!(config.keybindings.accept_and_enter, "enter");
        assert_eq!(config.keybindings.dismiss, "escape");
        assert_eq!(config.keybindings.navigate_down, "arrow_down");
        assert_eq!(config.keybindings.trigger, "ctrl+/");
        assert_eq!(config.keybindings.toggle_match_mode, "ctrl+r");
    }

    #[test]
    fn toggle_match_mode_defaults_to_ctrl_r() {
        assert_eq!(KeybindingsConfig::default().toggle_match_mode, "ctrl+r");
    }

    #[test]
    fn keybindings_toggle_match_mode_parses() {
        let toml_str = r#"
[keybindings]
toggle_match_mode = "ctrl+space"
"#;
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.keybindings.toggle_match_mode, "ctrl+space");
    }

    #[test]
    fn popup_index_hints_defaults_true() {
        assert!(PopupConfig::default().index_hints);
    }

    #[test]
    fn popup_key_hints_defaults_true() {
        assert!(PopupConfig::default().key_hints);
    }

    #[test]
    fn popup_hints_parse_from_toml() {
        let toml_str = r#"
[popup]
index_hints = false
key_hints = false
"#;
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.popup.index_hints);
        assert!(!config.popup.key_hints);
    }

    #[test]
    fn test_full_config_parses() {
        let toml_str = r#"
[trigger]
delay_ms = 200

[popup]
max_visible = 15

[suggest]
max_results = 100
max_history_results = 3

[suggest.providers]
commands = true
filesystem = true

[keybindings]
accept = "enter"
accept_and_enter = "tab"
dismiss = "escape"
navigate_up = "arrow_up"
navigate_down = "arrow_down"
trigger = "ctrl+space"
"#;
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.trigger.delay_ms, 200);
        assert_eq!(config.popup.max_visible, 15);
        assert_eq!(config.suggest.max_results, 100);
        assert_eq!(config.suggest.max_history_results, 3);
        assert!(config.suggest.providers.commands);
        assert_eq!(config.keybindings.accept, "enter");
        assert_eq!(config.keybindings.accept_and_enter, "tab");
    }

    #[test]
    fn test_resolve_no_preset_uses_dark() {
        let config = ThemeConfig::default();
        let resolved = config.resolve(None).unwrap();
        assert_eq!(resolved.selected, "reverse");
        assert_eq!(resolved.description, "dim");
        assert_eq!(resolved.match_highlight, "bold");
        assert_eq!(resolved.item_text, "");
        assert_eq!(resolved.scrollbar, "dim");
        assert_eq!(resolved.border, "dim");
        assert_eq!(resolved.feedback_loading, "dim");
        assert_eq!(resolved.feedback_empty, "dim");
        assert_eq!(resolved.feedback_error, "dim fg:196");
        assert_eq!(resolved.kind_icon, "dim");
    }

    #[test]
    fn test_all_builtin_themes_have_required_fields() {
        let builtins = [
            "dark",
            "light",
            "catppuccin",
            "material-darker",
            "gruvbox",
            "nord",
            "dracula",
            "tokyo-night",
        ];
        for name in builtins {
            let config = ThemeConfig {
                name: name.into(),
                ..Default::default()
            };
            let resolved = config
                .resolve(None)
                .unwrap_or_else(|e| panic!("theme {name:?} must resolve: {e}"));

            // Fields that must be non-empty in every theme.
            // (item_text, background, description_box_background are
            // legitimately empty in dark — not checked here.)
            let required: &[(&str, &str)] = &[
                ("selected", &resolved.selected),
                ("description", &resolved.description),
                ("match_highlight", &resolved.match_highlight),
                ("scrollbar", &resolved.scrollbar),
                ("border", &resolved.border),
                ("feedback_loading", &resolved.feedback_loading),
                ("feedback_empty", &resolved.feedback_empty),
                ("feedback_error", &resolved.feedback_error),
                ("kind_icon", &resolved.kind_icon),
            ];
            for (field, value) in required {
                assert!(
                    !value.is_empty(),
                    "theme {name:?}: {field} must not be empty"
                );
            }

            // Invariant: feedback styles mirror description.
            assert_eq!(
                resolved.feedback_loading, resolved.description,
                "theme {name:?}: feedback_loading must equal description"
            );
            assert_eq!(
                resolved.feedback_empty, resolved.description,
                "theme {name:?}: feedback_empty must equal description"
            );
        }
    }

    #[test]
    fn test_resolve_invalid_preset_errors() {
        let config = ThemeConfig {
            name: "nonexistent".into(),
            ..Default::default()
        };
        assert!(config.resolve(None).is_err());
    }

    #[test]
    fn test_transparency_clears_backgrounds() {
        let config = ThemeConfig {
            name: "catppuccin".into(),
            transparency: true,
        };
        let resolved = config.resolve(None).unwrap();
        assert_eq!(resolved.background, "");
        assert_eq!(resolved.description_box_background, "");
        // Non-background fields are untouched
        assert_eq!(resolved.selected, "fg:#cdd6f4 bg:#45475a bold");
    }

    #[test]
    fn test_resolve_user_theme_file_overrides_builtin_dark() {
        let dir = tempfile::TempDir::new().unwrap();
        let themes = dir.path().join("themes");
        std::fs::create_dir_all(&themes).unwrap();
        std::fs::write(
            themes.join("custom.toml"),
            "selected = \"bold fg:#ff0000\"\ndescription = \"dim\"\n",
        )
        .unwrap();

        let config = ThemeConfig {
            name: "custom".into(),
            ..Default::default()
        };
        let resolved = config.resolve(Some(dir.path())).unwrap();
        // Overridden fields come from the user file
        assert_eq!(resolved.selected, "bold fg:#ff0000");
        assert_eq!(resolved.description, "dim");
        // Omitted fields fall back to built-in dark
        assert_eq!(resolved.match_highlight, "bold");
        assert_eq!(resolved.scrollbar, "dim");
        assert_eq!(resolved.feedback_error, "dim fg:196");
        assert_eq!(resolved.kind_icon, "dim");
    }

    #[test]
    fn test_legacy_providers_history_field_ignored() {
        let toml_str = r#"
[suggest.providers]
history = false
"#;
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        // Field is silently ignored; max_history_results keeps its default
        assert_eq!(config.suggest.max_history_results, 5);
    }

    #[test]
    fn test_popup_width_fields_parse() {
        let toml_str = r#"
[popup]
max_visible = 10
min_width = 25
max_width = 80
"#;
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.popup.max_visible, 10);
        assert_eq!(config.popup.min_width, 25);
        assert_eq!(config.popup.max_width, 80);
    }

    #[test]
    fn test_popup_width_defaults() {
        let cfg = PopupConfig::default();
        assert_eq!(cfg.min_width, 40);
        assert_eq!(cfg.max_width, 60);
    }

    #[test]
    fn test_popup_tab_accepts_top_defaults_false() {
        assert!(
            !PopupConfig::default().tab_accepts_top,
            "tab_accepts_top must default off to preserve historical behavior"
        );
    }

    #[test]
    fn ai_ask_defaults_false() {
        assert!(!AiConfig::default().ask.enabled);
    }

    #[test]
    fn ai_ask_parse() {
        let config: TermcmpConfig = toml::from_str("[ai.ask]\nenabled = true").unwrap();
        assert!(config.ai.ask.enabled);
    }

    #[test]
    fn ai_completion_full_parse() {
        let config: TermcmpConfig = toml::from_str(
            "[ai.completion]\nenabled = true\nprovider = \"p\"\nmodel = \"m\"\ntimeout_ms = 5000\nmax_results = 5\nmax_tokens = 512\nthinking = \"on\""
        ).unwrap();
        let c = &config.ai.completion;
        assert!(c.enabled);
        assert_eq!(c.provider, "p");
        assert_eq!(c.model, "m");
        assert_eq!(c.timeout_ms, 5000);
        assert_eq!(c.max_results, 5);
        assert_eq!(c.max_tokens, 512);
        assert_eq!(c.thinking, AiThinking::On);
    }

    #[test]
    fn ai_thinking_defaults_off() {
        assert_eq!(AiCompletionConfig::default().thinking, AiThinking::Off);
        assert_eq!(AiAskConfig::default().thinking, AiThinking::Off);
    }

    #[test]
    fn ai_thinking_auto_parses() {
        let config: TermcmpConfig = toml::from_str("[ai.completion]\nthinking = \"auto\"").unwrap();
        assert_eq!(config.ai.completion.thinking, AiThinking::Auto);
    }

    #[test]
    fn ai_normalize_clamps_ask_timeout() {
        let mut config = TermcmpConfig::default();
        config.ai.ask.timeout_ms = 99999;
        config.normalize();
        assert_eq!(config.ai.ask.timeout_ms, 30000);
    }

    #[test]
    fn ai_legacy_flat_config_leaves_completion_disabled() {
        // Legacy flat [ai] enabled=true must NOT enable completion (clean cutover).
        let config: TermcmpConfig = toml::from_str("[ai]\nenabled = true").unwrap();
        assert!(!config.ai.completion.enabled);
    }

    #[test]
    fn ai_features_independent_config() {
        let config: TermcmpConfig = toml::from_str(
            "[ai.completion]\nprovider = \"a\"\ntimeout_ms = 1000\nmax_tokens = 128\n[ai.ask]\nprovider = \"b\"\ntimeout_ms = 20000\nmax_tokens = 1024\nthinking = \"on\""
        ).unwrap();
        assert_eq!(config.ai.completion.provider, "a");
        assert_eq!(config.ai.completion.timeout_ms, 1000);
        assert_eq!(config.ai.completion.max_tokens, 128);
        assert_eq!(config.ai.ask.provider, "b");
        assert_eq!(config.ai.ask.timeout_ms, 20000);
        assert_eq!(config.ai.ask.max_tokens, 1024);
        assert_eq!(config.ai.ask.thinking, AiThinking::On);
    }

    #[test]
    fn test_popup_border_radius_defaults_true() {
        assert!(
            PopupConfig::default().border_radius,
            "border_radius must default to true (rounded corners)"
        );
    }

    #[test]
    fn test_popup_tab_accepts_top_parses() {
        let toml_str = "[popup]\ntab_accepts_top = true\n";
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        assert!(config.popup.tab_accepts_top);
    }

    #[test]
    fn test_popup_max_width_clamps_ceiling() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\nmax_width = 1000").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.max_width, 500);
    }

    #[test]
    fn test_popup_max_width_above_u16_still_clamps_ceiling() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\nmax_width = 100000").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.max_width, 500);
    }

    #[test]
    fn test_popup_min_width_clamps_floor() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\nmin_width = 1").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.min_width, 10);
    }

    #[test]
    fn test_popup_min_width_clamps_ceiling() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\nmin_width = 600").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.min_width, 500);
        assert_eq!(config.popup.max_width, 500);
    }

    #[test]
    fn test_popup_min_width_above_u16_still_clamps_ceiling() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\nmin_width = 100000").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.min_width, 500);
        assert_eq!(config.popup.max_width, 500);
    }

    #[test]
    fn test_popup_max_below_min_raised_to_min() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\nmin_width = 50\nmax_width = 30").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.min_width, 50);
        assert_eq!(config.popup.max_width, 50);
    }

    #[test]
    fn test_description_box_defaults_off() {
        let cfg = PopupConfig::default();
        assert_eq!(cfg.description_box, DescriptionBoxMode::Off);
        assert_eq!(cfg.description_box_max_width, 60);
        assert_eq!(cfg.description_box_lines, 5);
        assert_eq!(cfg.description_box_debounce_ms, 80);
    }

    #[test]
    fn test_description_box_mode_parses_lowercase() {
        let toml_str = r#"
[popup]
description_box = "side"
"#;
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.popup.description_box, DescriptionBoxMode::Side);
    }

    #[test]
    fn test_description_box_lines_zero_clamps_to_default() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\ndescription_box_lines = 0").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.description_box_lines, 5);
    }

    #[test]
    fn test_description_box_lines_clamps_ceiling() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\ndescription_box_lines = 999").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.description_box_lines, 20);
    }

    #[test]
    fn test_description_box_lines_above_u16_still_clamps_ceiling() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\ndescription_box_lines = 100000").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.description_box_lines, 20);
    }

    #[test]
    fn test_description_box_max_width_clamps_floor() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\ndescription_box_max_width = 5").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.description_box_max_width, 20);
    }

    #[test]
    fn test_description_box_max_width_zero_clamps_to_floor() {
        // Pin the documented contract: 0 must clamp up to DESC_BOX_MAX_WIDTH_FLOOR (20),
        // not pass through. Guards against a regression that swapped `<` for
        // `> 0 && <`, which would let zero leak through and render a degenerate box.
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\ndescription_box_max_width = 0").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.description_box_max_width, 20);
    }

    #[test]
    fn test_description_box_max_width_clamps_ceiling() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\ndescription_box_max_width = 9999").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.description_box_max_width, 200);
    }

    #[test]
    fn test_description_box_max_width_above_u16_still_clamps_ceiling() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\ndescription_box_max_width = 100000").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.description_box_max_width, 200);
    }

    #[test]
    fn test_description_box_debounce_ms_clamps_ceiling() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\ndescription_box_debounce_ms = 9999").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.description_box_debounce_ms, 500);
    }

    #[test]
    fn test_description_box_debounce_ms_above_u16_still_clamps_ceiling() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\ndescription_box_debounce_ms = 100000").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.description_box_debounce_ms, 500);
    }

    #[test]
    fn test_popup_negative_min_width_rejected() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\nmin_width = -10").unwrap();
        let result = TermcmpConfig::load(Some(tmp.path()));
        let err = result.expect_err("negative min_width must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nonnegative integer"),
            "error must mention the expected shape, got: {msg}",
        );
    }

    #[test]
    fn test_popup_negative_description_box_lines_rejected() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\ndescription_box_lines = -1").unwrap();
        let result = TermcmpConfig::load(Some(tmp.path()));
        let err = result.expect_err("negative description_box_lines must be rejected");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nonnegative integer"),
            "error must mention the expected shape, got: {msg}",
        );
    }

    #[test]
    fn test_removed_suggest_fields_ignored() {
        // `max_history_entries` was renamed — parsing should succeed and
        // leave the replacement field at its default.
        let toml_str = r#"
[suggest]
max_results = 50
max_history_entries = 5000
"#;
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.suggest.max_results, 50);
    }

    #[test]
    fn normalize_clamps_max_history_results_above_max() {
        let mut cfg = TermcmpConfig::default();
        cfg.suggest.max_history_results = 999;
        cfg.normalize();
        assert_eq!(cfg.suggest.max_history_results, 100);
    }

    #[test]
    fn normalize_keeps_max_history_results_zero() {
        // Zero legitimately disables history results — must NOT be clamped.
        let mut cfg = TermcmpConfig::default();
        cfg.suggest.max_history_results = 0;
        cfg.normalize();
        assert_eq!(cfg.suggest.max_history_results, 0);
    }

    #[test]
    fn test_experimental_defaults_to_off() {
        let config = TermcmpConfig::default();
        assert!(!config.experimental.multi_terminal);
    }

    #[test]
    fn test_experimental_multi_terminal_enabled() {
        let toml_str = r#"
[experimental]
multi_terminal = true
"#;
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        assert!(config.experimental.multi_terminal);
    }

    #[test]
    fn test_experimental_missing_uses_default() {
        let toml_str = r#"
[popup]
max_visible = 5
"#;
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.experimental.multi_terminal);
    }

    #[test]
    fn test_clamp_max_visible_over_limit() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\nmax_visible = 100000").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.max_visible, MAX_VISIBLE_UPPER);
    }

    #[test]
    fn test_clamp_max_results_over_limit() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[suggest]\nmax_results = 999999").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.suggest.max_results, MAX_RESULTS_UPPER);
    }

    #[test]
    fn test_no_clamp_when_within_bounds() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmp,
            "[popup]\nmax_visible = 25\n[suggest]\nmax_results = 500"
        )
        .unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.max_visible, 25);
        assert_eq!(config.suggest.max_results, 500);
    }

    #[test]
    fn test_clamp_at_exact_boundary() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmp,
            "[popup]\nmax_visible = 50\n[suggest]\nmax_results = 10000"
        )
        .unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.max_visible, 50);
        assert_eq!(config.suggest.max_results, 10000);
    }

    #[test]
    fn test_clamp_max_results_zero_to_default() {
        // max_results=0 is a footgun — it would truncate every ranked result
        // set to empty. Clamp to the default instead of rendering a
        // permanently blank popup.
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[suggest]\nmax_results = 0").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.suggest.max_results, MAX_RESULTS_DEFAULT);
    }

    #[test]
    fn test_clamp_max_visible_zero_to_default() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\nmax_visible = 0").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.max_visible, 10);
    }

    #[test]
    fn test_popup_feedback_knobs_parse_and_clamp() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmp,
            "[popup]\nfeedback_dismiss_ms = 20000\nspinner = false\nshow_provider_errors = true"
        )
        .unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.feedback_dismiss_ms, 10000);
        assert!(!config.popup.spinner);
        assert!(config.popup.show_provider_errors);
    }

    #[test]
    fn test_delay_ms_zero_is_allowed() {
        // delay_ms=0 disables the typing-pause debounce — still a valid
        // choice, so it must pass through untouched.
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[trigger]\ndelay_ms = 0").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.trigger.delay_ms, 0);
    }

    #[test]
    fn test_feedback_dismiss_ms_zero_is_allowed() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "[popup]\nfeedback_dismiss_ms = 0").unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        assert_eq!(config.popup.feedback_dismiss_ms, 0);
    }

    #[test]
    fn test_diff_unknown_keys_flat_top_level() {
        let loose: toml::Value = toml::from_str("known = 1\nbogus = 2").unwrap();
        let strict: toml::Value = toml::from_str("known = 1").unwrap();
        let mut out = Vec::new();
        let mut path = Vec::new();
        diff_unknown_keys(&loose, &strict, &mut path, &mut out);
        assert_eq!(out, vec!["bogus".to_string()]);
    }

    #[test]
    fn test_diff_unknown_keys_nested_table() {
        let loose: toml::Value = toml::from_str(
            r#"
[suggest]
max_results = 50
typo_field = 42

[suggest.providers]
git = true
"#,
        )
        .unwrap();
        let strict: toml::Value = toml::from_str(
            r#"
[suggest]
max_results = 50

[suggest.providers]
git = true
"#,
        )
        .unwrap();
        let mut out = Vec::new();
        let mut path = Vec::new();
        diff_unknown_keys(&loose, &strict, &mut path, &mut out);
        assert_eq!(out, vec!["suggest.typo_field".to_string()]);
    }

    #[test]
    fn test_diff_unknown_keys_deep_nested() {
        let loose: toml::Value = toml::from_str(
            r#"
[suggest.providers]
commands = true
unknown_provider = false
"#,
        )
        .unwrap();
        let strict: toml::Value = toml::from_str(
            r#"
[suggest.providers]
commands = true
"#,
        )
        .unwrap();
        let mut out = Vec::new();
        let mut path = Vec::new();
        diff_unknown_keys(&loose, &strict, &mut path, &mut out);
        assert_eq!(out, vec!["suggest.providers.unknown_provider".to_string()]);
    }

    #[test]
    fn test_diff_unknown_keys_all_known() {
        let loose: toml::Value = toml::from_str(
            r#"
[suggest]
max_results = 100
max_history_results = 10
"#,
        )
        .unwrap();
        let strict = loose.clone();
        let mut out = Vec::new();
        let mut path = Vec::new();
        diff_unknown_keys(&loose, &strict, &mut path, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn test_load_with_unknown_key_succeeds() {
        // The two-pass load warns on unknown keys but must still succeed —
        // a typo in config.toml should never take the proxy down.
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmp,
            "[trigger]\ndelay_ms = 200\ndelay_ms_typo = 999\n\n[suggest]\nmax_results = 75"
        )
        .unwrap();
        let config = TermcmpConfig::load(Some(tmp.path())).unwrap();
        // Known fields still applied correctly.
        assert_eq!(config.trigger.delay_ms, 200);
        assert_eq!(config.suggest.max_results, 75);
    }

    #[test]
    fn test_missing_file_returns_default_via_notfound() {
        // Verifies the TOCTOU-safe path: read_to_string NotFound → default
        let config =
            TermcmpConfig::load(Some(Path::new("/tmp/definitely_not_a_real_config_42.toml")))
                .unwrap();
        assert_eq!(config.popup.max_visible, 10);
        assert_eq!(config.suggest.max_results, 50);
    }

    #[test]
    fn test_config_dir_returns_none_yields_default() {
        // Simulate the load() code path when config_dir() returns None:
        // it must return Self::default(), NOT load from CWD.
        let result: Option<PathBuf> = None;
        let config = match result {
            Some(dir) => {
                let path = dir.join("config.toml");
                if path.exists() {
                    toml::from_str::<TermcmpConfig>(&std::fs::read_to_string(&path).unwrap())
                        .unwrap()
                } else {
                    TermcmpConfig::default()
                }
            }
            None => TermcmpConfig::default(),
        };
        // Should be identical to defaults — never loaded from CWD
        assert_eq!(config.popup.max_visible, 10);
        assert_eq!(config.trigger.delay_ms, 150);
        assert_eq!(config.suggest.max_results, 50);
    }

    #[test]
    fn test_auto_trigger_defaults_to_true() {
        let config = TermcmpConfig::default();
        assert!(config.trigger.auto_trigger);
    }

    #[test]
    fn test_auto_trigger_false_from_toml() {
        let toml_str = r#"
[trigger]
auto_trigger = false
"#;
        let config: TermcmpConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.trigger.auto_trigger);
        // Other trigger defaults preserved
        assert_eq!(config.trigger.delay_ms, 150);
    }

    #[test]
    fn render_block_ms_default_is_80() {
        let cfg = PopupConfig::default();
        assert_eq!(cfg.render_block_ms, 80);
    }

    #[test]
    fn render_block_ms_clamps_above_300_during_normalize() {
        let mut cfg = TermcmpConfig::default();
        cfg.popup.render_block_ms = 500;
        cfg.normalize();
        assert_eq!(cfg.popup.render_block_ms, 300);
    }

    #[test]
    fn render_block_ms_zero_is_allowed() {
        let mut cfg = TermcmpConfig::default();
        cfg.popup.render_block_ms = 0;
        cfg.normalize();
        assert_eq!(cfg.popup.render_block_ms, 0);
    }

    #[test]
    fn normalize_drops_unknown_order_names() {
        let mut cfg = TermcmpConfig::default();
        cfg.suggest.order = vec!["commands".into(), "bogus".into()];
        cfg.normalize();
        assert_eq!(cfg.suggest.order, vec!["commands".to_string()]);
    }

    #[test]
    fn normalize_deduplicates_order() {
        let mut cfg = TermcmpConfig::default();
        cfg.suggest.order = vec!["commands".into(), "commands".into(), "ai".into()];
        cfg.normalize();
        assert_eq!(
            cfg.suggest.order,
            vec!["commands".to_string(), "ai".to_string()]
        );
    }

    #[test]
    fn normalize_resets_empty_order_to_default() {
        let mut cfg = TermcmpConfig::default();
        cfg.suggest.order = vec!["bogus".into()];
        cfg.normalize();
        assert_eq!(cfg.suggest.order, SuggestConfig::default().order);
    }
}

#[cfg(test)]
mod docs_drift_tests {
    use super::all_field_paths;

    /// Read docs/CONFIGURATION.md via the workspace root computed from
    /// CARGO_MANIFEST_DIR (config is at `<root>/crates/config`,
    /// so we go up two levels).
    fn configuration_md() -> String {
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let root = manifest
            .parent()
            .expect("crates/")
            .parent()
            .expect("repo root");
        let path = root.join("docs/CONFIGURATION.md");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e))
    }

    /// Decide whether a single field key is "documented" in CONFIGURATION.md.
    ///
    /// Accept any of three forms — all are how the docs reference fields:
    ///   1. Markdown code span:        `key`
    ///   2. TOML assignment with ws:   key = ...
    ///   3. TOML assignment no ws:     key=...
    ///
    /// Bare prose matches don't count — many leaf keys (`background`,
    /// `description`, `accept`) are common English and would false-positive
    /// against unrelated docs text.
    fn is_documented(doc: &str, key: &str) -> bool {
        let backtick = format!("`{key}`");
        let toml_eq_ws = format!("{key} =");
        let toml_eq_tight = format!("{key}=");
        doc.contains(&backtick) || doc.contains(&toml_eq_ws) || doc.contains(&toml_eq_tight)
    }

    /// Every schema field must be referenced in CONFIGURATION.md. This is
    /// the actual drift guard — a section-only check would silently allow
    /// a field to be removed from the docs as long as the section header
    /// stayed.
    #[test]
    fn configuration_md_lists_every_field() {
        let doc = configuration_md();
        let mut missing: Vec<&str> = Vec::new();
        for path in all_field_paths() {
            let (_section, key) = path.rsplit_once('.').expect("dotted path");
            if !is_documented(&doc, key) {
                missing.push(path);
            }
        }
        assert!(
            missing.is_empty(),
            "CONFIGURATION.md is missing these schema fields: {:#?}",
            missing,
        );
    }

    /// Section headers are a cheaper smoke check — useful if the field
    /// test fails wholesale (entire section gone) to surface a clearer
    /// failure first.
    #[test]
    fn configuration_md_mentions_every_section() {
        let doc = configuration_md();
        let sections = [
            "[trigger]",
            "[popup]",
            "[suggest]",
            "[suggest.providers]",
            "[keybindings]",
            "[theme]",
            "[ai]",
            "[experimental]",
        ];
        for s in sections {
            assert!(doc.contains(s), "CONFIGURATION.md missing section {}", s);
        }
    }
}
