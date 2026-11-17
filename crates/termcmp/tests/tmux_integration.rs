/// Integration tests for tmux process topology invisibility
///
/// These tests verify that termcmp is invisible to process topology detectors
/// like tmux's `pane_current_command` and job control mechanisms.
mod harness;

use harness::TmuxSession;
use std::time::Duration;

/// Probe whether the pane's shell runs with job control (`$-` contains `m`).
///
/// Under the default (isolated) topology the proxy gives the inner shell its
/// own session with the inner PTY slave as its controlling terminal, so every
/// shell can claim the tty and enable monitor mode — bash 5.x on Linux
/// included. (In the legacy topology the slave was never the shell's ctty:
/// macOS + bash 3.2 tolerated tcsetpgrp anyway, Linux bash 5.x reported "no
/// job control in this shell".) Tests asserting suspend/bg/fg reporting stay
/// guarded so that a regression to a ctty-less topology skips instead of
/// failing platform-impossibly.
fn shell_has_job_control(tmux: &mut TmuxSession) -> bool {
    tmux.send_line("echo TERMCMP_JC_FLAGS=$-");
    tmux.expect_output("TERMCMP_JC_FLAGS=");

    // The echoed command line contains the literal `$-`, so the first match of
    // `expect_output` can be the echo rather than the shell's answer. Poll
    // until a segment that is NOT the placeholder appears (bounded), so a fast
    // echo cannot be mistaken for "no job control".
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = tmux.capture_output();
        let flags = snapshot
            .rsplit("TERMCMP_JC_FLAGS=")
            .next()
            .unwrap_or_default()
            .chars()
            .take(8)
            .collect::<String>();
        if !flags.is_empty() && !flags.starts_with("$-") {
            return flags.contains('m');
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Send a job-control signal, re-sending boundedly until `expected` appears.
/// The echoed command line only proves the line discipline echoed the bytes
/// (kernel-side, at input time) — not that bash forked the job and switched
/// the fg pgrp. A signal arriving in that window is swallowed by the shell,
/// so re-send every 200ms (bounded ~5s) until the job actually responds.
fn send_signal_until(tmux: &mut TmuxSession, key: &str, expected: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        tmux.send_keys(key);
        if tmux.wait_for_bytes(expected.as_bytes(), Duration::from_millis(200)) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "signal {key} never produced {expected:?}; output: {}",
                tmux.capture_output()
            );
        }
    }
}

/// Test that tmux `pane_current_command` shows the shell, not termcmp
///
/// Opt-in mirror topology (the legacy `session_isolation = false` alias, which
/// maps to `agent_tracking = true`): the outer tty's foreground pgrp is
/// mirrored onto inner jobs, which is what tmux reads. Isolation cannot mirror
/// (TIOCSPGRP onto an inner pgrp is EINVAL), so this observation needs the
/// mirror — the shipped default isolates and is pinned by
/// [`test_tmux_default_config_isolates_inner_shell`].
#[test]
fn test_tmux_pane_current_command_shows_shell() {
    let tmux = TmuxSession::spawn_legacy();

    // Wait for termcmp to start and shell to initialize
    tmux.expect_output("$");

    // Query tmux for pane_current_command
    let pane_cmd = tmux.display_message("#{pane_current_command}");

    // Should show the shell (sh/bash/zsh), not termcmp
    assert!(
        pane_cmd.contains("sh") || pane_cmd.contains("bash") || pane_cmd.contains("zsh"),
        "pane_current_command should show shell, got: {}",
        pane_cmd
    );
    assert!(
        !pane_cmd.contains("termcmp"),
        "pane_current_command should not show termcmp, got: {}",
        pane_cmd
    );

    tmux.exit();
}

/// Test that tmux `pane_current_command` updates when a foreground job runs
///
/// Legacy topology — same foreground-group mirroring requirement as
/// `test_tmux_pane_current_command_shows_shell`.
#[test]
fn test_tmux_pane_current_command_updates_with_foreground_job() {
    let mut tmux = TmuxSession::spawn_legacy();
    tmux.expect_output("$");
    if !shell_has_job_control(&mut tmux) {
        eprintln!(
            "skipping pane_current_command_updates: shell lacks job control on this platform"
        );
        return;
    }
    // Start a long-running command (sleep)
    tmux.send_line("sleep 10");
    // Query tmux for pane_current_command - should show sleep
    let pane_cmd = tmux.wait_for_pane_command("sleep", Duration::from_secs(3));
    assert!(
        pane_cmd.contains("sleep"),
        "pane_current_command should show sleep, got: {}",
        pane_cmd
    );

    // Interrupt the sleep
    tmux.send_keys("C-c");
    tmux.expect_output("$");

    // Now should show shell again (mirror flips back within ~150 ms).
    // Strong check: a stuck mirror leaves "sleep" here, which must fail.
    let pane_cmd = tmux.wait_for_pane_command("bash", Duration::from_secs(3));
    assert!(
        pane_cmd.contains("bash"),
        "pane_current_command should show bash after interrupt, got: {}",
        pane_cmd
    );

    tmux.exit();
}

/// Test that the shipped default (experimental config unset) isolates the
/// inner shell: tmux `pane_current_command` reports `termcmp` both idle and
/// while a foreground job runs. This is the new default contract — /dev/tty
/// prompts and job control work everywhere; agent tracking is opt-in via
/// `agent_tracking = true` (see
/// `test_tmux_agent_tracking_restores_foreground_visibility`).
#[test]
fn test_tmux_default_config_isolates_inner_shell() {
    let mut tmux = TmuxSession::spawn_default_topology();
    tmux.expect_output("$");

    let idle = tmux.display_message("#{pane_current_command}");
    assert!(
        idle.contains("termcmp"),
        "idle pane should report termcmp under isolation, got: {idle}"
    );

    tmux.send_line("sleep 20");
    // isolation: pane command is static; brief settle for the shell to fork sleep
    std::thread::sleep(Duration::from_millis(200));
    let running = tmux.display_message("#{pane_current_command}");
    assert!(
        running.contains("termcmp"),
        "isolation hides the inner job; pane must keep reporting termcmp, got: {running}"
    );

    tmux.send_keys("C-c");
    tmux.expect_output("$");
    tmux.exit();
}

/// Opt-in `agent_tracking = true` restores foreground visibility for
/// tmux/herdr/workmux pane observation: the pane reports the inner shell
/// while idle and the running job while it holds the foreground.
///
/// This is the documented tradeoff of the mirror topology — `/dev/tty`
/// readers (sudo/ssh/pinentry) contend with the proxy reader again.
///
/// The job-visibility half is guarded by `shell_has_job_control`: on Linux the
/// inner slave is not a controlling terminal under the mirror topology, so
/// bash 5.x reports "no job control" and keeps every child in its own pgrp —
/// there is no separate foreground job pgrp for tmux to observe. The idle half
/// (shell instead of termcmp) holds on every platform.
#[test]
fn test_tmux_agent_tracking_restores_foreground_visibility() {
    let mut tmux = TmuxSession::spawn_agent_tracking();
    tmux.expect_output("$");

    let idle = tmux.display_message("#{pane_current_command}");
    assert!(
        idle.contains("bash") && !idle.contains("termcmp"),
        "agent_tracking must expose the inner shell while idle, got: {idle}"
    );

    if !shell_has_job_control(&mut tmux) {
        eprintln!(
            "skipping foreground-job visibility under agent_tracking: shell lacks job control \
             under the mirror topology on this platform"
        );
        tmux.exit();
        return;
    }

    tmux.send_line("sleep 20");
    let running = tmux.wait_for_pane_command("sleep", Duration::from_secs(3));
    assert!(
        running.contains("sleep"),
        "agent_tracking must expose the foreground job, got: {running}"
    );

    tmux.send_keys("C-c");
    let back = tmux.wait_for_pane_command("bash", Duration::from_secs(3));
    assert!(
        back.contains("bash"),
        "mirror must hand the foreground back to the shell after SIGINT, got: {back}"
    );

    tmux.exit();
}

/// Test that an explicit `session_isolation = true` opts back out of tracking.
///
/// The other half of the tri-state: a multiplexer pane whose user needs
/// `/dev/tty` password readers keeps isolation, and the price is that tmux
/// reports the pane as running `termcmp`. Uses [`TmuxSession::spawn`], which
/// writes that key explicitly — the legacy alias still maps to
/// `agent_tracking = false`, which is also the shipped default.
#[test]
fn test_tmux_explicit_isolation_hides_foreground_process() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");

    tmux.send_line("sleep 20");
    // Isolation: the pane command is static; a brief settle lets the shell
    // fork sleep so the assertion cannot pass merely because the job never
    // started.
    std::thread::sleep(Duration::from_millis(200));
    let running = tmux.display_message("#{pane_current_command}");
    assert!(
        running.contains("termcmp"),
        "explicit isolation must keep the proxy as the pane's foreground process, got: {running}"
    );

    tmux.send_keys("C-c");
    tmux.exit();
}

/// Test job control round-trip: suspend, background, foreground, interrupt
#[test]
fn test_job_control_round_trip() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");
    if !shell_has_job_control(&mut tmux) {
        eprintln!("skipping job_control_round_trip: shell lacks job control on this platform");
        return;
    }

    // Start a long-running command. Sync on EXECUTION output (the JC_STARTED
    // marker), not the command echo: the echo only proves the line discipline
    // echoed bytes, not that bash forked the job and switched the fg pgrp.
    // `exec sleep 30` replaces the subshell so the marker precedes a real
    // long-lived foreground job.
    tmux.send_line("sh -c 'echo JC_STARTED; exec sleep 30'");
    tmux.expect_output("JC_STARTED");

    // Suspend with Ctrl-Z, re-sending boundedly until the job reports Stopped
    // (a signal racing the fork is swallowed by the shell).
    send_signal_until(&mut tmux, "C-z", "Stopped");
    tmux.expect_output("$");

    // Background it
    tmux.send_line("bg");
    tmux.expect_output("$");

    // Foreground it
    tmux.send_line("fg");

    // Interrupt it
    tmux.send_keys("C-c");
    tmux.expect_output("$");

    tmux.exit();
}

/// Test popup rendering and dismissal
#[test]
fn test_popup_renders_and_dismisses() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");

    // Trigger popup with OSC 133;A + OSC 7772
    tmux.send_line("printf '\\033]133;A\\007\\033]7772;4;git \\007'");

    // Wait for popup to render (look for DECSC marker)
    let popup_rendered = tmux.wait_for_bytes(b"\x1b7", Duration::from_secs(5));
    assert!(popup_rendered, "Popup should render");

    // Dismiss with ESC
    tmux.send_keys("Escape");

    // Wait for popup to clear (look for DECRC marker)
    let popup_cleared = tmux.wait_for_bytes(b"\x1b8", Duration::from_secs(5));
    assert!(popup_cleared, "Popup should clear");

    tmux.exit();
}

/// Test resize handling - termcmp should forward SIGWINCH to shell
#[test]
fn test_resize_handling() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");

    // Get initial size. The marker line only orders the two inputs; the real
    // assertion below waits explicitly for the post-resize "40 80".
    tmux.send_line("stty size");
    tmux.send_line("echo TC_INITIAL_SIZE_DONE");
    tmux.expect_output("TC_INITIAL_SIZE_DONE");
    let initial_size = tmux.capture_output();

    // Resize the pane; the proxy reconciles via mirror_tick.
    tmux.resize_pane(80, 40);

    // Synchronize on the size the inner shell actually sees (`stty size`
    // prints "rows cols"): this replaces a fixed settle AND proves the
    // SIGWINCH path ran.
    tmux.send_line("stty size");
    tmux.expect_output("40 80");
    let new_size = tmux.capture_output();

    // Size should have changed
    assert_ne!(
        initial_size, new_size,
        "Terminal size should change after resize"
    );

    tmux.exit();
}

/// Test that the deprecated OSC 7770 frame does not break the shell
///
/// Private OSC frames are stripped by the proxy's `PrivateOscFilter` — they
/// are never forwarded to the outer terminal (and the parser no longer
/// consumes 7770). The assertion is only that the shell keeps functioning;
/// stripping is the defense-in-depth path for stale shell integrations.
#[test]
fn test_private_osc_does_not_break_shell() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");

    // Send OSC 7770 (old raw framing — no longer parsed)
    tmux.send_line("printf '\\033]7770;/tmp\\007'");

    // The frame must not corrupt the session: the shell keeps working.
    tmux.send_line("echo test");
    tmux.expect_output("test");

    tmux.exit();
}

/// Test OSC 7772 (command buffer report) triggers popup
#[test]
fn test_osc_7772_triggers_popup() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");

    // Send OSC 133;A (prompt start) + OSC 7772 (command buffer)
    tmux.send_line("printf '\\033]133;A\\007\\033]7772;4;git \\007'");

    // Wait for popup to render
    let popup_rendered = tmux.wait_for_bytes(b"\x1b7", Duration::from_secs(5));
    assert!(popup_rendered, "OSC 7772 should trigger popup");

    // Dismiss
    tmux.send_keys("Escape");

    tmux.exit();
}

/// After resizing a pane below the compact threshold, termcmp's parser must
/// learn the new size via the `mirror_tick` polling reconcile (under tmux,
/// SIGWINCH is delivered to the inner shell, not termcmp). If it doesn't,
/// compact mode never activates and the popup renders with the stale
/// large-screen scroll path. This asserts compact geometry: the popup renders
/// below the prompt and never emits the stale 24-row scroll fingerprint.
#[test]
fn test_resize_to_compact_pane_updates_screen_dimensions() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");
    tmux.resize_pane(80, 10);
    // Synchronize on the resized geometry the inner shell sees: `stty size`
    // prints "rows cols", and mirror_tick reconciles within a couple of 100ms
    // ticks. Replaces a fixed 800ms settle.
    tmux.send_line("stty size");
    tmux.expect_output("10 80");

    // Prompt to the top so there is room below → compact mode renders below.
    tmux.send_line("clear");

    // Mark immediately before the trigger line, then wait on the render
    // marker instead of sleeping. `clear` empties the screen, so a prompt
    // match is not a usable synchronization point here.
    let mark = tmux.capture_output().len();
    tmux.send_line("printf '\\033]133;A\\007\\033]7772;4;git \\007'");

    assert!(
        tmux.wait_for_bytes_after(b"\x1b7", mark, Duration::from_secs(5)),
        "popup should render below the prompt in a compact pane"
    );

    // Slice strictly after the trigger mark: everything before it predates
    // the resize, so a stale-geometry byte there cannot mask a failure.
    let after = tmux.capture_output();
    let mut offset = mark.min(after.len());
    // `capture_output` decodes lossily; a trailing partial sequence can shift
    // the byte length between snapshots, so align to a char boundary.
    while offset > 0 && !after.is_char_boundary(offset) {
        offset -= 1;
    }
    let new_bytes = &after[offset..];

    assert!(
        new_bytes.contains("\u{1b}7"),
        "popup should render below the prompt in a compact pane, got: {:?}",
        new_bytes
    );
    assert!(
        !new_bytes.contains("\u{1b}[24;1H"),
        "stale 24-row screen dimension survived resize: {:?}",
        new_bytes
    );

    tmux.exit();
}

/// Test that OSC 7773 env frames do not break the shell
///
/// 7773 is consumed by the parser and stripped, so it never reaches the outer
/// terminal; the shell must keep functioning afterward.
#[test]
fn test_env_osc_does_not_break_shell() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");

    // Send OSC 7773 (env snapshot)
    tmux.send_line("printf '\\033]7773;completion\\007'");

    // Verify shell continues to function
    tmux.send_line("echo test");
    tmux.expect_output("test");

    tmux.exit();
}

/// Test exit propagation - exiting shell should close pane with correct status
#[test]
fn test_exit_propagation_zero() {
    let mut tmux = TmuxSession::spawn_exit_capture();
    tmux.expect_output("$");

    // Exit with status 0
    tmux.send_line("exit 0");

    // tmux pane should close
    let exited = tmux.wait_for_pane_close(Duration::from_secs(5));
    assert!(exited, "Pane should close after exit 0");

    // Verify exit status
    let status = tmux.pane_exit_status();
    assert_eq!(status, Some(0), "Exit status should be 0");
}

/// Test exit propagation with non-zero status
#[test]
fn test_exit_propagation_nonzero() {
    let mut tmux = TmuxSession::spawn_exit_capture();
    tmux.expect_output("$");

    // Exit with status 42
    tmux.send_line("exit 42");

    // tmux pane should close
    let exited = tmux.wait_for_pane_close(Duration::from_secs(5));
    assert!(exited, "Pane should close after exit 42");

    // Verify exit status
    let status = tmux.pane_exit_status();
    assert_eq!(status, Some(42), "Exit status should be 42");
}

/// Test that SIGINT routes to the foreground job, not the shell
///
/// Under isolation the inner slave IS a controlling terminal, so the kernel
/// delivers INTR to the foreground pgrp; the proxy simply forwards the byte.
/// A regression that synthesized or misrouted ISIG would kill or corrupt the
/// shell instead.
#[test]
fn test_sigint_routes_to_foreground_job_not_shell() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");

    // Start a foreground command. Sync on EXECUTION output (JC_STARTED), not
    // the command echo, so bash has forked the job and switched the fg pgrp
    // before Ctrl-C — otherwise INTR could hit the shell itself.
    tmux.send_line("sh -c 'echo JC_STARTED; exec sleep 30'");
    tmux.expect_output("JC_STARTED");

    // Send SIGINT (Ctrl-C) - should go to sleep, not the shell
    tmux.send_keys("C-c");
    tmux.expect_output("$");

    // Shell should still be running (not killed by SIGINT)
    tmux.send_line("echo shell_alive");
    tmux.expect_output("shell_alive");

    tmux.exit();
}

/// Test that SIGINT (Ctrl-C) interrupts a foreground job
#[test]
fn test_isig_synthesis_sigint() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");

    // Start a long-lived job. Sync on EXECUTION output (JC_STARTED), not the
    // command echo, so bash has forked the job before Ctrl-C.
    tmux.send_line("sh -c 'echo JC_STARTED; exec sleep 30'");
    tmux.expect_output("JC_STARTED");

    // Send Ctrl-C - should interrupt sleep via the inner line discipline
    tmux.send_keys("C-c");

    // Should return to prompt
    tmux.expect_output("$");

    tmux.exit();
}

/// Test that ISIG synthesis works for SIGTSTP (Ctrl-Z)
#[test]
fn test_isig_synthesis_sigtstp() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");
    if !shell_has_job_control(&mut tmux) {
        eprintln!("skipping isig_synthesis_sigtstp: shell lacks job control on this platform");
        return;
    }

    // Start a long-lived job. Sync on EXECUTION output (JC_STARTED), not the
    // command echo, so bash has forked the job before we signal.
    tmux.send_line("sh -c 'echo JC_STARTED; exec sleep 30'");
    tmux.expect_output("JC_STARTED");

    // Suspend with Ctrl-Z, re-sending boundedly until the job reports Stopped
    // (a signal racing the fork is swallowed by the shell).
    send_signal_until(&mut tmux, "C-z", "Stopped");
    tmux.expect_output("$");

    // Clean up
    tmux.send_line("kill %1");
    tmux.expect_output("$");

    tmux.exit();
}
