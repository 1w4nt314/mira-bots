//! Accept loop: Windows named pipe (one instance per connection) or, for Linux testing,
//! a Unix domain socket. Each connection is handled by [`handle_connection`] on its own task.

use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::handler::{handle_connection, HandlerCtx};

/// Attempts at creating a pipe instance (or binding the socket) before the server gives up.
pub const CREATE_ATTEMPTS: u32 = 5;
/// Pause between two such attempts.
pub const CREATE_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Calls `create` up to `attempts` times (at least once), sleeping `delay` between failures.
/// Returns the first success or the last error.
pub async fn create_with_retry<T, E, F, S, Fut>(
    attempts: u32,
    delay: Duration,
    mut create: F,
    mut sleep: S,
) -> Result<T, E>
where
    E: std::fmt::Display,
    F: FnMut() -> Result<T, E>,
    S: FnMut(Duration) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut attempt = 1;
    loop {
        match create() {
            Ok(v) => return Ok(v),
            Err(e) if attempt < attempts => {
                log::warn!("pipe: create attempt {attempt}/{attempts} failed: {e}; retrying");
                attempt += 1;
                sleep(delay).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// [`create_with_retry`] with the default attempts/delay on the tokio timer.
async fn create_retrying<T, F>(create: F) -> io::Result<T>
where
    F: FnMut() -> io::Result<T>,
{
    create_with_retry(
        CREATE_ATTEMPTS,
        CREATE_RETRY_DELAY,
        create,
        tokio::time::sleep,
    )
    .await
}

/// Starts the accept loop on Tauri's async runtime. `ready` is set once the pipe exists and
/// cleared if the server stops; on failure the app keeps running without hooks (error is logged)
/// and `spawn_agent` refuses to start agents.
pub fn start(name: String, ctx: HandlerCtx, ready: Arc<AtomicBool>) {
    tauri::async_runtime::spawn(async move {
        let result = run(name.clone(), ctx, Arc::clone(&ready)).await;
        ready.store(false, Ordering::Release);
        if let Err(e) = result {
            log::error!("pipe server on {name} stopped: {e}; hook events are disabled");
        }
    });
}

/// Serves `\\.\pipe\mira-bots-<pid>` until an instance cannot be created (after retries).
// TODO(windows-verify): first_pipe_instance(true) + a fresh instance per connection under
// concurrent hook events (several agents, PreToolUse/PostToolUse bursts) (plan D.7).
#[cfg(windows)]
pub async fn run(name: String, ctx: HandlerCtx, ready: Arc<AtomicBool>) -> io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut server = create_retrying(|| {
        ServerOptions::new()
            .first_pipe_instance(true)
            .reject_remote_clients(true)
            .create(&name)
    })
    .await?;
    ready.store(true, Ordering::Release);
    log::info!("pipe server listening on {name}");
    loop {
        let connected = server.connect().await;
        // Create the next instance before handing this one off, so clients never find no pipe.
        let next = create_retrying(|| {
            ServerOptions::new()
                .reject_remote_clients(true)
                .create(&name)
        })
        .await;
        let next = match next {
            Ok(next) => next,
            Err(e) => {
                ready.store(false, Ordering::Release);
                // Still serve the client that is already connected.
                if connected.is_ok() {
                    tokio::spawn(handle_connection(server, ctx.clone()));
                }
                return Err(e);
            }
        };
        let current = std::mem::replace(&mut server, next);
        match connected {
            Ok(()) => {
                tokio::spawn(handle_connection(current, ctx.clone()));
            }
            Err(e) => log::warn!("pipe connect failed: {e}"),
        }
    }
}

/// Serves a Unix socket at `name` (a path). Linux/macOS are only used for development/tests.
#[cfg(unix)]
pub async fn run(name: String, ctx: HandlerCtx, ready: Arc<AtomicBool>) -> io::Result<()> {
    let listener = create_retrying(|| {
        let _ = std::fs::remove_file(&name);
        tokio::net::UnixListener::bind(&name)
    })
    .await?;
    ready.store(true, Ordering::Release);
    log::info!("pipe server listening on {name}");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle_connection(stream, ctx.clone()));
            }
            Err(e) => {
                log::warn!("socket accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    async fn no_sleep(_: Duration) {}

    #[tokio::test]
    async fn retry_succeeds_after_transient_failures() {
        let calls = Cell::new(0);
        let slept = RefCell::new(Vec::new());
        let r: Result<&str, String> = create_with_retry(
            5,
            Duration::from_millis(200),
            || {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    Err(format!("busy {}", calls.get()))
                } else {
                    Ok("pipe")
                }
            },
            |d| {
                slept.borrow_mut().push(d);
                no_sleep(d)
            },
        )
        .await;
        assert_eq!(r, Ok("pipe"));
        assert_eq!(calls.get(), 3);
        assert_eq!(*slept.borrow(), vec![Duration::from_millis(200); 2]);
    }

    #[tokio::test]
    async fn retry_gives_up_with_the_last_error_after_all_attempts() {
        let calls = Cell::new(0);
        let slept = Cell::new(0);
        let r: Result<(), String> = create_with_retry(
            CREATE_ATTEMPTS,
            CREATE_RETRY_DELAY,
            || {
                calls.set(calls.get() + 1);
                Err(format!("fail {}", calls.get()))
            },
            |d| {
                slept.set(slept.get() + 1);
                no_sleep(d)
            },
        )
        .await;
        assert_eq!(r, Err("fail 5".to_string()));
        assert_eq!(calls.get(), 5);
        assert_eq!(slept.get(), 4, "no sleep after the last attempt");
    }

    #[tokio::test]
    async fn retry_with_zero_attempts_still_tries_once() {
        let calls = Cell::new(0);
        let r: Result<(), &str> = create_with_retry(
            0,
            Duration::ZERO,
            || {
                calls.set(calls.get() + 1);
                Err("x")
            },
            no_sleep,
        )
        .await;
        assert_eq!(r, Err("x"));
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn default_retry_waits_5_x_200_ms_in_total_800_ms() {
        let t0 = tokio::time::Instant::now();
        let r: io::Result<()> = create_retrying(|| Err(io::Error::other("taken"))).await;
        assert!(r.is_err());
        assert_eq!(t0.elapsed(), Duration::from_millis(800));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent::AgentManager;
    use crate::hooks::status::AgentStatus;
    use crate::permissions::PendingPermissions;
    use std::sync::Mutex;

    /// Full chain on Linux: mira-hook's `run` (blocking client) → Unix socket → server → handler.
    #[tokio::test]
    async fn hook_client_to_server_round_trip() {
        let mut m = AgentManager::new(5);
        let agent = m.insert_fake("sess-e2e", "/w/demo");
        m.whitelist_add(&agent, "Bash").unwrap();
        let manager = Arc::new(Mutex::new(m));
        let ctx = HandlerCtx {
            manager: Arc::clone(&manager),
            pending: Arc::new(Mutex::new(PendingPermissions::new())),
            emit: Arc::new(|_: &str, _| {}),
            stats: Arc::new(crate::diagnostics::HookStats::default()),
            observer: None,
        };
        let name = std::env::temp_dir()
            .join(format!("mira-bots-test-{}.sock", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        let ready = Arc::new(AtomicBool::new(false));
        let server = tokio::spawn(run(name.clone(), ctx, Arc::clone(&ready)));
        for _ in 0..200 {
            if ready.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(ready.load(Ordering::Acquire), "server must report ready");
        assert!(std::path::Path::new(&name).exists());

        let pipe = name.clone();
        let stdout = tokio::task::spawn_blocking(move || {
            let input = br#"{"hook_event_name":"PermissionRequest","session_id":"sess-e2e","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
            mira_hook::run(input, Some(pipe), None, false)
        })
        .await
        .unwrap();
        assert_eq!(
            stdout.as_deref(),
            Some(
                r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
            )
        );
        assert_eq!(
            manager.lock().unwrap().get(&agent).unwrap().status,
            AgentStatus::Running
        );

        let pipe = name.clone();
        let agent_for_hook = agent.clone();
        let stdout = tokio::task::spawn_blocking(move || {
            mira_hook::run(
                br#"{"hook_event_name":"Stop","session_id":"sess-e2e"}"#,
                Some(pipe),
                Some(agent_for_hook),
                false,
            )
        })
        .await
        .unwrap();
        assert_eq!(stdout, None);
        // The Stop frame is handled asynchronously after the hook exits.
        for _ in 0..200 {
            if manager.lock().unwrap().get(&agent).unwrap().status == AgentStatus::Idle {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            manager.lock().unwrap().get(&agent).unwrap().status,
            AgentStatus::Idle
        );

        server.abort();
        let _ = std::fs::remove_file(&name);
    }

    #[tokio::test(start_paused = true)]
    async fn bind_failure_is_retried_then_reported_and_never_ready() {
        let ctx = HandlerCtx {
            manager: Arc::new(Mutex::new(AgentManager::new(5))),
            pending: Arc::new(Mutex::new(PendingPermissions::new())),
            emit: Arc::new(|_: &str, _| {}),
            stats: Arc::new(crate::diagnostics::HookStats::default()),
            observer: None,
        };
        let name = std::env::temp_dir()
            .join(format!("mira-bots-no-such-dir-{}", uuid::Uuid::new_v4()))
            .join("x.sock")
            .to_string_lossy()
            .into_owned();
        let ready = Arc::new(AtomicBool::new(false));
        let t0 = tokio::time::Instant::now();
        assert!(run(name, ctx, Arc::clone(&ready)).await.is_err());
        assert_eq!(t0.elapsed(), CREATE_RETRY_DELAY * (CREATE_ATTEMPTS - 1));
        assert!(!ready.load(Ordering::Acquire));
    }
}
