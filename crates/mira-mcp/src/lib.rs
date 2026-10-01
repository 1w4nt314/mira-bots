//! mira-mcp: a minimal MCP server (JSON-RPC 2.0 over stdio, one JSON message per line) that
//! gives a Claude Code agent five tools for the mira-bots tickets. Each `tools/call` is relayed
//! to the app over the same pipe the hooks use (one connection per call) and answered with the
//! app's result.
//!
//! Hard rules: stdout carries nothing but JSON-RPC lines; diagnostics go to stderr and only
//! with `MIRA_MCP_DEBUG=1`; a business error is a `tools/call` result with `isError: true`,
//! never a crash; stdin EOF ends the process with exit code 0.

pub mod bridge;
pub mod rpc;
pub mod tools;
pub mod transport;

use std::io::{BufRead, Read, Write};
use std::time::Duration;

pub use bridge::{PipeBackend, ToolBackend};
pub use transport::PIPE_ENV;

/// Env var carrying the app's agent id (set by the app on the claude process; Claude Code passes
/// it on to this server, by inheritance and/or `${MIRA_AGENT_ID}` in mcp.json). Same value as
/// `mira_hook::AGENT_ID_ENV`.
pub const AGENT_ID_ENV: &str = "MIRA_AGENT_ID";
/// Env var enabling diagnostics on stderr (`1`).
pub const DEBUG_ENV: &str = "MIRA_MCP_DEBUG";
/// Env override of [`MCP_CALL_TIMEOUT_MS`] (tests/debugging only).
pub const TIMEOUT_ENV: &str = "MIRA_MCP_TIMEOUT_MS";
/// The server name in mcp.json; Claude Code names the tools `mcp__mira-bots__<tool>`.
pub const SERVER_NAME: &str = "mira-bots";
/// How long one `tools/call` may wait for the app (connect + reply).
pub const MCP_CALL_TIMEOUT_MS: u64 = 10_000;
/// Maximum stdin line (bytes, newline excluded). Longer lines are answered with `-32600` when an
/// id can be found, otherwise ignored.
pub const MAX_LINE: usize = 1 << 20;
/// Maximum reply line read from the app (bytes). A longer reply is reported as
/// "Svar fra mira-bots for stort" (`list_tickets` truncates summaries, so this is a safety net).
pub const MAX_REPLY: u64 = 1 << 20;

/// Whether an env value counts as missing: empty, or an unexpanded `${VAR}` placeholder (Claude
/// Code leaves `${VAR}` in mcp.json's `env` as literal text when VAR is not set; research4 Q2).
pub fn is_missing(value: &str) -> bool {
    value.trim().is_empty() || value.starts_with("${")
}

/// Reads env var `name`; empty or `${…}` counts as unset (see [`is_missing`]).
pub fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !is_missing(v))
}

pub fn debug_from_env() -> bool {
    std::env::var(DEBUG_ENV).is_ok_and(|v| v == "1")
}

/// [`MCP_CALL_TIMEOUT_MS`], or a positive [`TIMEOUT_ENV`] value.
pub fn timeout_from_env() -> Duration {
    let ms = env_value(TIMEOUT_ENV)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .unwrap_or(MCP_CALL_TIMEOUT_MS);
    Duration::from_millis(ms)
}

/// One diagnostics line on stderr, only when `debug`. Never logs tool arguments.
pub fn log(debug: bool, msg: &str) {
    if debug {
        let _ = writeln!(std::io::stderr().lock(), "mira-mcp: {msg}");
    }
}

/// One stdin line: its text, or the fact that it was longer than [`MAX_LINE`] (with up to
/// [`ID_PROBE`] bytes from its start and end, to look for an id).
enum Line {
    Text(String),
    TooLong { head: Vec<u8>, tail: Vec<u8> },
}

/// Bytes kept from the start and the end of an over-long line.
const ID_PROBE: usize = 256;

/// Reads one line (without its `\n`). `None` at EOF or on a read error.
fn read_line<R: BufRead>(reader: &mut R) -> Option<Line> {
    let mut buf = Vec::new();
    let n = reader
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)
        .ok()?;
    if n == 0 {
        return None;
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        return Some(Line::Text(String::from_utf8_lossy(&buf).into_owned()));
    }
    if buf.len() <= MAX_LINE {
        // Last line without a newline before EOF.
        return Some(Line::Text(String::from_utf8_lossy(&buf).into_owned()));
    }
    // Over-long: keep the head, skip to the end of the line keeping its tail.
    let head = buf[..ID_PROBE].to_vec();
    let mut tail = buf[buf.len() - ID_PROBE..].to_vec();
    loop {
        let (consumed, done) = match reader.fill_buf() {
            Ok([]) | Err(_) => break,
            Ok(chunk) => match chunk.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    tail.extend_from_slice(&chunk[..i]);
                    (i + 1, true)
                }
                None => {
                    tail.extend_from_slice(chunk);
                    (chunk.len(), false)
                }
            },
        };
        reader.consume(consumed);
        if tail.len() > ID_PROBE {
            tail.drain(..tail.len() - ID_PROBE);
        }
        if done {
            break;
        }
    }
    Some(Line::TooLong { head, tail })
}

/// Best effort: the top-level `"id"` of an over-long message, from its first or last bytes
/// (clients put `id` first or last). Only a string or number counts.
fn probe_id(head: &[u8], tail: &[u8]) -> Option<serde_json::Value> {
    let parse_after = |text: &str, at: usize| {
        let rest = text[at + 4..].trim_start().strip_prefix(':')?;
        let v = serde_json::Deserializer::from_str(rest.trim_start())
            .into_iter::<serde_json::Value>()
            .next()?
            .ok()?;
        (v.is_string() || v.is_number()).then_some(v)
    };
    let head = String::from_utf8_lossy(head);
    let tail = String::from_utf8_lossy(tail);
    head.find("\"id\"")
        .and_then(|i| parse_after(&head, i))
        .or_else(|| tail.rfind("\"id\"").and_then(|i| parse_after(&tail, i)))
}

/// The server loop: one stdin line → [`rpc::handle_line`] → at most one stdout line (flushed).
/// Returns at stdin EOF (or a read error) and when stdout is gone (Claude Code closed it).
/// Serial: a `tools/call` blocks the loop for at most the backend's timeout.
pub fn run_loop<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    backend: &dyn ToolBackend,
    debug: bool,
) {
    while let Some(line) = read_line(&mut reader) {
        let reply = match line {
            Line::Text(text) => {
                if text.trim().is_empty() {
                    continue;
                }
                rpc::handle_line(&text, backend)
            }
            Line::TooLong { head, tail } => {
                log(debug, "stdin line longer than MAX_LINE");
                probe_id(&head, &tail).map(|id| {
                    rpc::error_response(id, rpc::INVALID_REQUEST, "Message too large").to_string()
                })
            }
        };
        let Some(reply) = reply else {
            continue;
        };
        let written = writer
            .write_all(reply.as_bytes())
            .and_then(|()| writer.write_all(b"\n"))
            .and_then(|()| writer.flush());
        if let Err(e) = written {
            log(debug, &format!("stdout closed: {e}"));
            return;
        }
    }
    log(debug, "stdin closed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    struct Echo;
    impl ToolBackend for Echo {
        fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
            Ok(json!({"tool": tool, "args": args}))
        }
    }

    fn run(input: &[u8]) -> Vec<Value> {
        let mut out = Vec::new();
        run_loop(input, &mut out, &Echo, false);
        let text = String::from_utf8(out).unwrap();
        assert!(text.is_empty() || text.ends_with('\n'));
        text.lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn placeholders_and_empty_values_count_as_missing() {
        assert!(is_missing("${MIRA_AGENT_ID}"));
        assert!(is_missing("${MIRA_BOTS_PIPE:-x}"));
        assert!(is_missing(""));
        assert!(is_missing("  "));
        assert!(!is_missing("agent-1"));
        assert!(!is_missing(r"\\.\pipe\mira-bots-42"));
    }

    #[test]
    fn env_value_treats_placeholder_as_unset() {
        // A name no one else uses; set and read within this test only.
        let name = "MIRA_MCP_TEST_ENV_VALUE";
        std::env::set_var(name, "${MIRA_AGENT_ID}");
        assert_eq!(env_value(name), None);
        std::env::set_var(name, "a1");
        assert_eq!(env_value(name).as_deref(), Some("a1"));
        std::env::remove_var(name);
        assert_eq!(env_value(name), None);
    }

    #[test]
    fn constants_match_the_plan() {
        assert_eq!(PIPE_ENV, "MIRA_BOTS_PIPE");
        assert_eq!(AGENT_ID_ENV, "MIRA_AGENT_ID");
        assert_eq!(DEBUG_ENV, "MIRA_MCP_DEBUG");
        assert_eq!(SERVER_NAME, "mira-bots");
        assert_eq!(MCP_CALL_TIMEOUT_MS, 10_000);
        assert_eq!(MAX_LINE, 1_048_576);
        assert_eq!(MAX_REPLY, 1_048_576);
    }

    #[test]
    fn loop_answers_requests_only_and_skips_blank_lines() {
        let input = b"\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n   \n\
            {\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\
            {\"jsonrpc\":\"2.0\",\"id\":\"x\",\"method\":\"ping\"}";
        let out = run(input);
        assert_eq!(
            out,
            vec![
                json!({"jsonrpc":"2.0","id":1,"result":{}}),
                json!({"jsonrpc":"2.0","id":"x","result":{}}),
            ]
        );
    }

    #[test]
    fn crlf_lines_are_accepted() {
        let out = run(b"{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\r\n");
        assert_eq!(out, vec![json!({"jsonrpc":"2.0","id":7,"result":{}})]);
    }

    #[test]
    fn over_long_line_gets_32600_when_id_is_found_and_loop_continues() {
        let big = "x".repeat(MAX_LINE + 10);
        // id last (as Claude Code sends it).
        let mut input = format!(
            "{{\"method\":\"tools/call\",\"params\":{{\"name\":\"mira_create_ticket\",\"arguments\":{{\"body\":\"{big}\"}}}},\"jsonrpc\":\"2.0\",\"id\":5}}\n"
        );
        // id first, nothing findable afterwards.
        input.push_str(&format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":\"a\",\"method\":\"x\",\"params\":\"{big}\"}}\n"
        ));
        // no id at all → ignored.
        input.push_str(&format!("{{\"params\":\"{big}\"}}\n"));
        input.push_str("{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\n");
        let out = run(input.as_bytes());
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[0]["id"], 5);
        assert_eq!(out[0]["error"]["code"], -32600);
        assert_eq!(out[1]["id"], "a");
        assert_eq!(out[1]["error"]["code"], -32600);
        assert_eq!(out[2], json!({"jsonrpc":"2.0","id":9,"result":{}}));
    }

    #[test]
    fn loop_stops_when_stdout_is_gone() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        // Would loop over both lines if the write error were ignored; must return.
        run_loop(
            &b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n"[..],
            Closed,
            &Echo,
            false,
        );
    }
}
