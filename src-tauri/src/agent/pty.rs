//! One child process in a pseudo terminal (ConPTY on Windows) with a reader and a waiter thread.

use std::io::{Read, Write};
use std::path::PathBuf;

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

use super::AgentError;

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
            on_exit(code);
        })?;

    Ok(PtyHandle {
        master,
        writer,
        killer,
        pid,
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

    /// Kills the child. The waiter thread then reports the exit.
    // TODO(windows-verify): kill() on the ConPTY child really ends claude.exe and leaves no
    // orphaned node/claude processes after quit_app (plan D.9).
    pub fn kill(&mut self) -> Result<(), AgentError> {
        self.killer.kill()?;
        Ok(())
    }
}
