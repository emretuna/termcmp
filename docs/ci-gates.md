# CI Gates

## Overview

Three CI gates live in `.github/workflows/ci.yml`: binary size, zsh/ZLE shell smoke, and fish shell smoke. Two more gates live outside `ci.yml`: `cargo-deny check` runs alongside `cargo audit` in [`.github/workflows/audit.yml`](../.github/workflows/audit.yml) (Cargo manifest / lockfile changes and a weekly cron), and `Smoke packaged artifacts` runs in [`.github/workflows/release.yml`](../.github/workflows/release.yml) on every release tag — those two are documented under [Audit workflow](#audit-workflow) and [Release-only gates](#release-only-gates) below. Benchmark-regression checking is intentionally **not** a CI gate — it is run manually at release time (see [Release-time benchmark checking](#release-time-benchmark-checking) below). The gates are wired via `needs:` dependencies so they run only after the `check` job succeeds.

---

## Gates

### Binary size gate

**Job name in CI:** `Binary size gate`
**YAML key:** `binary-size-gate`
**Trigger:** `needs: [check]` — runs after the `check` job succeeds.

**Purpose:** enforces two independent size constraints on the release binary, and records the measured size as a workflow artifact:

1. **Recorded size artifact** — every CI run writes `size.txt` (single integer, bytes, with trailing newline — same format as [`benchmarks/binary-size-baseline.txt`](../benchmarks/binary-size-baseline.txt)) and uploads it as the `termcmp-size` workflow artifact. PR reviewers and the release author can download the artifact from the run summary page to see the exact byte count without re-running the job. The size is computed with `wc -c` rather than `du -b` because BSD `du` on `macos-latest` runners has no `-b` flag.
2. **Absolute ceiling (11 MB)** — the binary must not exceed 11 MB. Raising it requires an explicit plan amendment. The ceiling was lowered from 110 MB to 11 MB when the embedded Fig completion-spec corpus was removed; the release binary dropped from ~21 MB to ~7.5 MB, and the new ceiling is the measured size rounded up to the next 5 MB step plus ~10% headroom.
3. **Per-phase delta budget (default +2 MB, label override +5 MB)** — the binary must not have grown by more than the delta budget since the size recorded in [`benchmarks/binary-size-baseline.txt`](../benchmarks/binary-size-baseline.txt). The default budget is `PHASE_BUDGET` (`2MB`). On `pull_request` events, applying the **`binary-size-allow-delta`** label raises the budget to `LABEL_OVERRIDE_BUDGET` (`5MB`) for that PR only — the gate's "Pick delta budget" step inspects `github.event.pull_request.labels` and emits the override decision in the job log. Label add/remove events rerun the PR workflow, so adding the override after a failed size gate is enough to re-evaluate the current label set. Pushes to trunk branches (`master` or `main`) always use the strict 2 MB budget (no PR labels to read). The label is the explicit acknowledgement that a PR is expected to grow the binary; without it, growth >2 MB fails the gate. Update the baseline file in the same PR (see "Baseline maintenance" below) once the change is justified — the override is for the merge, not for permanent tolerance. Create the label one-time via `gh label create binary-size-allow-delta --description "Raise binary-size delta budget from 2MB to 5MB for this PR" --color FBCA04`; the gate fails closed (strict 2 MB) if the label is missing.

**Stripping note.** The release profile sets `strip = "symbols"`. The size measurement in this gate reflects the stripped binary, and [`benchmarks/binary-size-baseline.txt`](../benchmarks/binary-size-baseline.txt) is captured from the same stripped build — baseline and live measurement use the same shape. Toggling `strip` off would invalidate the baseline.

**Failure modes:**

- Absolute ceiling failure: binary size exceeds 11 MB.
- Delta budget failure: binary grew by more than the selected budget (2 MB strict / 5 MB with label) since the baseline was recorded.

**Status today:** production-live and **passing**. The binary-size baseline is 7,539,264 bytes (~7.2 MiB), well below the 11 MB absolute ceiling. Removing the embedded Fig spec corpus shrank the binary from ~21 MB to ~7.5 MB. The artifact upload + label override were added in `ux-9b` Phase 4. Ready to add to branch protection.

**How to debug locally:**

```bash
cargo build --release
scripts/check-binary-size.sh --absolute-max 11MB
scripts/check-binary-size.sh --delta-max 2MB
# Equivalent of the artifact upload step:
wc -c < target/release/termcmp | tr -d ' ' > size.txt
```

For exploratory size attribution (which crate / function dominates the binary), run `cargo bloat`:

```bash
cargo install cargo-bloat                    # one-time
cargo bloat --release --crates                # crate-level breakdown
cargo bloat --release -n 30                   # top 30 functions by size
cargo bloat --release --filter '^termcmp'      # focus on a path prefix
```

`cargo bloat` is a debugging tool, **not a CI gate** — its codegen-unit estimates are too coarse for a hard fail. Use it locally when investigating an unexpected binary growth flagged by the delta gate.

**Baseline maintenance:** when a change legitimately grows the binary, update the baseline file. The script accepts both formats (bare integer or `du -b` output) but the canonical form for macOS-latest CI runners is the bare-integer `wc -c` output:

```bash
wc -c < target/release/termcmp | tr -d ' ' > benchmarks/binary-size-baseline.txt
# or equivalently on a GNU coreutils machine:
du -b target/release/termcmp > benchmarks/binary-size-baseline.txt
```

---

### Zsh/ZLE shell smoke

**Job name in CI:** `zsh/ZLE shell smoke`
**YAML key:** `zsh-zle-smoke`
**Trigger:** `needs: [check]` — runs after the `check` job succeeds. Blocking gate.

**Purpose:** exercises the production zsh shell integration (`shell/termcmp.zsh`) under a real `/bin/zsh --no-rcs` and asserts that the ZLE widget `_tc_report_buffer` emits OSC 7772 frames with the correct percent-encoding. Catches regressions in the encoder for characters that would otherwise corrupt a frame mid-stream (semicolons, BEL `0x07`, ESC `0x1B`, literal `%`), validates UTF-8 round-trip, exercises the OSC 7 path encoder, and verifies the `TERMCMP_ACTIVE` gate guard turns the widget into a no-op outside the proxy. The matching runtime parser path lives in `parser` and is unit-tested in Rust; this gate validates the shell-side producer end-to-end against a real zsh so a shell-script regression cannot ship undetected by `cargo test`.

**Failure modes:**

- Encoder regression: a frame is missing percent-encoding for one of the documented byte classes.
- Gate guard regression: `_tc_report_buffer` emits OSC 7772 when `TERMCMP_ACTIVE` is unset.
- Environment failure: `zsh` is not on `PATH`, or `shell/termcmp.zsh` is missing.

**Status today:** production-live. The check runs on every PR and trunk push. Ready to add to branch protection.

**How to debug locally:**

```bash
scripts/check-zsh-zle-smoke.sh
```

---

### Fish shell smoke

**Job name in CI:** `fish shell smoke`
**YAML key:** `fish-smoke`
**Trigger:** `needs: [check]` — runs after the `check` job succeeds. Blocking gate.

**Purpose:** exercises the production fish shell integration (`shell/termcmp.fish`) under a real `fish --no-config` and asserts that `_termcmp_report_buffer` emits OSC 7772 frames with correct percent-encoding. Catches regressions in the byte-level encoder for characters that would corrupt a frame (semicolons, BEL `0x07`, ESC `0x1B`, literal `%`), validates multibyte UTF-8 round-trip, exercises the OSC 7 path encoder, and verifies binding-install idempotency (re-sourcing must not stack duplicate bindings). Unlike the zsh smoke, fish's `commandline` builtin requires an interactive tty, so the test shadows it with a controlled buffer/cursor pair — the production `_termcmp_report_buffer` and `_termcmp_urlencode_buffer` run unmodified. Fish's `_termcmp_report_buffer` is intentionally not gated on `TERMCMP_ACTIVE` (the binding only exists when the integration is sourced, which `init.fish` arranges under the proxy).

**Failure modes:**

- Encoder regression: a frame is missing percent-encoding for one of the documented byte classes.
- UTF-8 regression: multibyte characters are encoded by codepoint instead of by byte.
- Binding regression: re-sourcing stacks duplicate `_termcmp_report_buffer` bindings.
- Environment failure: `fish` is not on `PATH`, or `shell/termcmp.fish` is missing.

**Status today:** production-live. The check runs on every PR and trunk push. Ready to add to branch protection.

**How to debug locally:**

```bash
scripts/check-fish-smoke.sh
```

---

## Release-time benchmark checking

Benchmark regression is **not** enforced on every PR. Hosted runner variance (±15–20% on single-threaded latency benches) makes CI-gated benchmarking noisy enough that the signal-to-noise ratio doesn't justify the minutes spent. Instead, the release process runs benchmarks locally on a quiet machine and records the numbers in the release PR.

The tooling is preserved:

- [`.github/workflows/bench.yml`](../.github/workflows/bench.yml) — manual `workflow_dispatch` job that runs `cargo bench --workspace` and uploads Criterion reports as an artifact.
- [`scripts/check-bench.sh`](../scripts/check-bench.sh) — threshold-based comparator against a saved Criterion baseline.
- [`benchmarks/`](../benchmarks/) — per-release report files (`v<version>.md`) plus `baseline-pre-js-port.json` for historical diffs.

**Release workflow:**

```bash
cargo bench --workspace -- --save-baseline release-<prev>    # one-time, on the prior release tag
cargo bench --workspace -- --baseline release-<prev>         # on the release candidate
scripts/check-bench.sh --threshold 10                         # optional gate for the release author
```

Include the Criterion summary and any regression >10% in `benchmarks/v<version>.md` as part of the release PR per the process in [`CLAUDE.md`](../CLAUDE.md#benchmarking).

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

The `release` workflow ([`.github/workflows/release.yml`](../.github/workflows/release.yml)) runs on `push` of any version-shaped tag. It hosts one smoke gate that is **not** part of CI and only ever runs at release time.

### Smoke packaged artifacts

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
# Approximate the packaged path: build, tar, run the smoke script against
# the archive. The smoke script itself is the canonical reproducer:
scripts/check-release-artifact-smoke.sh <path/to/termcmp-*-apple-darwin.tar.{gz,xz}>
```

---

## Branch-protection configuration

These steps require repo admin access. Without them the gates run but **do not block merge**.

1. Go to <https://github.com/EmreTuna/termcmp/settings/branches>.
2. Edit the branch protection rule for `master`, or create one if none exists.
3. Enable **"Require status checks to pass before merging"**.
4. In the status check search box, add the checks listed as "Ready to add" in the table below by their **exact display names** (the human-readable `name:` values from the CI YAML, not the YAML job keys).
5. Save the rule.

These checks are added **alongside** any existing required checks (e.g. `Check`, `Test (macos-latest)`, `Test (ubuntu-latest)`, `Clippy`, `Format`, `MSRV (1.86)`). They replace nothing.

### Readiness table

| Gate | Branch protection status |
|---|---|
| `Binary size gate` | Ready to add. |
| `zsh/ZLE shell smoke (macos-latest)` / `zsh/ZLE shell smoke (ubuntu-latest)` | Ready to add. |
| `fish shell smoke (macos-latest)` / `fish shell smoke (ubuntu-latest)` | Ready to add. |
| `cargo audit` (audit workflow — covers both `cargo audit` and `cargo deny check` steps) | Path-filtered to Cargo manifest / lockfile changes. Branch protection cannot express "required only when this path changed"; leave unenforced and let the workflow's own path filter gate it. |
| `Smoke packaged artifacts` (release workflow) | Release-only — not a PR check. Gates the `host` job inside `release.yml`; cannot meaningfully be added to PR branch protection. |

> **Note on job names vs. YAML keys:** GitHub branch protection displays the `name:` field of each job, not the YAML key. `Binary size gate` (the name) corresponds to `binary-size-gate` (the key). Using the YAML key in the search box will not match.

---

## FAQ

**"Why is the ceiling 11 MB?"**

The ceiling was lowered from 110 MB to 11 MB when the embedded Fig completion-spec corpus was removed from the binary: the release binary shrank from ~21 MB (baseline 21,383,696 bytes) to 7,539,264 bytes (~7.2 MiB). 11 MB is the measured size rounded up to the next 5 MB step plus ~10% headroom. The delta budget (`PHASE_BUDGET=2MB`) still handles the near-term constraint — "don't grow from the current baseline". These are two independent checks; both must pass.

**"When should I apply the `binary-size-allow-delta` label?"**

Only when a PR is *expected* to grow the binary by more than 2 MB and the growth is reviewed and justified — for example, adding a new built-in provider with substantial static data, or opting into a new compile-time feature. The label raises the delta gate from 2 MB to 5 MB for that PR. The 11 MB absolute ceiling still applies; the label cannot override it. Pushes to trunk branches (`master` or `main`) always use the strict 2 MB budget (no PR labels to read), so the label only affects the PR build that introduces the change. Update [`benchmarks/binary-size-baseline.txt`](../benchmarks/binary-size-baseline.txt) in the same PR — the override exists to admit a single justified jump, not to live with permanent slack.

**"Can I skip a gate on a specific PR?"**

No. Required status checks are all-or-nothing. For a legitimate one-off exception (e.g. an unavoidable binary size overrun covered by a plan amendment), the admin must:

1. Temporarily remove the specific status check from branch protection.
2. Merge the PR.
3. Re-add the status check immediately after.

This is an emergency procedure. Document the exception in the PR description and in the relevant plan file.

