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
| bash  | Full support |

## Features

- **Hierarchical command extraction** — learns your command tree (commands → subcommands → sub-subcommands) via shell-native completions
- **Frizbee fuzzy matching** — type `gco` to match `git checkout`, `cl` to match `clone`
- **LLM completions** (optional) — AI-powered suggestions when enabled
- **Privacy-first** — secrets (API keys, tokens, passwords) are redacted before any LLM request
- **Multiplexer-safe** — works inside tmux, wezterm, zellij, herdr, screen. By default the inner shell is isolated so `/dev/tty` password prompts (sudo/ssh/git/pinentry) and job control work everywhere. Set `[experimental] agent_tracking = true` to restore foreground-process-group mirroring for tmux/herdr/workmux agent observation (at the cost of `/dev/tty` prompt contention)
- **Password prompts work** — `sudo`, `ssh`, `git` and `pinentry` read `/dev/tty` directly; the default isolated session gives them the inner PTY
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
