# Boundaries

Non-negotiable design constraints. Every change — feature, bug fix, or refactor — must respect these. When a proposed change conflicts with a boundary, the boundary wins; redesign the change, don't weaken the rule.

## 1. Never block the terminal

Termcmp is an autocompleter, not a shell. It must render only at the interactive prompt and must never get in the way of the terminal's natural flow.

- **Prompt-only activation.** The popup appears only when the prompt is the active screen region — never during password entry, full-screen TUI apps, `read` prompts, or any context that expects the terminal to behave like a dumb pipe.
- **Detection first, blocklist as supplement.** Gating must be driven by positive prompt detection (OSC 133 / OSC 7771 markers + the `in_prompt` / alt-screen state), not by a deny-list of "bad" programs alone. A blocklist is allowed only as a supplement for programs that never emit prompt markers — Go TUIs (omp, btm, …) that hold `in_prompt == true` without entering the alt screen — and it must never be the primary gate. The next full-screen app not on the list must already be caught by detection, not by the blocklist.
- **Non-interference is the default.** When in doubt, don't draw. Termcmp's worst failure mode is not a missing suggestion; it is corrupting the terminal state of the program the user is actually running.

Implication for contributors: when adding input handling or rendering, first answer "how do I know I'm at the prompt right now?" — then "what happens if I'm wrong?"

## 2. Pass keypresses through; don't own them

Termcmp runs as a PTY proxy between the terminal and the shell. Other TUIs and multiplexers — tmux, wezterm, zellij, herdr, screen — also sit in or next to this stream and need to see the same bytes.

- **Act on the fewest keys.** Only interception keys (Tab, arrows, Escape, Enter) may be consumed, and only while the popup is visible. Everything else is forwarded untouched.
- **Never swallow input speculatively.** If Termcmp doesn't consume a key, it must reach the shell (or the multiplexer below) byte-for-byte, in order.
- **Assume neighbors on the stream.** Any new key-capture feature must account for a TUI app or multiplexer that also wants that key.

Implication for contributors: before claiming a key, ask "who else below me would want this?" If you can't intercept it without risk of stealing it from a TUI, forward it.

## 3. Lightweight, fast, small footprint

Termcmp runs inside every interactive session. Its cost is paid on every keystroke, over the entire life of the shell.

- **Idle is cheap.** Memory stays in the single-digit MB range; idle CPU is near zero.
- **Latency budget.** Keystroke-to-suggestion stays well under 50ms; PTY forwarding adds under 1ms. Ranking 10,000 candidates must stay under 1ms.
- **Minimal footprint on the system.** No Accessibility APIs, no IME hooks, no persistent daemons beyond the proxy itself. Few dependencies; no speculative abstractions.
- **No assumptions of idle time.** A slow path justified as "rare" still runs on someone's keystroke path.

Implication for contributors: prefer the boring, allocation-light path. One `Arc` copied per keystroke is fine; a hash map rebuilt per keystroke is not.

## 4. Platform priority: macOS first, Linux second

- **macOS is the primary target**; correctness and polish land there first.
- **Linux is supported** and must not be broken by macOS-first changes.
- **Windows is not supported and not planned.** No `cfg(windows)` scaffolding, no conditional compilation for Windows correctness, no "works except on Windows" compromises worth paying for.

Implication for contributors: a fix must not trade macOS quality for Linux support or vice versa. Don't add Windows-only code paths that would otherwise never be exercised.

## 5. Frizbee is the matching engine

Frizbee is the sole provider of fuzzy and substring matching.

- **No second matcher.** Subsequence and substring modes, case sensitivity, and candidate scoring all go through frizbee. Don't hand-roll a ranking heuristic beside it.
- **Frecency is termcmp's own concern.** frizbee has no recency/frequency API; historical weighting lives in `crates/suggest/src/frecency.rs` and is applied as a ranking boost on top of frizbee scores. frizbee owns the match; termcmp owns recency.
- **Integrate, don't fork.** If a matching behavior is missing, the fix is to use frizbee correctly or fix frizbee — not to bolt on a parallel matcher.

Implication for contributors: before writing any matching code, confirm it isn't already frizbee's responsibility. Frecency weighting is a separate, local concern and is never a matcher.

## 6. Config-driven, never hardcoded over behavior

Configuration (`~/.config/termcmp/config.toml`, hot-reloaded) is the source of truth for behavior. Hardcoded values must not silently override what the user configured.

- **New features read their config.** Every threshold, timeout, limit, binding, and display option is configurable, and the code reads the configured value.
- **Bug fixes respect config.** A fix must not short-circuit a configured option with a literal "works now because I hardcoded it."
- **Defaults belong in the config crate**, applied only when the user has not set a value — not baked into the calling code where a literal bypasses user intent.
- **Reject the override, not the config.** If two configured values conflict, surface that conflict or resolve it explicitly; don't pick a hardcoded winner.

Implication for contributors: search for the setting first; if you're about to write a numeric literal or `true`/`false` into feature code, ask why it isn't a config value.

## 7. Respect terminal height: limit, then scroll

Small terminals are a first-class use case. The popup must never overflow a short pane, and must never take more space than it needs.

- **Limit first.** On small heights, the number of items shown is capped so the popup fits and remains useful.
- **Scroll as capacity allows.** On larger panes, scrolling may reveal more items — but only when the terminal height actually permits it.
- **User config wins, capacity follows.** Follow the user's configured limits and preferences first; then respect what the physical terminal height can hold.
- **No overflow, no push-down of live output.** Scrollback protection is preserved regardless of height — overlay clearing must never flood the user's scrollback.

Implication for contributors: render with `min(user_limit, height_capacity)` in mind. A short pane is a legitimate environment, not a degraded one.

## 8. Coexist with agent-status tooling

Herdr and the tmux plugin workmux inspect running agents and their status by observing terminal activity. Termcmp must never interfere with that observation.

- **Don't obscure or masquerade as an agent.** Termcmp's output and markers must remain distinguishable from agent or process status signals those tools rely on.
- **Don't flood the stream.** Spurious rendering churn or marker traffic can make status detection noisy or wrong. Emit only what is necessary.
- **Leave their data alone.** Don't consume, strip, or rewrite escape sequences or markers that those tools need to do their job.

Implication for contributors: before adding output or markers, ask "will herdr or workmux misread this as an agent state change?" If it might, redesign the signaling.