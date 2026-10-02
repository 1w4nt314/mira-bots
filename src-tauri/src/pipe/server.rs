//! Accept loop: Windows named pipe (one instance per connection) or a Unix domain socket
//! (Linux for tests, macOS in production). Each connection is handled by [`handle_connection`] on
//! its own task.

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
/// cleared if the server stops; on failure the app keeps running without hooks (the error is
/// logged and stored in `error` for Diagnostik's Pipe-note) and `spawn_agent` refuses to start
/// agents.
pub fn start(
    name: String,
    ctx: HandlerCtx,
    ready: Arc<AtomicBool>,
    error: Arc<std::sync::Mutex<Option<String>>>,
) {
    tauri::async_runtime::spawn(async move {
        let result = run(name.clone(), ctx, Arc::clone(&ready)).await;
        ready.store(false, Ordering::Release);
        if let Err(e) = result {
            log::error!("pipe server on {name} stopped: {e}; hook events are disabled");
            *error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
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

/// Serves a Unix domain socket at `name` (a path; Linux for tests, macOS in production) in a
/// private directory (plan7 C7.2). A too long path or a directory that is not private fails at
/// once (no retries); binding is retried. The socket file is removed when this task ends or is
/// aborted ([`unix_socket::SocketGuard`]) and on app exit/panic (`cleanup_registered`).
#[cfg(unix)]
pub async fn run(name: String, ctx: HandlerCtx, ready: Arc<AtomicBool>) -> io::Result<()> {
    use super::unix_socket;
    let path = std::path::Path::new(&name);
    unix_socket::check_length(path).map_err(io::Error::other)?;
    unix_socket::prepare(path, unix_socket::uid())?;
    let listener = create_retrying(|| {
        let _ = std::fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path)?;
        unix_socket::restrict(path)?;
        Ok(listener)
    })
    .await?;
    let _guard = unix_socket::SocketGuard::new(path.into());
    unix_socket::register(path.into());
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
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    fn ctx() -> HandlerCtx {
        HandlerCtx {
            manager: Arc::new(Mutex::new(AgentManager::new(5))),
            pending: Arc::new(Mutex::new(PendingPermissions::new())),
            emit: Arc::new(|_: &str, _| {}),
            stats: Arc::new(crate::diagnostics::HookStats::default()),
            observer: None,
            tools: None,
        }
    }

    /// `<tmp>/mira-bots-t<pid>-<8 hex>/<file>` (short: a macOS runner's `$TMPDIR` is already 49
    /// characters), or the same under the production fallback base when that is too long; the
    /// dir does not exist yet (run creates it 0700). The prefix lets `cleanup` remove the dir.
    fn test_socket(file: &str) -> PathBuf {
        use crate::pipe::unix_socket;
        let dir = format!(
            "{}t{}-{}",
            crate::config::SOCKET_DIR_PREFIX,
            std::process::id(),
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        unix_socket::first_that_fits(
            std::env::temp_dir().join(&dir).join(file),
            Path::new(unix_socket::FALLBACK_BASE).join(&dir).join(file),
        )
    }

    #[test]
    fn test_sockets_always_fit() {
        for file in ["e2e.sock", "x.sock", "g.sock"] {
            let p = test_socket(file);
            assert!(
                crate::pipe::unix_socket::check_length(&p).is_ok(),
                "{}",
                p.display()
            );
        }
    }

    async fn wait_ready(ready: &AtomicBool) {
        for _ in 0..200 {
            if ready.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(ready.load(Ordering::Acquire), "server must report ready");
    }

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
            tools: None,
        };
        let name = test_socket("e2e.sock").to_string_lossy().into_owned();
        let ready = Arc::new(AtomicBool::new(false));
        let server = tokio::spawn(run(name.clone(), ctx, Arc::clone(&ready)));
        wait_ready(&ready).await;
        assert!(std::path::Path::new(&name).exists());

        let pipe = name.clone();
        let stdout = tokio::task::spawn_blocking(move || {
            let input = br#"{"hook_event_name":"PermissionRequest","session_id":"sess-e2e","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
            mira_hook::run(input, Some(pipe), None, false, None)
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
                None,
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
        let _ = server.await;
        crate::pipe::unix_socket::cleanup(Path::new(&name));
    }

    #[tokio::test(start_paused = true)]
    async fn bind_failure_is_retried_then_reported_and_never_ready() {
        // The socket path is an existing directory: bind fails (EADDRINUSE) every time.
        let path = test_socket("x.sock");
        let dir = path.parent().unwrap().to_path_buf();
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        std::fs::create_dir(&path).unwrap();
        let name = path.to_string_lossy().into_owned();
        let ready = Arc::new(AtomicBool::new(false));
        let t0 = tokio::time::Instant::now();
        assert!(run(name, ctx(), Arc::clone(&ready)).await.is_err());
        assert_eq!(t0.elapsed(), CREATE_RETRY_DELAY * (CREATE_ATTEMPTS - 1));
        assert!(!ready.load(Ordering::Acquire));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn prepare_failure_is_reported_at_once() {
        // The socket's directory is a file: no retries, the Danish text comes back.
        let path = test_socket("x.sock");
        let dir = path.parent().unwrap().to_path_buf();
        std::fs::write(&dir, b"file").unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        let t0 = tokio::time::Instant::now();
        let err = run(
            path.to_string_lossy().into_owned(),
            ctx(),
            Arc::clone(&ready),
        )
        .await
        .unwrap_err();
        assert_eq!(t0.elapsed(), Duration::ZERO);
        assert!(
            err.to_string()
                .starts_with("Kunne ikke oprette socket-mappen"),
            "{err}"
        );
        assert!(!ready.load(Ordering::Acquire));
        std::fs::remove_file(&dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn too_long_path_is_reported_at_once() {
        // Injected bases (no real long $TMPDIR): primary and fallback are both too long.
        let id = uuid::Uuid::new_v4().simple().to_string();
        let tmp = Path::new("/tmp").join(format!("mb-long-{id}-{}", "l".repeat(80)));
        let base = Path::new("/tmp").join(format!("mb-fb-{id}-{}", "f".repeat(80)));
        let long = crate::pipe::unix_socket::choose_path_in(&tmp, &base, 501, 4242);
        let ready = Arc::new(AtomicBool::new(false));
        let t0 = tokio::time::Instant::now();
        let err = run(
            long.to_string_lossy().into_owned(),
            ctx(),
            Arc::clone(&ready),
        )
        .await
        .unwrap_err();
        assert_eq!(t0.elapsed(), Duration::ZERO);
        assert!(
            err.to_string().starts_with("Socket-stien er for lang ("),
            "{err}"
        );
        assert!(!long.parent().unwrap().exists(), "nothing created");
        assert!(!tmp.exists() && !base.exists(), "nothing created");
        assert!(!ready.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn start_stores_the_error_for_diagnostics() {
        let path = test_socket("x.sock");
        let dir = path.parent().unwrap().to_path_buf();
        std::fs::write(&dir, b"file").unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        start(
            path.to_string_lossy().into_owned(),
            ctx(),
            Arc::clone(&ready),
            Arc::clone(&error),
        );
        let mut stored = None;
        for _ in 0..200 {
            stored = error.lock().unwrap().clone();
            if stored.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let stored = stored.expect("the error is stored");
        assert!(
            stored.starts_with("Kunne ikke oprette socket-mappen"),
            "{stored}"
        );
        assert!(!ready.load(Ordering::Acquire));
        std::fs::remove_file(&dir).unwrap();
    }

    #[tokio::test]
    async fn bind_sets_0600_and_guard_removes_file() {
        let path = test_socket("g.sock");
        let dir = path.parent().unwrap().to_path_buf();
        let ready = Arc::new(AtomicBool::new(false));
        let server = tokio::spawn(run(
            path.to_string_lossy().into_owned(),
            ctx(),
            Arc::clone(&ready),
        ));
        wait_ready(&ready).await;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir), 0o700);
        server.abort();
        for _ in 0..100 {
            if !path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !path.exists(),
            "the guard removes the socket when the task ends"
        );
        assert!(!dir.exists(), "and its empty directory");
    }
}
