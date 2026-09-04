use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;

use anyhow::{Context, Result};

/// Spawn the shell with stdio dup2'd to the inner PTY slave.
/// The shell stays in the same session (no setsid), gets its own pgrp via setpgid,
/// and inherits the outer tty as its ctty.
pub fn spawn_shell_with_pty(
    shell: &OsStr,
    args: &[OsString],
    slave_fd: RawFd,
) -> Result<std::process::Child> {
    let mut cmd = std::process::Command::new(shell);
    cmd.args(args);
    cmd.env("TERMCMP_ACTIVE", "1");

    // Explicitly set PS1 if it's in the environment (for testing)
    if let Ok(ps1) = std::env::var("PS1") {
        cmd.env("PS1", ps1);
    }

    // Pane-local recursion guard for tmux.
    if std::env::var("TMUX").is_ok() {
        if let Ok(pane) = std::env::var("TMUX_PANE") {
            cmd.env("TERMCMP_PANE", pane);
        } else {
            tracing::warn!("TMUX is set but TMUX_PANE is not — subshell recursion guard degraded");
        }
    }

    // Inherit environment
    for (key, value) in std::env::vars() {
        cmd.env(key, value);
    }
    // fish 4.x hard-exits at startup when tcgetpgrp(0) fails ENOTTY ("No
    // TTY for interactive shell"). The single-session design leaves the
    // inner PTY slave session-less, so the kernel can never answer. Inject
    // a tiny dylib that interposes tcgetpgrp/tcsetpgrp on fd 0 so fish sees
    // itself as foreground owner; every other shell is untouched.
    //
    // This must run AFTER the env-inherit loop above: cmd.env() here would
    // otherwise be overwritten by the inherited parent DYLD_INSERT_LIBRARIES,
    // silently discarding the merged injection.
    if is_fish(shell) {
        if let Some(dylib) = ensure_ttyshim() {
            let mut inserted = std::env::var("DYLD_INSERT_LIBRARIES").unwrap_or_default();
            if !inserted.is_empty() {
                inserted.push(':');
                inserted.push_str(&dylib);
            } else {
                inserted = dylib;
            }
            cmd.env("DYLD_INSERT_LIBRARIES", inserted);
        }
    }

    // Set cwd
    cmd.current_dir(std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/")));

    // Pre-exec: dup2 slave to 0/1/2, setpgid(0,0). NO setsid.
    unsafe {
        cmd.pre_exec(move || {
            // dup2 slave_fd to stdin/stdout/stderr
            if libc::dup2(slave_fd, 0) < 0
                || libc::dup2(slave_fd, 1) < 0
                || libc::dup2(slave_fd, 2) < 0
            {
                return Err(std::io::Error::last_os_error());
            }

            // Close slave_fd if it's not 0/1/2
            if slave_fd > 2 {
                libc::close(slave_fd);
            }

            // Make shell its own pgrp leader (stays in same session)
            if libc::setpgid(0, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(())
        });
    }

    let child = cmd.spawn().context("failed to spawn shell process")?;
    Ok(child)
}

/// Open a PTY pair using libc::openpty. Returns (master_fd, slave_fd).
pub fn open_pty_pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;

    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };

    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

/// Wrapper around a PTY master file descriptor that provides Read/Write traits.
pub struct PtyMaster {
    fd: OwnedFd,
}

impl PtyMaster {
    pub fn new(fd: OwnedFd) -> Self {
        Self { fd }
    }

    pub fn try_clone_reader(&self) -> Result<PtyReader> {
        let fd = self.fd.try_clone()?;
        Ok(PtyReader { fd })
    }

    pub fn take_writer(&self) -> Result<PtyWriter> {
        let fd = self.fd.try_clone()?;
        Ok(PtyWriter { fd })
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let ret = unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ as _, &size) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    /// Reads the current window size of the PTY slave side.
    pub fn winsize(&self) -> Result<crate::resize::TerminalSize> {
        let mut size = libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let ret = unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCGWINSZ as _, &mut size) };
        if ret < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(crate::resize::TerminalSize {
            rows: size.ws_row,
            cols: size.ws_col,
        })
    }
}

pub struct PtyReader {
    fd: OwnedFd,
}

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let ret = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len()) };
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }
}

pub struct PtyWriter {
    fd: OwnedFd,
}

impl Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let ret = unsafe { libc::write(self.fd.as_raw_fd(), buf.as_ptr() as *const _, buf.len()) };
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(ret as usize)
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Fork a reader child that reads from the outer tty with SIGTTIN dance.
/// Returns Some(pid) in parent, None in child (which never returns).
/// The child writes bytes to the UnixStream connected to `stream_fd`.
pub fn fork_reader(outer_fd: RawFd, stream_fd: RawFd) -> std::io::Result<Option<u32>> {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if pid > 0 {
        // Parent
        return Ok(Some(pid as u32));
    }

    // Child: install SIGTTIN handler (empty), ignore SIGINT/SIGQUIT/SIGTSTP
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_IGN;
        libc::sigaction(libc::SIGTTIN, &sa, std::ptr::null_mut());
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);
    }

    // SIGTTIN dance loop: read from outer_fd, write to stream_fd
    let mut buf = [0u8; 4096];
    loop {
        // Get current foreground pgrp of outer tty
        let mut pgid: libc::pid_t = 0;
        let ret = unsafe { libc::ioctl(outer_fd, libc::TIOCGPGRP as _, &mut pgid) };
        if ret < 0 {
            // Outer tty closed or invalid
            unsafe { libc::_exit(0) };
        }

        // If we're not in that pgrp, join it
        let my_pgid = unsafe { libc::getpgid(0) };
        if pgid != my_pgid && pgid > 0 {
            let ret = unsafe { libc::setpgid(0, pgid) };
            if ret < 0 {
                // EPERM or ESRCH — pgrp died, retry
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
        }

        // Read from outer tty
        let n = unsafe { libc::read(outer_fd, buf.as_mut_ptr() as *mut _, buf.len()) };

        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue; // EINTR from SIGTTIN, retry
            }
            if err.raw_os_error() == Some(libc::EIO) {
                // Orphaned pgrp or dead tty
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            }
            // EBADF or ENOTTY — exit
            unsafe { libc::_exit(0) };
        }

        if n == 0 {
            unsafe { libc::_exit(0) };
        }

        // Write to stream_fd
        let mut written = 0;
        while written < n as usize {
            let w = unsafe {
                libc::write(
                    stream_fd,
                    buf.as_ptr().add(written) as *const _,
                    n as usize - written,
                )
            };
            if w < 0 {
                unsafe { libc::_exit(0) };
            }
            written += w as usize;
        }
    }
}

/// Whether the requested shell is fish (basename match on the resolved
/// program path or a bare "fish" argv).
fn is_fish(shell: &OsStr) -> bool {
    let s = shell.to_string_lossy();
    let last = s.rsplit('/').next().unwrap_or(&s);
    let last = last.strip_prefix('-').unwrap_or(last);
    last == "fish" || last.starts_with("fish-")
}

/// Locate (or extract) the ttyshim dylib and return its absolute path.
///
/// The dylib ships inside the termcmp binary via include_bytes!; it is
/// written once to a stable cache path because DYLD_INSERT_LIBRARIES must
/// point at a real file on disk. Returns None when extraction fails — fish
/// will then hit its normal startup error rather than a harder failure.
fn ensure_ttyshim() -> Option<String> {
    const DYLIB: &[u8] = include_bytes!("../share/ttyshim.dylib");

    let cache_dir = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("termcmp");
    if std::fs::create_dir_all(&cache_dir).is_err() {
        return None;
    }

    let path = cache_dir.join("ttyshim.dylib");
    // Rewrite when missing or stale (size mismatch catches upgrades well
    // enough for a shim this tiny).
    let stale = std::fs::metadata(&path)
        .map(|m| m.len() != DYLIB.len() as u64)
        .unwrap_or(true);
    if stale && std::fs::write(&path, DYLIB).is_err() {
        return None;
    }

    Some(path.to_string_lossy().into_owned())
}
