//! The bridge to the app: one pipe connection per tool call (plan4 C4.2).
//!
//! `{"v":1,"kind":"tool","agent_id",…}` + `\n` out, one `tool_result` line back, then the handle
//! is closed at once (the app waits for our EOF before it drops its end). All pipe I/O runs on a
//! worker thread; the caller waits at most `timeout` (`recv_timeout`), so a frozen app costs one
//! `isError` result, never a hung Claude Code.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::{log, transport, MAX_REPLY};

/// Runs one tool for the MCP layer. `Err` is Danish text for the model (`isError: true`).
pub trait ToolBackend {
    fn call(&self, tool: &str, args: Value) -> Result<Value, String>;
}

pub const ERR_NO_PIPE: &str = "mira-bots kører ikke (MIRA_BOTS_PIPE mangler)";
pub const ERR_NOT_RUNNING: &str = "mira-bots kører ikke";
pub const ERR_NO_AGENT: &str = "Agent-id mangler (MIRA_AGENT_ID)";
pub const ERR_BAD_REPLY: &str = "Ugyldigt svar fra mira-bots";
pub const ERR_TOO_LARGE: &str = "Svar fra mira-bots for stort";

/// "mira-bots svarede ikke inden for 10 s" (ms when not whole seconds, e.g. in tests).
pub fn timeout_text(timeout: Duration) -> String {
    if timeout.subsec_millis() == 0 {
        format!("mira-bots svarede ikke inden for {} s", timeout.as_secs())
    } else {
        format!(
            "mira-bots svarede ikke inden for {} ms",
            timeout.as_millis()
        )
    }
}

pub struct PipeBackend {
    pub pipe: Option<String>,
    pub agent_id: Option<String>,
    pub timeout: Duration,
    pub debug: bool,
    seq: AtomicU64,
}

/// Why the worker produced no reply line.
enum WorkerError {
    Connect(std::io::Error),
    Io(std::io::Error),
    /// The reply line was longer than `MAX_REPLY` (no newline within the limit).
    TooLarge,
}

/// Connect, send the frame, read one line (at most `MAX_REPLY` bytes), close.
fn exchange(pipe: &str, frame: &str, deadline: Instant) -> Result<String, WorkerError> {
    let mut stream = transport::connect(pipe, deadline).map_err(WorkerError::Connect)?;
    stream
        .write_all(frame.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(WorkerError::Io)?;
    let mut reader = BufReader::new(stream.take(MAX_REPLY));
    let line = read_reply_line(&mut reader);
    // Close our handle right away: the app's side waits for this EOF (drain_until_closed).
    drop(reader);
    line
}

/// Reads one line from `reader`, which is limited to `MAX_REPLY` bytes (`Read::take`). A line
/// that fills the whole limit without ending in `\n` is cut off: [`WorkerError::TooLarge`]
/// (checked on the bytes, since the cut may fall inside a multi-byte character).
fn read_reply_line(reader: &mut impl BufRead) -> Result<String, WorkerError> {
    let mut buf = Vec::new();
    reader
        .read_until(b'\n', &mut buf)
        .map_err(WorkerError::Io)?;
    if buf.last() != Some(&b'\n') && buf.len() as u64 >= MAX_REPLY {
        return Err(WorkerError::TooLarge);
    }
    String::from_utf8(buf)
        .map_err(|e| WorkerError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

impl PipeBackend {
    pub fn new(
        pipe: Option<String>,
        agent_id: Option<String>,
        timeout: Duration,
        debug: bool,
    ) -> Self {
        PipeBackend {
            pipe,
            agent_id,
            timeout,
            debug,
            seq: AtomicU64::new(0),
        }
    }

    /// `MIRA_BOTS_PIPE`, `MIRA_AGENT_ID` (empty or `${…}` = missing), `MIRA_MCP_TIMEOUT_MS`.
    pub fn from_env(debug: bool) -> Self {
        PipeBackend::new(
            crate::env_value(crate::PIPE_ENV),
            crate::env_value(crate::AGENT_ID_ENV),
            crate::timeout_from_env(),
            debug,
        )
    }

    fn next_request_id(&self) -> String {
        let n = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        format!("{}-{n}", std::process::id())
    }
}

/// Reads a `tool_result` line for `request_id` (plan4 C4.2).
pub fn parse_reply(line: &str, request_id: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(line.trim_end()).map_err(|_| ERR_BAD_REPLY.to_string())?;
    let well_formed = v.get("v").and_then(Value::as_u64) == Some(1)
        && v.get("kind").and_then(Value::as_str) == Some("tool_result")
        && v.get("request_id").and_then(Value::as_str) == Some(request_id);
    if !well_formed {
        return Err(ERR_BAD_REPLY.into());
    }
    match v.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
        Some(false) => Err(v
            .get("error")
            .and_then(Value::as_str)
            .filter(|e| !e.is_empty())
            .unwrap_or(ERR_BAD_REPLY)
            .to_string()),
        None => Err(ERR_BAD_REPLY.into()),
    }
}

impl ToolBackend for PipeBackend {
    fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
        let Some(pipe) = self.pipe.clone() else {
            log(self.debug, "MIRA_BOTS_PIPE not set");
            return Err(ERR_NO_PIPE.into());
        };
        let Some(agent_id) = self.agent_id.as_deref() else {
            log(self.debug, "MIRA_AGENT_ID not set");
            return Err(ERR_NO_AGENT.into());
        };
        let request_id = self.next_request_id();
        let mut frame = json!({
            "v": 1,
            "kind": "tool",
            "agent_id": agent_id,
            "request_id": request_id,
            "tool": tool,
            "args": args,
        })
        .to_string();
        frame.push('\n');
        log(self.debug, &format!("{tool} {request_id} -> {pipe}"));

        let deadline = Instant::now() + self.timeout;
        let (tx, rx) = mpsc::channel();
        let debug = self.debug;
        let spawned = std::thread::Builder::new()
            .name("mira-mcp-call".into())
            .spawn(move || {
                // The receiver may be gone after a timeout; nothing to do then.
                let _ = tx.send(exchange(&pipe, &frame, deadline));
            });
        if spawned.is_err() {
            log(debug, "could not start worker thread");
            return Err(ERR_NOT_RUNNING.into());
        }
        // On timeout the worker is left behind (blocked on the pipe); the next call uses a new
        // one, and the process is torn down with the session.
        let line = match rx.recv_timeout(self.timeout) {
            Ok(Ok(line)) => line,
            Ok(Err(WorkerError::Connect(e))) => {
                log(debug, &format!("connect failed: {e}"));
                return Err(ERR_NOT_RUNNING.into());
            }
            Ok(Err(WorkerError::Io(e))) => {
                log(debug, &format!("pipe I/O failed: {e}"));
                return Err(ERR_BAD_REPLY.into());
            }
            Ok(Err(WorkerError::TooLarge)) => {
                log(debug, "reply longer than MAX_REPLY");
                return Err(ERR_TOO_LARGE.into());
            }
            Err(e) => {
                log(debug, &format!("no reply: {e}"));
                return Err(timeout_text(self.timeout));
            }
        };
        let r = parse_reply(&line, &request_id);
        log(
            debug,
            &format!(
                "{tool} {request_id} <- {}",
                if r.is_ok() { "ok" } else { "error" }
            ),
        );
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reply_follows_c4_2() {
        assert_eq!(
            parse_reply(
                "{\"v\":1,\"kind\":\"tool_result\",\"request_id\":\"1-1\",\"ok\":true,\"result\":{\"a\":1}}\n",
                "1-1"
            ),
            Ok(json!({"a":1}))
        );
        assert_eq!(
            parse_reply(
                r#"{"v":1,"kind":"tool_result","request_id":"1-1","ok":false,"error":"Ukendt agent"}"#,
                "1-1"
            ),
            Err("Ukendt agent".into())
        );
        for bad in [
            "",
            "garbage",
            r#"{"v":1,"kind":"tool_result","request_id":"other","ok":true,"result":{}}"#,
            r#"{"v":2,"kind":"tool_result","request_id":"1-1","ok":true,"result":{}}"#,
            r#"{"v":1,"kind":"decision","request_id":"1-1","ok":true}"#,
            r#"{"v":1,"kind":"tool_result","request_id":"1-1"}"#,
        ] {
            assert_eq!(parse_reply(bad, "1-1"), Err(ERR_BAD_REPLY.into()), "{bad}");
        }
    }

    #[test]
    fn reply_line_limit() {
        use std::io::Cursor;
        // Over-long line without a newline (cut inside a two-byte character): too large.
        let mut big = vec![b'a'];
        big.extend("æ".repeat(MAX_REPLY as usize).into_bytes());
        let mut r = BufReader::new(Cursor::new(big).take(MAX_REPLY));
        assert!(matches!(
            read_reply_line(&mut r),
            Err(WorkerError::TooLarge)
        ));
        // A line that fits (also when it ends exactly at the limit) is returned whole.
        let mut fits = vec![b'a'; MAX_REPLY as usize - 1];
        fits.push(b'\n');
        let mut r = BufReader::new(Cursor::new(fits).take(MAX_REPLY));
        assert_eq!(
            read_reply_line(&mut r).ok().map(|l| l.len() as u64),
            Some(MAX_REPLY)
        );
        // Short line, EOF without newline, and invalid UTF-8.
        let mut r = BufReader::new(Cursor::new(b"hej".to_vec()).take(MAX_REPLY));
        assert_eq!(read_reply_line(&mut r).ok().as_deref(), Some("hej"));
        let mut r = BufReader::new(Cursor::new(vec![0xff, b'\n']).take(MAX_REPLY));
        assert!(matches!(read_reply_line(&mut r), Err(WorkerError::Io(_))));
    }

    #[test]
    fn too_large_text_is_clear_danish() {
        assert_eq!(ERR_TOO_LARGE, "Svar fra mira-bots for stort");
        assert_ne!(ERR_TOO_LARGE, ERR_BAD_REPLY);
    }

    #[test]
    fn missing_env_is_an_error_without_io() {
        let t = Instant::now();
        let b = PipeBackend::new(None, Some("a".into()), Duration::from_secs(10), false);
        assert_eq!(
            b.call("mira_list_tickets", json!({})),
            Err(ERR_NO_PIPE.into())
        );
        let b = PipeBackend::new(Some("x".into()), None, Duration::from_secs(10), false);
        assert_eq!(
            b.call("mira_list_tickets", json!({})),
            Err(ERR_NO_AGENT.into())
        );
        assert!(t.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn unreachable_pipe_fails_fast() {
        let nope = std::env::temp_dir()
            .join("mira-mcp-does-not-exist.sock")
            .to_string_lossy()
            .into_owned();
        let b = PipeBackend::new(Some(nope), Some("a".into()), Duration::from_secs(10), false);
        let t = Instant::now();
        assert_eq!(
            b.call("mira_list_tickets", json!({})),
            Err(ERR_NOT_RUNNING.into())
        );
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn timeout_texts() {
        assert_eq!(
            timeout_text(Duration::from_millis(crate::MCP_CALL_TIMEOUT_MS)),
            "mira-bots svarede ikke inden for 10 s"
        );
        assert_eq!(
            timeout_text(Duration::from_millis(300)),
            "mira-bots svarede ikke inden for 300 ms"
        );
    }

    #[test]
    fn request_ids_are_pid_and_sequence() {
        let b = PipeBackend::new(None, None, Duration::from_secs(1), false);
        let pid = std::process::id();
        assert_eq!(b.next_request_id(), format!("{pid}-1"));
        assert_eq!(b.next_request_id(), format!("{pid}-2"));
    }

    #[cfg(unix)]
    mod unix_socket {
        use super::*;
        use std::net::Shutdown;
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::path::PathBuf;
        use std::thread::JoinHandle;

        fn sock_path(tag: &str) -> PathBuf {
            std::env::temp_dir().join(format!("mira-mcp-test-{}-{tag}.sock", std::process::id()))
        }

        /// What the fake app does with the one connection.
        enum Reply {
            /// Echo a tool_result for the frame's request_id: `Ok(result)` or `Err(error)`.
            Echo(Result<Value, String>),
            /// Answer with this exact line.
            Raw(String),
            /// Read the frame, never answer (keeps the connection open until the test ends).
            Silent(Duration),
        }

        /// How long the fake app waits for the client to close after the reply.
        const CLOSE_WAIT: Duration = Duration::from_secs(2);

        /// Accepts one connection; returns the received frame and whether the client closed its
        /// end after the reply (EOF seen within 2 s).
        fn serve_once(path: &PathBuf, reply: Reply) -> JoinHandle<(Value, bool)> {
            let _ = std::fs::remove_file(path);
            let listener = UnixListener::bind(path).unwrap();
            std::thread::spawn(move || {
                let (s, _) = listener.accept().unwrap();
                handle(s, reply, CLOSE_WAIT)
            })
        }

        /// The fake app's side of one connection. The read timeout is armed right after accept,
        /// before anything is written: XNU refuses every setsockopt with EINVAL once the peer has
        /// closed and our last write failed (both directions shut down), which made
        /// `oversized_reply_is_reported_as_too_large` flaky on macOS. A failing socket option or
        /// a peer that is already gone counts as "client closed" (`(Value::Null, true)`), never a
        /// panic.
        fn handle(s: UnixStream, reply: Reply, read_timeout: Duration) -> (Value, bool) {
            if s.set_read_timeout(Some(read_timeout)).is_err() {
                return (Value::Null, true);
            }
            let Ok(r) = s.try_clone() else {
                return (Value::Null, true);
            };
            let mut reader = BufReader::new(r);
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(n) if n > 0 => {}
                _ => return (Value::Null, true),
            }
            let frame: Value = serde_json::from_str(&line).unwrap();
            let mut w = s.try_clone().unwrap();
            match reply {
                Reply::Echo(outcome) => {
                    let rid = frame["request_id"].clone();
                    let v = match outcome {
                        Ok(r) => {
                            json!({"v":1,"kind":"tool_result","request_id":rid,"ok":true,"result":r})
                        }
                        Err(e) => {
                            json!({"v":1,"kind":"tool_result","request_id":rid,"ok":false,"error":e})
                        }
                    };
                    writeln!(w, "{v}").unwrap();
                }
                // Empty: hang up without answering.
                Reply::Raw(l) if l.is_empty() => return (frame, false),
                // The client may hang up before the end (e.g. an over-long reply).
                Reply::Raw(l) => {
                    let _ = w.write_all(l.as_bytes());
                }
                Reply::Silent(hold) => {
                    std::thread::sleep(hold);
                    return (frame, false);
                }
            }
            let mut rest = Vec::new();
            let closed = reader.read_to_end(&mut rest).is_ok();
            (frame, closed)
        }

        fn backend(path: &std::path::Path, timeout: Duration) -> PipeBackend {
            PipeBackend::new(
                Some(path.to_string_lossy().into_owned()),
                Some("agent-7".into()),
                timeout,
                false,
            )
        }

        #[test]
        fn ok_round_trip_sends_c4_2_frame_and_closes() {
            let path = sock_path("ok");
            let server = serve_once(&path, Reply::Echo(Ok(json!({"ok":true,"ticketId":null}))));
            let b = backend(&path, Duration::from_secs(5));
            let r = b.call("mira_update_status", json!({"note":"Kører tests"}));
            assert_eq!(r, Ok(json!({"ok":true,"ticketId":null})));
            let (frame, closed) = server.join().unwrap();
            assert_eq!(frame["v"], 1);
            assert_eq!(frame["kind"], "tool");
            assert_eq!(frame["agent_id"], "agent-7");
            assert_eq!(frame["tool"], "mira_update_status");
            assert_eq!(frame["args"], json!({"note":"Kører tests"}));
            assert_eq!(frame["request_id"], format!("{}-1", std::process::id()));
            assert!(
                closed,
                "the client must close its handle after the reply line"
            );
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn error_reply_becomes_err() {
            let path = sock_path("err");
            let server = serve_once(&path, Reply::Echo(Err("Du har ingen ticket i gang".into())));
            let r = backend(&path, Duration::from_secs(5))
                .call("mira_submit_for_review", json!({"summary":"x"}));
            assert_eq!(r, Err("Du har ingen ticket i gang".into()));
            server.join().unwrap();
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn wrong_request_id_is_rejected() {
            let path = sock_path("rid");
            let server = serve_once(
                &path,
                Reply::Raw("{\"v\":1,\"kind\":\"tool_result\",\"request_id\":\"nope\",\"ok\":true,\"result\":{}}\n".into()),
            );
            let r = backend(&path, Duration::from_secs(5)).call("mira_list_tickets", json!({}));
            assert_eq!(r, Err(ERR_BAD_REPLY.into()));
            server.join().unwrap();
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn oversized_reply_is_reported_as_too_large() {
            let path = sock_path("big");
            let server = serve_once(
                &path,
                Reply::Raw(format!("{}{}", "x".repeat(MAX_REPLY as usize + 10), "\n")),
            );
            let r = backend(&path, Duration::from_secs(5)).call("mira_list_tickets", json!({}));
            assert_eq!(r, Err(ERR_TOO_LARGE.into()));
            server.join().unwrap();
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn silent_app_times_out() {
            let path = sock_path("silent");
            let server = serve_once(&path, Reply::Silent(Duration::from_millis(1500)));
            let t = Instant::now();
            let r = backend(&path, Duration::from_millis(300)).call("mira_list_tickets", json!({}));
            let took = t.elapsed();
            assert_eq!(r, Err("mira-bots svarede ikke inden for 300 ms".into()));
            assert!(took >= Duration::from_millis(300), "{took:?}");
            assert!(took < Duration::from_millis(1500), "{took:?}");
            server.join().unwrap();
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn app_closing_without_reply_is_a_bad_reply() {
            let path = sock_path("close");
            let server = serve_once(&path, Reply::Raw(String::new()));
            let r = backend(&path, Duration::from_secs(5)).call("mira_list_tickets", json!({}));
            assert_eq!(r, Err(ERR_BAD_REPLY.into()));
            server.join().unwrap();
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn client_gone_before_the_timeout_is_armed_is_not_a_panic() {
            // C2: the client connects and shuts both directions down before the fake app arms
            // its read timeout (on macOS that setsockopt fails with EINVAL; Linux allows it).
            let path = sock_path("gone");
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path).unwrap();
            let client = UnixStream::connect(&path).unwrap();
            client.shutdown(Shutdown::Both).unwrap();
            let (s, _) = listener.accept().unwrap();
            let _ = s.shutdown(Shutdown::Write);
            let (frame, closed) = handle(s, Reply::Raw("late\n".into()), CLOSE_WAIT);
            assert_eq!(frame, Value::Null);
            assert!(closed);
            drop(client);
            let _ = std::fs::remove_file(&path);
        }

        #[test]
        fn failing_socket_option_counts_as_closed() {
            // std rejects a zero timeout with InvalidInput, the same kind as XNU's EINVAL: the
            // error path must end the connection quietly instead of panicking.
            let (s, client) = UnixStream::pair().unwrap();
            let (frame, closed) = handle(s, Reply::Raw("x\n".into()), Duration::ZERO);
            assert_eq!(frame, Value::Null);
            assert!(closed);
            drop(client);
        }
    }
}
