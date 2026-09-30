//! Pipe wire format (C.4): newline-delimited JSON, one line each way.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::permissions::Decision;

pub const PROTOCOL_VERSION: u32 = 1;

/// Hook → app: `{"v":1,"kind":"hook","agent_id":"<id>","event":{...}}` (`agent_id` optional,
/// C2.4; hook exes from step 1 never send it).
#[derive(Deserialize, Debug)]
struct Frame {
    v: u32,
    kind: String,
    #[serde(default)]
    agent_id: Option<String>,
    event: Value,
}

/// A parsed hook frame: the app's agent id from `MIRA_AGENT_ID` (if the hook sent one) and the
/// hook event JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct HookFrame {
    pub agent_id: Option<String>,
    pub event: Value,
}

/// App → hook (PermissionRequest only): `{"v":1,"kind":"decision","decision":"allow|deny|none","message":...}`.
#[derive(Serialize, Debug)]
struct Reply<'a> {
    v: u32,
    kind: &'static str,
    decision: &'static str,
    message: Option<&'a str>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("invalid JSON frame: {0}")]
    Json(String),
    #[error("unsupported protocol version {0}")]
    Version(u32),
    #[error("unexpected frame kind {0:?}")]
    Kind(String),
}

/// Parses one received line. An empty `agent_id` counts as absent.
pub fn parse_frame(line: &str) -> Result<HookFrame, FrameError> {
    let f: Frame =
        serde_json::from_str(line.trim_end()).map_err(|e| FrameError::Json(e.to_string()))?;
    if f.v != PROTOCOL_VERSION {
        return Err(FrameError::Version(f.v));
    }
    if f.kind != "hook" {
        return Err(FrameError::Kind(f.kind));
    }
    Ok(HookFrame {
        agent_id: f.agent_id.filter(|s| !s.is_empty()),
        event: f.event,
    })
}

/// Renders the newline-terminated reply line.
pub fn render_reply(decision: Decision, message: Option<&str>) -> String {
    let mut s = serde_json::to_string(&Reply {
        v: PROTOCOL_VERSION,
        kind: "decision",
        decision: decision.as_str(),
        message,
    })
    .expect("Reply serialization cannot fail");
    s.push('\n');
    s
}

/// Windows `\\.\pipe\mira-bots-<pid>`.
#[cfg(windows)]
pub fn pipe_name(pid: u32) -> String {
    format!(r"\\.\pipe\mira-bots-{pid}")
}

/// Unix `<tmpdir>/mira-bots-<pid>.sock`.
#[cfg(not(windows))]
pub fn pipe_name(pid: u32) -> String {
    std::env::temp_dir()
        .join(format!("mira-bots-{pid}.sock"))
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_hook_frame() {
        let f =
            parse_frame("{\"v\":1,\"kind\":\"hook\",\"event\":{\"hook_event_name\":\"Stop\"}}\n")
                .unwrap();
        assert_eq!(f.event, json!({"hook_event_name":"Stop"}));
        assert_eq!(f.agent_id, None);
    }

    #[test]
    fn parses_frame_level_agent_id() {
        let f = parse_frame(
            r#"{"v":1,"kind":"hook","agent_id":"a1","event":{"hook_event_name":"SubagentStop","agent_id":"sub"}}"#,
        )
        .unwrap();
        assert_eq!(f.agent_id.as_deref(), Some("a1"));
        assert_eq!(f.event["agent_id"], "sub");
        let f = parse_frame(r#"{"v":1,"kind":"hook","agent_id":"","event":{}}"#).unwrap();
        assert_eq!(f.agent_id, None, "empty agent_id counts as absent");
    }

    #[test]
    fn rejects_bad_frames() {
        assert!(matches!(parse_frame("nope"), Err(FrameError::Json(_))));
        assert!(matches!(
            parse_frame(r#"{"v":1,"kind":"hook"}"#),
            Err(FrameError::Json(_))
        ));
        assert_eq!(
            parse_frame(r#"{"v":2,"kind":"hook","event":{}}"#),
            Err(FrameError::Version(2))
        );
        assert_eq!(
            parse_frame(r#"{"v":1,"kind":"decision","event":{}}"#),
            Err(FrameError::Kind("decision".into()))
        );
    }

    #[test]
    fn renders_reply_lines() {
        let s = render_reply(Decision::Allow, None);
        assert!(s.ends_with('\n'));
        assert_eq!(
            serde_json::from_str::<Value>(&s).unwrap(),
            json!({"v":1,"kind":"decision","decision":"allow","message":null})
        );
        let s = render_reply(Decision::Deny, Some("nej"));
        assert_eq!(
            serde_json::from_str::<Value>(&s).unwrap(),
            json!({"v":1,"kind":"decision","decision":"deny","message":"nej"})
        );
        let s = render_reply(Decision::None, None);
        assert_eq!(
            serde_json::from_str::<Value>(&s).unwrap()["decision"],
            "none"
        );
    }

    #[test]
    fn reply_is_understood_by_the_hook_exe() {
        use mira_hook::decision::{parse_reply, Decision as HookDecision};
        assert_eq!(
            parse_reply(&render_reply(Decision::Allow, None)),
            HookDecision::Allow
        );
        assert_eq!(
            parse_reply(&render_reply(Decision::Deny, None)),
            HookDecision::Deny {
                message: String::new()
            }
        );
        assert_eq!(
            parse_reply(&render_reply(Decision::None, None)),
            HookDecision::None
        );
    }

    #[test]
    fn hook_exe_frame_is_understood_by_the_app() {
        let p =
            mira_hook::payload::parse(r#"{"hook_event_name":"Stop","session_id":"s"}"#).unwrap();
        let f = parse_frame(&mira_hook::payload::to_frame(&p, None)).unwrap();
        assert_eq!(f.event["session_id"], "s");
        assert_eq!(f.agent_id, None);
        let f = parse_frame(&mira_hook::payload::to_frame(&p, Some("a1"))).unwrap();
        assert_eq!(f.event["session_id"], "s");
        assert_eq!(f.agent_id.as_deref(), Some("a1"));
    }

    #[test]
    fn pipe_name_contains_pid() {
        let n = pipe_name(4242);
        assert!(n.contains("mira-bots-4242"));
        #[cfg(windows)]
        assert!(n.starts_with(r"\\.\pipe\"));
        #[cfg(not(windows))]
        assert!(n.ends_with(".sock"));
    }
}
