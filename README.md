[![termcmp](/assets/termcmp.png)](https://github.com/emretuna/termcmp)

Terminal autocompletion via frizbee fuzzy matching. A PTY proxy that sits between your shell and terminal, providing inline command completions from multiple sources: shell-native completions (fish/zsh), command history, filesystem paths, and optional LLM-powered suggestions.

## Installation

```sh
# Homebrew (macOS)
brew install EmreTuna/tap/termcmp

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
- **Multiplexer-safe** — works inside tmux, wezterm, zellij, herdr, screen. Agent tracking works out of the box inside tmux and herdr; setting `[experimental] session_isolation = true` keeps `/dev/tty` prompts working in a multiplexer at the cost of that tracking
- **Password prompts work** — `sudo`, `ssh`, `git` and `pinentry` read `/dev/tty` directly, so the default session isolation is what makes their prompts receive every keystroke
- **TUI-aware** — popup only appears at the prompt, never inside neovim/lazygit/omp agent
- **Small terminal friendly** — compact mode for dropdown terminals and small panes
- **Hot-reload config** — edit `~/.config/termcmp/config.toml`, changes apply immediately
- **Custom providers** — user-defined command shortcuts from `~/.config/termcmp/providers/*.toml`, fuzzy-matched in the popup ([Providers](#providers))

## Providers

Define custom command shortcuts in `~/.config/termcmp/providers/*.toml` and activate them via `[providers] enabled`. Enabled providers inject their commands into the normal popup as fuzzy-matched candidates — accept with Tab to fill the prompt for review, or Enter to run immediately. Independent of which terminal or multiplexer you use. See [`[providers]`](docs/CONFIGURATION.md#providers).

```toml
# ~/.config/termcmp/providers/mytools.toml
name = "mytools"

[[commands]]
name = "deploy-staging"
command = "kubectl rollout restart deployment/api -n staging"
```

## Documentation

- [Configuration](docs/CONFIGURATION.md) — Full config.toml reference
- [Architecture](docs/ARCHITECTURE.md) — Technical design and internals
- [CI Gates](docs/CI.md) — Continuous integration and testing

## License

MIT
