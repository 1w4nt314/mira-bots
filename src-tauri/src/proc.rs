//! Running a short-lived child process with captured output and a timeout (step 6b, plan6b
//! punkt 8): git (B3) and the project checks (B4). No new crates: `std::process::Command`, two
//! reader threads that keep only the tail of stdout/stderr (so a full pipe never blocks the
//! child — the 64 KB deadlock of reading after `wait`), a 50 ms `try_wait` poll and a timeout.
//!
//! Unix: the child leads its own process group (`process_group(0)`); a timeout kills the whole
//! group with `killpg(SIGKILL)` while the leader is still unreaped (its pid, and so the group
//! id, cannot have been reused). Windows: `CREATE_NO_WINDOW`, and a timeout runs
//! `taskkill /PID <pid> /T /F` (the tree; `child.kill()` alone would only end `cmd.exe`).
//!
//! [`PidRegistry`]: children started with a registry are listed while they run, so the app can
//! end them at exit ([`PidRegistry::kill_running`], called next to `kill_all`). A pid leaves the
//! registry in the same critical section in which it is reaped, so `kill_running` never signals
//! a reaped (reusable) pid.
// TODO(windows-verify): a check `ping -n 60 127.0.0.1` with a 2 s timeout ends without a
// leftover ping.exe (taskkill /T), and no console window flashes (plan6b D.97/D.98).

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How often the child is polled.
const POLL: Duration = Duration::from_millis(50);
/// After the child exited (or was killed): how long the readers get to deliver what is still in
/// the pipes. A grandchild that keeps a pipe open must not hang the caller.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(500);
/// Timeout of `taskkill` (Windows).
#[cfg(windows)]
const TASKKILL_TIMEOUT: Duration = Duration::from_secs(5);
/// `CREATE_NO_WINDOW` (Windows): no console window for the child.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// What to run. `raw_arg` is appended after `args` verbatim on Windows (`cmd /S /C "<line>"`:
/// Rust's normal quoting uses `\"`, which cmd does not understand); on Unix it is appended as
/// one ordinary argument ([`shell_command`] never sets it there).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub raw_arg: Option<String>,
}

impl CommandSpec {
    /// `program args…` without a raw argument.
    pub fn new<I, S>(program: impl Into<PathBuf>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        CommandSpec {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            raw_arg: None,
        }
    }
}

/// Result of [`run_capture`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Captured {
    /// The exit code; `None` after a timeout or when a signal ended the child (Unix).
    pub code: Option<i32>,
    pub timed_out: bool,
    /// stdout and stderr interleaved in arrival order (the last `tail_chars` chars).
    pub output: String,
    /// stdout alone (the last `tail_chars` chars).
    pub stdout: String,
    /// stderr alone (the last `tail_chars` chars).
    pub stderr: String,
    /// Some output was cut: only the tail was kept.
    pub clipped: bool,
    pub elapsed_ms: u64,
}

impl Captured {
    /// Exited on its own with code 0.
    pub fn success(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }
}

/// Runs processes for the app; the trait lets git and the checks be tested without children.
pub trait ProcRunner: Send + Sync {
    fn run(
        &self,
        spec: &CommandSpec,
        cwd: &Path,
        env: &[(String, String)],
        timeout: Duration,
        tail_chars: usize,
    ) -> Result<Captured, String>;
}

/// [`ProcRunner`] over [`run_capture`], optionally registering its children.
#[derive(Clone, Copy, Default)]
pub struct SystemProc {
    pub registry: Option<&'static PidRegistry>,
}

impl ProcRunner for SystemProc {
    fn run(
        &self,
        spec: &CommandSpec,
        cwd: &Path,
        env: &[(String, String)],
        timeout: Duration,
        tail_chars: usize,
    ) -> Result<Captured, String> {
        run_capture(spec, cwd, env, timeout, tail_chars, self.registry)
    }
}

/// The pids of running children (started with this registry) of the app.
#[derive(Default)]
pub struct PidRegistry {
    pids: Mutex<Vec<u32>>,
}

impl PidRegistry {
    pub const fn new() -> Self {
        PidRegistry {
            pids: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<u32>> {
        self.pids.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Number of registered (running, unreaped) children.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Kills every registered child with its tree (app exit). The children stay registered
    /// until their runner reaps them. Returns how many were signalled.
    pub fn kill_running(&self) -> usize {
        let pids = self.lock();
        for pid in pids.iter() {
            kill_process_tree(*pid);
        }
        if !pids.is_empty() {
            log::info!("proc: killed {} running child process(es)", pids.len());
        }
        pids.len()
    }
}

/// The app's registry (checks and git); `lib.rs` calls `kill_running` at exit.
pub fn registry() -> &'static PidRegistry {
    static REGISTRY: PidRegistry = PidRegistry::new();
    &REGISTRY
}

/// Kills `pid` and its tree: Unix `killpg(pid, SIGKILL)` (the child must lead its own group, as
/// [`run_capture`]'s children do), Windows `taskkill /PID <pid> /T /F`. Only for a child of this
/// process that has not been reaped yet (otherwise the pid may belong to someone else).
pub fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    {
        if let Ok(pgid) = i32::try_from(pid) {
            // SAFETY: killpg only takes integers; the caller guarantees the group leader is an
            // unreaped child of ours, so the group id cannot have been reused.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        taskkill(pid);
    }
}

#[cfg(windows)]
fn taskkill(pid: u32) {
    let exe = std::env::var_os("SystemRoot")
        .filter(|r| !r.is_empty())
        .map(|r| PathBuf::from(r).join("System32").join("taskkill.exe"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("taskkill"));
    let pid = pid.to_string();
    // taskkill prints "SUCCESS: …" on stdout; an error (already gone) is fine.
    if let Err(e) = crate::diagnostics::run_version_command(
        &exe,
        &["/PID", pid.as_str(), "/T", "/F"],
        TASKKILL_TIMEOUT,
    ) {
        log::debug!("proc: taskkill {pid}: {e}");
    }
}

/// The user's shell line as a command: Unix `$SHELL -lc <line>` (fallback
/// [`crate::agent::login_env::FALLBACK_SHELL`]); Windows `%ComSpec% /D /S /C "<line>"` with the
/// quoted line as the raw argument. The line is passed as one argument, never combined with
/// other text.
pub fn shell_command(line: &str) -> CommandSpec {
    #[cfg(unix)]
    {
        CommandSpec::new(shell_program(), ["-lc", line])
    }
    #[cfg(windows)]
    {
        let program = std::env::var_os("ComSpec")
            .filter(|c| !c.is_empty())
            .map_or_else(|| PathBuf::from("cmd.exe"), PathBuf::from);
        CommandSpec {
            program,
            args: vec!["/D".into(), "/S".into(), "/C".into()],
            raw_arg: Some(format!("\"{line}\"")),
        }
    }
}

/// `$SHELL` when set, else [`crate::agent::login_env::FALLBACK_SHELL`] (as the login PATH).
#[cfg(unix)]
pub fn shell_program() -> PathBuf {
    std::env::var_os("SHELL")
        .filter(|s| !s.is_empty())
        .map_or_else(
            || PathBuf::from(crate::agent::login_env::FALLBACK_SHELL),
            PathBuf::from,
        )
}

/// The last `n` chars of `s` (cut at a char boundary) and whether something was cut.
pub fn tail_chars(s: &str, n: usize) -> (String, bool) {
    let count = s.chars().count();
    if count <= n {
        return (s.to_string(), false);
    }
    let start = s.char_indices().nth(count - n).map_or(s.len(), |(i, _)| i);
    (s[start..].to_string(), true)
}

/// A byte buffer that keeps only its last `cap` bytes, never starting inside a UTF-8 sequence.
struct Tail {
    bytes: VecDeque<u8>,
    cap: usize,
    clipped: bool,
}

impl Tail {
    fn new(cap: usize) -> Self {
        Tail {
            bytes: VecDeque::new(),
            cap,
            clipped: false,
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.bytes.extend(data.iter().copied());
        if self.bytes.len() > self.cap {
            let cut = self.bytes.len() - self.cap;
            self.bytes.drain(..cut);
            // Do not start with a continuation byte (10xxxxxx) of a cut character.
            while self.bytes.front().is_some_and(|b| b & 0xC0 == 0x80) {
                self.bytes.pop_front();
            }
            self.clipped = true;
        }
    }

    /// Lossy UTF-8, then the last `n` chars.
    fn finish(&mut self, n: usize) -> (String, bool) {
        let text = String::from_utf8_lossy(self.bytes.make_contiguous()).into_owned();
        let (text, cut) = tail_chars(&text, n);
        (text, cut || self.clipped)
    }
}

struct Buffers {
    out: Tail,
    err: Tail,
    merged: Tail,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn spawn_reader<R: Read + Send + 'static>(
    mut pipe: R,
    bufs: Arc<Mutex<Buffers>>,
    is_err: bool,
    done: mpsc::Sender<()>,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name(if is_err { "proc-stderr" } else { "proc-stdout" }.into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut b = lock(&bufs);
                        if is_err {
                            b.err.push(&buf[..n]);
                        } else {
                            b.out.push(&buf[..n]);
                        }
                        b.merged.push(&buf[..n]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let _ = done.send(());
        })
        .map(|_| ())
}

/// `try_wait`, with the pid leaving `registry` in the same critical section as the reap.
fn try_wait_registered(
    child: &mut Child,
    registry: Option<&PidRegistry>,
) -> io::Result<Option<ExitStatus>> {
    let Some(r) = registry else {
        return child.try_wait();
    };
    let mut pids = r.lock();
    let res = child.try_wait();
    if !matches!(res, Ok(None)) {
        let pid = child.id();
        pids.retain(|p| *p != pid);
    }
    res
}

/// Kills the child's tree (still unreaped), then reaps it (leaving `registry` in the same
/// critical section).
fn kill_and_reap(child: &mut Child, registry: Option<&PidRegistry>) -> Option<ExitStatus> {
    let guard = registry.map(PidRegistry::lock);
    kill_process_tree(child.id());
    let _ = child.kill();
    let status = child.wait().ok();
    if let Some(mut pids) = guard {
        let pid = child.id();
        pids.retain(|p| *p != pid);
    }
    status
}

fn platform_setup(cmd: &mut Command, spec: &CommandSpec) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        if let Some(raw) = &spec.raw_arg {
            cmd.arg(raw);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
        if let Some(raw) = &spec.raw_arg {
            cmd.raw_arg(raw);
        }
    }
}

/// Runs `spec` in `cwd` with `env` added to the app's environment, stdin null, and waits at most
/// `timeout` (then the tree is killed, `timed_out`). Output: the last `tail_chars` chars of
/// stdout, stderr and both interleaved (lossy UTF-8). `Err` only when the child could not be
/// started (or waiting failed). Blocking.
pub fn run_capture(
    spec: &CommandSpec,
    cwd: &Path,
    env: &[(String, String)],
    timeout: Duration,
    tail_chars: usize,
    registry: Option<&PidRegistry>,
) -> Result<Captured, String> {
    let started = Instant::now();
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    platform_setup(&mut cmd, spec);
    let mut child = {
        // Registered under the lock, so `kill_running` sees the child as soon as it exists.
        let guard = registry.map(PidRegistry::lock);
        let child = cmd
            .spawn()
            .map_err(|e| format!("kunne ikke starte {}: {e}", spec.program.display()))?;
        if let Some(mut pids) = guard {
            pids.push(child.id());
        }
        child
    };
    let cap = tail_chars.saturating_mul(4).max(4);
    let bufs = Arc::new(Mutex::new(Buffers {
        out: Tail::new(cap),
        err: Tail::new(cap),
        merged: Tail::new(cap),
    }));
    let (tx, rx) = mpsc::channel();
    let mut readers = 0;
    if let Some(out) = child.stdout.take() {
        match spawn_reader(out, Arc::clone(&bufs), false, tx.clone()) {
            Ok(()) => readers += 1,
            Err(e) => log::warn!("proc: could not start the stdout reader: {e}"),
        }
    }
    if let Some(err) = child.stderr.take() {
        match spawn_reader(err, Arc::clone(&bufs), true, tx.clone()) {
            Ok(()) => readers += 1,
            Err(e) => log::warn!("proc: could not start the stderr reader: {e}"),
        }
    }
    drop(tx);

    let deadline = started + timeout;
    let (status, timed_out) = loop {
        match try_wait_registered(&mut child, registry) {
            Ok(Some(status)) => break (Some(status), false),
            Ok(None) if Instant::now() >= deadline => {
                kill_and_reap(&mut child, registry);
                break (None, true);
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => {
                kill_and_reap(&mut child, registry);
                return Err(format!(
                    "ventede forgæves på {}: {e}",
                    spec.program.display()
                ));
            }
        }
    };

    // Bounded drain: whatever is still in the pipes, unless a grandchild keeps one open.
    let drain_until = Instant::now() + DRAIN_AFTER_EXIT;
    let mut done = 0;
    while done < readers {
        let left = drain_until.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(()) => done += 1,
            Err(_) => break,
        }
    }

    let mut b = lock(&bufs);
    let (stdout, out_cut) = b.out.finish(tail_chars);
    let (stderr, err_cut) = b.err.finish(tail_chars);
    let (output, merged_cut) = b.merged.finish(tail_chars);
    Ok(Captured {
        code: if timed_out {
            None
        } else {
            status.and_then(|s| s.code())
        },
        timed_out,
        output,
        stdout,
        stderr,
        clipped: out_cut || err_cut || merged_cut,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// One recorded call of a mock [`ProcRunner`] (tests): spec, cwd, env, timeout.
#[cfg(test)]
pub(crate) type ProcCall = (CommandSpec, PathBuf, Vec<(String, String)>, Duration);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_chars_cuts_at_char_boundary() {
        assert_eq!(tail_chars("æøå", 2), ("øå".to_string(), true));
        assert_eq!(tail_chars("æøå", 3), ("æøå".to_string(), false));
        assert_eq!(tail_chars("", 0), (String::new(), false));
        assert_eq!(tail_chars("abc", 0), (String::new(), true));

        // The byte buffer never starts inside a cut character.
        let mut t = Tail::new(3);
        t.push("æø".as_bytes()); // 4 bytes: the first byte of æ is dropped, then its tail
        assert_eq!(t.finish(10), ("ø".to_string(), true));
        let mut t = Tail::new(8);
        t.push(b"abc");
        t.push(b"def");
        assert_eq!(t.finish(10), ("abcdef".to_string(), false));
        assert_eq!(t.finish(4), ("cdef".to_string(), true));
    }

    #[test]
    fn captured_success_needs_code_zero_without_timeout() {
        let ok = Captured {
            code: Some(0),
            ..Captured::default()
        };
        assert!(ok.success());
        assert!(!Captured {
            code: Some(1),
            ..Captured::default()
        }
        .success());
        assert!(!Captured {
            code: None,
            timed_out: true,
            ..Captured::default()
        }
        .success());
    }

    /// The mock runner as git and the checks use it (records what would run).
    #[derive(Default)]
    struct FakeProc {
        calls: Mutex<Vec<ProcCall>>,
    }

    impl ProcRunner for FakeProc {
        fn run(
            &self,
            spec: &CommandSpec,
            cwd: &Path,
            env: &[(String, String)],
            timeout: Duration,
            _tail_chars: usize,
        ) -> Result<Captured, String> {
            lock(&self.calls).push((spec.clone(), cwd.to_path_buf(), env.to_vec(), timeout));
            Ok(Captured {
                code: Some(0),
                stdout: "ok\n".into(),
                ..Captured::default()
            })
        }
    }

    #[test]
    fn a_runner_can_be_mocked() {
        let fake = FakeProc::default();
        let runner: &dyn ProcRunner = &fake;
        let spec = CommandSpec::new("git", ["status"]);
        let c = runner
            .run(&spec, Path::new("/x"), &[], Duration::from_secs(1), 10)
            .unwrap();
        assert!(c.success());
        let calls = lock(&fake.calls);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.args, vec![OsString::from("status")]);
        assert_eq!(calls[0].1, PathBuf::from("/x"));
    }

    #[cfg(unix)]
    mod unix {
        use super::*;

        fn sh(script: &str) -> CommandSpec {
            CommandSpec::new("/bin/sh", ["-c", script])
        }

        fn tmp() -> PathBuf {
            std::env::temp_dir()
        }

        #[test]
        fn run_capture_reports_exit_code() {
            let c = run_capture(
                &sh("echo out; echo err >&2; exit 3"),
                &tmp(),
                &[],
                Duration::from_secs(10),
                1000,
                None,
            )
            .unwrap();
            assert_eq!(c.code, Some(3));
            assert!(!c.timed_out && !c.clipped && !c.success());
            assert_eq!((c.stdout.as_str(), c.stderr.as_str()), ("out\n", "err\n"));
            assert!(c.output.contains("out\n") && c.output.contains("err\n"));
        }

        #[test]
        fn run_capture_runs_in_cwd_with_env() {
            let dir = std::env::temp_dir().join(format!("mira-proc-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let c = run_capture(
                &sh("pwd; printf '%s' \"$MIRA_PROC_TEST\""),
                &dir,
                &[("MIRA_PROC_TEST".into(), "hej".into())],
                Duration::from_secs(10),
                1000,
                None,
            )
            .unwrap();
            assert!(c.success());
            let real = std::fs::canonicalize(&dir).unwrap();
            assert_eq!(c.stdout, format!("{}\nhej", real.display()));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn run_capture_keeps_tail_and_flags_clipped() {
            let c = run_capture(
                &sh("yes | head -c 200000; printf END"),
                &tmp(),
                &[],
                Duration::from_secs(20),
                1000,
                None,
            )
            .unwrap();
            assert!(c.success());
            assert!(c.clipped);
            assert_eq!(c.stdout.chars().count(), 1000);
            assert!(c.stdout.ends_with("y\nEND"));
            assert!(c.output.ends_with("END"));
        }

        fn dead_or_zombie(pid: i32) -> bool {
            // SAFETY: kill with signal 0 only checks for existence.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return true;
            }
            // PID 1 in a container may not reap orphans (env.md): a zombie counts as dead.
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .map(|s| {
                    s.rsplit(')')
                        .next()
                        .unwrap_or("")
                        .trim_start()
                        .starts_with('Z')
                })
                .unwrap_or(false)
        }

        #[test]
        fn run_capture_times_out_and_kills_group() {
            let started = Instant::now();
            let c = run_capture(
                &sh("sleep 30 & echo $!; wait"),
                &tmp(),
                &[],
                Duration::from_millis(300),
                1000,
                None,
            )
            .unwrap();
            assert!(c.timed_out);
            assert_eq!(c.code, None);
            assert!(started.elapsed() < Duration::from_secs(5), "{c:?}");
            let grandchild: i32 = c.stdout.trim().parse().expect("pid printed");
            // SIGKILL is delivered when the grandchild leaves the kernel: on a loaded CI runner a
            // freshly exec'd `sleep` can sit in uninterruptible IO for seconds (seen on GitHub's
            // ubuntu runner: alive after 3 s, gone a few seconds later). `sleep 30` on its own
            // would outlive this window, so a survivor is still a real failure.
            // The verdict is taken once: on CI (run 116) the pid read as dead inside the loop and
            // as alive again right after, so a second look can land on a reused pid number.
            let until = Instant::now() + Duration::from_secs(15);
            let mut dead = false;
            while Instant::now() < until {
                if dead_or_zombie(grandchild) {
                    dead = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(dead, "sleep {grandchild} survived");
        }

        #[test]
        fn a_spawn_failure_is_an_error() {
            let e = run_capture(
                &CommandSpec::new("/nonexistent/mira-proc", Vec::<String>::new()),
                &tmp(),
                &[],
                Duration::from_secs(1),
                10,
                None,
            )
            .unwrap_err();
            assert!(
                e.starts_with("kunne ikke starte /nonexistent/mira-proc"),
                "{e}"
            );
        }

        #[test]
        fn registry_lists_running_children_and_kill_running_ends_them() {
            let reg: &'static PidRegistry = Box::leak(Box::new(PidRegistry::new()));
            let runner = SystemProc {
                registry: Some(reg),
            };
            let worker = std::thread::spawn(move || {
                runner.run(&sh("sleep 30"), &tmp(), &[], Duration::from_secs(20), 100)
            });
            let until = Instant::now() + Duration::from_secs(5);
            while reg.is_empty() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(reg.len(), 1);
            assert_eq!(reg.kill_running(), 1);
            let c = worker.join().unwrap().unwrap();
            assert!(!c.timed_out && c.code.is_none(), "{c:?}");
            assert!(reg.is_empty());
        }

        #[test]
        fn shell_command_uses_login_shell_and_lc() {
            let spec = shell_command("echo mira-$((1+1))");
            assert_eq!(spec.program, shell_program());
            assert_eq!(
                spec.args,
                vec![OsString::from("-lc"), OsString::from("echo mira-$((1+1))")]
            );
            assert_eq!(spec.raw_arg, None);
            let c = run_capture(&spec, &tmp(), &[], Duration::from_secs(20), 1000, None).unwrap();
            assert!(c.success(), "{c:?}");
            assert!(c.stdout.trim_end().ends_with("mira-2"), "{c:?}");
        }
    }

    #[cfg(windows)]
    mod windows {
        use super::*;

        #[test]
        fn shell_command_uses_cmd_with_raw_quoted_line() {
            let spec = shell_command("echo \"a b\"");
            assert_eq!(
                spec.args,
                vec![
                    OsString::from("/D"),
                    OsString::from("/S"),
                    OsString::from("/C")
                ]
            );
            assert_eq!(spec.raw_arg.as_deref(), Some("\"echo \"a b\"\""));
        }

        // TODO(windows-verify): runs on a Windows host only (plan6b D.97/D.98).
        #[test]
        fn cmd_echo_and_timeout() {
            let c = run_capture(
                &shell_command("echo mira"),
                &std::env::temp_dir(),
                &[],
                Duration::from_secs(20),
                1000,
                None,
            )
            .unwrap();
            assert!(c.success() && c.stdout.contains("mira"), "{c:?}");
            let c = run_capture(
                &shell_command("ping -n 30 127.0.0.1"),
                &std::env::temp_dir(),
                &[],
                Duration::from_millis(500),
                1000,
                None,
            )
            .unwrap();
            assert!(c.timed_out);
        }
    }
}
