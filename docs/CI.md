# CI Gates

## Overview

Four CI gates live in `.github/workflows/ci.yml`: Check, Test (macOS + Ubuntu), Shell smoke (zsh + fish, macOS + Ubuntu matrix), and Clippy & Format. Two more gates live outside `ci.yml`: `cargo-deny check` runs alongside `cargo audit` in [`.github/workflows/audit.yml`](../.github/workflows/audit.yml) (Cargo manifest / lockfile changes and a weekly cron), and `Smoke packaged artifacts` runs in [`.github/workflows/release.yml`](../.github/workflows/release.yml) on every release tag — those two are documented under [Audit workflow](#audit-workflow) and [Release-only gates](#release-only-gates) below. Benchmark-regression checking is intentionally **not** a CI gate — it is run manually at release time (see [Release-time benchmark checking](#release-time-benchmark-checking) below).

---

## Gates

### Check

**Job name in CI:** `Check`
**YAML key:** `check`
**Runner:** `macos-latest`

**Purpose:** fast compilation sanity check. Runs `cargo check --all-targets` to verify the workspace compiles without running tests or producing binaries.

---

### Test

**Job name in CI:** `Test (macos-latest)` / `Test (ubuntu-latest)`
**YAML key:** `test`
**Runner matrix:** `macos-latest`, `ubuntu-latest`

**Purpose:** runs `cargo test` across both supported platforms. On Linux, zsh is installed via apt before the test step (macOS ships with zsh pre-installed).

---

### Shell smoke

**Job name in CI:** `Shell smoke (zsh, macos-latest)` / `Shell smoke (fish, ubuntu-latest)` / etc.
**YAML key:** `shell-smoke`
**Runner matrix:** `macos-latest` × `ubuntu-latest`
**Shell matrix:** `zsh` × `fish`
**Trigger:** `needs: [check]` — runs after the `check` job succeeds. Blocking gate.

This is a single matrix job that produces four combinations (2 shells × 2 OS). Each combination runs the appropriate smoke script for its shell.

**Purpose:** exercises the production shell integration scripts (`shell/termcmp.zsh`, `shell/termcmp.fish`) under real shells with `--no-rcs` / `--no-config` and asserts that the buffer-reporting widgets emit OSC 7772 frames with correct percent-encoding. Catches regressions in the encoder for characters that would corrupt a frame mid-stream (semicolons, BEL `0x07`, ESC `0x1B`, literal `%`), validates UTF-8 round-trip, exercises the OSC 7 path encoder, and verifies gate guards.

**ttyshim scrub (macOS only):** on macOS runners, a `Run ttyshim scrub test` step runs `scripts/check-ttyshim-scrub.sh` after the shell smoke. This validates that the PTY shim layer correctly scrubs terminal escape sequences.

**Failure modes:**

- Encoder regression: a frame is missing percent-encoding for one of the documented byte classes.
- Gate guard regression: the buffer-reporting widget emits OSC 7772 when `TERMCMP_ACTIVE` is unset.
- Binding regression (fish): re-sourcing stacks duplicate bindings.
- Environment failure: the shell is not on `PATH`, or the integration script is missing.

**How to debug locally:**

```bash
scripts/check-zsh-zle-smoke.sh
scripts/check-fish-smoke.sh
scripts/check-ttyshim-scrub.sh   # macOS only
```

---

### Clippy & Format

**Job name in CI:** `Clippy & Format`
**YAML key:** `clippy`
**Runner:** `macos-latest`

**Purpose:** single job running both `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`. Both must pass for the gate to succeed.

---

## Release-time benchmark checking

Benchmark regression is **not** enforced on every PR. Hosted runner variance (±15–20% on single-threaded latency benches) makes CI-gated benchmarking noisy enough that the signal-to-noise ratio doesn't justify the minutes spent. Instead, the release process runs benchmarks locally on a quiet machine and records the numbers in the release PR.

The tooling is preserved:

- [`.github/workflows/bench.yml`](../.github/workflows/bench.yml) — manual `workflow_dispatch` job that runs `cargo bench --workspace` and uploads Criterion reports as an artifact.
- [`scripts/check-bench.sh`](../scripts/check-bench.sh) — threshold-based comparator against a saved Criterion baseline.
- [`benchmarks/`](../benchmarks/) — per-release report files (e.g. `0.1.0-benchmark.md`).

**Release workflow:**

```bash
cargo bench --workspace -- --save-baseline release-<prev>    # one-time, on the prior release tag
cargo bench --workspace -- --baseline release-<prev>         # on the release candidate
scripts/check-bench.sh --threshold 10                         # optional gate for the release author
```

---

## Audit workflow

The `audit` workflow ([`.github/workflows/audit.yml`](../.github/workflows/audit.yml)) runs two dependency-policy checks on every Cargo manifest / lockfile change and on a weekly Monday cron. Both checks are blocking — a failure fails the workflow.

> Both checks live in the single `cargo audit` job in `audit.yml`; failure of either step fails the job.

### cargo audit step

**Action:** `rustsec/audit-check@v2`.
**Trigger:** Cargo.toml or Cargo.lock changes (PR or push to `master`), changes to `audit.yml` itself, and the weekly cron (`0 12 * * 1`).

**Purpose:** scans the resolved dependency graph against the RustSec advisory database. Flags known vulnerabilities. Posts a GitHub Check annotation with the affected crates and advisory IDs.

**Failure modes:** any unyanked advisory at `error` severity (per `audit-check`'s defaults) against a crate in `Cargo.lock`.

### cargo-deny step

**Step name:** `Run cargo-deny`, using `EmbarkStudios/cargo-deny-action@v2` with `command: check` and `arguments: --all-features`.
**Trigger:** same trigger set as the `cargo audit` step (they share the `cargo audit` job).

**Purpose:** enforces the policy in [`deny.toml`](../deny.toml) — license allow/deny lists, banned crates, source allowlist, and duplicate-version policy. `cargo deny check` runs the full check matrix (`advisories`, `bans`, `licenses`, `sources`).

**Failure modes:**

- Disallowed license: a dependency carries a license outside the allow list in `deny.toml`.
- Banned crate: a dependency matches a `[bans] deny` entry.
- Untrusted source: a dependency comes from a registry/git source outside the `[sources]` allowlist.
- Duplicate-version policy: `multiple-versions` is currently `warn` while we ladder up to `deny`; future tightening will turn this into a hard fail.

**Status today:** production-live. Should not be added as a branch-protection check on PRs unless the PR touches Cargo manifests (the workflow's path filter already gates it); branch protection cannot express "required only when this path changed".

**How to debug locally:**

```bash
cargo audit                              # one-time: cargo install cargo-audit
cargo deny check                         # one-time: cargo install cargo-deny
cargo deny check --all-features
```

---

## Release-only gates

The `release` workflow ([`.github/workflows/release.yml`](../.github/workflows/release.yml)) runs on `push` of any version-shaped tag. It hosts two smoke gates that are **not** part of CI and only ever run at release time.

### Smoke packaged artifacts (macOS)

**Job name in release workflow:** `Smoke packaged artifacts`
**YAML key:** `artifact-smoke`
**Trigger:** `needs: [build-local-artifacts, build-global-artifacts]` inside `release.yml`. Runs after every successful artifact build and gates the downstream `host` job (which is what actually publishes the GitHub Release).

**Purpose:** refuses to publish a release whose packaged macOS artifact can't execute `--version`, `--help`, or `install --dry-run` cleanly. Native-arch binaries (arm64 on the `macos-latest` runner) execute end-to-end against an isolated `HOME` and `cwd` so the test reflects the binary's behavior only — not anything that would otherwise leak in from the runner's filesystem. Cross-arch binaries (x86_64) get a structural smoke (extract + `file(1)` arch check) since the runner can't execute them; the script warns loudly if arch detection is inconclusive.

**Failure modes:**

- No executable `termcmp` extracted from the archive.
- `install --dry-run` writes to the isolated `HOME` (a real side effect during what is supposed to be a dry run).
- Arch detection inconclusive (WARN on stderr; structural-only smoke for that artifact).
- Zero artifacts of the expected shape found at all (driver loop in the workflow step fails closed).

**Status today:** production-live. Gates the `host` job in `release.yml`; nothing publishes without it.

**How to debug locally:**

```bash
cargo build --release
scripts/check-release-artifact-smoke.sh <path/to/termcmp-*-apple-darwin.tar.{gz,xz}>
```

### Smoke packaged Linux artifacts

**Job name in release workflow:** `Smoke packaged Linux artifacts`
**YAML key:** `linux-artifact-smoke`
**Runner:** `ubuntu-latest`
**Trigger:** `needs: [build-local-artifacts, build-global-artifacts]` inside `release.yml`. Runs whenever either build job succeeded; the loop fails closed when no Linux tarballs exist.

**Purpose:** refuses to publish a release whose packaged Linux artifact can't execute `--version`, `--help`, or `install --dry-run` cleanly. The x86_64 ELF executes end-to-end on the `ubuntu-latest` runner; the aarch64 ELF is inspected structurally (the smoke script's arch mismatch path degrades to extraction + `file(1)` checks).

**Status today:** production-live. Gates the `host` job alongside the macOS smoke.

---

## Branch-protection configuration

These steps require repo admin access. Without them the gates run but **do not block merge**.

1. Go to <https://github.com/EmreTuna/termcmp/settings/branches>.
2. Edit the branch protection rule for `master`, or create one if none exists.
3. Enable **"Require status checks to pass before merging"**.
4. In the status check search box, add the checks listed as "Ready to add" in the table below by their **exact display names** (the human-readable `name:` values from the CI YAML, not the YAML job keys).
5. Save the rule.

These checks are added **alongside** any existing required checks (e.g. `Check`, `Test (macos-latest)`, `Test (ubuntu-latest)`, `Clippy & Format`). They replace nothing.

### Readiness table

| Gate | Branch protection status |
|---|---|
| `Shell smoke (zsh, macos-latest)` / `Shell smoke (zsh, ubuntu-latest)` / `Shell smoke (fish, macos-latest)` / `Shell smoke (fish, ubuntu-latest)` | Ready to add. |
| `cargo audit` (audit workflow — covers both `cargo audit` and `cargo deny check` steps) | Path-filtered to Cargo manifest / lockfile changes. Branch protection cannot express "required only when this path changed"; leave unenforced and let the workflow's own path filter gate it. |
| `Smoke packaged artifacts` (release workflow) | Release-only — not a PR check. Gates the `host` job inside `release.yml`; cannot meaningfully be added to PR branch protection. |
| `Smoke packaged Linux artifacts` (release workflow) | Release-only — not a PR check. |

> **Note on job names vs. YAML keys:** GitHub branch protection displays the `name:` field of each job, not the YAML key. `Shell smoke (zsh, macos-latest)` (the name) corresponds to the `shell-smoke` matrix job. Using the YAML key in the search box will not match.
