# Configuration Reference

Termcmp reads its configuration from `~/.config/termcmp/config.toml`. All fields are optional — unset values use their defaults.

Run `termcmp install` to generate a default config with all fields documented as comments.

## Sections

### `[trigger]`

Controls when the autocomplete popup appears.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `delay_ms` | integer | `150` | Milliseconds to wait after typing pauses before showing suggestions. Set to `0` to disable debounce (trigger immediately). |
| `auto_trigger` | boolean | `true` | When `false`, disables all automatic popup triggers. Only the manual keybinding opens the popup. |

```toml
[trigger]
delay_ms = 150
auto_trigger = true
```

### `[popup]`

Controls the popup appearance.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_visible` | integer | `10` | Maximum number of suggestions shown at once |
| `borders` | bool | `false` | Draw a border around the popup |
| `border_radius` | bool | `true` | When `true`, popup and description-box borders use rounded corners (`╭╮╰╯`). Set `false` for square corners (`┌┐└┘`). |
| `feedback_dismiss_ms` | integer | `1200` | Milliseconds to keep Empty/Error feedback visible. Set to `0` to disable auto-dismiss. Values above `10000` are clamped. |
| `spinner` | bool | `true` | Animate Loading feedback when the popup is wide enough |
| `show_provider_errors` | bool | `false` | Show provider names in error feedback. Disabled by default for shared-screen privacy. |
| `render_block_ms` | integer | `80` | Maximum time in milliseconds to block before painting sync results while waiting for the first high-priority async generator. Set to `0` to paint immediately. Clamped to `[0, 300]`. |
| `min_width` | integer | `40` | Lower bound for popup width in display columns. Clamped to `[10, 500]`. If `max_width` is lower after normalization, `max_width` is raised to `min_width`. |
| `max_width` | integer | `60` | Upper bound for popup width in display columns. Clamped to `[min_width, 500]` and additionally to the live `screen_cols` at render time. Bump this on wide terminals to give descriptions more room before the truncation ellipsis (`…`) kicks in. |
| `description_box` | string | `"off"` | Adjacent description box mode. `"off"` keeps the legacy inline-truncated behavior. `"side"` renders a wrapped multi-line box next to the main popup for the selected suggestion when the inline description would be hidden or truncated. The box is capped by `description_box_lines` and available rows; short descriptions that already fit don't trigger it. Falls back to a stacked-below box when there's no horizontal room, and to inline truncation when neither fits. |
| `description_box_max_width` | integer | `60` | Maximum width (display columns) for the description box. Clamped to `[20, 200]`. The actual rendered width adapts to the columns remaining beside the main popup. |
| `description_box_lines` | integer | `5` | Maximum wrapped lines in the description box. Long descriptions are hard-truncated with an ellipsis on the final line. `0` resets to default `5`; values above `20` are clamped to `20`. |
| `description_box_debounce_ms` | integer | `80` | Debounce window (ms) for description-box updates on selection change. Holding arrow keys causes the box to update at most once per window, avoiding flicker. Set to `0` to disable debounce. Clamped to `[0, 500]`. |
| `tab_accepts_top` | bool | `false` | When `true`, the accept key (Tab) accepts the top-ranked suggestion even when you haven't navigated yet, instead of forwarding a literal tab to the shell. Restores the Fig/Kiro "type, glance, Tab" flow without the extra arrow-key press. Only the `accept` action is affected: with the default bindings the `accept_and_enter` action (Enter) is a separate binding and still runs the command line, so a stray Enter never silently accepts the top suggestion. (If you rebind the `accept` action itself onto Enter, Enter becomes the accept key and will accept the top item.) |
| `index_hints` | bool | `true` | Show `selected/total` index in the popup header row. |
| `key_hints` | bool | `true` | Show keybinding hints in the popup footer row. |
| `nerd_icons` | bool | `true` | Use Nerd Font glyphs for kind icons in the popup gutter. When `false`, plain ASCII fallbacks are used. |

```toml
[popup]
max_visible = 10
borders = false
border_radius = true
feedback_dismiss_ms = 1200
spinner = true
show_provider_errors = false
render_block_ms = 80
min_width = 40
max_width = 60
description_box = "off"
description_box_max_width = 60
description_box_lines = 5
description_box_debounce_ms = 80
tab_accepts_top = false
index_hints = true
key_hints = true
nerd_icons = true
```

Popup width is content-driven (sized to the longest visible suggestion) and clamped to `[min_width, max_width]`. Descriptions that don't fit are truncated with a single-column ellipsis (`…`). Set `description_box = "side"` to surface a wrapped description when the inline description would be hidden or truncated. The box is capped by `description_box_lines` and available rows without permanently widening the main popup.

### `[suggest]`

Controls the suggestion engine behavior.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max_results` | integer | `50` | Maximum total candidates to consider |
| `max_history_results` | integer | `5` | Maximum history entries shown in popup. Set to `0` to disable history. |
| `match_mode` | string | `"fuzzy"` | How the typed query filters candidates. `"fuzzy"` matches characters as an in-order subsequence (`gco` → `git checkout`); `"substring"` requires the characters to appear contiguously (`cl` → `clone`, `include`, but not `calendar`). Space-separated words are matched as independent substrings. |
| `order` | string array | `["ai", "history", "shell", "filesystem", "commands", "env", "ssh"]` | Source-group ordering for the popup. All items from an earlier-listed source appear before all items from a later one; within a group, items sort by score, then priority, then text. Recognised names: `commands`, `filesystem`, `history`, `ai`, `env`, `shell`, `ssh`. Unknown names are dropped with a warning. |

```toml
[suggest]
max_results = 50
max_history_results = 5
match_mode = "fuzzy"  # or "substring" for contiguous matching
order = ["ai", "history", "shell", "filesystem", "commands", "env", "ssh"]
```

Shell history loads up to 10,000 entries.

### `[suggest.providers]`

Enable or disable individual suggestion providers.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `commands` | bool | `true` | `$PATH` command completions |
| `filesystem` | bool | `true` | File and directory completions |
| `shell_completions` | bool | `true` | Enable fish/zsh shell-native completion providers. When running under fish or zsh, the shell's own completion engine is queried asynchronously for additional suggestions. |

```toml
[suggest.providers]
commands = true
filesystem = true
shell_completions = true
```

### `[keybindings]`

Customize keyboard shortcuts. Each value is a key name string. Invalid key names cause a startup error (fail-fast).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `accept` | string | `"tab"` | Accept the selected suggestion |
| `accept_and_enter` | string | `"enter"` | Accept and execute |
| `dismiss` | string | `"escape"` | Dismiss the popup |
| `navigate_up` | string | `"arrow_up"` | Move selection up |
| `navigate_down` | string | `"arrow_down"` | Move selection down |
| `trigger` | string | `"ctrl+/"` | Manually trigger completions |
| `toggle_match_mode` | string | `"ctrl+r"` | Toggle match mode between fuzzy and substring at runtime. |

```toml
[keybindings]
accept = "tab"
accept_and_enter = "enter"
dismiss = "escape"
navigate_up = "arrow_up"
navigate_down = "arrow_down"
trigger = "ctrl+/"
toggle_match_mode = "ctrl+r"
```

#### Key Name Syntax

- Lowercase letters: `a` through `z`
- Special keys: `tab`, `enter`, `escape`, `backspace`, `space`
- Arrow keys: `arrow_up`, `arrow_down`, `arrow_left`, `arrow_right`
- Modifiers: `ctrl+<key>` (e.g., `ctrl+space`, `ctrl+/`)

### `[theme]`

Select the popup color scheme. All styling lives in theme files — built-in themes ship with termcmp, and custom themes are TOML files in your config directory. Changes are applied live when config hot-reload is active.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | `"dark"` | Theme name: built-in (`dark`, `light`, `catppuccin`, `material-darker`, `gruvbox`, `nord`, `dracula`, `tokyo-night`) or a custom `themes/<name>.toml` file in the config directory. |
| `transparency` | bool | `false` | Clear popup and description-box backgrounds so the terminal's own background (including transparency/blur compositors) shows through. |

```toml
[theme]
name = "catppuccin"
```

#### Built-in Themes

| Theme | Selected | Description | Match Highlight | Item Text | Kind Icon | Scrollbar | Border | Feedback Error |
|-------|----------|-------------|-----------------|-----------|-----------|-----------|--------|----------------|
| `dark` | `reverse` | `dim` | `bold` | *(none)* | `dim` | `dim` | `dim` | `dim fg:196` |
| `light` | `fg:#1f2328 bg:#e7edf3 bold` | `fg:#6e7781` | `fg:#cf222e bold` | `fg:#24292f` | `fg:#d0d7de` | `fg:#8c959f` | `fg:#d0d7de` | `fg:#cf222e bold` |
| `catppuccin` | `fg:#cdd6f4 bg:#585b70 bold` | `fg:#6c7086` | `fg:#f9e2af bold` | *(none)* | `fg:#a6e3a1` | `fg:#585b70` | `fg:#585b70` | `dim fg:#f38ba8` |
| `material-darker` | `fg:#eeffff bg:#424242 bold` | `fg:#616161` | `fg:#ffcb6b bold` | *(none)* | `fg:#c3e88d` | `fg:#616161` | `fg:#616161` | `dim fg:#ff5370` |
| `gruvbox` | `fg:#ebdbb2 bg:#504945 bold` | `fg:#928374` | `fg:#fabd2f bold` | *(none)* | `fg:#b8bb26` | `fg:#504945` | `fg:#504945` | `dim fg:#fb4934` |
| `nord` | `fg:#d8dee9 bg:#4c566a bold` | `fg:#4c566a` | `fg:#ebcb8b bold` | *(none)* | `fg:#a3be8c` | `fg:#4c566a` | `fg:#4c566a` | `dim fg:#bf616a` |
| `dracula` | `fg:#f8f8f2 bg:#44475a bold` | `fg:#6272a4` | `fg:#f1fa8c bold` | *(none)* | `fg:#50fa7b` | `fg:#44475a` | `fg:#44475a` | `dim fg:#ff5555` |
| `tokyo-night` | `fg:#c0caf5 bg:#283457 bold` | `fg:#565f89` | `fg:#e0af68 bold` | *(none)* | `fg:#9ece6a` | `fg:#3b4261` | `fg:#3b4261` | `dim fg:#f7768e` |

All built-in themes except `light` leave `item_text` unstyled (default terminal foreground). `feedback_loading` and `feedback_empty` inherit `description` by default.

#### Custom Themes

Theme files are the ONLY way to customize styles. Place a TOML file at `~/.config/termcmp/themes/<name>.toml` with any of these keys: `selected`, `description`, `match_highlight`, `item_text`, `kind_icon`, `scrollbar`, `border`, `feedback_loading`, `feedback_empty`, `feedback_error`, `background`, `description_box_background`. Omitted keys fall back to the built-in `dark` theme. Set `name = "<name>"` in config.toml to load it.

#### Style String Syntax

Styles are space-separated tokens:

| Token | Effect |
|-------|--------|
| `bold` | Bold text |
| `dim` | Dim/faint text |
| `underline` | Underlined text |
| `reverse` | Swap foreground/background |
| `italic` | Italic text |
| `fg:N` | Set foreground to 256-color index N (0-255) |
| `bg:N` | Set background to 256-color index N (0-255) |
| `fg:#RRGGBB` | Set foreground to 24-bit truecolor |
| `bg:#RRGGBB` | Set background to 24-bit truecolor |

Examples:
- `"reverse"` — inverted colors (default selected style)
- `"bold fg:255"` — bold white text
- `"dim"` — faint text (default description style)
- `"fg:#cdd6f4 bg:#585b70 bold"` — Catppuccin-style selection
- `"bold underline fg:208"` — bold underlined orange text

### `[ai]`

LLM-powered features. Two independent subtables configure inline autocompletion
and on-demand "Ask AI" separately — each can use a different provider, model,
timeout, and token budget. Both share the `[ai.providers]` map.

#### `[ai.completion]`

Inline autocompletion. When enabled, an async provider queries the LLM for
command completions based on the current buffer context.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Master switch for LLM completions |
| `provider` | string | `""` | Key into `[ai.providers]` selecting the active provider |
| `model` | string | `""` | Model ID sent to the provider |
| `timeout_ms` | integer | `2000` | Request timeout in milliseconds. Clamped to `[200, 30000]`. |
| `max_results` | integer | `3` | Maximum LLM suggestions shown. Clamped to `[1, 10]`. |
| `max_tokens` | integer | `256` | Maximum tokens for the LLM response. Clamped to `[16, 4096]`. |
| `thinking` | string | `"off"` | Thinking/reasoning toggle: `"on"`, `"off"`, or `"auto"`. See note below. |

#### `[ai.ask]`

On-demand "Ask AI". When enabled, a pinned "Ask AI" item appears at the top of
the popup. Selecting it sends your typed question to the LLM and fills the
prompt with its answer.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `false` | Show the on-demand "Ask AI" item |
| `provider` | string | `""` | Key into `[ai.providers]` selecting the active provider |
| `model` | string | `""` | Model ID sent to the provider |
| `timeout_ms` | integer | `15000` | Request timeout in milliseconds. Clamped to `[200, 30000]`. |
| `max_tokens` | integer | `512` | Maximum tokens for the LLM response. Clamped to `[16, 4096]`. |
| `thinking` | string | `"off"` | Thinking/reasoning toggle: `"on"`, `"off"`, or `"auto"`. See note below. |

> **`thinking` toggle:** For `openai-chat` providers the toggle maps to
> `chat_template_kwargs.enable_thinking` (`true`/`false`); for
> `openai-responses` it maps to `reasoning.effort` (`"high"`/`"minimal"`).
> `"auto"` sends no thinking-related field and lets the server decide.
> For providers that use a different thinking field, set it via
> `[ai.providers.<name>.extra_body]` — which overrides the toggle on key
> collision.

> **Safety:** AI responses may be faulty or unsafe. Accepting an "Ask AI" result only fills the prompt — it never executes the command. Always double-check the command before pressing Enter.

### Custom AI completion prompt

The built-in system prompt for inline LLM completions is embedded in the binary.
Advanced users can override it by creating `~/.config/termcmp/prompt.md`:

    You are a shell completion expert. ...

The entire file contents are used verbatim as the system prompt. An empty or
missing file falls back to the built-in default. Changes take effect on the next
config reload (save `config.toml`) or proxy restart.

> **Note:** This override applies only to inline autocompletion (`ai.completion.enabled`).
> The "Ask AI" feature (`ai.ask.enabled`) always uses its own fixed prompt.

```toml
[ai.completion]
enabled = true
provider = "my-provider"
model = "gpt-4o"
timeout_ms = 2000
max_results = 3
max_tokens = 256
thinking = "off"

[ai.ask]
enabled = false
provider = "my-provider"
model = "gpt-4o"
timeout_ms = 15000
max_tokens = 512
thinking = "off"

[ai.providers.my-provider]
base_url = "https://api.openai.com/v1"
api_key = "OPENAI_API_KEY"  # env var name or literal key; empty = no auth header
api = "openai-chat"  # "openai-chat" or "openai-responses"
thinking_budget = 0  # 0 disables extended thinking
# Optional: extra fields merged verbatim into the request body for
# server-specific options. termcmp's own fields always win on collision.
# [ai.providers.my-provider.extra_body]
# chat_template_kwargs = { enable_thinking = false }  # e.g. Qwen3 on llama.cpp

[[ai.providers.my-provider.models]]
id = "gpt-4o"
name = "GPT-4o"
```

The optional `extra_body` table is merged verbatim into the outgoing request
body. Use it for server-specific options the typed fields don't cover — for
example `chat_template_kwargs = { enable_thinking = false }` to stop Qwen3
thinking models on llama.cpp from spending their whole token budget on a
reasoning trace. termcmp's own fields (`model`, `messages`, `max_tokens`, …)
always take precedence if a key collides.

### `[experimental]`

Opt-in features that are not yet considered stable.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `multi_terminal` | bool | `false` | Enable unsupported/unknown terminals. All 10 supported terminals (Ghostty, Otty, Kitty, WezTerm, Alacritty, Rio, iTerm2, Terminal.app, Zed, VSCode) work without this flag. Set to `true` only if you want to try Termcmp on an unlisted terminal. |

```toml
[experimental]
multi_terminal = true
```

Termcmp auto-detects the terminal via `TERM_PROGRAM` and terminal-specific env vars (`KITTY_WINDOW_ID`, `WEZTERM_UNIX_SOCKET`, `ALACRITTY_SOCKET`, `ZED_TERM`, `VSCODE_IPC_HOOK_CLI`), then selects the appropriate rendering strategy:

- **Ghostty, Otty, Kitty, WezTerm, Rio, Zed** — DECSET 2026 synchronized output, native OSC 133 prompt markers. Otty is a Ghostty fork (`TERM_PROGRAM=otty`) and inherits Ghostty's exact capability profile; unlike Ghostty proper it does not set `GHOSTTY_RESOURCES_DIR`, so it is detected purely via `TERM_PROGRAM`.
- **VSCode** (and forks: VSCodium, Cursor, Windsurf, Positron, Trae) — DECSET 2026 synchronized output via xterm.js, native OSC 133. Coexists with VSCode's own shell integration: the proxy forwards the editor's OSC 633 sequences untouched so command decorations / sticky scroll / "run recent command" keep working, and Termcmp's own shell integration suppresses its redundant OSC 7771 emission when `VSCODE_INJECTION=1` is set.
- **Alacritty** — DECSET 2026 synchronized output, OSC 7771 shell integration prompt markers (Alacritty does not support OSC 133).
- **iTerm2 / Terminal.app** — pre-render buffer (single `write()` atomicity), OSC 7771 shell integration prompt markers.

**tmux support:** Ghostty, Kitty, WezTerm, Alacritty, iTerm2, Zed, and VSCode are detected inside tmux via their respective env vars. Terminal.app inside tmux is not detected (it sets no env var that leaks through tmux).

## Completion Cache

Termcmp maintains a persistent tree cache of shell completions so that subcommand and argument suggestions appear instantly on subsequent triggers. The cache is populated lazily — entries are written the first time you trigger completions after a command (e.g. typing `git ` caches git's subcommands).

### Cache File Location

```
$XDG_STATE_HOME/termcmp/completions-{shell}.json
```

When `XDG_STATE_HOME` is unset, it falls back to:

```
~/.local/state/termcmp/completions-{shell}.json
```

`{shell}` is `zsh`, `fish`, or `bash` depending on your login shell.

### How It Populates

The cache builds through normal use. Each time the async shell-completion provider (zsh or fish) returns results, those results are backfilled into the tree. After a session or two of regular use, the cache covers your most-used commands and their subcommands.

Fuzzy shortcuts like `gdiff` → `git diff` depend on the cache: the match works because `"git diff"` exists as a cached candidate string (built from the `"git"` node's `"diff"` entry). Until you've triggered completions after `git ` at least once, that candidate doesn't exist. Shell history entries (e.g. a prior `git diff` invocation) also serve as candidates, so frequently-used commands may match immediately via history even with an empty cache.

### Rebuilding the Cache

To start fresh, delete the cache file and restart termcmp:

```bash
rm "${XDG_STATE_HOME:-$HOME/.local/state}/termcmp/completions-zsh.json"
```

The file is recreated automatically on the next flush (periodic background write or clean exit). No configuration change is needed.

Entries expire after a TTL (currently 30 days). Stale nodes are pruned on read, so an old cache self-heals without manual intervention.

## Logging

Termcmp logs through the `tracing` crate. Logging is configured via CLI flags and the `RUST_LOG` environment variable, not via `config.toml`.

### CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--log-level <level>` | `warn` | One of `trace`, `debug`, `info`, `warn`, `error`. Ignored when `RUST_LOG` is set. |
| `--log-file <path>` | (see below) | Write logs to this file. When unset in proxy mode, the default path is used. |

### Default Log File

In proxy mode (`termcmp` wrapping the shell), logs default to a file — never stderr — to avoid corrupting the terminal stream. The default path is:

```
$XDG_STATE_HOME/termcmp/termcmp.log
```

When `XDG_STATE_HOME` is unset, it falls back to:

```
~/.local/state/termcmp/termcmp.log
```

The parent directory is created automatically on startup. If directory creation fails, Termcmp prints a one-line warning to stderr and falls back to stderr logging for the duration of that run.

Subcommands (`config`, `doctor`, `install`, `uninstall`) log to stderr by default; pass `--log-file` to redirect them.

### Level Hierarchy

```
error < warn < info < debug < trace
```

Setting `--log-level info` enables `info`, `warn`, and `error` events. `trace` is the most verbose and includes every internal decision point.

### `RUST_LOG` Precedence

`RUST_LOG` is read first and overrides `--log-level` when both are set. It uses the standard [`tracing-subscriber` `EnvFilter` syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html), which supports per-crate, per-module, and per-span filters.

Examples:

```bash
# Everything at debug or higher
RUST_LOG=debug termcmp

# Debug only in the suggest engine; everything else at warn
RUST_LOG=warn,suggest=debug termcmp

# Debug in the suggest engine and info in the PTY loop
RUST_LOG=suggest=debug,pty=info termcmp

# Trace a single module
RUST_LOG=parser::osc=trace termcmp
```

Crate names use underscores (e.g. `suggest`), not hyphens. Filter directives are comma-separated; the first bare level (if any) sets the global default.

### Tail-f Recipe

Open the log in a second terminal while reproducing an issue:

```bash
tail -f "${XDG_STATE_HOME:-$HOME/.local/state}/termcmp/termcmp.log"
```

### Generating a Bug Report

1. Start the proxy with verbose logging: `termcmp --log-level debug`.
2. Reproduce the bug in that session.
3. Attach `$XDG_STATE_HOME/termcmp/termcmp.log` (or the fallback path) to the GitHub issue.

For crate-targeted investigations, combine `--log-level` with `RUST_LOG`, e.g. `RUST_LOG=pty=trace termcmp` to inspect the PTY loop in isolation.

## Full Example

```toml
[trigger]
delay_ms = 200

[popup]
max_visible = 8

[suggest]
max_results = 100
max_history_results = 3

[suggest.providers]
commands = true
filesystem = true

[keybindings]
accept = "tab"
accept_and_enter = "enter"
dismiss = "escape"
trigger = "ctrl+/"

[theme]
name = "catppuccin"

[ai.completion]
enabled = false
# provider = "my-provider"
# model = "gpt-4o"
# timeout_ms = 2000
# max_results = 3
# max_tokens = 256
# thinking = "off"

[ai.ask]
enabled = false
# provider = "my-provider"
# model = "gpt-4o"
# timeout_ms = 15000
# max_tokens = 512
# thinking = "off"

[ai.providers.my-provider]
base_url = "https://api.openai.com/v1"
api_key = "OPENAI_API_KEY"
api = "openai-chat"

[[ai.providers.my-provider.models]]
id = "gpt-4o"
name = "GPT-4o"
```

## Notes

- **Config hot-reload:** Some fields are applied live without restarting your shell. Others require a shell restart. See the table below.
- **Nerd Font icons:** The popup gutter uses Nerd Font icons. If your terminal font doesn't include Nerd Font patches, you'll see placeholder characters. Use a [Nerd Font](https://www.nerdfonts.com/) for the best experience.
- **History control:** Use `max_history_results` (not `providers.history`) to control history. Set to `0` to disable history entirely.
- **Popup navigation:** PageUp, PageDown, Home, and End navigate the popup when it is visible and are forwarded to the shell when it is hidden. These structural keys are not user-configurable.

### Hot-Reload Behavior

| Section | Fields | Live Reload |
|---------|--------|:-----------:|
| `[theme]` | `name` | Yes |
| `[keybindings]` | All fields | Yes |
| `[trigger]` | `delay_ms`, `auto_trigger` | Yes |
| `[popup]` | `max_visible`, `borders`, `border_radius`, `feedback_dismiss_ms`, `spinner`, `show_provider_errors`, `render_block_ms`, `min_width`, `max_width`, `description_box`, `description_box_max_width`, `description_box_lines`, `description_box_debounce_ms`, `tab_accepts_top`, `index_hints`, `key_hints`, `nerd_icons` | Yes |
| `[suggest]` | `order`, `max_results`, `max_history_results`, `match_mode` | Yes |
| `[suggest.providers]` | All fields | Yes |
| `[ai]` | All fields | Yes |
| `[experimental]` | All fields | No |

Fields marked "No" require a shell restart (`source ~/.zshrc` or open a new terminal).
