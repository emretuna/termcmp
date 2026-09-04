/// Integration tests for tmux process topology invisibility
///
/// These tests verify that termcmp is invisible to process topology detectors
/// like tmux's `pane_current_command` and job control mechanisms.
mod harness;

use harness::TmuxSession;
use std::time::Duration;

/// Test that tmux `pane_current_command` shows the shell, not termcmp
#[test]
fn test_tmux_pane_current_command_shows_shell() {
    let tmux = TmuxSession::spawn();

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
#[test]
fn test_tmux_pane_current_command_updates_with_foreground_job() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");
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

/// Test job control round-trip: suspend, background, foreground, interrupt
#[test]
fn test_job_control_round_trip() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");

    // Start a long-running command
    tmux.send_line("sleep 5");
    std::thread::sleep(Duration::from_millis(300));

    // Suspend with Ctrl-Z
    tmux.send_keys("C-z");
    tmux.expect_output("Stopped");
    tmux.expect_output("$");

    // Background it
    tmux.send_line("bg");
    tmux.expect_output("$");

    // Foreground it
    tmux.send_line("fg");
    std::thread::sleep(Duration::from_millis(300));

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

    // Get initial size
    tmux.send_line("stty size");
    let initial_size = tmux.capture_output();

    // Resize the pane
    tmux.resize_pane(80, 40);
    std::thread::sleep(Duration::from_millis(500));

    // Query size again
    tmux.send_line("stty size");
    let new_size = tmux.capture_output();

    // Size should have changed
    assert_ne!(
        initial_size, new_size,
        "Terminal size should change after resize"
    );

    tmux.exit();
}

/// Test OSC 7770 (cwd report) is forwarded
#[test]
fn test_osc_7770_forwarded() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");

    // Send OSC 7770 (cwd report)
    tmux.send_line("printf '\\033]7770;/tmp\\007'");

    // termcmp should forward this to the outer terminal
    // We can't directly observe this in the test, but we verify no errors occur
    // and the shell continues to function
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
    // mirror_tick polls at 100ms; give it several ticks to reconcile.
    std::thread::sleep(Duration::from_millis(800));

    // Prompt to the top so there is room below → compact mode renders below.
    tmux.send_line("clear");
    std::thread::sleep(Duration::from_millis(500));

    let before = tmux.capture_output();
    tmux.send_line("printf '\\033]133;A\\007\\033]7772;4;git \\007'");
    std::thread::sleep(Duration::from_millis(1500));

    let after = tmux.capture_output();
    let new_bytes = &after[before.len().min(after.len())..];

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

/// Test OSC 7773 (completion) is forwarded
#[test]
fn test_osc_7773_forwarded() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");

    // Send OSC 7773 (completion)
    tmux.send_line("printf '\\033]7773;completion\\007'");

    // Verify shell continues to function
    tmux.send_line("echo test");
    tmux.expect_output("test");

    tmux.exit();
}

/// Test exit propagation - exiting shell should close pane with correct status
#[test]
fn test_exit_propagation_zero() {
    let mut tmux = TmuxSession::spawn();
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
    let mut tmux = TmuxSession::spawn();
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

/// Test that shell's foreground process group is correctly mirrored
#[test]
fn test_foreground_process_group_mirroring() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");

    // Start a command that will be in the foreground
    tmux.send_line("sleep 10");
    std::thread::sleep(Duration::from_millis(300));

    // Send SIGINT (Ctrl-C) - should go to sleep, not the shell
    tmux.send_keys("C-c");
    tmux.expect_output("$");

    // Shell should still be running (not killed by SIGINT)
    tmux.send_line("echo shell_alive");
    tmux.expect_output("shell_alive");

    tmux.exit();
}

/// Test that ISIG synthesis works for SIGINT
#[test]
fn test_isig_synthesis_sigint() {
    let mut tmux = TmuxSession::spawn();
    tmux.expect_output("$");

    // Start sleep
    tmux.send_line("sleep 10");
    std::thread::sleep(Duration::from_millis(300));

    // Send Ctrl-C - should interrupt sleep via ISIG synthesis
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

    // Start sleep
    tmux.send_line("sleep 10");
    std::thread::sleep(Duration::from_millis(300));

    // Send Ctrl-Z - should suspend sleep via ISIG synthesis
    tmux.send_keys("C-z");

    // Should see "Stopped" and return to prompt
    tmux.expect_output("Stopped");
    tmux.expect_output("$");

    // Clean up
    tmux.send_line("kill %1");
    tmux.expect_output("$");

    tmux.exit();
}
