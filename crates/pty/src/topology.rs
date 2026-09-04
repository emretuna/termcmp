//! Foreground process group mirroring for PTY topology transparency.
//!
//! The supervisor watches the shell's process tree and mirrors the foreground
//! job's pgrp onto the outer tty so that kernel-anchored detectors (tmux
//! `pane_current_command`, herdr `foreground_process_group_id`) see the
//! actual foreground command instead of `termcmp`.
//!
//! Implementation note: we enumerate processes via `sysctl(KERN_PROC_ALL)`
//! with explicit, clang-verified byte offsets rather than libproc's
//! `proc_listchildpids`/`proc_pidinfo`. The libproc calls can block for
//! seconds (observed errno=ETIMEDOUT from an in-kernel MIG request) which
//! stalls the supervisor's select loop; KERN_PROC_ALL is a plain copy-out.

use libc::{self, c_int, pid_t};
#[cfg(target_os = "macos")]
use libc::{c_void, sysctl, CTL_KERN, KERN_PROC, KERN_PROC_ALL};
use std::io;

// macOS `struct kinfo_proc` layout (LP64 arm64/x86_64), verified with
// clang offsetof() against the SDK: entry stride 648 bytes.
//   kp_proc.p_stat @ 36   (u8)
//   kp_proc.p_pid  @ 40   (i32)
//   kp_eproc.e_ppid@ 560  (i32)
//   kp_eproc.e_pgid@ 564  (i32)
#[cfg(target_os = "macos")]
const KINFO_PROC_SIZE: usize = 648;
#[cfg(target_os = "macos")]
const OFF_P_STAT: usize = 36;
#[cfg(target_os = "macos")]
const OFF_P_PID: usize = 40;
#[cfg(target_os = "macos")]
const OFF_E_PPID: usize = 560;
#[cfg(target_os = "macos")]
const OFF_E_PGID: usize = 564;

// p_stat values from xnu bsd/sys/proc.h; Linux state comes from /proc stat.
#[cfg(target_os = "macos")]
const SSTOP: u8 = 4;
#[cfg(target_os = "macos")]
const SZOMB: u8 = 5;

/// Whether a process row is considered live for mirroring — not stopped (a
/// stopped fg job hands the tty back to the shell) and not a zombie/unreaped
/// exit (which must not pin the mirror to a dead group).
#[cfg(target_os = "macos")]
fn is_running(stat: u8) -> bool {
    !matches!(stat, SSTOP | SZOMB)
}

#[cfg(target_os = "linux")]
fn is_running(stat: u8) -> bool {
    // 'T'/'t' = stopped (job control / ptrace), 'Z'/'X'/'x' = zombie/dead.
    !matches!(stat, b'T' | b't' | b'Z' | b'X' | b'x')
}

/// One parsed process record (KERN_PROC_ALL on macOS, /proc/<pid>/stat on
/// Linux).
struct ProcRow {
    pid: pid_t,
    ppid: pid_t,
    pgid: pid_t,
    stat: u8,
}

/// Mirrors the shell's foreground job pgrp onto the outer tty.
pub struct ForegroundMirror {
    outer_fd: c_int,
    shell_pgid: pid_t,
}

impl ForegroundMirror {
    pub fn new(outer_fd: c_int, shell_pgid: pid_t) -> Self {
        Self {
            outer_fd,
            shell_pgid,
        }
    }

    /// Capture the outer tty's fg pgrp as it was before termcmp took over.
    /// This is the value to restore at shutdown so the parent shell regains
    /// the tty cleanly (and our own teardown isn't SIGTTOU-stopped).
    pub fn initial_fg(outer_fd: c_int) -> Option<pid_t> {
        let mut pgid: c_int = 0;
        let ret = unsafe { libc::ioctl(outer_fd, libc::TIOCGPGRP as _, &mut pgid) };
        if ret < 0 || pgid <= 0 {
            None
        } else {
            Some(pgid as pid_t)
        }
    }

    /// Restore a previously captured fg pgrp on the outer tty. Used at
    /// shutdown; static because the mirror's shell is gone by then.
    pub fn restore_fg(outer_fd: c_int, pgid: pid_t) -> bool {
        let mut pgid_mut = pgid as c_int;
        let ret = unsafe { libc::ioctl(outer_fd, libc::TIOCSPGRP as _, &mut pgid_mut) };
        if ret < 0 {
            tracing::debug!(
                "TIOCSPGRP(restore {pgid}) failed: {}",
                io::Error::last_os_error()
            );
            false
        } else {
            true
        }
    }

    /// Walk the process table once and return the pgrp that should be
    /// foreground on the outer tty. Returns the newest direct-child pgrp of
    /// the shell, or the shell's own pgrp if no children are running.
    ///
    /// Stopped jobs are excluded because a stopped fg job hands the tty back
    /// to the shell — mirroring its pgrp would leave signal delivery targeting
    /// a group nobody can wake. Zombies are excluded because an unreaped exit
    /// must not pin the mirror to a dead group.
    pub fn resolve_fg_target(&self) -> Option<pid_t> {
        let rows = get_all_processes().ok()?;

        // Direct children of the shell that are neither stopped nor zombies,
        // as (pid, pgid); newest (highest pid) wins.
        let mut live: Vec<(pid_t, pid_t)> = Vec::new();
        for r in &rows {
            if r.ppid == self.shell_pgid && r.pid != self.shell_pgid && is_running(r.stat) {
                live.push((r.pid, r.pgid));
            }
        }

        if live.is_empty() {
            Some(self.shell_pgid)
        } else {
            live.sort_by_key(|&(pid, _)| pid);
            Some(live.last().unwrap().1)
        }
    }

    /// Set the outer tty's foreground pgrp to `target`.
    pub fn mirror(&self, target: Option<pid_t>) -> bool {
        match target {
            Some(pgid) => {
                if pgid == self.current_fg().unwrap_or(0) {
                    return true; // already correct; avoid ioctl churn
                }
                let mut pgid_mut = pgid as c_int;
                let ret =
                    unsafe { libc::ioctl(self.outer_fd, libc::TIOCSPGRP as _, &mut pgid_mut) };
                if ret < 0 {
                    tracing::debug!("TIOCSPGRP({pgid}) failed: {}", io::Error::last_os_error());
                    false
                } else {
                    true
                }
            }
            None => false,
        }
    }

    /// Get the current foreground pgrp of the outer tty.
    pub fn current_fg(&self) -> Option<pid_t> {
        let mut pgid: c_int = 0;
        let ret = unsafe { libc::ioctl(self.outer_fd, libc::TIOCGPGRP as _, &mut pgid) };
        if ret < 0 {
            None
        } else {
            Some(pgid as pid_t)
        }
    }
}

/// Enumerate all processes on Linux by reading `/proc/<pid>/stat` for each
/// numeric `/proc` entry. Only the state, ppid, and pgrp fields are needed.
#[cfg(target_os = "linux")]
fn get_all_processes() -> io::Result<Vec<ProcRow>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<pid_t>() else {
            continue;
        };
        if let Some(row) = parse_proc_stat(pid)? {
            out.push(row);
        }
    }
    Ok(out)
}

/// Parse `/proc/<pid>/stat` into a `ProcRow`. Returns `Ok(None)` when the
/// process vanished mid-scan or the line can't be parsed.
#[cfg(target_os = "linux")]
fn parse_proc_stat(pid: pid_t) -> io::Result<Option<ProcRow>> {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return Ok(None);
    };
    // "pid (comm) state ppid pgrp ..." — comm can contain spaces and parens,
    // so split on the final ')' and parse state/ppid/pgrp from the tail.
    let Some(close) = stat.rfind(')') else {
        return Ok(None);
    };
    let mut fields = stat[close + 1..].split_whitespace();
    let Some(state) = fields.next().and_then(|s| s.as_bytes().first()).copied() else {
        return Ok(None);
    };
    let ppid = fields.next().and_then(|s| s.parse::<pid_t>().ok());
    let pgid = fields.next().and_then(|s| s.parse::<pid_t>().ok());
    let (Some(ppid), Some(pgid)) = (ppid, pgid) else {
        return Ok(None);
    };
    Ok(Some(ProcRow {
        pid,
        ppid,
        pgid,
        stat: state,
    }))
}

/// Fetch all processes via sysctl KERN_PROC_ALL and parse only the four
/// fields we need using verified byte offsets. KERN_PROC_ALL can grow
/// between the size probe and the fetch, so retry on ENOMEM.
#[cfg(target_os = "macos")]
fn get_all_processes() -> io::Result<Vec<ProcRow>> {
    let mut mib = [CTL_KERN, KERN_PROC, KERN_PROC_ALL, 0];
    let mut size: libc::size_t = 0;

    let mut buf: Vec<u8> = Vec::new();
    for _ in 0..4 {
        let ret = unsafe {
            sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if ret < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
            return Err(io::Error::last_os_error());
        }
        // Over-allocate 10% for processes spawned between probe and fetch.
        buf.resize(size + size / 10 + 64, 0);
        size = buf.len();
        let ret = unsafe {
            sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                buf.as_mut_ptr() as *mut c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if ret == 0 {
            break;
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::ENOMEM) {
            return Err(io::Error::last_os_error());
        }
    }
    buf.truncate(size);

    let n = size / KINFO_PROC_SIZE;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let base = i * KINFO_PROC_SIZE;
        let read_i32 = |off: usize| -> i32 {
            i32::from_ne_bytes(buf[base + off..base + off + 4].try_into().unwrap())
        };
        out.push(ProcRow {
            stat: buf[base + OFF_P_STAT],
            pid: read_i32(OFF_P_PID),
            ppid: read_i32(OFF_E_PPID),
            pgid: read_i32(OFF_E_PGID),
        });
    }
    Ok(out)
}
