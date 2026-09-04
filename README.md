[![termcmp](/assets/termcmp.png)](https://github.com/emretuna/termcmp)

Terminal autocompletion via frizbee fuzzy matching. A PTY proxy that sits between your shell and terminal, providing inline command completions from multiple sources: shell-native completions (fish/zsh), command history, filesystem paths, and optional LLM-powered suggestions.

## Installation

```sh
# Homebrew (macOS)
brew install termcmp/tap/termcmp

# Cargo
cargo install https://github.com/emretuna/termcmp
```

## Quick Start

```sh
# Install shell integration (adds init to your .zshrc / config.fish)
termcmp install

# Restart your terminal, then start typing commands
# Tab accepts the top suggestion, Enter runs the command
```

## Shell Support

| Shell | Status |
|-------|--------|
| zsh   | Full support |
| fish  | Full support |
| bash  | Planned |

## Features

- **Hierarchical command extraction** — learns your command tree (commands → subcommands → sub-subcommands) via shell-native completions
- **Frizbee fuzzy matching** — type `gco` to match `git checkout`, `cl` to match `clone`
- **LLM completions** (optional) — AI-powered suggestions when enabled
- **Privacy-first** — secrets (API keys, tokens, passwords) are redacted before any LLM request
- **Multiplexer-safe** — works inside tmux, wezterm, zellij, herdr, screen
- **TUI-aware** — popup only appears at the prompt, never inside neovim/lazygit/omp agent
- **Small terminal friendly** — compact mode for dropdown terminals and small panes
- **Hot-reload config** — edit `~/.config/termcmp/config.toml`, changes apply immediately

## Documentation

- [Configuration](docs/CONFIGURATION.md) — Full config.toml reference
- [Architecture](docs/ARCHITECTURE.md) — Technical design and internals
- [CI Gates](docs/CI.md) — Continuous integration and testing

## License

MIT
