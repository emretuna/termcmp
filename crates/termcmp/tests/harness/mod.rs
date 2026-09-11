use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

static PTY_PROCESS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock_pty_process() -> MutexGuard<'static, ()> {
    PTY_PROCESS_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[allow(dead_code)]
pub fn acquire_pty_process_lock_for_test() -> MutexGuard<'static, ()> {
    lock_pty_process()
}

#[allow(dead_code)]
pub fn pty_process_lock_is_available_for_test() -> bool {
    PTY_PROCESS_LOCK
        .get_or_init(|| Mutex::new(()))
        .try_lock()
        .is_ok()
}

/// A termcmp process running inside a PTY for integration testing.
///
/// Creates a PTY-in-PTY architecture: test PTY → termcmp → inner PTY → /bin/sh.
/// Writer that sends input to a tmux session via tmux send-keys.
#[allow(dead_code)]
struct TmuxWriter {
    session_name: String,
}

#[allow(dead_code)]
impl std::io::Write for TmuxWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Convert bytes to tmux send-keys format
        // For simplicity, we'll send the raw bytes as a string
        let text = String::from_utf8_lossy(buf);

        // Use tmux send-keys to send the text
        let status = std::process::Command::new("tmux")
            .args(["send-keys", "-t", &self.session_name, "-l", &text])
            .status()
            .map_err(std::io::Error::other)?;

        if !status.success() {
            return Err(std::io::Error::other("tmux send-keys failed"));
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Dump live tmux pane state for the `expect_output` timeout panic.
/// Distinguishes the three stall modes CI-only failures fall into:
/// pane dead on arrival (instant termcmp death, with its exit status),
/// pane alive but silent (startup hang before first write), and
/// pane loud but pipe empty (pipe-pane capture broken).
fn tmux_pane_diag(session_name: &str) -> String {
    let run = |args: &[&str]| {
        std::process::Command::new("tmux")
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|e| format!("<tmux failed: {e}>"))
    };
    let pane = run(&["capture-pane", "-t", session_name, "-p"]);
    let state = run(&[
        "display-message",
        "-t",
        session_name,
        "-p",
        "dead=#{pane_dead} dead_status=#{pane_dead_status} dead_signal=#{pane_dead_signal} cmd=#{pane_current_command}",
    ]);
    format!("tmux pane [{session_name}] state: {state}\n pane content:\n{pane}")
}

pub struct TermcmpProcess {
    writer: Box<dyn Write + Send>,
    output: Arc<(Mutex<Vec<u8>>, Condvar)>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    pid: Option<u32>,
    _pty_process_guard: MutexGuard<'static, ()>,
    /// Extra context appended to the `expect_output` timeout panic.
    /// The tmux backend sets this to dump live pane state (`capture-pane`,
    /// `pane_dead`, current command) so CI-only stalls are diagnosable from
    /// the log; the PTY backend leaves it empty.
    timeout_diag: Option<Arc<dyn Fn() -> String + Send + Sync>>,
}
// `mod harness` compiles per integration-test target; targets that only
// drive `TmuxSession` (e.g. tmux_integration) don't call every method.
// `spawn`/`exit_with_code` are used by smoke/cli_help_routing targets.
#[allow(dead_code)]
impl TermcmpProcess {
    pub fn spawn() -> Self {
        let pty_process_guard = lock_pty_process();

        let pty_system = native_pty_system();
        let pty_pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("failed to open PTY pair");

        let bin = env!("CARGO_BIN_EXE_termcmp");

        let mut cmd = CommandBuilder::new(bin);
        // Point --config at a non-existent path so load() returns
        // TermcmpConfig::default(). Without this, the harness inherits the
        // developer's ~/.config/termcmp/config.toml (e.g. Ask AI enabled),
        // which changes popup content and key handling, breaking assertions.
        let fake_config = std::env::temp_dir().join("termcmp-smoke-nonexistent/config.toml");
        // Portable shell invocation: /bin/sh is dash on Linux (no --norc),
        // so passing --norc unconditionally breaks shell startup there.
        // Prefer /bin/bash with --norc --noprofile when present; otherwise
        // fall back to plain /bin/sh with no extra args.
        let (shell, extra_args): (&str, &[&str]) = if std::path::Path::new("/bin/bash").exists() {
            ("/bin/bash", &["--norc", "--noprofile"])
        } else {
            ("/bin/sh", &[])
        };
        cmd.args([
            "--log-level",
            "error",
            "--config",
            fake_config.to_str().unwrap(),
            shell,
        ]);
        cmd.args(extra_args);
        // Pin the proxy's working directory. portable-pty defaults an unset
        // CommandBuilder cwd to $HOME, which on Linux CI is /root — dotfiles
        // only, so the filesystem-fallback popup finds zero candidates and
        // never renders. The crate root always has visible entries (src/,
        // tests/), making the popup tests deterministic on both platforms.
        cmd.cwd(env!("CARGO_MANIFEST_DIR"));
        // Force a known terminal so the proxy does not fall back to plain
        // shell on CI runners where TERM_PROGRAM is unset. Without this, the
        // proxy replaces itself with /bin/sh and the popup path never runs
        // (see should_fallback_to_shell in proxy.rs — Unknown terminals
        // require `[experimental] multi_terminal = true`).
        cmd.env("TERM_PROGRAM", "ghostty");

        let child = pty_pair
            .slave
            .spawn_command(cmd)
            .expect("failed to spawn termcmp");

        let pid = child.process_id();

        let writer = pty_pair
            .master
            .take_writer()
            .expect("failed to take PTY writer");
        let mut reader = pty_pair
            .master
            .try_clone_reader()
            .expect("failed to clone PTY reader");

        // Shared output buffer with condvar for blocking reads.
        let output = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let output_clone = Arc::clone(&output);

        // Background reader thread: accumulates PTY output.
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let (lock, cvar) = &*output_clone;
                        let mut data = lock.lock().unwrap();
                        data.extend_from_slice(&buf[..n]);
                        cvar.notify_all();
                    }
                    Err(_) => break,
                }
            }
        });

        // Wait for shell readiness by polling for the first PTY output bytes
        // instead of a fixed sleep. A fixed sleep races shell startup under
        // CI load: an `exit` line sent before the shell's first read is lost
        // and `wait_for_exit` burns its full 15s timeout (macOS
        // `test_exit_code_zero` flake). Bounded at 10s so a hung child still
        // fails fast; falls through regardless so a quiet-but-alive shell
        // doesn't hard-fail here.
        let deadline = Instant::now() + Duration::from_secs(10);
        {
            let (lock, cvar) = &*output;
            let mut data = lock.lock().unwrap();
            while data.is_empty() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let (guard, _) = cvar.wait_timeout(data, remaining).unwrap();
                data = guard;
            }
        }
        // Small settle so the shell reaches its prompt/read after first bytes.
        thread::sleep(Duration::from_millis(200));

        TermcmpProcess {
            writer,
            output,
            child,
            pid,
            _pty_process_guard: pty_process_guard,
            timeout_diag: None,
        }
    }

    /// Spawn a TermcmpProcess that monitors a tmux session.
    /// Uses tmux pipe-pane to capture output and tmux send-keys for input.
    #[allow(dead_code)]
    pub fn spawn_in_tmux(session_name: &str) -> Self {
        let pty_process_guard = lock_pty_process();

        // Create a temporary file for capturing pane output - unique per test
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let thread_id = format!("{:?}", std::thread::current().id());
        let thread_num = thread_id
            .trim_start_matches("ThreadId(")
            .trim_end_matches(")");
        let output_file = std::env::temp_dir().join(format!(
            "tmux-output-{}-t{}-{}.log",
            std::process::id(),
            thread_num,
            nanos
        ));

        // Clear the file if it exists
        let _ = std::fs::remove_file(&output_file);
        std::fs::write(&output_file, "").expect("failed to create output file");

        // Start piping pane output to the file
        let status = std::process::Command::new("tmux")
            .args([
                "pipe-pane",
                "-t",
                session_name,
                "-o",
                &format!("cat >> {}", output_file.display()),
            ])
            .status()
            .expect("failed to start pipe-pane");

        assert!(status.success(), "tmux pipe-pane failed");

        // Wait a bit for pipe to initialize
        thread::sleep(Duration::from_millis(200));

        // Create a writer that uses tmux send-keys
        let writer = Box::new(TmuxWriter {
            session_name: session_name.to_string(),
        });

        // Create a reader that reads from the output file
        let output = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let output_clone = Arc::clone(&output);
        let output_file_clone = output_file.clone();

        // Background reader thread: accumulates output from the file
        thread::spawn(move || {
            let mut last_pos = 0;
            loop {
                if let Ok(metadata) = std::fs::metadata(&output_file_clone) {
                    let current_len = metadata.len() as usize;
                    if current_len > last_pos {
                        if let Ok(mut file) = std::fs::File::open(&output_file_clone) {
                            use std::io::Seek;
                            if file.seek(std::io::SeekFrom::Start(last_pos as u64)).is_ok() {
                                let mut buf = vec![0u8; current_len - last_pos];
                                if let Ok(n) = file.read(&mut buf) {
                                    let (lock, cvar) = &*output_clone;
                                    let mut data = lock.lock().unwrap();
                                    data.extend_from_slice(&buf[..n]);
                                    cvar.notify_all();
                                    last_pos += n;
                                }
                            }
                        }
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
        });

        // Wait for shell to initialize
        thread::sleep(Duration::from_millis(500));

        // Force shell to produce output after pipe-pane is configured
        // Send an empty command to get a fresh prompt
        let _ = std::process::Command::new("tmux")
            .args(["send-keys", "-t", session_name, "Enter"])
            .status();

        // Wait for the prompt to be captured
        thread::sleep(Duration::from_millis(1000));

        // Create a dummy child (we don't actually own the tmux process)
        let pty_system = native_pty_system();
        let pty_pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("failed to open dummy PTY pair");

        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.arg("-c");
        cmd.arg("sleep 3600"); // Keep alive
        let child = pty_pair
            .slave
            .spawn_command(cmd)
            .expect("failed to spawn dummy child");

        TermcmpProcess {
            writer,
            output,
            child,
            pid: None,
            _pty_process_guard: pty_process_guard,
            timeout_diag: Some(Arc::new({
                let session = session_name.to_owned();
                move || tmux_pane_diag(&session)
            })),
        }
    }

    /// Send a line to the PTY (appends \r for "Enter").
    pub fn send_line(&mut self, line: &str) {
        let data = format!("{}\r", line);
        self.writer
            .write_all(data.as_bytes())
            .expect("failed to write to PTY");
        self.writer.flush().expect("failed to flush PTY writer");
    }

    /// Write raw bytes to the PTY.
    #[allow(dead_code)]
    pub fn write_raw(&mut self, data: &[u8]) {
        self.writer
            .write_all(data)
            .expect("failed to write raw to PTY");
        self.writer.flush().expect("failed to flush PTY writer");
    }

    /// Block until `substr` appears in the accumulated output, or timeout after 10s.
    pub fn expect_output(&self, substr: &str) {
        let timeout = Duration::from_secs(10);
        let start = Instant::now();
        let (lock, cvar) = &*self.output;

        loop {
            let data = lock.lock().unwrap();
            let text = String::from_utf8_lossy(&data);
            if text.contains(substr) {
                return;
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                let diag = self
                    .timeout_diag
                    .as_ref()
                    .map(|f| format!("\n{}", f()))
                    .unwrap_or_default();
                panic!(
                    "Timed out after {:?} waiting for {:?} in output.\nOutput so far ({} bytes):\n{}{}",
                    timeout,
                    substr,
                    data.len(),
                    String::from_utf8_lossy(&data[..data.len().min(2000)]),
                    diag,
                );
            }
            let remaining = timeout - elapsed;
            let (data, _) = cvar.wait_timeout(data, remaining).unwrap();
            let text = String::from_utf8_lossy(&data);
            if text.contains(substr) {
                return;
            }
        }
    }

    /// Return a snapshot of all accumulated output.
    pub fn output_snapshot(&self) -> Vec<u8> {
        let (lock, _) = &*self.output;
        lock.lock().unwrap().clone()
    }

    /// Return the current length of the accumulated output. Used with
    /// `wait_for_bytes_after` to scan only bytes produced after a mark.
    #[allow(dead_code)]
    pub fn output_len(&self) -> usize {
        let (lock, _) = &*self.output;
        lock.lock().unwrap().len()
    }

    /// Block until `needle` bytes appear in the output at or after
    /// `start_offset`, or `timeout` elapses. Returns `true` on match, `false`
    /// on timeout — never panics.
    ///
    /// Unlike `expect_output`, this works on raw bytes (so ANSI escape
    /// markers like `\x1b7` can be matched) and does not panic on timeout
    /// so callers can build non-fatal readiness probes.
    #[allow(dead_code)]
    pub fn wait_for_bytes_after(
        &self,
        needle: &[u8],
        start_offset: usize,
        timeout: Duration,
    ) -> bool {
        let start = Instant::now();
        let (lock, cvar) = &*self.output;
        loop {
            let data = lock.lock().unwrap();
            if data.len() > start_offset && contains_subslice(&data[start_offset..], needle) {
                return true;
            }
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return false;
            }
            let remaining = timeout - elapsed;
            let (data, _) = cvar.wait_timeout(data, remaining).unwrap();
            if data.len() > start_offset && contains_subslice(&data[start_offset..], needle) {
                return true;
            }
        }
    }

    /// Send `exit <code>` and wait for the process to exit. Returns the exit code.
    ///
    /// The `exit` line can be lost if the inner shell hasn't reached its first
    /// read yet (startup race under CI load). Resend it every 500ms while the
    /// child is alive so one lost line doesn't cost a 15s timeout. Bounded at
    /// 15s total so the suite can't balloon.
    pub fn exit_with_code(&mut self, code: i32) -> i32 {
        let timeout = Duration::from_secs(15);
        let retry_interval = Duration::from_millis(500);
        let start = Instant::now();
        self.send_line(&format!("exit {}", code));
        let mut last_send = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait failed") {
                return status.exit_code().try_into().unwrap_or(1);
            }
            if start.elapsed() >= timeout {
                self.child.kill().ok();
                panic!("Process did not exit within {:?}", timeout);
            }
            if last_send.elapsed() >= retry_interval {
                // Best-effort resend: the PTY may be half-torn-down, so a
                // failed write must not panic — the next poll observes exit.
                let data = format!("exit {}\r", code);
                if self.writer.write_all(data.as_bytes()).is_ok() {
                    let _ = self.writer.flush();
                }
                last_send = Instant::now();
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Return the PID of the termcmp process (if available).
    #[allow(dead_code)]
    pub fn child_pid(&self) -> Option<u32> {
        self.pid
    }
}

impl Drop for TermcmpProcess {
    fn drop(&mut self) {
        // Kill the process if it's still running.
        if self.child.try_wait().ok().flatten().is_none() {
            self.child.kill().ok();
        }
    }
}

/// Byte-level substring search — avoids `String::from_utf8_lossy`
/// allocations on hot polling paths when watching raw ANSI output.
#[allow(dead_code)]
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Unique token for session names and marker files: PID + thread + nanos.
/// Parallel tests share a tmux server; names must never collide.
#[allow(dead_code)]
fn unique_suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let thread_id = format!("{:?}", std::thread::current().id());
    let thread_num = thread_id
        .trim_start_matches("ThreadId(")
        .trim_end_matches(")");
    format!("{}-t{}-{}", std::process::id(), thread_num, nanos)
}

/// A tmux session running termcmp for integration testing.
///
/// Creates a tmux session with termcmp running inside it, allowing us to
/// test process topology invisibility (pane_current_command, job control, etc).
#[allow(dead_code)]
pub struct TmuxSession {
    session_name: String,
    termcmp: TermcmpProcess,
    /// Path to the exit-code marker written by the wrapper shell (see
    /// `spawn_exit_capture`). `None` for plain `spawn`, where the pane's
    /// direct child is termcmp itself.
    exit_marker: Option<PathBuf>,
}

#[allow(dead_code)]
impl TmuxSession {
    /// Spawn a tmux session with termcmp as the pane's DIRECT child.
    ///
    /// Every topology/foreground/OSC test depends on termcmp being the pane
    /// process (pane_current_command == "termcmp", job control mirroring,
    /// etc.). Do not wrap termcmp here.
    pub fn spawn() -> Self {
        let pane_cmd: Vec<String> = vec![
            env!("CARGO_BIN_EXE_termcmp").to_string(),
            "--log-level".to_string(),
            "error".to_string(),
            "/bin/bash".to_string(),
            "--norc".to_string(),
        ];
        Self::spawn_internal(pane_cmd, None)
    }

    /// Spawn a tmux session whose pane runs termcmp under a wrapper shell.
    ///
    /// The wrapper records termcmp's exit code to a marker file before it
    /// exits itself:
    ///
    /// ```text
    /// sh -c 'CARGO_BIN_EXE_termcmp --log-level error /bin/bash --norc
    ///        rc=$?; echo $rc > MARKER; exit $rc'
    /// ```
    ///
    /// This is the exit-propagation oracle: tmux's `#{pane_dead_status}` is
    /// dropped on Linux in a pty-EOF-vs-SIGCHLD race even for a clean
    /// `exit 42` (~43-57% of runs, tmux 3.3a AND 3.4), while `pane_dead=1`
    /// and a marker file written before shell exit are both deterministic.
    /// Only the two exit-propagation tests use this; all other tests keep
    /// the direct-child `spawn()` so the pane topology is unperturbed.
    pub fn spawn_exit_capture() -> Self {
        let suffix = unique_suffix();
        let marker = std::env::temp_dir().join(format!("termcmp-exit-{suffix}"));
        let _ = std::fs::remove_file(&marker);

        let bin = env!("CARGO_BIN_EXE_termcmp");
        // Single-quote both paths: neither contains a single quote, so this
        // is exact. `rc=$?` captures termcmp's exit BEFORE `echo` so the
        // marker holds termcmp's code, not echo's.
        let script = format!(
            "'{bin}' --log-level error /bin/bash --norc; rc=$?; echo $rc > '{}'; exit $rc",
            marker.display()
        );
        let pane_cmd: Vec<String> = vec!["sh".to_string(), "-c".to_string(), script];

        Self::spawn_internal(pane_cmd, Some(marker))
    }

    /// Shared construction: create the tmux session with `pane_cmd` as the
    /// pane command, set `remain-on-exit on`, attach pipe-pane, record the
    /// optional exit marker.
    fn spawn_internal(pane_cmd: Vec<String>, exit_marker: Option<PathBuf>) -> Self {
        let session_name = format!("termcmp-test-{}", unique_suffix());
        // Create tmux session with the pane command
        // NOTE: `-e GHOSTTY_RESOURCES_DIR=/tmp` is load-bearing. Panes
        // inherit the tmux *server* env (from whichever client started it),
        // not this client's `.env()` overrides — so bare CI servers leak no
        // terminal markers, detection yields Unknown, and the proxy falls
        // back to a plain shell (every popup test stalls). Worse, tmux
        // unconditionally rewrites TERM_PROGRAM=tmux in pane environments,
        // so `-e TERM_PROGRAM=...` can never work; GHOSTTY_RESOURCES_DIR
        // passes through untouched and still maps to Ghostty in-tmux (any
        // existing dir satisfies the detection check).
        // `new-session -e` needs tmux >= 3.2 (CI installs current tmux).
        let output = std::process::Command::new("tmux")
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "80",
                "-y",
                "24",
                "-e",
                "GHOSTTY_RESOURCES_DIR=/tmp",
            ])
            .args(&pane_cmd)
            .env("TERM_PROGRAM", "ghostty")
            .env("PS1", "$ ")
            .output()
            .expect("failed to spawn tmux session");

        if !output.status.success() {
            panic!(
                "tmux new-session failed with status {:?}\nstdout: {}\nstderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        // Keep dead panes around so tests can query #{pane_dead}. Assert
        // success: without remain-on-exit a dead pane destroys its session,
        // and every later query fails — surfacing as a confusing failure
        // instead of the real setup error. (Target the session, not
        // `session:N`: window indexes depend on base-index, but a session
        // target resolves to its active window.)
        let remain = std::process::Command::new("tmux")
            .args(["set-option", "-t", &session_name, "remain-on-exit", "on"])
            .output()
            .expect("failed to set remain-on-exit");
        assert!(
            remain.status.success(),
            "tmux set-option remain-on-exit failed: {}",
            String::from_utf8_lossy(&remain.stderr)
        );

        // Create a TermcmpProcess that connects to the tmux session
        // We'll use tmux's pipe-pane to capture output
        let termcmp = TermcmpProcess::spawn_in_tmux(&session_name);

        TmuxSession {
            session_name,
            termcmp,
            exit_marker,
        }
    }

    /// Send a line to the tmux session.
    pub fn send_line(&mut self, line: &str) {
        self.termcmp.send_line(line);
    }

    /// Send special keys to the tmux session.
    pub fn send_keys(&mut self, keys: &str) {
        let status = std::process::Command::new("tmux")
            .args(["send-keys", "-t", &self.session_name, keys])
            .status()
            .expect("failed to send keys");
        assert!(status.success(), "tmux send-keys failed");
    }

    /// Wait for output to appear in the tmux session.
    pub fn expect_output(&self, substr: &str) {
        self.termcmp.expect_output(substr);
    }

    /// Query tmux for a format string (e.g., "#{pane_current_command}").
    pub fn display_message(&self, format: &str) -> String {
        let output = std::process::Command::new("tmux")
            .args(["display-message", "-t", &self.session_name, "-p", format])
            .output()
            .expect("failed to run tmux display-message");

        assert!(output.status.success(), "tmux display-message failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// Poll `#{pane_current_command}` until `needle` appears or timeout.
    /// The ForegroundMirror updates on SIGCHLD plus a 100 ms tick, so the
    /// pane command lags process changes by up to ~150 ms.
    pub fn wait_for_pane_command(&self, needle: &str, timeout: Duration) -> String {
        let start = std::time::Instant::now();
        loop {
            let cmd = self.display_message("#{pane_current_command}");
            if cmd.contains(needle) {
                return cmd;
            }
            if start.elapsed() >= timeout {
                return cmd; // let the caller's assert show the mismatch
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait for raw bytes to appear in pane output.
    pub fn wait_for_bytes(&self, needle: &[u8], timeout: Duration) -> bool {
        self.termcmp.wait_for_bytes_after(needle, 0, timeout)
    }

    /// Capture current output snapshot as a String.
    pub fn capture_output(&self) -> String {
        let snapshot = self.termcmp.output_snapshot();
        String::from_utf8_lossy(&snapshot).to_string()
    }

    /// Resize the tmux pane.
    pub fn resize_pane(&self, width: u32, height: u32) {
        let status = std::process::Command::new("tmux")
            .args([
                "resize-pane",
                "-t",
                &self.session_name,
                "-x",
                &width.to_string(),
                "-y",
                &height.to_string(),
            ])
            .status()
            .expect("failed to resize pane");
        assert!(status.success(), "tmux resize-pane failed");
    }

    /// Wait for the pane's process to exit. With `remain-on-exit on` the
    /// dead pane stays visible, so we poll #{pane_dead} rather than pane
    /// disappearance.
    pub fn wait_for_pane_close(&self, timeout: Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < timeout {
            let gone = std::process::Command::new("tmux")
                .args([
                    "display-message",
                    "-t",
                    &self.session_name,
                    "-p",
                    "#{pane_dead}",
                ])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

            // Only an explicit dead flag means closed. A failed query
            // (server hiccup, session briefly unresolvable) is NOT
            // evidence of death — treating it as closed produced phantom
            // `pane_exit_status() == None` failures downstream.
            if let Ok("1") = gone.as_deref() {
                return true;
            }

            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Get the pane exit status.
    ///
    /// For `spawn_exit_capture` sessions this reads the code from the
    /// wrapper-written marker file — deterministic and cross-platform.
    /// tmux's own `#{pane_dead_status}` is deliberately NOT used: Linux
    /// drops it in a pty-EOF-vs-SIGCHLD race even for a clean `exit 42`
    /// (~43-57% of runs on tmux 3.3a and 3.4), so it is not a usable
    /// oracle for exit propagation.
    pub fn pane_exit_status(&self) -> Option<i32> {
        let marker = self.exit_marker.as_ref()?;
        // The wrapper writes the marker before it exits, and
        // `wait_for_pane_close` observes `pane_dead=1` only after that exit
        // completes (`echo` fopen/write/close precedes `exit $rc`). The file
        // is therefore already populated here; the retry loop is defensive
        // against tmpfs flush/lookup ordering.
        let budget = Duration::from_secs(3);
        let start = std::time::Instant::now();
        loop {
            if let Ok(content) = std::fs::read_to_string(marker) {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    return trimmed.parse::<i32>().ok();
                }
            }
            if start.elapsed() >= budget {
                eprintln!(
                    "pane_exit_status: marker {} never populated for {}",
                    marker.display(),
                    self.session_name
                );
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Exit the tmux session cleanly.
    pub fn exit(mut self) {
        self.send_line("exit 0");
        std::thread::sleep(Duration::from_millis(500));
        // Drop runs kill-session as a safety net.
    }
}

impl Drop for TmuxSession {
    fn drop(&mut self) {
        // Clean up tmux session
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &self.session_name])
            .status();
    }
}
