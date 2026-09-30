//! Accept loop: Windows named pipe (one instance per connection) or, for Linux testing,
//! a Unix domain socket. Each connection is handled by [`handle_connection`] on its own task.

use std::io;

use super::handler::{handle_connection, HandlerCtx};

/// Starts the accept loop on Tauri's async runtime. On failure the app keeps running without
/// hooks (error is logged).
pub fn start(name: String, ctx: HandlerCtx) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = run(name.clone(), ctx).await {
            log::error!("pipe server on {name} stopped: {e}; hook events are disabled");
        }
    });
}

/// Serves `\\.\pipe\mira-bots-<pid>` until an instance cannot be created.
// TODO(windows-verify): first_pipe_instance(true) + a fresh instance per connection under
// concurrent hook events (several agents, PreToolUse/PostToolUse bursts) (plan D.7).
#[cfg(windows)]
pub async fn run(name: String, ctx: HandlerCtx) -> io::Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .create(&name)?;
    log::info!("pipe server listening on {name}");
    loop {
        let connected = server.connect().await;
        // Create the next instance before handing this one off, so clients never find no pipe.
        let next = ServerOptions::new()
            .reject_remote_clients(true)
            .create(&name)?;
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
pub async fn run(name: String, ctx: HandlerCtx) -> io::Result<()> {
    let _ = std::fs::remove_file(&name);
    let listener = tokio::net::UnixListener::bind(&name)?;
    log::info!("pipe server listening on {name}");
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(handle_connection(stream, ctx.clone()));
            }
            Err(e) => {
                log::warn!("socket accept failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent::AgentManager;
    use crate::hooks::status::AgentStatus;
    use crate::permissions::PendingPermissions;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

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
        };
        let name = std::env::temp_dir()
            .join(format!("mira-bots-test-{}.sock", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        let server = tokio::spawn(run(name.clone(), ctx));
        for _ in 0..200 {
            if std::path::Path::new(&name).exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let pipe = name.clone();
        let stdout = tokio::task::spawn_blocking(move || {
            let input = br#"{"hook_event_name":"PermissionRequest","session_id":"sess-e2e","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
            mira_hook::run(input, Some(pipe), false)
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
        let stdout = tokio::task::spawn_blocking(move || {
            mira_hook::run(
                br#"{"hook_event_name":"Stop","session_id":"sess-e2e"}"#,
                Some(pipe),
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
}
