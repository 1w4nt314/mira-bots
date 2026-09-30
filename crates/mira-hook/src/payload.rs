//! Hook stdin JSON: parsing, trimming, per-event time budget and the pipe frame.

use std::fmt;
use std::time::Duration;

use serde_json::{json, Value};

/// Maximum length (chars) of any string value forwarded to the app, including the `…` marker.
pub const MAX_STRING_CHARS: usize = 2000;

/// Top-level fields that are never forwarded (large and useless for status).
const DROPPED_FIELDS: [&str; 2] = ["tool_response", "transcript_path"];

/// A parsed hook event. `event_name` comes from `hook_event_name` (argv is ignored).
#[derive(Debug, Clone, PartialEq)]
pub struct HookPayload {
    pub event_name: String,
    pub session_id: String,
    pub json: Value,
}

#[derive(Debug)]
pub enum Error {
    Json(serde_json::Error),
    NotAnObject,
    MissingField(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Json(e) => write!(f, "invalid JSON: {e}"),
            Error::NotAnObject => write!(f, "hook input is not a JSON object"),
            Error::MissingField(k) => write!(f, "missing string field `{k}`"),
        }
    }
}

impl std::error::Error for Error {}

fn string_field(v: &Value, key: &'static str) -> Result<String, Error> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(Error::MissingField(key))
}

/// Parses the hook's stdin. Requires an object with string fields `hook_event_name` and `session_id`.
pub fn parse(stdin: &str) -> Result<HookPayload, Error> {
    let json: Value = serde_json::from_str(stdin).map_err(Error::Json)?;
    if !json.is_object() {
        return Err(Error::NotAnObject);
    }
    Ok(HookPayload {
        event_name: string_field(&json, "hook_event_name")?,
        session_id: string_field(&json, "session_id")?,
        json,
    })
}

fn truncate_in_place(s: &mut String) {
    if s.chars().count() > MAX_STRING_CHARS {
        let mut out: String = s.chars().take(MAX_STRING_CHARS - 1).collect();
        out.push('…');
        *s = out;
    }
}

fn truncate_strings(v: &mut Value) {
    match v {
        Value::String(s) => truncate_in_place(s),
        Value::Array(items) => items.iter_mut().for_each(truncate_strings),
        Value::Object(map) => map.values_mut().for_each(truncate_strings),
        _ => {}
    }
}

/// Drops top-level `tool_response` and `transcript_path` and truncates every string value
/// (recursively) to [`MAX_STRING_CHARS`] chars. Nothing else is changed.
pub fn trim(json: &mut Value) {
    if let Some(map) = json.as_object_mut() {
        for key in DROPPED_FIELDS {
            map.remove(key);
        }
    }
    truncate_strings(json);
}

/// Total time the hook exe may spend before it gives up and exits 0 with empty stdout.
/// PermissionRequest 110 s (app answers by 108 s, hooks.json kills at 120 s), SessionEnd 1 s,
/// everything else 2 s.
pub fn budget(event_name: &str) -> Duration {
    match event_name {
        "PermissionRequest" => Duration::from_secs(110),
        "SessionEnd" => Duration::from_secs(1),
        _ => Duration::from_secs(2),
    }
}

/// Whether the hook waits for a decision line from the app.
pub fn expects_reply(event_name: &str) -> bool {
    event_name == "PermissionRequest"
}

/// Frames larger than this (bytes, newline included) are re-rendered with `tool_input` replaced
/// by `{"_truncated": true}`. Leaves headroom below the app's 1 MiB line limit.
pub const MAX_FRAME_BYTES: usize = 900 * 1024;

fn render_frame(event: &Value) -> String {
    let mut s = json!({"v": 1, "kind": "hook", "event": event}).to_string();
    s.push('\n');
    s
}

/// Renders the newline-terminated pipe frame `{"v":1,"kind":"hook","event":{...}}`.
///
/// Strings are already capped by [`trim`], but a `tool_input` with very many elements can still
/// exceed the app's line limit; if the frame is over [`MAX_FRAME_BYTES`], `tool_input` is replaced
/// by `{"_truncated": true}` (all other fields, e.g. `tool_name`, are kept) so the status update
/// and permission card still arrive.
pub fn to_frame(payload: &HookPayload) -> String {
    let frame = render_frame(&payload.json);
    if frame.len() <= MAX_FRAME_BYTES {
        return frame;
    }
    let mut event = payload.json.clone();
    match event.get_mut("tool_input") {
        Some(input) => *input = json!({"_truncated": true}),
        None => return frame,
    }
    render_frame(&event)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMON: &str = r#""session_id":"sess-1","transcript_path":"C:\\Users\\u\\.claude\\projects\\x.jsonl","cwd":"C:\\work\\demo","permission_mode":"default""#;

    fn fixture(rest: &str) -> String {
        format!("{{{COMMON},{rest}}}")
    }

    fn fixtures() -> Vec<(&'static str, String)> {
        vec![
            (
                "SessionStart",
                fixture(r#""hook_event_name":"SessionStart","source":"startup","model":"x""#),
            ),
            (
                "UserPromptSubmit",
                fixture(r#""hook_event_name":"UserPromptSubmit","prompt":"fix the bug""#),
            ),
            (
                "PreToolUse",
                fixture(
                    r#""hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"file_path":"src/main.rs","old_string":"a","new_string":"b"},"tool_use_id":"toolu_1""#,
                ),
            ),
            (
                "PermissionRequest",
                fixture(
                    r#""hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{"command":"npm test"},"tool_use_id":"toolu_2""#,
                ),
            ),
            (
                "PermissionDenied",
                fixture(
                    r#""hook_event_name":"PermissionDenied","tool_name":"Bash","tool_input":{"command":"rm -rf x"},"tool_use_id":"toolu_3""#,
                ),
            ),
            (
                "PostToolUse",
                fixture(
                    r#""hook_event_name":"PostToolUse","tool_name":"Read","tool_input":{"file_path":"/a/b"},"tool_response":{"content":"lots of text"},"tool_use_id":"toolu_4""#,
                ),
            ),
            (
                "PostToolUseFailure",
                fixture(
                    r#""hook_event_name":"PostToolUseFailure","tool_name":"Bash","tool_input":{"command":"false"},"tool_use_id":"toolu_5","error":"exit 1""#,
                ),
            ),
            (
                "Notification",
                fixture(
                    r#""hook_event_name":"Notification","notification_type":"permission_prompt","message":"Claude needs your permission to use Bash""#,
                ),
            ),
            (
                "Stop",
                fixture(r#""hook_event_name":"Stop","last_assistant_message":"done""#),
            ),
            ("StopFailure", fixture(r#""hook_event_name":"StopFailure""#)),
            (
                "SessionEnd",
                fixture(r#""hook_event_name":"SessionEnd","reason":"prompt_input_exit""#),
            ),
        ]
    }

    #[test]
    fn parses_and_trims_every_event() {
        for (name, raw) in fixtures() {
            let mut p = parse(&raw).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(p.event_name, name);
            assert_eq!(p.session_id, "sess-1");
            trim(&mut p.json);
            assert!(p.json.get("transcript_path").is_none(), "{name}");
            assert!(p.json.get("tool_response").is_none(), "{name}");
            assert_eq!(p.json["cwd"], "C:\\work\\demo");
            assert_eq!(p.json["hook_event_name"], name);
        }
    }

    #[test]
    fn trim_drops_large_tool_response_but_keeps_tool_input() {
        let big = "x".repeat(100_000);
        let raw = fixture(&format!(
            r#""hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{{"command":"ls"}},"tool_response":{{"stdout":"{big}"}}"#
        ));
        let mut p = parse(&raw).unwrap();
        trim(&mut p.json);
        assert!(p.json.get("tool_response").is_none());
        assert_eq!(p.json["tool_input"]["command"], "ls");
        assert!(to_frame(&p).len() < 1000);
    }

    #[test]
    fn trim_only_drops_top_level_fields() {
        let mut v =
            serde_json::json!({"tool_input": {"tool_response": "kept", "transcript_path": "kept"}});
        trim(&mut v);
        assert_eq!(v["tool_input"]["tool_response"], "kept");
        assert_eq!(v["tool_input"]["transcript_path"], "kept");
    }

    #[test]
    fn strings_are_truncated_to_2000_chars_recursively() {
        let long = "æ".repeat(5000);
        let exact = "y".repeat(MAX_STRING_CHARS);
        let mut v = serde_json::json!({
            "a": long, "b": {"c": [long, 1, true, null]}, "d": exact, "e": "short"
        });
        trim(&mut v);
        for s in [&v["a"], &v["b"]["c"][0]] {
            let s = s.as_str().unwrap();
            assert_eq!(s.chars().count(), MAX_STRING_CHARS);
            assert!(s.ends_with('…'));
        }
        assert_eq!(v["b"]["c"][1], 1);
        assert_eq!(v["d"].as_str().unwrap(), exact);
        assert_eq!(v["e"], "short");
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert!(matches!(parse("not json"), Err(Error::Json(_))));
        assert!(matches!(parse("[1,2]"), Err(Error::NotAnObject)));
        assert!(matches!(
            parse(r#"{"session_id":"x"}"#),
            Err(Error::MissingField("hook_event_name"))
        ));
        assert!(matches!(
            parse(r#"{"hook_event_name":"Stop"}"#),
            Err(Error::MissingField("session_id"))
        ));
        assert!(matches!(
            parse(r#"{"hook_event_name":1,"session_id":"x"}"#),
            Err(Error::MissingField("hook_event_name"))
        ));
    }

    #[test]
    fn budget_per_event() {
        assert_eq!(budget("PermissionRequest"), Duration::from_secs(110));
        assert_eq!(budget("SessionEnd"), Duration::from_secs(1));
        for e in [
            "Stop",
            "PreToolUse",
            "Notification",
            "SessionStart",
            "Whatever",
        ] {
            assert_eq!(budget(e), Duration::from_secs(2));
        }
        assert!(expects_reply("PermissionRequest"));
        assert!(!expects_reply("PreToolUse"));
    }

    #[test]
    fn oversized_tool_input_is_replaced_but_tool_name_kept() {
        // 50 000 small edits: every string is short, but the frame is about 2 MiB.
        let edits: Vec<Value> = (0..50_000)
            .map(|i| json!({"old_string": format!("a{i}"), "new_string": "b"}))
            .collect();
        let raw = json!({
            "hook_event_name": "PermissionRequest",
            "session_id": "sess-1",
            "cwd": "C:\\work",
            "tool_name": "MultiEdit",
            "tool_input": {"file_path": "src/x.rs", "edits": edits},
        })
        .to_string();
        let mut p = parse(&raw).unwrap();
        trim(&mut p.json);
        assert!(render_frame(&p.json).len() > MAX_FRAME_BYTES);

        let f = to_frame(&p);
        assert!(f.len() <= MAX_FRAME_BYTES, "{}", f.len());
        assert!(f.ends_with('\n'));
        assert_eq!(f.matches('\n').count(), 1);
        let v: Value = serde_json::from_str(f.trim_end()).unwrap();
        assert_eq!(v["event"]["tool_input"], json!({"_truncated": true}));
        assert_eq!(v["event"]["tool_name"], "MultiEdit");
        assert_eq!(v["event"]["hook_event_name"], "PermissionRequest");
        assert_eq!(v["event"]["session_id"], "sess-1");
        assert_eq!(v["event"]["cwd"], "C:\\work");
        // The payload itself is untouched.
        assert_eq!(
            p.json["tool_input"]["edits"].as_array().unwrap().len(),
            50_000
        );
    }

    #[test]
    fn small_frames_keep_tool_input() {
        let p = parse(
            r#"{"hook_event_name":"PreToolUse","session_id":"s","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
        )
        .unwrap();
        let v: Value = serde_json::from_str(to_frame(&p).trim_end()).unwrap();
        assert_eq!(v["event"]["tool_input"]["command"], "ls");
    }

    #[test]
    fn frame_is_one_json_line() {
        let p = parse(r#"{"hook_event_name":"Stop","session_id":"s","x":"a\nb"}"#).unwrap();
        let f = to_frame(&p);
        assert!(f.ends_with('\n'));
        assert_eq!(
            f.matches('\n').count(),
            1,
            "embedded newlines must be escaped"
        );
        let v: Value = serde_json::from_str(f.trim_end()).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["kind"], "hook");
        assert_eq!(v["event"]["hook_event_name"], "Stop");
        assert_eq!(v["event"]["x"], "a\nb");
    }
}
