//! Client side of the pipe: Windows named pipe via `std::fs::OpenOptions`, Unix domain socket
//! elsewhere (Unix domain socket: Linux til test, macOS i produktion).
//!
//! Copy of `crates/mira-hook/src/transport.rs`, kept in sync BY HAND (plan4 A: duplicated on
//! purpose instead of a shared crate). Change both files together; never change the hook side
//! just to suit this one.

use std::io::{self, Read, Write};
use std::time::Instant;

/// Env var set by the app on the `claude` child and inherited by hook processes.
pub const PIPE_ENV: &str = "MIRA_BOTS_PIPE";

/// Maximum time spent retrying a busy pipe.
pub const CONNECT_RETRY_BUDGET: std::time::Duration = std::time::Duration::from_millis(300);

pub trait ReadWrite: Read + Write + Send {}
impl<T: Read + Write + Send> ReadWrite for T {}

/// The pipe name from `MIRA_BOTS_PIPE`; `None` (or empty) means we were not started by the app.
pub fn pipe_name_from_env() -> Option<String> {
    std::env::var(PIPE_ENV).ok().filter(|s| !s.is_empty())
}

#[cfg(windows)]
const ERROR_PIPE_BUSY: i32 = 231;

/// Opens the app's named pipe (`\\.\pipe\mira-bots-<pid>`). Retries only on ERROR_PIPE_BUSY,
/// sleeping 20 ms between attempts, for at most 300 ms (or until `deadline`, if earlier).
/// Any other error (e.g. ERROR_FILE_NOT_FOUND = 2: app not running) fails immediately.
// TODO(windows-verify): `OpenOptions::open` against the tokio named-pipe server from the hook exe,
// and the ERROR_PIPE_BUSY retry under concurrent hook events (plan D.7); from mira-mcp: one
// connection per tool call, closed right after the reply line (plan4 D.45).
#[cfg(windows)]
pub fn connect(name: &str, deadline: Instant) -> io::Result<Box<dyn ReadWrite>> {
    use std::time::Duration;
    let give_up = deadline.min(Instant::now() + CONNECT_RETRY_BUDGET);
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
        {
            Ok(f) => return Ok(Box::new(f)),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < give_up => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Connects to the app's Unix socket (the "pipe name" is a socket path in the app's private
/// `mira-bots-<uid>` directory; Linux til test, macOS i produktion). `deadline` is unused:
/// a Unix connect either succeeds or fails immediately.
#[cfg(unix)]
pub fn connect(name: &str, _deadline: Instant) -> io::Result<Box<dyn ReadWrite>> {
    let s = std::os::unix::net::UnixStream::connect(name)?;
    Ok(Box::new(s))
}
