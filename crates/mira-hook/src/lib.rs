//! mira-hook: forwards one Claude Code hook event (stdin JSON) to the mira-bots app over a pipe
//! and, for PermissionRequest only, turns the app's answer into Claude Code's decision JSON.
//!
//! Hard rule: this must never block or break Claude Code. Every failure path ends in exit code 0
//! with empty stdout, within the per-event budget ([`payload::budget`]).

pub mod decision;
pub mod payload;
pub mod transport;

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::mpsc;
use std::time::Instant;

use decision::Decision;

/// Maximum hook stdin accepted (bytes). Larger input is ignored (exit 0, no output).
pub const MAX_STDIN: usize = 4 << 20;
/// Maximum reply line read from the app (bytes).
pub const MAX_REPLY: u64 = 64 << 10;

/// Env var carrying the app's agent id (set by the app on the claude process, inherited by the
/// hook). Sent back as the frame's top-level `agent_id` when set and non-empty.
pub const AGENT_ID_ENV: &str = "MIRA_AGENT_ID";

/// Reads [`AGENT_ID_ENV`]; empty counts as unset.
pub fn agent_id_from_env() -> Option<String> {
    std::env::var(AGENT_ID_ENV).ok().filter(|s| !s.is_empty())
}

/// Env var enabling one stderr line per step.
pub const DEBUG_ENV: &str = "MIRA_HOOK_DEBUG";

pub fn debug_from_env() -> bool {
    std::env::var(DEBUG_ENV).is_ok_and(|v| v == "1")
}

fn log(debug: bool, msg: &str) {
    if debug {
        let _ = writeln!(std::io::stderr().lock(), "mira-hook: {msg}");
    }
}

/// Sends the frame and, if `wait_reply`, reads one reply line.
fn exchange(
    pipe: &str,
    frame: &str,
    wait_reply: bool,
    deadline: Instant,
) -> std::io::Result<Decision> {
    let mut stream = transport::connect(pipe, deadline)?;
    stream.write_all(frame.as_bytes())?;
    stream.flush()?;
    if !wait_reply {
        return Ok(Decision::None);
    }
    let mut line = String::new();
    BufReader::new(stream.take(MAX_REPLY)).read_line(&mut line)?;
    Ok(decision::parse_reply(&line))
}

/// Runs one hook invocation and returns what must be written to stdout (if anything).
///
/// `pipe` is the pipe name from `MIRA_BOTS_PIPE`; `None` means "not started by the app".
/// `agent_id` comes from `MIRA_AGENT_ID` and is forwarded in the frame when present.
/// All I/O runs on a worker thread; the calling thread waits at most `payload::budget(event)`.
/// The worker is never joined: the caller is expected to exit the process right after.
pub fn run(
    input: &[u8],
    pipe: Option<String>,
    agent_id: Option<String>,
    debug: bool,
) -> Option<String> {
    if input.len() > MAX_STDIN {
        log(debug, "stdin too large, ignoring");
        return None;
    }
    let text = std::str::from_utf8(input).ok()?;
    let mut payload = match payload::parse(text) {
        Ok(p) => p,
        Err(e) => {
            log(debug, &format!("parse failed: {e}"));
            return None;
        }
    };
    payload::trim(&mut payload.json);
    let Some(pipe) = pipe else {
        log(debug, "MIRA_BOTS_PIPE not set, nothing to do");
        return None;
    };

    let budget = payload::budget(&payload.event_name);
    let deadline = Instant::now() + budget;
    let wait_reply = payload::expects_reply(&payload.event_name);
    let frame = payload::to_frame(&payload, agent_id.as_deref());
    log(
        debug,
        &format!("{} -> {pipe} (budget {budget:?})", payload.event_name),
    );

    let (tx, rx) = mpsc::channel::<Decision>();
    let spawned = std::thread::Builder::new()
        .name("mira-hook-worker".into())
        .spawn(
            move || match exchange(&pipe, &frame, wait_reply, deadline) {
                Ok(d) => {
                    let _ = tx.send(d);
                }
                Err(e) => {
                    log(debug, &format!("pipe error: {e}"));
                    // Dropping `tx` wakes the main thread with Disconnected.
                }
            },
        );
    if spawned.is_err() {
        log(debug, "could not start worker thread");
        return None;
    }

    match rx.recv_timeout(budget) {
        Ok(d) => {
            log(debug, &format!("decision: {d:?}"));
            decision::to_stdout(&d)
        }
        Err(e) => {
            log(debug, &format!("no decision: {e}"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn no_pipe_env_means_no_output() {
        let t = Instant::now();
        assert_eq!(
            run(
                br#"{"hook_event_name":"PermissionRequest","session_id":"s"}"#,
                None,
                None,
                false
            ),
            None
        );
        assert!(t.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn garbage_or_oversized_input_means_no_output() {
        assert_eq!(run(b"not json", Some("x".into()), None, false), None);
        assert_eq!(run(&[0xff, 0xfe], Some("x".into()), None, false), None);
        let big = vec![b' '; MAX_STDIN + 1];
        assert_eq!(run(&big, Some("x".into()), None, false), None);
    }

    #[test]
    fn unreachable_pipe_returns_quickly_without_output() {
        let t = Instant::now();
        let out = run(
            br#"{"hook_event_name":"PermissionRequest","session_id":"s"}"#,
            Some(
                std::env::temp_dir()
                    .join("mira-bots-does-not-exist.sock")
                    .to_string_lossy()
                    .into_owned(),
            ),
            None,
            false,
        );
        assert_eq!(out, None);
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "must not wait out the 110 s budget"
        );
    }

    #[cfg(unix)]
    mod unix_socket {
        use super::super::*;
        use std::os::unix::net::UnixListener;
        use std::path::PathBuf;
        use std::time::Duration;

        fn sock_path(tag: &str) -> PathBuf {
            std::env::temp_dir().join(format!("mira-hook-test-{}-{tag}.sock", std::process::id()))
        }

        /// Accepts one connection, returns the frame it received, replies with `reply` (if any).
        fn serve_once(
            path: &PathBuf,
            reply: Option<&'static str>,
        ) -> std::thread::JoinHandle<String> {
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path).unwrap();
            std::thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(s.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                if let Some(r) = reply {
                    s.write_all(r.as_bytes()).unwrap();
                }
                line
            })
        }

        #[test]
        fn permission_request_allow_round_trip() {
            let path = sock_path("allow");
            let server = serve_once(
                &path,
                Some("{\"v\":1,\"kind\":\"decision\",\"decision\":\"allow\",\"message\":null}\n"),
            );
            let input = br#"{"hook_event_name":"PermissionRequest","session_id":"s","transcript_path":"/x","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
            let out = run(
                input,
                Some(path.to_string_lossy().into_owned()),
                Some("agent-42".into()),
                false,
            );
            assert_eq!(out, decision::to_stdout(&Decision::Allow));
            let frame: serde_json::Value = serde_json::from_str(&server.join().unwrap()).unwrap();
            assert_eq!(frame["kind"], "hook");
            assert_eq!(frame["agent_id"], "agent-42");
            assert_eq!(frame["event"]["tool_name"], "Bash");
            assert!(frame["event"].get("transcript_path").is_none());
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn permission_request_none_reply_prints_nothing() {
            let path = sock_path("none");
            let server = serve_once(
                &path,
                Some("{\"v\":1,\"kind\":\"decision\",\"decision\":\"none\",\"message\":null}\n"),
            );
            let input = br#"{"hook_event_name":"PermissionRequest","session_id":"s"}"#;
            assert_eq!(
                run(
                    input,
                    Some(path.to_string_lossy().into_owned()),
                    None,
                    false
                ),
                None
            );
            server.join().unwrap();
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn other_events_do_not_wait_for_a_reply() {
            let path = sock_path("stop");
            let server = serve_once(&path, None);
            let t = Instant::now();
            let input = br#"{"hook_event_name":"Stop","session_id":"s"}"#;
            assert_eq!(
                run(
                    input,
                    Some(path.to_string_lossy().into_owned()),
                    None,
                    false
                ),
                None
            );
            assert!(t.elapsed() < Duration::from_secs(1));
            let frame: serde_json::Value = serde_json::from_str(&server.join().unwrap()).unwrap();
            assert_eq!(frame["event"]["hook_event_name"], "Stop");
            assert!(frame.get("agent_id").is_none(), "no MIRA_AGENT_ID → no key");
            let _ = std::fs::remove_file(&path);
        }
    }
}
