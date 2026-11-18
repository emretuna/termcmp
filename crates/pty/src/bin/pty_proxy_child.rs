//! Standalone proxy child used by `crates/pty/tests/foreground_mirror.rs`.
//!
//! The test cannot `fork()` directly: the cargo test harness runs tests on
//! threads of a multithreaded process, and fork-from-a-threaded-process is
//! unsafe here (ObjC/Swift runtime aborts in the child). Instead the test
//! spawns this binary via `std::process::Command::pre_exec`, which performs
//! the pty wiring in the fork-before-exec window and then execs a clean,
//! single-threaded image.
//!
//! Protocol: stdin/stdout/stderr must already be the outer tty slave
//! (arranged by the caller's `pre_exec`). This program then reproduces the
//! exact production invocation order from `crates/termcmp/src/main.rs`:
//! UnixStream pair → `fork_reader` → drop reader end → fresh tokio runtime →
//! `run_proxy`.
//!
//! Environment:
//! - `PTY_PROXY_CHILD_SHELL`: path of the shell to run (required).
//! - `PTY_PROXY_CHILD_AGENT_TRACKING`: tri-state explicit override, mirroring
//!   `[experimental] agent_tracking`. `1` forces the mirror topology (inner
//!   shell shares this process's session, so `ForegroundMirror` can point the
//!   outer tty at inner jobs), `0` forces isolation, and unset (or any other
//!   value) leaves the key unset so the proxy resolves per topology. The test
//!   harness sets `1` explicitly so its topology never depends on ambient
//!   config.

use config::TermcmpConfig;
use std::ffi::OsStr;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

fn main() {
    let shell = std::env::var_os("PTY_PROXY_CHILD_SHELL").expect("PTY_PROXY_CHILD_SHELL unset");

    // Fresh session + controlling terminal: this is what makes the proxy
    // "foreground" for its first mirror call and permanently background
    // afterwards — the condition xnu gates behind SIGTTOU handling.
    //
    // Safety: raw libc syscalls on fds we own; no allocation before the
    // mirror setup, matching the production process state.
    unsafe {
        let _ = libc::setsid();
        libc::ioctl(0, libc::TIOCSCTTY as libc::c_ulong, 0);
    }

    // Reader→supervisor transport, same as termcmp/src/main.rs.
    let (reader_stream, supervisor_stream) =
        UnixStream::pair().expect("UnixStream pair in proxy child");
    let reader_pid =
        pty::spawn::fork_reader(0, reader_stream.as_raw_fd()).expect("fork_reader in proxy child");
    drop(reader_stream); // fork_reader's parent keeps the supervisor end.
    supervisor_stream.set_nonblocking(true).ok();

    let mut config = TermcmpConfig::default();
    config.experimental.agent_tracking =
        match std::env::var("PTY_PROXY_CHILD_AGENT_TRACKING").as_deref() {
            Ok("1") => Some(true),
            Ok("0") => Some(false),
            _ => None,
        };

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime in proxy child");
    let code = rt
        .block_on(pty::run_proxy(
            OsStr::new(&shell),
            &[],
            &config,
            supervisor_stream,
            reader_pid,
        ))
        .unwrap_or(1);
    std::process::exit(code);
}
