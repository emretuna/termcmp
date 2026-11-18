//! Regression tests for foreground-pgrp mirroring on a **bare pty**.
//!
//! The tmux integration test (`crates/termcmp/tests/tmux_integration.rs`)
//! cannot catch SIGTTOU-gating regressions: tmux-spawned shells inherit
//! `SIGTTOU=ignored` across exec, so background `TIOCSPGRP` calls succeed
//! there even when the proxy is broken. These tests drive `run_proxy`
//! directly on an openpty pair with no multiplexer in between — the exact
//! topology herdr login panes and raw ptys use — where every mirror update
//! after the initial one used to fail with EIO, freezing the outer tty's
//! fg pgrp on the inner shell.
//!
//! Topology (production invocation order lives in `src/bin/pty_proxy_child.rs`):
//!
//! ```text
//! test ── master ── openpty ── slave = outer tty
//!                    pre_exec: setsid-free dup2(0,1,2) + close master
//!                    exec pty-proxy-child (its own setsid + TIOCSCTTY)
//!                    └─ run_proxy(bash) ── inner pty ── bash
//! ```
//!
//! The inner shell is `/bin/bash` (present on both CI platforms) rather than
//! `fish`: on Linux the proxy force-keeps session isolation for fish (fish 4.x
//! hard-exits without a controlling terminal and there is no Linux ttyshim),
//! which would defeat the mirror assertions this file exists to make.
//!
//! The child is a separate binary because fork-from-the-multithreaded cargo
//! harness is unsafe; `Command::pre_exec` does the fd wiring in the sanctioned
//! fork-before-exec window and hands off to a clean single-threaded image.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Locate the inner shell: `/bin/bash` when present, else skip (local dev),
/// matching the smoke-script convention used by the other integration tests.
/// bash exists on both CI platforms (macOS ships 3.2, Linux 5.x).
fn find_bash() -> Option<OsString> {
    if std::path::Path::new("/bin/bash").is_file() {
        Some(OsString::from("/bin/bash"))
    } else {
        None
    }
}

/// A running proxy child wired to an openpty pair we hold the master of.
struct Harness {
    master: std::os::fd::RawFd,
    child: std::process::Child,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let pid = self.child.id() as i32;
        unsafe {
            // The child called setsid(), so its pgid == pid; signalling the
            // process group also reaches the shell and any leftover job (sleep).
            libc::kill(pid, libc::SIGTERM);
            libc::kill(-pid, libc::SIGTERM);
        }
        // The proxy's graceful shutdown tears down tasks and restores the
        // tty, which can take several seconds; reap non-blockingly, then
        // escalate. Every wait here is bounded so Drop can never hang the
        // test harness.
        let mut reaped = false;
        let grace = Instant::now() + Duration::from_secs(7);
        while Instant::now() < grace {
            if unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } == pid {
                reaped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        if !reaped {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::kill(-pid, libc::SIGKILL);
            }
            let hard = Instant::now() + Duration::from_secs(2);
            while Instant::now() < hard {
                if unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } == pid {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        // Best-effort cleanup of any surviving group members (shell/sleep).
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        unsafe {
            libc::close(self.master);
        }
    }
}

/// Spawn the proxy helper over a fresh openpty pair. The parent keeps only
/// the master fd; the child gets the slave as fds 0–2 and its controlling
/// terminal.
unsafe fn spawn_harness(shell: &OsString) -> Result<Harness, String> {
    use std::os::fd::RawFd;

    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    if libc::openpty(
        &mut master,
        &mut slave,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    ) < 0
    {
        return Err(format!("openpty: {}", std::io::Error::last_os_error()));
    }

    // Parent polls reads without blocking between assertion ticks.
    let flags = libc::fcntl(master, libc::F_GETFL);
    libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK);

    // Async-signal-safe wiring in the fork-before-exec window: slave onto
    // 0/1/2, drop our copies of both ends. The helper then runs setsid +
    // TIOCSCTTY itself, mirroring production's session setup order.
    let wire = move || {
        for fd in 0..3 {
            libc::dup2(slave, fd);
        }
        if slave > 2 {
            libc::close(slave);
        }
        libc::close(master);
        Ok::<(), std::io::Error>(())
    };

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pty_proxy_child"));
    cmd.env("PTY_PROXY_CHILD_SHELL", shell)
        // Both tests in this file exist to cover the explicit mirror topology:
        // `agent_tracking = true` keeps the inner shell in this process's
        // session, which is the only topology where the outer tty's fg pgrp
        // can be pointed at inner jobs.
        .env("PTY_PROXY_CHILD_AGENT_TRACKING", "1")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    unsafe {
        cmd.pre_exec(wire);
    }

    let child = cmd.spawn().map_err(|e| format!("spawn proxy child: {e}"))?;
    Ok(Harness { master, child })
}

fn tcgetpgrp(master: std::os::fd::RawFd) -> libc::pid_t {
    unsafe { libc::tcgetpgrp(master) }
}

fn write_master(master: std::os::fd::RawFd, bytes: &[u8]) {
    unsafe {
        let written = libc::write(master, bytes.as_ptr() as *const _, bytes.len());
        assert_eq!(written, bytes.len() as isize, "short write to pty master");
    }
}

/// Non-blocking drain of whatever the proxy has produced so far.
fn drain_master(master: std::os::fd::RawFd, into: &mut String) {
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { libc::read(master, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n <= 0 {
            break;
        }
        into.push_str(&String::from_utf8_lossy(&buf[..n as usize]));
    }
}

/// Leader's command name for a pgid, via `ps -eo pid,pgid,comm`.
fn comm_of_leader(pgid: libc::pid_t) -> Option<String> {
    let out = Command::new("ps")
        .args(["-o", "comm=", "-p", &pgid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Poll `cond` every 50 ms until it holds or `secs` elapse (tick interval is
/// 100 ms; generous deadlines keep CI timing noise out of the assertions).
fn wait_for(mut cond: impl FnMut() -> bool, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    cond()
}

/// True when the inner shell has job control (`$-` contains `m`).
///
/// Under the mirror topology the inner slave is not a controlling terminal:
/// macOS bash 3.2 tolerates `tcsetpgrp` there and does give foreground jobs
/// their own pgrps, while Linux bash 5.x reports "no job control" and keeps
/// every child in the shell's own pgrp. The probe is empirical (not a cfg)
/// because it is the shell's actual decision, not the OS, that matters.
fn shell_has_job_control(master: std::os::fd::RawFd) -> bool {
    let mut output = String::new();
    write_master(master, b"echo TC_JC_FLAGS=$-\n");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        drain_master(master, &mut output);
        // The echoed command carries the literal `$-`; the shell's response
        // carries the real flags. Ignore the placeholder so the echo racing
        // ahead of the answer cannot be read as "no job control".
        let token = output
            .split("TC_JC_FLAGS=")
            .skip(1)
            .map(|chunk| {
                chunk
                    .chars()
                    .take_while(|c| !c.is_whitespace())
                    .collect::<String>()
            })
            .find(|token| !token.is_empty() && !token.starts_with("$-"));
        if let Some(token) = token {
            return token.contains('m');
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The previously-broken raw-pty topology: after the initial mirror the proxy
/// is background on its own controlling terminal, and without the SIGTTOU fix
/// every subsequent TIOCSPGRP fails with EIO, freezing the outer fg pgrp at
/// the inner shell forever. This asserts a real foreground job (`sleep`) takes
/// over the outer tty's fg pgrp and hands it back after Ctrl-C.
///
/// Skipped on hosts where the inner shell has no job control under the mirror
/// topology (Linux): without a controlling terminal the shell keeps the job in
/// its own pgrp, so there is no separate foreground pgrp to track. The resize
/// test still exercises the mirror tick there.
#[test]
fn test_outer_fg_tracks_inner_foreground_job() {
    let Some(shell) = find_bash() else {
        eprintln!("skipping test_outer_fg_tracks_inner_foreground_job: /bin/bash absent");
        return;
    };
    let h = unsafe { spawn_harness(&shell) }.expect("spawn proxy harness");

    // Baseline: the mirror points the outer tty at the inner shell's pgrp
    // (≠ the proxy's own pid). Before the fix this value then never changes.
    let shell_fg_pgid = std::cell::RefCell::new(0);
    let got_baseline = wait_for(
        || {
            let g = tcgetpgrp(h.master);
            if g > 0 && g != h.child.id() as i32 {
                *shell_fg_pgid.borrow_mut() = g;
                true
            } else {
                false
            }
        },
        5,
    );
    assert!(
        got_baseline,
        "outer fg pgrp never left the proxy pgrp (mirror broken?)"
    );
    let shell_fg = *shell_fg_pgid.borrow();

    if !shell_has_job_control(h.master) {
        eprintln!(
            "skipping test_outer_fg_tracks_inner_foreground_job: inner shell has no job \
             control under the mirror topology on this platform (no controlling terminal)"
        );
        return;
    }

    // Start a foreground job inside the shell; the outer tty must follow it.
    write_master(h.master, b"sleep 300\n");
    let tracked = wait_for(
        || {
            let g = tcgetpgrp(h.master);
            g != shell_fg && comm_of_leader(g).is_some_and(|c| c.ends_with("sleep"))
        },
        3,
    );
    assert!(
        tracked,
        "outer fg pgrp did not track inner foreground job (frozen at pgid {}); \
         current fg: {} {:?}",
        shell_fg,
        tcgetpgrp(h.master),
        comm_of_leader(tcgetpgrp(h.master)),
    );

    // Ctrl-C kills sleep; the shell regains the foreground and the mirror must
    // hand the outer tty back to the shell's pgrp.
    write_master(h.master, &[0x03]);
    let restored = wait_for(|| tcgetpgrp(h.master) == shell_fg, 3);
    assert!(
        restored,
        "outer fg pgrp did not return to shell after SIGINT; current fg: {}",
        tcgetpgrp(h.master)
    );
}

/// Window-size propagation: SIGWINCH from the terminal goes to the outer
/// tty's fg pgrp — an inner process, never the proxy — so the proxy must
/// reconcile sizes on its mirror tick. Setting the outer size and asking the
/// inner shell (`stty size`) proves the inner pty follows. Fails on the
/// pre-fix tree where nothing ever resizes the inner PTY.
#[test]
fn test_resize_propagates_to_inner_tty() {
    let Some(shell) = find_bash() else {
        eprintln!("skipping test_resize_propagates_to_inner_tty: /bin/bash absent");
        return;
    };
    let h = unsafe { spawn_harness(&shell) }.expect("spawn proxy harness");

    // Wait for the inner shell to start and draw its prompt before resizing.
    // The inner PTY is born 0x0 and the proxy reconciles it to the outer size
    // on the mirror tick; resizing before bash has initialized races that
    // startup reconciliation. Real terminals resize a running shell, so wait
    // for the prompt first. Match any common prompt suffix: macOS bash 3.2
    // draws "bash-3.2$ ", Linux bash 5.x (root) draws "root@host:/path# ".
    let mut startup = String::new();
    let ready = wait_for(
        || {
            drain_master(h.master, &mut startup);
            startup.contains("$ ") || startup.contains("# ") || startup.contains("bash-")
        },
        5,
    );
    assert!(ready, "inner shell never drew a prompt; output: {startup}");

    // Resize the "terminal" (master side) to something no spawn default uses.
    let size = libc::winsize {
        ws_row: 50,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        assert_eq!(
            libc::ioctl(h.master, libc::TIOCSWINSZ as _, &size),
            0,
            "TIOCSWINSZ on master failed"
        );
    }

    // Give the proxy at least one mirror tick to reconcile, then ask the
    // inner shell what it sees.
    std::thread::sleep(Duration::from_millis(300));
    let mut output = String::new();
    write_master(h.master, b"stty size\n");
    let propagated = wait_for(
        || {
            drain_master(h.master, &mut output);
            output.contains("50 120")
        },
        5,
    );
    let tail: String = output
        .chars()
        .rev()
        .take(400)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    assert!(
        propagated,
        "inner tty never saw the new window size (rows cols = 50 120); output tail: ...{tail}"
    );
}
