//! Deserialised hook event as received from `mira-hook` over the pipe.

use serde::Deserialize;
use serde_json::{Map, Value};

/// Maximum length (chars) of a tool-input summary.
pub const SUMMARY_MAX_CHARS: usize = 120;

/// One Claude Code hook event (stdin JSON of the hook, already trimmed by `mira-hook`).
/// Unknown fields end up in `extra`.
#[derive(Deserialize, Clone, Debug)]
pub struct HookEvent {
    pub hook_event_name: String,
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<Value>,
    #[serde(default)]
    pub tool_use_id: Option<String>,
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Parses the `event` value of a pipe frame.
pub fn parse(v: &Value) -> Result<HookEvent, serde_json::Error> {
    HookEvent::deserialize(v)
}

/// Truncates to `max` chars; the last char becomes `…` when something was cut.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Keeps only the last two components of a path (accepts `/` and `\`).
fn short_path(p: &str) -> String {
    let parts: Vec<&str> = p.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
    match parts.len() {
        0 => String::new(),
        1 => parts[0].to_string(),
        n => format!("{}/{}", parts[n - 2], parts[n - 1]),
    }
}

fn str_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

/// Short human-readable summary of what a tool call does (max 120 chars).
/// Empty string when there is nothing sensible to show.
pub fn summarize_tool_input(tool_name: &str, input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    let raw: String = match tool_name {
        "Bash" | "PowerShell" => str_field(input, "command").unwrap_or_default().to_string(),
        "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "NotebookRead" => {
            short_path(str_field(input, "file_path").unwrap_or_default())
        }
        "Glob" | "Grep" => str_field(input, "pattern").unwrap_or_default().to_string(),
        "WebFetch" => str_field(input, "url").unwrap_or_default().to_string(),
        "WebSearch" => str_field(input, "query").unwrap_or_default().to_string(),
        "Task" | "Agent" => str_field(input, "description")
            .unwrap_or_default()
            .to_string(),
        // Unknown tool: first string field (serde_json maps are key-sorted, so this is deterministic).
        _ => input
            .as_object()
            .and_then(|m| m.values().find_map(Value::as_str))
            .unwrap_or_default()
            .to_string(),
    };
    truncate(&raw, SUMMARY_MAX_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::fixtures as fx;
    use serde_json::json;

    fn parse_fixture(s: &str) -> HookEvent {
        parse(&serde_json::from_str::<Value>(s).unwrap()).unwrap()
    }

    #[test]
    fn parses_every_fixture() {
        let cases = [
            (fx::SESSION_START, "SessionStart"),
            (fx::USER_PROMPT_SUBMIT, "UserPromptSubmit"),
            (fx::PRE_TOOL_USE, "PreToolUse"),
            (fx::PERMISSION_REQUEST, "PermissionRequest"),
            (fx::PERMISSION_DENIED, "PermissionDenied"),
            (fx::POST_TOOL_USE, "PostToolUse"),
            (fx::POST_TOOL_USE_FAILURE, "PostToolUseFailure"),
            (fx::NOTIFICATION_PERMISSION, "Notification"),
            (fx::NOTIFICATION_IDLE, "Notification"),
            (fx::NOTIFICATION_OTHER, "Notification"),
            (fx::STOP, "Stop"),
            (fx::STOP_FAILURE, "StopFailure"),
            (fx::SESSION_END, "SessionEnd"),
            (fx::SUBAGENT_STOP, "SubagentStop"),
        ];
        for (json, name) in cases {
            let ev = parse_fixture(json);
            assert_eq!(ev.hook_event_name, name);
            assert_eq!(ev.session_id, "sess-1");
            assert_eq!(ev.cwd.as_deref(), Some("C:\\work\\demo"));
        }
    }

    #[test]
    fn parses_event_specific_fields() {
        let pre = parse_fixture(fx::PRE_TOOL_USE);
        assert_eq!(pre.tool_name.as_deref(), Some("Edit"));
        assert_eq!(pre.tool_use_id.as_deref(), Some("toolu_1"));
        assert!(pre.tool_input.is_some());
        assert!(pre.extra.contains_key("permission_mode"));

        let n = parse_fixture(fx::NOTIFICATION_PERMISSION);
        assert_eq!(n.notification_type.as_deref(), Some("permission_prompt"));
        assert!(n.message.unwrap().contains("permission"));

        assert_eq!(
            parse_fixture(fx::SESSION_START).source.as_deref(),
            Some("startup")
        );
        assert_eq!(
            parse_fixture(fx::SESSION_END).reason.as_deref(),
            Some("prompt_input_exit")
        );
        assert_eq!(
            parse_fixture(fx::USER_PROMPT_SUBMIT).prompt.as_deref(),
            Some("fix the bug")
        );
        assert_eq!(
            parse_fixture(fx::SUBAGENT_STOP).agent_id.as_deref(),
            Some("ag-1")
        );
    }

    #[test]
    fn missing_required_fields_is_an_error() {
        assert!(parse(&json!({"hook_event_name":"Stop"})).is_err());
        assert!(parse(&json!({"session_id":"x"})).is_err());
        assert!(parse(&json!("not an object")).is_err());
    }

    #[test]
    fn summarize_bash_uses_command() {
        let v = json!({"command":"npm test","description":"x"});
        assert_eq!(summarize_tool_input("Bash", Some(&v)), "npm test");
        assert_eq!(summarize_tool_input("PowerShell", Some(&v)), "npm test");
    }

    #[test]
    fn summarize_file_tools_keep_last_two_components() {
        let v = json!({"file_path":"C:\\work\\demo\\src\\main.rs"});
        assert_eq!(summarize_tool_input("Edit", Some(&v)), "src/main.rs");
        let v = json!({"file_path":"/a/b/c.txt"});
        assert_eq!(summarize_tool_input("Read", Some(&v)), "b/c.txt");
        let v = json!({"file_path":"single.txt"});
        assert_eq!(summarize_tool_input("Write", Some(&v)), "single.txt");
    }

    #[test]
    fn summarize_other_known_tools() {
        assert_eq!(
            summarize_tool_input("Glob", Some(&json!({"pattern":"**/*.rs"}))),
            "**/*.rs"
        );
        assert_eq!(
            summarize_tool_input("Grep", Some(&json!({"pattern":"fn main"}))),
            "fn main"
        );
        assert_eq!(
            summarize_tool_input("WebFetch", Some(&json!({"url":"https://x.dk"}))),
            "https://x.dk"
        );
        assert_eq!(
            summarize_tool_input("WebSearch", Some(&json!({"query":"tauri"}))),
            "tauri"
        );
        assert_eq!(
            summarize_tool_input("Task", Some(&json!({"description":"explore"}))),
            "explore"
        );
    }

    #[test]
    fn summarize_unknown_tool_uses_first_string_field() {
        let v = json!({"n":1,"alpha":"hello","zeta":"later"});
        assert_eq!(summarize_tool_input("mcp__x__y", Some(&v)), "hello");
        assert_eq!(summarize_tool_input("Mystery", Some(&json!({"n":1}))), "");
        assert_eq!(summarize_tool_input("Bash", None), "");
    }

    #[test]
    fn summarize_truncates_to_120_chars() {
        let long = "x".repeat(500);
        let s = summarize_tool_input("Bash", Some(&json!({ "command": long })));
        assert_eq!(s.chars().count(), SUMMARY_MAX_CHARS);
        assert!(s.ends_with('…'));
        let exact = "y".repeat(SUMMARY_MAX_CHARS);
        let s = summarize_tool_input("Bash", Some(&json!({ "command": exact.clone() })));
        assert_eq!(s, exact);
    }
}
