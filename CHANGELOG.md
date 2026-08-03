# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-07-28

First public release. Termcmp is a terminal-native autocomplete engine for
macOS that runs as a PTY proxy: one binary sits between your terminal
emulator and shell, intercepting I/O and rendering fuzzy-ranked suggestions
as native ANSI popups. No macOS Accessibility API, no IME hooks, no Electron
overlay.

### Core engine

- **PTY proxy** — transparent proxy between terminal and shell built on
  `portable-pty` and `tokio`. All rendering is pure ANSI escape sequences,
  so popups work in any terminal that speaks VT.
- **VT parsing** — `vte`-based escape sequence parser with cursor and prompt
  tracking; command line reconstruction and context detection drive what
  gets completed.
- **Fuzzy ranking** — frizbee-powered fuzzy matcher.
- **Synchronized output** — popup frames are wrapped in DECSET 2026
  synchronized updates on terminals that support it, eliminating flicker.
- **Async streaming** — providers run concurrently; results merge into the
  popup as they arrive behind a loading indicator.

### Completion providers

- **Commands** — executables on `PATH`.
- **Filesystem** — path completion for the current word.
- **Environment variables** — `echo $HOM` → `$HOME`.
- **SSH hosts** — parsed from `~/.ssh/config` with mtime caching.
- **Shell aliases** — `alias g=git` → `g push` completes like git.
- **History** — command history as completion candidates.
- **Shell-native completions** — fish (via `complete -C`) and zsh (compsys
  via `compinit -C`) completion systems queried as async subprocess
  providers. Toggle with `[suggest.providers] shell_completions`.
- **LLM completions** — optional model-powered suggestions through any
  OpenAI-compatible API (Chat Completions or Responses wire format), with
  per-provider model lists, thinking-budget control for reasoning models,
  custom system prompts, and an on-demand "Ask AI" popup entry
  (`[ai.completion]` / `[ai.ask]`).

### Terminal support

Ten terminals auto-detected out of the box: Ghostty, Otty (a Ghostty fork),
Kitty, WezTerm, Alacritty, Rio, iTerm2, Terminal.app, Zed, and VSCode
(including VSCodium, Cursor, Windsurf, Positron, and Trae). Each gets a
capability profile — synchronized vs pre-render-buffer rendering and OSC 133
vs shell-integration prompt detection — selected automatically. tmux works
on every terminal whose identity survives multiplexing. Unsupported
terminals can be forced with `[experimental] multi_terminal = true`.

### Shell support

- **zsh and fish** — full support: auto-trigger on typing, PTY proxy
  wrapping, OSC 133 prompt markers.
- **bash** — manual trigger only (Ctrl+/).

Default keybindings: Tab accepts, Enter accepts and executes, arrow keys
navigate, Escape dismisses, Ctrl+/ triggers manually. All remappable in
`[keybindings]`.

### Theming

- Four built-in themes: `dark` (default), `light`, `catppuccin`,
  `material-darker`.
- Custom themes: drop a TOML file at `~/.config/termcmp/themes/<name>.toml`
  and set `[theme] name = "<name>"`. Omitted fields fall back to the
  built-in `dark` theme, so partial overrides work.
- Theme files style every popup element: selected row, description text,
  match highlight, item text, scrollbar, border, feedback states
  (loading/empty/error), and backgrounds.
- Theme changes hot-reload — no shell restart needed.

### Configuration

- TOML config at `~/.config/termcmp/config.toml`, generated with commented
  defaults by `termcmp install` (never overwrites an existing file).
- Trigger characters and delay (`[trigger]`), popup dimensions (`[popup]`),
  keybindings (`[keybindings]`), theme (`[theme]`), suggestion limits and
  provider toggles (`[suggest]`), and AI providers (`[ai]`).
- Theme, keybindings, trigger chars, and popup dimensions hot-reload;
  other changes need a shell restart.

### CLI

- `termcmp` — run the PTY proxy (what the shell integration invokes).
- `termcmp install [--dry-run]` — add managed shell integration to every rc
  file that exists (`~/.zshrc`, `~/.bashrc`, `~/.config/fish/config.fish`),
  deploy shell scripts to `~/.config/termcmp/shell/`, create the themes
  directory, and write a default config.
- `termcmp uninstall` — remove shell integration.
- `termcmp config` — print the resolved configuration.
- `termcmp doctor` — run health checks against the installation.

### Logging

- `tracing`-based logging to `$XDG_STATE_HOME/termcmp/termcmp.log` (proxy
  mode never writes to stderr, so logs can't corrupt the terminal stream).
- `--log-level`, `--log-file`, and per-crate `RUST_LOG` filters.

[0.1.0]: https://github.com/emretuna/termcmp/releases/tag/v0.1.0
