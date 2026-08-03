
<img src="assets/termcmp.png" alt="Termcmp" width="300">

**Terminal-native autocompletion engine that uses pure shell autocompletion sources built with RUST.**

[![CI](https://github.com/EmreTuna/termcmp/actions/workflows/ci.yml/badge.svg)](https://github.com/EmreTuna/termcmp/actions/workflows/ci.yml)
[![GitHub Release](https://img.shields.io/github/v/release/EmreTuna/termcmp)](https://github.com/EmreTuna/termcmp/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)


## Overview

Termcmp sits inside your terminal's data stream as a PTY proxy, intercepting I/O between your terminal emulator and your shell. When you type a command, it renders autocomplete suggestions as native ANSI popups with built-in caching for fast completions.

Optional AI-powered suggestions from any OpenAI-compatible provider: cloud (OpenAI, Groq, OpenRouter) or local (llama.cpp, Ollama, LM Studio). 

## Status

Termcmp is under active development. Contributions and bug reports are welcome.

- **10 supported terminals on macOS and Linux:** Ghostty, Otty (a Ghostty fork), Kitty, WezTerm, Alacritty, Rio, iTerm2, Terminal.app, Zed, and VSCode (incl. VSCodium, Cursor, Windsurf, Positron, Trae) — all work out of the box with no additional configuration.
- **zsh and fish are the primary shells.** Bash supports manual trigger only (Ctrl+/).
- **macOS and Linux.** No Windows support right now.
- **Pre-1.0.** Config format and behavior may change between releases.

Found a bug? [Open an issue](https://github.com/EmreTuna/termcmp/issues).

## Requirements

- **Terminal:** [Ghostty](https://ghostty.org), [Otty](https://otty.app), [Kitty](https://sw.kovidgoyal.net/kitty/), [WezTerm](https://wezfurlong.org/wezterm/), [Alacritty](https://alacritty.org), [Rio](https://raphamorim.io/rio/), [iTerm2](https://iterm2.com), Terminal.app, [Zed](https://zed.dev), or [VSCode](https://code.visualstudio.com) (and forks: VSCodium, Cursor, Windsurf, Positron, Trae)
- **OS:** macOS, Linux
- **Shell:** zsh and fish (full), bash (Ctrl+/ trigger only)
- **Rust:** 1.86+ (for building from source)

## Installation

### Homebrew (recommended on macOS)

```bash
brew install EmreTuna/tap/termcmp
termcmp install
```

### Shell installer (macOS and Linux)

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/EmreTuna/termcmp/releases/latest/download/termcmp-installer.sh | sh
termcmp install
```

### Cargo

```bash
cargo install --git https://github.com/EmreTuna/termcmp.git
termcmp install
```

### From source

```bash
git clone https://github.com/EmreTuna/termcmp.git
cd termcmp
cargo build --release
cp target/release/termcmp ~/.cargo/bin/
termcmp install
```

### What `termcmp install` does

- Probes `~/.zshrc`, `~/.bashrc`, and `~/.config/fish/config.fish` and adds managed shell integration to every one that exists (zsh/fish get a PTY-proxy init block; bash gets integration only). Halts with an error if none exist — never creates a missing rc file
- Deploys shell scripts for zsh, bash, and fish to `~/.config/termcmp/shell/`
- Creates a `~/.config/termcmp/themes/` directory for custom theme files
- Creates default config at `~/.config/termcmp/config.toml` (never overwrites existing)

### Uninstall

```bash
termcmp uninstall
brew uninstall termcmp  # if installed via Homebrew
```

## Quick Start

After installation, restart your terminal. Termcmp activates automatically in zsh and fish.

- **Type a command** and suggestions appear after a short delay
- **Tab** to accept the selected suggestion
- **Enter** to accept and execute
- **Arrow keys** to navigate the popup
- **Escape** to dismiss
- **Ctrl+/** to manually trigger completions
- **Ctrl+R** to toggle fuzzy/substring matching while the popup is open (the footer flashes the new mode for a second)

### Supported Terminals

Termcmp auto-detects your terminal and selects the best rendering strategy. All supported terminals work out of the box — no config flag needed.

| Terminal | Rendering | Prompt Detection | tmux Support |
|----------|-----------|-----------------|:------------:|
| [Ghostty](https://ghostty.org) | Synchronized (DECSET 2026) | OSC 133 (native) | Yes |
| [Otty](https://otty.app) (Ghostty fork) | Synchronized (DECSET 2026) | OSC 133 (native) | — |
| [Kitty](https://sw.kovidgoyal.net/kitty/) | Synchronized (DECSET 2026) | OSC 133 (native) | Yes |
| [WezTerm](https://wezfurlong.org/wezterm/) | Synchronized (DECSET 2026) | OSC 133 (native) | Yes |
| [Alacritty](https://alacritty.org) | Synchronized (DECSET 2026) | Shell integration | Yes |
| [Rio](https://raphamorim.io/rio/) | Synchronized (DECSET 2026) | OSC 133 (native) | — |
| [iTerm2](https://iterm2.com) | Pre-render buffer | Shell integration | Yes |
| Terminal.app | Pre-render buffer | Shell integration | No |
| [Zed](https://zed.dev) | Synchronized (DECSET 2026) | OSC 133 (native) | Yes |
| [VSCode](https://code.visualstudio.com) (and forks) | Synchronized (DECSET 2026) | OSC 133 (native) | Yes |

## Configuration

Config lives at `~/.config/termcmp/config.toml`:

The snippet below is illustrative — the install-time default config is generated by `termcmp install` and covers every supported option. For the exhaustive reference see [docs/CONFIGURATION.md](docs/CONFIGURATION.md).

```toml
[trigger]
delay_ms = 150
auto_trigger = true

[popup]
max_visible = 10
index_hints = true   # "selected/total" header row
key_hints = true     # keybinding hints footer row

[keybindings]
accept = "tab"
dismiss = "escape"
trigger = "ctrl+/"
toggle_match_mode = "ctrl+r"

[theme]
name = "dark"  # dark, light, catppuccin, material-darker, gruvbox, nord, dracula, tokyo-night

[suggest]
max_results = 50
max_history_results = 5

[suggest.providers]
commands = true
filesystem = true
shell_completions = true

# Optional — AI completions (disabled by default)
[ai.completion]
enabled = true
provider = "openai"
model = "gpt-4o"

[ai.providers.openai]
base_url = "https://api.openai.com/v1"
api_key = "OPENAI_API_KEY"  # env var name or literal key
api = "openai-chat"         # or "openai-responses"

[[ai.providers.openai.models]]
id = "gpt-4o"
name = "GPT-4o"
```

Theme, keybindings, trigger chars, and popup dimensions are hot-reloaded. Other changes need a shell restart.

See [docs/CONFIGURATION.md](docs/CONFIGURATION.md) for the full reference.

## Completion Sources

Termcmp ranks suggestions from several built-in providers:
- **Commands** — executables on your `PATH`
- **Filesystem** — path completion for the current word
- **Environment variables** — `echo $HOM` → `$HOME`
- **SSH hosts** — parsed from `~/.ssh/config` with mtime caching
- **Shell alias resolution** — `alias g=git` → `g push` completes like git
- **Shell history** — recent command history as completion candidates
- **Shell-native completions** — fish/zsh completion providers (enable with `[suggest.providers] shell_completions = true`)
- **LLM completions** — optional model-powered suggestions (see `[ai]` config)

Async providers stream results into the popup as they arrive; a loading indicator (`...`) appears while they run.

## Architecture

Rust workspace with 9 crates:

| Crate | Role |
|-------|------|
| `termcmp` | Binary entry point, CLI, install/uninstall |
| `pty` | PTY proxy event loop (portable-pty + tokio) |
| `parser` | VT escape sequence parsing (vte), cursor/prompt tracking |
| `buffer` | Command line reconstruction, context detection |
| `suggest` | Suggestion engine with fuzzy ranking (frizbee) |
| `overlay` | ANSI popup rendering with synchronized output |
| `config` | TOML config, keybindings, themes |
| `terminal` | Terminal detection, capability profiling, render strategy selection |
| `llm` | LLM-powered completion provider (OpenAI-compatible API) |

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the full design — data flow, dependency graph, key design decisions, and performance characteristics.

## Benchmarks

Criterion benchmarks live in [`benchmarks/`](benchmarks/) — latest results: [v0.1.0](benchmarks/0.1.0-benchmark.md). Run locally with `cargo bench --workspace`.

Headline numbers (Apple M3 Pro): VT parsing at ~358 MB/s on plain text, and a full overlay update frame (clear + render popup + detail for 50 suggestions) at ~7 µs — well under a 16 ms frame budget.

## Shell Support

| Feature | zsh | bash | fish |
|---------|-----|------|------|
| Auto-trigger on typing | Yes | No | Yes |
| Ctrl+/ manual trigger | Yes | Yes | Yes |
| PTY proxy wrapping | Yes | Yes | Yes |
| OSC 133 prompt markers | Yes | Yes | Yes |

## Known Limitations

- **Terminal.app inside tmux is not detected.** Terminal.app sets no environment variable that leaks through tmux, so Termcmp cannot identify it. Ghostty, Kitty, WezTerm, Alacritty, and iTerm2 in tmux work correctly via their respective env vars (`GHOSTTY_RESOURCES_DIR`, `KITTY_WINDOW_ID`, `WEZTERM_UNIX_SOCKET`, `ALACRITTY_SOCKET`, `ITERM_SESSION_ID`).
- **Dynamic generators stream in.** Async generator results merge into the popup as they arrive — including on an idle shell — via the dynamic merge loop in `pty`. The first paint is gated by `popup.render_block_ms` (default 80ms, range 0-300ms): set to `0` for instant paint with later merging, or higher to race fast generators into the same frame as static flags.
- **Bash: manual trigger only.** Auto-trigger on typing is not implemented for bash. Use Ctrl+/ to manually invoke completions.
- **No Windows support.** macOS and Linux are fully supported; terminal detection is env-var based (`TERM_PROGRAM` and friends) and works identically on both platforms.
- **Alacritty uses shell integration markers, not OSC 133.** Alacritty does not support OSC 133 natively; Termcmp uses its own shell integration markers instead. No functional difference — just a different detection path.
- **VSCode forks share detection.** VSCodium, Cursor, Windsurf, Positron, and Trae use the xterm.js frontend and shell integration model. Termcmp coexists with VSCode's own shell integration (OSC 633) — the proxy forwards editor sequences untouched so command decorations, sticky scroll, and "run recent command" continue to work.
- **Unsupported terminals are experimental.** Other terminals can be enabled with `[experimental] multi_terminal = true` in config.

## FAQ

**AI used during development?**

Yes, AI tools highly used to overcome issues during development. For my own usecase I highly satisfied with the result, hope it works for you as well. 

**How is this different from zsh/fish built-in autocomplete?**

Built-in completions work great — Termcmp doesn't replace them. It adds a visual popup layer on top, doesn't replace ghost text suggestions from fish/zsh autocompletion. Suggestions are fuzzy-ranked from multiple sources (commands, filesystem, shell history, shell-native completions) and displayed in a single view. Think of it as complementary, not a replacement.

**Why a PTY proxy instead of a shell plugin?**

The PTY proxy sits between the terminal and the shell, rendering popups via pure ANSI escape sequences. This means no zle widget conflicts, no plugin manager dependencies, no RPROMPT corruption, and no fragile shell internals to hook into. It's more complex under the hood, but the UX is cleaner — one binary, works immediately after install.

**Where's the config documentation? I'm having popup alignment issues.**

Full config reference lives at [docs/CONFIGURATION.md](docs/CONFIGURATION.md). Running `termcmp install` generates a commented default config at `~/.config/termcmp/config.toml` with all available options.

For popup alignment: Termcmp uses ANSI cursor positioning within the terminal grid, so popups always track the cursor position directly. This avoids the window-level coordinate issues that plague Accessibility API approaches (the kind of drift reported with tools like Amazon Q / Kiro). If popups are misaligned, it's likely a terminal compatibility issue — please [open an issue](https://github.com/EmreTuna/termcmp/issues) with your setup details.

## Logging

Termcmp logs through `tracing`. In proxy mode the default sink is a file under `$XDG_STATE_HOME/termcmp/termcmp.log` (falling back to `~/.local/state/termcmp/termcmp.log` when `XDG_STATE_HOME` is unset). stderr is not used by default in proxy mode, so log output never corrupts the terminal stream.

- `--log-level <trace|debug|info|warn|error>` sets the level (default: `warn`). Level hierarchy: `error` < `warn` < `info` < `debug` < `trace`.
- `--log-file <path>` overrides the default log path.
- `RUST_LOG` takes precedence over `--log-level`. It supports per-crate filters, e.g. `RUST_LOG=suggest=debug,pty=info`.

Tail the log in real time:

```bash
tail -f "${XDG_STATE_HOME:-$HOME/.local/state}/termcmp/termcmp.log"
```

Reporting a bug: run with `--log-level debug`, reproduce the issue, and attach the log file to your issue.

See [docs/CONFIGURATION.md](docs/CONFIGURATION.md#logging) for the full reference.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[MIT](LICENSE)

---
