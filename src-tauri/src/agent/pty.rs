//! One child process in a pseudo terminal (ConPTY on Windows, openpty on unix) with a reader and
//! a waiter thread.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

use super::{process, AgentError};
use crate::config::PROCESS_KILL_GRACE;

/// Everything needed to start a process in a PTY.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Added on top of the inherited environment.
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

/// Owns the PTY master (dropping it closes the pseudo terminal), the input writer and a killer.
pub struct PtyHandle {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    pid: Option<u32>,
    /// Set by the waiter thread once the child was reaped, BEFORE `on_exit` runs (so never under
    /// the manager lock). `process::terminate` skips portable-pty's killer once it is set; the
    /// group signals themselves are gated on the group still having members (plan7 C7.1).
    exited: Arc<AtomicBool>,
}

fn pty_err(e: impl std::fmt::Display) -> AgentError {
    AgentError::Pty(e.to_string())
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Starts `spec` in a new PTY.
///
/// * `on_output` runs on the reader thread for every chunk read (up to 8 KiB).
/// * `on_exit` runs once on the waiter thread with the exit code (`None` if waiting failed).
///
/// Both callbacks must not block for long and must not call back into this handle's `kill`.
// TODO(windows-verify): interactive claude.exe under ConPTY via portable-pty: startup, no hanging
// reader thread, exit code after `/exit` (plan D.4). Under ConPTY the reader may only see EOF
// once the master is dropped (see `AgentManager::mark_exited`).
pub fn spawn(
    spec: &SpawnSpec,
    on_output: impl Fn(&[u8]) + Send + 'static,
    on_exit: impl FnOnce(Option<i32>) + Send + 'static,
) -> Result<PtyHandle, AgentError> {
    let pair = native_pty_system()
        .openpty(size(spec.cols, spec.rows))
        .map_err(pty_err)?;

    let mut cmd = CommandBuilder::new(&spec.program);
    cmd.args(&spec.args);
    cmd.cwd(&spec.cwd);
    // Platform defaults first (unix: TERM/COLORTERM when the app has none); `spec.env` wins.
    // TODO(macos-verify): Claude Code's TUI shows colours when started from Finder/Dock, where
    // the app has no TERM (plan7 M.8).
    for (k, v) in process::spawn_env_extra() {
        cmd.env(k, v);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    let mut child = pair.slave.spawn_command(cmd).map_err(pty_err)?;
    // The slave end must be closed in this process, otherwise the reader never sees EOF.
    drop(pair.slave);

    let master = pair.master;
    let mut reader = master.try_clone_reader().map_err(pty_err)?;
    let writer = master.take_writer().map_err(pty_err)?;
    let killer = child.clone_killer();
    let pid = child.process_id();
    let tag = pid.map_or_else(|| "?".to_string(), |p| p.to_string());
    let exited = Arc::new(AtomicBool::new(false));
    let exited_w = Arc::clone(&exited);

    std::thread::Builder::new()
        .name(format!("pty-reader-{tag}"))
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => on_output(&buf[..n]),
                }
            }
        })?;

    std::thread::Builder::new()
        .name(format!("pty-waiter-{tag}"))
        .spawn(move || {
            // Exit codes above i32::MAX (e.g. Windows NTSTATUS values) wrap to negative on purpose.
            let code = child.wait().ok().map(|s| s.exit_code() as i32);
            // Before on_exit: `kill_all` may wait for this flag while holding the manager lock
            // that on_exit's sink needs.
            exited_w.store(true, Ordering::SeqCst);
            on_exit(code);
        })?;

    Ok(PtyHandle {
        master,
        writer,
        killer,
        pid,
        exited,
    })
}

impl PtyHandle {
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Writes raw bytes to the child's terminal input.
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), AgentError> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), AgentError> {
        self.master.resize(size(cols, rows)).map_err(pty_err)
    }

    /// Set once the waiter thread has reaped the child (before it reports the exit).
    pub fn exit_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.exited)
    }

    /// Starts ending the child and never blocks; the waiter thread then reports the exit.
    /// Unix: SIGTERM to the process group, SIGKILL after [`PROCESS_KILL_GRACE`]. Windows:
    /// portable-pty's killer (TerminateProcess). See [`process::terminate`].
    // TODO(windows-verify): kill() on the ConPTY child really ends claude.exe and leaves no
    // orphaned node/claude processes after quit_app (plan D.9).
    pub fn kill(&mut self) -> Result<(), AgentError> {
        self.kill_with_grace(PROCESS_KILL_GRACE)
    }

    /// [`Self::kill`] for a restart (step 6b, plan A.7): Unix the same (the child leads its
    /// process group). Windows: `taskkill /PID <pid> /T /F` on a helper thread before
    /// portable-pty's killer, so node/mira-mcp of the old session do not survive it (one
    /// restart per ticket would leak a set per ticket). Never blocks. See
    /// [`process::terminate_tree`].
    // TODO(windows-verify): after a fresh-session restart no node/mira-mcp of the old session is
    // left in Task Manager (plan6b D.101).
    pub fn kill_tree(&mut self) -> Result<(), AgentError> {
        process::terminate_tree(
            self.pid,
            &self.exited,
            self.killer.as_mut(),
            PROCESS_KILL_GRACE,
        )
    }

    /// [`Self::kill`] with another grace period (tests).
    pub(crate) fn kill_with_grace(&mut self, grace: std::time::Duration) -> Result<(), AgentError> {
        process::terminate(self.pid, &self.exited, self.killer.as_mut(), grace)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent::process::{self, pid_is_dead};
    use std::sync::{mpsc, Mutex};
    use std::time::{Duration, Instant};

    struct Child {
        handle: PtyHandle,
        output: Arc<Mutex<Vec<u8>>>,
        exit: mpsc::Receiver<Option<i32>>,
    }

    fn start(script: &str, env: Vec<(String, String)>) -> Child {
        let spec = SpawnSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            cwd: std::env::temp_dir(),
            env,
            cols: 80,
            rows: 24,
        };
        let output = Arc::new(Mutex::new(Vec::new()));
        let out = Arc::clone(&output);
        let (tx, exit) = mpsc::channel();
        let handle = spawn(
            &spec,
            move |b: &[u8]| out.lock().unwrap().extend_from_slice(b),
            move |code| {
                let _ = tx.send(code);
            },
        )
        .unwrap();
        Child {
            handle,
            output,
            exit,
        }
    }

    fn text(c: &Child) -> String {
        String::from_utf8_lossy(&c.output.lock().unwrap()).into_owned()
    }

    /// Waits until the output has a complete `<name>=<digits>` line and returns the number.
    fn read_pid(c: &Child, name: &str) -> u32 {
        let t = Instant::now();
        let needle = format!("{name}=");
        while t.elapsed() < Duration::from_secs(5) {
            let s = text(c);
            if let Some(rest) = s.split(needle.as_str()).nth(1) {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if !digits.is_empty() && rest[digits.len()..].starts_with(['\r', '\n']) {
                    return digits.parse().unwrap();
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("no {name}=<pid> in output: {:?}", text(c));
    }

    fn wait_dead(pid: u32, within: Duration) -> bool {
        let t = Instant::now();
        while t.elapsed() < within {
            if pid_is_dead(pid) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        pid_is_dead(pid)
    }

    #[test]
    fn kill_ends_the_whole_process_group() {
        // HUP ignored (inherited by the children): only the group SIGTERM can end them, not the
        // kernel hangup when the session leader exits.
        let mut c = start(
            r#"trap "" HUP; sleep 30 & echo "P1=$!"; sleep 30 & echo "P2=$!"; wait"#,
            vec![],
        );
        let (p1, p2) = (read_pid(&c, "P1"), read_pid(&c, "P2"));
        c.handle.kill().unwrap();
        c.exit
            .recv_timeout(Duration::from_secs(10))
            .expect("Exited within 10 s");
        assert!(wait_dead(p1, Duration::from_secs(3)), "P1 {p1} survived");
        assert!(wait_dead(p2, Duration::from_secs(3)), "P2 {p2} survived");
    }

    #[test]
    fn kill_escalates_to_sigkill_after_grace() {
        let mut c = start(r#"trap "" TERM; sleep 30 & echo "P1=$!"; wait"#, vec![]);
        let p1 = read_pid(&c, "P1");
        let t = Instant::now();
        c.handle
            .kill_with_grace(Duration::from_millis(300))
            .unwrap();
        c.exit
            .recv_timeout(Duration::from_secs(3))
            .expect("Exited within 3 s");
        assert!(
            t.elapsed() >= Duration::from_millis(250),
            "SIGTERM was ignored, so only SIGKILL ends it"
        );
        assert!(wait_dead(p1, Duration::from_secs(3)), "P1 {p1} survived");
    }

    #[test]
    fn exit_flag_is_set_before_on_exit() {
        let slot: Arc<Mutex<Option<Arc<AtomicBool>>>> = Arc::new(Mutex::new(None));
        let seen = Arc::clone(&slot);
        let (tx, rx) = mpsc::channel();
        let spec = SpawnSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "exit 0".into()],
            cwd: std::env::temp_dir(),
            env: vec![],
            cols: 80,
            rows: 24,
        };
        let handle = spawn(
            &spec,
            |_| {},
            move |_| {
                // The handle may not be stored yet when the child exits at once.
                let t = Instant::now();
                let flag = loop {
                    if let Some(f) = seen.lock().unwrap().clone() {
                        break f;
                    }
                    assert!(t.elapsed() < Duration::from_secs(5));
                    std::thread::sleep(Duration::from_millis(5));
                };
                let _ = tx.send(flag.load(Ordering::SeqCst));
            },
        )
        .unwrap();
        *slot.lock().unwrap() = Some(handle.exit_flag());
        assert!(rx.recv_timeout(Duration::from_secs(10)).unwrap());
        assert!(handle.exit_flag().load(Ordering::SeqCst));
    }

    #[test]
    fn spec_env_wins_over_platform_defaults() {
        let c = start(
            r#"printf "<%s>" "$TERM""#,
            vec![("TERM".into(), "mira-test".into())],
        );
        c.exit.recv_timeout(Duration::from_secs(10)).unwrap();
        let t = Instant::now();
        while !text(&c).contains('>') && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(text(&c).contains("<mira-test>"), "{:?}", text(&c));
    }

    #[test]
    fn finish_all_returns_fast_when_everything_exited() {
        let c = start("exit 0", vec![]);
        c.exit.recv_timeout(Duration::from_secs(10)).unwrap();
        let pid = c.handle.pid().unwrap();
        let t = Instant::now();
        process::finish_all(&[(pid, c.handle.exit_flag())], Duration::from_millis(1500));
        assert!(
            t.elapsed() < Duration::from_millis(100),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn finish_all_kills_survivors() {
        let mut c = start(r#"trap "" TERM; sleep 30 & echo "P1=$!"; wait"#, vec![]);
        let p1 = read_pid(&c, "P1");
        let pid = c.handle.pid().unwrap();
        // SIGTERM only (the reaper would wait a minute): finish_all must do the SIGKILL.
        c.handle.kill_with_grace(Duration::from_secs(60)).unwrap();
        let t = Instant::now();
        process::finish_all(&[(pid, c.handle.exit_flag())], Duration::from_millis(300));
        let took = t.elapsed();
        assert!(
            took >= Duration::from_millis(250) && took < Duration::from_secs(1),
            "{took:?}"
        );
        c.exit
            .recv_timeout(Duration::from_secs(3))
            .expect("Exited within 3 s");
        assert!(wait_dead(p1, Duration::from_secs(3)), "P1 {p1} survived");
    }

    #[test]
    fn kill_sends_nothing_when_the_group_is_empty() {
        let mut c = start("exit 0", vec![]);
        c.exit.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(c.handle.exit_flag().load(Ordering::SeqCst));
        assert!(!process::group_alive(c.handle.pid().unwrap()));
        assert!(
            c.handle.kill().is_ok(),
            "already reaped, group empty: no signal, no error"
        );
    }

    /// The leader dies on SIGTERM (and is reaped); a member that ignores TERM and HUP (the
    /// session leader's exit sends SIGHUP) keeps the group alive and must get the SIGKILL.
    const LEADER_EXITS_MEMBER_SURVIVES: &str =
        r#"sh -c 'trap "" TERM HUP; exec sleep 30' & echo "P1=$!"; sleep 30"#;

    #[test]
    fn reaper_kills_survivors_after_the_leader_exited() {
        let mut c = start(LEADER_EXITS_MEMBER_SURVIVES, vec![]);
        let p1 = read_pid(&c, "P1");
        // Let the inner sh install its traps before the signal.
        std::thread::sleep(Duration::from_millis(200));
        c.handle
            .kill_with_grace(Duration::from_millis(500))
            .unwrap();
        c.exit
            .recv_timeout(Duration::from_secs(3))
            .expect("the leader exits on SIGTERM");
        assert!(c.handle.exit_flag().load(Ordering::SeqCst));
        assert!(wait_dead(p1, Duration::from_secs(3)), "P1 {p1} survived");
    }

    #[test]
    fn finish_all_kills_survivors_after_the_leader_exited() {
        let mut c = start(LEADER_EXITS_MEMBER_SURVIVES, vec![]);
        let p1 = read_pid(&c, "P1");
        let pid = c.handle.pid().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // SIGTERM only (the reaper would wait a minute): finish_all must do the SIGKILL.
        c.handle.kill_with_grace(Duration::from_secs(60)).unwrap();
        c.exit
            .recv_timeout(Duration::from_secs(3))
            .expect("the leader exits on SIGTERM");
        assert!(!pid_is_dead(p1), "P1 ignores SIGTERM");
        let t = Instant::now();
        process::finish_all(&[(pid, c.handle.exit_flag())], Duration::from_millis(300));
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        assert!(wait_dead(p1, Duration::from_secs(3)), "P1 {p1} survived");
    }
}
