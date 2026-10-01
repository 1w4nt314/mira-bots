//! Pipe wire format (C.4, plan4 C4.2): newline-delimited JSON, one line each way. Two frame
//! kinds arrive: `hook` (mira-hook) and `tool` (mira-mcp).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::permissions::Decision;

pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum `request_id` length (chars) of a tool frame.
pub const MAX_REQUEST_ID: usize = 64;

/// Any frame. Hook → app: `{"v":1,"kind":"hook","agent_id":"<id>","event":{...}}` (`agent_id`
/// optional, C2.4; hook exes from step 1 never send it). mira-mcp → app:
/// `{"v":1,"kind":"tool","agent_id","request_id","tool","args"}` (C4.2).
#[derive(Deserialize, Debug)]
struct Frame {
    v: u32,
    kind: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    event: Option<Value>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    args: Option<Value>,
}

/// A parsed hook frame: the app's agent id from `MIRA_AGENT_ID` (if the hook sent one) and the
/// hook event JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct HookFrame {
    pub agent_id: Option<String>,
    pub event: Value,
}

/// A parsed tool frame from mira-mcp. `args` is always an object (`{}` when absent).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolFrame {
    pub agent_id: Option<String>,
    pub request_id: String,
    pub tool: String,
    pub args: Value,
}

/// One received frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    Hook(HookFrame),
    Tool(ToolFrame),
}

/// The app's answer to a tool frame: `Ok(result)` or a Danish error for the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub request_id: String,
    pub outcome: Result<Value, String>,
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

/// Parses one received line: version first, then by `kind` (`hook` | `tool`; anything else is
/// [`FrameError::Kind`]). An empty `agent_id` counts as absent.
pub fn parse_frame(line: &str) -> Result<Incoming, FrameError> {
    let f: Frame =
        serde_json::from_str(line.trim_end()).map_err(|e| FrameError::Json(e.to_string()))?;
    if f.v != PROTOCOL_VERSION {
        return Err(FrameError::Version(f.v));
    }
    let agent_id = f.agent_id.filter(|s| !s.is_empty());
    match f.kind.as_str() {
        "hook" => Ok(Incoming::Hook(HookFrame {
            agent_id,
            event: f
                .event
                .ok_or_else(|| FrameError::Json("hook frame without event".into()))?,
        })),
        "tool" => {
            let request_id = f
                .request_id
                .filter(|r| !r.is_empty())
                .ok_or_else(|| FrameError::Json("tool frame without request_id".into()))?;
            if request_id.chars().count() > MAX_REQUEST_ID {
                return Err(FrameError::Json("tool frame request_id too long".into()));
            }
            let tool = f
                .tool
                .ok_or_else(|| FrameError::Json("tool frame without tool".into()))?;
            let args = match f.args {
                None => Value::Object(Default::default()),
                Some(a @ Value::Object(_)) => a,
                Some(_) => return Err(FrameError::Json("tool frame args is not an object".into())),
            };
            Ok(Incoming::Tool(ToolFrame {
                agent_id,
                request_id,
                tool,
                args,
            }))
        }
        _ => Err(FrameError::Kind(f.kind)),
    }
}

/// Renders the newline-terminated `tool_result` line (C4.2).
pub fn render_tool_result(r: &ToolResult) -> String {
    let v = match &r.outcome {
        Ok(result) => serde_json::json!({
            "v": PROTOCOL_VERSION,
            "kind": "tool_result",
            "request_id": r.request_id,
            "ok": true,
            "result": result,
        }),
        Err(error) => serde_json::json!({
            "v": PROTOCOL_VERSION,
            "kind": "tool_result",
            "request_id": r.request_id,
            "ok": false,
            "error": error,
        }),
    };
    let mut s = v.to_string();
    s.push('\n');
    s
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

    /// Parses a frame that must be a hook frame.
    fn parse_hook(line: &str) -> Result<HookFrame, FrameError> {
        match parse_frame(line)? {
            Incoming::Hook(h) => Ok(h),
            Incoming::Tool(_) => Err(FrameError::Kind("tool".into())),
        }
    }

    #[test]
    fn parses_hook_frame() {
        let f =
            parse_hook("{\"v\":1,\"kind\":\"hook\",\"event\":{\"hook_event_name\":\"Stop\"}}\n")
                .unwrap();
        assert_eq!(f.event, json!({"hook_event_name":"Stop"}));
        assert_eq!(f.agent_id, None);
    }

    #[test]
    fn parses_frame_level_agent_id() {
        let f = parse_hook(
            r#"{"v":1,"kind":"hook","agent_id":"a1","event":{"hook_event_name":"SubagentStop","agent_id":"sub"}}"#,
        )
        .unwrap();
        assert_eq!(f.agent_id.as_deref(), Some("a1"));
        assert_eq!(f.event["agent_id"], "sub");
        let f = parse_hook(r#"{"v":1,"kind":"hook","agent_id":"","event":{}}"#).unwrap();
        assert_eq!(f.agent_id, None, "empty agent_id counts as absent");
    }

    #[test]
    fn rejects_bad_frames() {
        assert!(matches!(parse_hook("nope"), Err(FrameError::Json(_))));
        assert!(matches!(
            parse_hook(r#"{"v":1,"kind":"hook"}"#),
            Err(FrameError::Json(_))
        ));
        assert_eq!(
            parse_hook(r#"{"v":2,"kind":"hook","event":{}}"#),
            Err(FrameError::Version(2))
        );
        assert_eq!(
            parse_hook(r#"{"v":1,"kind":"decision","event":{}}"#),
            Err(FrameError::Kind("decision".into()))
        );
    }

    #[test]
    fn parses_tool_frame() {
        let f = parse_frame(
            r#"{"v":1,"kind":"tool","agent_id":"a1","request_id":"42-1","tool":"mira_list_tickets","args":{"filter":"mine"}}"#,
        )
        .unwrap();
        assert_eq!(
            f,
            Incoming::Tool(ToolFrame {
                agent_id: Some("a1".into()),
                request_id: "42-1".into(),
                tool: "mira_list_tickets".into(),
                args: json!({"filter":"mine"}),
            })
        );
        // args default {}, empty agent_id = absent.
        let Incoming::Tool(f) = parse_frame(
            "{\"v\":1,\"kind\":\"tool\",\"agent_id\":\"\",\"request_id\":\"r\",\"tool\":\"x\"}\n",
        )
        .unwrap() else {
            panic!("not a tool frame")
        };
        assert_eq!(f.args, json!({}));
        assert_eq!(f.agent_id, None);
        // A hook frame still parses as one.
        assert!(matches!(
            parse_frame(r#"{"v":1,"kind":"hook","event":{}}"#),
            Ok(Incoming::Hook(_))
        ));
    }

    #[test]
    fn rejects_bad_tool_frames() {
        assert_eq!(
            parse_frame(r#"{"v":1,"kind":"tool","tool":"x","args":{}}"#),
            Err(FrameError::Json("tool frame without request_id".into()))
        );
        assert_eq!(
            parse_frame(r#"{"v":1,"kind":"tool","request_id":"","tool":"x"}"#),
            Err(FrameError::Json("tool frame without request_id".into()))
        );
        let long = format!(
            r#"{{"v":1,"kind":"tool","request_id":"{}","tool":"x"}}"#,
            "r".repeat(65)
        );
        assert!(matches!(parse_frame(&long), Err(FrameError::Json(_))));
        let max = format!(
            r#"{{"v":1,"kind":"tool","request_id":"{}","tool":"x"}}"#,
            "r".repeat(64)
        );
        assert!(parse_frame(&max).is_ok());
        assert!(matches!(
            parse_frame(r#"{"v":1,"kind":"tool","request_id":"r"}"#),
            Err(FrameError::Json(_))
        ));
        assert!(matches!(
            parse_frame(r#"{"v":1,"kind":"tool","request_id":"r","tool":"x","args":[1]}"#),
            Err(FrameError::Json(_))
        ));
        assert!(matches!(
            parse_frame(r#"{"v":1,"kind":"tool","request_id":7,"tool":"x"}"#),
            Err(FrameError::Json(_))
        ));
        assert_eq!(
            parse_frame(r#"{"v":2,"kind":"tool","request_id":"r","tool":"x"}"#),
            Err(FrameError::Version(2))
        );
    }

    #[test]
    fn unknown_kind_is_a_kind_error() {
        assert_eq!(
            parse_frame(r#"{"v":1,"kind":"tool_result","request_id":"r","ok":true}"#),
            Err(FrameError::Kind("tool_result".into()))
        );
        assert_eq!(
            parse_frame(r#"{"v":1,"kind":"future"}"#),
            Err(FrameError::Kind("future".into()))
        );
    }

    #[test]
    fn renders_tool_result_lines() {
        let ok = render_tool_result(&ToolResult {
            request_id: "42-1".into(),
            outcome: Ok(json!({"ok":true,"ticketId":null,"text":"a\nb"})),
        });
        assert!(ok.ends_with('\n'));
        assert_eq!(ok.matches('\n').count(), 1, "one line: {ok:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&ok).unwrap(),
            json!({"v":1,"kind":"tool_result","request_id":"42-1","ok":true,"result":{"ok":true,"ticketId":null,"text":"a\nb"}})
        );
        let err = render_tool_result(&ToolResult {
            request_id: "42-2".into(),
            outcome: Err("Ukendt agent".into()),
        });
        assert!(err.ends_with('\n'));
        assert_eq!(err.matches('\n').count(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(&err).unwrap(),
            json!({"v":1,"kind":"tool_result","request_id":"42-2","ok":false,"error":"Ukendt agent"})
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
        let p = mira_hook::payload::parse(r#"{"hook_event_name":"Stop","session_id":"s"}"#, None)
            .unwrap();
        let f = parse_hook(&mira_hook::payload::to_frame(&p, None)).unwrap();
        assert_eq!(f.event["session_id"], "s");
        assert_eq!(f.agent_id, None);
        let f = parse_hook(&mira_hook::payload::to_frame(&p, Some("a1"))).unwrap();
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
