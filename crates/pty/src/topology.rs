//! Foreground process group mirroring for PTY topology transparency.
//!
//! The supervisor watches the shell's process tree and mirrors the foreground
//! job's pgrp onto the outer tty so that kernel-anchored detectors (tmux
//! `pane_current_command`, herdr `foreground_process_group_id`) see the
//! actual foreground command instead of `termcmp`.
//!
//! Active only with `[experimental] agent_tracking = true` (or the legacy
//! `session_isolation = false` alias). The default gives the inner shell its
//! own session and controlling terminal (so `/dev/tty` password readers reach
//! the inner PTY), which makes the outer tty's fg pgrp unmirrorable:
//! `TIOCSPGRP` targeting an inner pgrp fails `EINVAL`. `ForegroundMirror` is
//! therefore never constructed under isolation.
//!
//! Cost: the 100ms `mirror_tick` skips the process-table walk only while
//! following a live non-shell job (an O(1) `sysctl(KERN_PROC_PID)` /
//! `/proc` liveness probe). Idle-at-shell ticks full-resolve to discover new
//! foreground jobs: SIGCHLD never fires for grandchildren, and a sessionless
//! slave reports no kernel fg pgrp (`tcgetpgrp` 0 on macOS, ENOTTY on Linux),
//! so the table walk is the only discovery path there. When the inner slave
//! does have a controlling terminal, `tcgetpgrp` is authoritative and O(1).
//!
//! Implementation note: we enumerate processes via `sysctl(KERN_PROC_ALL)`
//! with explicit, clang-verified byte offsets rather than libproc's
//! `proc_listchildpids`/`proc_pidinfo`. The libproc calls can block for
//! seconds (observed errno=ETIMEDOUT from an in-kernel MIG request) which
//! stalls the supervisor's select loop; KERN_PROC_ALL is a plain copy-out.

use libc::{self, c_int, pid_t};
#[cfg(target_os = "macos")]
use libc::{c_void, sysctl, CTL_KERN, KERN_PROC, KERN_PROC_ALL, KERN_PROC_PID};
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
    /// Last pgrp successfully installed on the outer tty. Drives the O(1)
    /// liveness gate in `mirror_tick` and the keep-live-target rule in
    /// `resolve_fg_target`.
    current_target: Option<pid_t>,
}

impl ForegroundMirror {
    pub fn new(outer_fd: c_int, shell_pgid: pid_t) -> Self {
        Self {
            outer_fd,
            shell_pgid,
            current_target: None,
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

    /// Resolve the pgrp that should be foreground on the outer tty.
    /// 1. Kernel truth: `tcgetpgrp` on the inner master, when the inner shell
    ///    has established job control (macOS bash 3.2 tolerates `tcsetpgrp`
    ///    on a sessionless slave; zsh manages its own pgrps).
    /// 2. Sessionless fallback: KEEP the current target while it has live
    ///    (non-stopped, non-zombie) members. A new background job must not
    ///    steal the mirror from a running foreground job — the previous
    ///    "newest child wins" rule did exactly that.
    /// 3. Current target dead/stopped (or none): pick the newest live
    ///    direct-child pgrp of the shell; else the shell's own pgrp.
    pub fn resolve_fg_target(&self, inner_master_fd: c_int) -> Option<pid_t> {
        let fg = unsafe { libc::tcgetpgrp(inner_master_fd) };
        if fg > 0 {
            return Some(fg);
        }
        let rows = get_all_processes().ok()?;
        // Keep-live-target applies only to a JOB pgrp with a live member that
        // is a direct child of the shell. Two traps:
        //  - fork_reader joins the outer tty's fg pgrp and lives there
        //    permanently (pid != shell_pgid, ppid == supervisor), so it would
        //    satisfy a bare `pgid == cur` predicate forever — both when cur is
        //    the shell pgrp (pinning discovery) and when cur is a job pgrp the
        //    reader followed (pinning a dead job). Requiring ppid == shell_pgid
        //    excludes the reader, whose parent is the supervisor, not the shell.
        //  - A stopped/zombie job must hand the tty back, hence is_running.
        if let Some(cur) = self.current_target {
            if cur != self.shell_pgid
                && rows
                    .iter()
                    .any(|r| r.pgid == cur && r.ppid == self.shell_pgid && is_running(r.stat))
            {
                return Some(cur);
            }
        }
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

    /// O(1) liveness probe for the current mirror target: `mirror_tick` uses
    /// this to skip the full process-table scan while the target still runs.
    /// A target that is stopped or zombie is not alive — it must hand the
    /// tty back to the shell.
    pub fn target_alive(&self) -> bool {
        self.current_target
            .and_then(proc_state)
            .is_some_and(is_running)
    }

    /// Per-tick mirror update. Kernel truth first: when the inner slave has
    /// a controlling terminal and the shell manages its own foreground pgrp
    /// (`tcgetpgrp` > 0), follow it directly — O(1). On a sessionless slave
    /// (the mirror topology on both macOS and Linux) `tcgetpgrp` yields 0 or
    /// ENOTTY, so discovery falls back to the process table: SIGCHLD never
    /// fires for grandchildren, and a live shell must not gate the scan.
    /// The scan is skipped only while following a live non-shell job (the
    /// keep-live-target rule); idle-at-shell and dead/stopped-job ticks
    /// full-resolve so new foreground jobs are found.
    pub fn tick(&mut self, inner_master_fd: c_int) {
        let fg = unsafe { libc::tcgetpgrp(inner_master_fd) };
        if fg > 0 {
            if Some(fg) != self.current_target {
                self.mirror(Some(fg));
            }
            return;
        }
        let following_job = self.current_target.is_some_and(|t| t != self.shell_pgid);
        if following_job && self.target_alive() {
            return;
        }
        if let Some(target) = self.resolve_fg_target(inner_master_fd) {
            self.mirror(Some(target));
        }
    }

    /// Set the outer tty's foreground pgrp to `target`, recording it as the
    /// current target so subsequent ticks can probe it in O(1).
    pub fn mirror(&mut self, target: Option<pid_t>) -> bool {
        let Some(pgid) = target else {
            return false;
        };
        if pgid == self.current_fg().unwrap_or(0) {
            // Already correct; avoid ioctl churn but still record the target.
            self.current_target = Some(pgid);
            return true;
        }
        let mut pgid_mut = pgid as c_int;
        let ret = unsafe { libc::ioctl(self.outer_fd, libc::TIOCSPGRP as _, &mut pgid_mut) };
        if ret < 0 {
            tracing::debug!("TIOCSPGRP({pgid}) failed: {}", io::Error::last_os_error());
            false
        } else {
            self.current_target = Some(pgid);
            true
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

/// State byte of a single process via `/proc/<pid>/stat`. None when the
/// process is gone or unreadable.
#[cfg(target_os = "linux")]
fn proc_state(pid: pid_t) -> Option<u8> {
    parse_proc_stat(pid).ok().flatten().map(|r| r.stat)
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

/// State byte of a single process via `sysctl(KERN_PROC_PID)` — the O(1)
/// liveness probe; no full process-table copy-out. None when the process is
/// gone (ESRCH, or a zero-length result).
#[cfg(target_os = "macos")]
fn proc_state(pid: pid_t) -> Option<u8> {
    let mut mib = [CTL_KERN, KERN_PROC, KERN_PROC_PID, pid];
    let mut buf = [0u8; KINFO_PROC_SIZE];
    let mut size = buf.len() as libc::size_t;
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
    if ret < 0 || size < KINFO_PROC_SIZE {
        return None;
    }
    Some(buf[OFF_P_STAT])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Kills and reaps any still-running children on drop so a failed
    /// assertion never leaks `sleep` processes into the developer's tree.
    struct ReapOnDrop(Vec<std::process::Child>);

    impl Drop for ReapOnDrop {
        fn drop(&mut self) {
            for child in &mut self.0 {
                if let Ok(None) = child.try_wait() {
                    unsafe { libc::kill(child.id() as pid_t, libc::SIGKILL) };
                    child.wait().ok();
                }
            }
        }
    }

    #[test]
    fn mirror_keeps_live_target_against_newer_background_child() {
        use std::os::unix::process::CommandExt;
        // Each child gets its own pgrp via pre_exec setpgid, and the test
        // process is the "shell" (children are direct children of it).
        let shell_pgid = std::process::id() as libc::pid_t;
        let spawn_sleep = || unsafe {
            std::process::Command::new("sleep")
                .arg("30")
                .pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                })
                .spawn()
                .expect("spawn sleep")
        };
        // Guard owns both children from the moment they exist.
        let mut guard = ReapOnDrop(vec![spawn_sleep()]);
        let a_pid = guard.0[0].id() as libc::pid_t;
        guard.0.push(spawn_sleep());
        let b_pid = guard.0[1].id() as libc::pid_t;
        assert!(b_pid > a_pid, "B must be newer than A");
        let mut m = ForegroundMirror::new(-1, shell_pgid);
        // outer_fd = -1: resolve_fg_target never touches it. Set the
        // current target directly (same module → private field access).
        m.current_target = Some(a_pid);
        // inner_master_fd = -1: tcgetpgrp fails → sessionless fallback path.
        assert_eq!(
            m.resolve_fg_target(-1),
            Some(a_pid),
            "live foreground target must not be stolen by a newer background child"
        );
        // Kill A → its pgrp has no live members → resolve picks B.
        unsafe { libc::kill(a_pid, libc::SIGKILL) };
        guard.0[0].wait().ok();
        // Retry briefly: libtest runs tests concurrently and a sibling
        // test's short-lived child can transiently outrank B in the
        // "newest live direct child" fallback.
        let mut resolved = m.resolve_fg_target(-1);
        for _ in 0..40 {
            if resolved == Some(b_pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
            resolved = m.resolve_fg_target(-1);
        }
        assert_eq!(
            resolved,
            Some(b_pid),
            "dead target must fall through to the newest live child"
        );
        unsafe { libc::kill(b_pid, libc::SIGKILL) };
        guard.0[1].wait().ok();
    }
}
