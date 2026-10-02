//! Deserialised hook event as received from `mira-hook` over the pipe.

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::config::MCP_TOOL_PREFIX;

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
    /// `mcp_server.source` of an MCP tool event (`"dynamic"` for `--mcp-config` servers, e.g.
    /// `"user"` for the user's own; Claude Code ≥ 2.1.274). `None` when absent or not a string;
    /// a malformed `mcp_server` never fails the event.
    #[serde(default, rename = "mcp_server", deserialize_with = "mcp_server_source")]
    pub mcp_server_source: Option<String>,
    /// `model`: an object `{id, display_name}` in a StatusLine payload, a string in SessionStart.
    /// See [`HookEvent::model_id`].
    #[serde(default)]
    pub model: Option<Value>,
    /// `effort`: `{level}` in StatusLine and tool events. See [`HookEvent::effort_level`].
    #[serde(default)]
    pub effort: Option<Value>,
    /// PostModelSwitch: the model switched to (also after `--resume`).
    #[serde(default)]
    pub to_model: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn mcp_server_source<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(v.get("source").and_then(Value::as_str).map(str::to_string))
}

impl HookEvent {
    /// The live model id: `model.id` of a StatusLine payload, `to_model` of PostModelSwitch.
    pub fn model_id(&self) -> Option<String> {
        let id = match self.hook_event_name.as_str() {
            "StatusLine" => self
                .model
                .as_ref()
                .and_then(|m| m.get("id"))
                .and_then(Value::as_str),
            "PostModelSwitch" => self.to_model.as_deref(),
            _ => None,
        };
        id.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// `effort.level` of a StatusLine payload (absent for models without effort).
    pub fn effort_level(&self) -> Option<String> {
        if self.hook_event_name != "StatusLine" {
            return None;
        }
        self.effort
            .as_ref()
            .and_then(|e| e.get("level"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }
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

/// Danish label (C4.9) for one of the app's own MCP tools (`mcp__mira-bots__<name>`); `None`
/// for any other tool.
pub fn mira_tool_label(tool_name: &str) -> Option<&'static str> {
    match tool_name.strip_prefix(MCP_TOOL_PREFIX)? {
        "mira_create_ticket" => Some("Opretter ticket"),
        "mira_list_tickets" => Some("Læser tickets"),
        "mira_get_ticket" => Some("Læser ticket"),
        "mira_submit_for_review" => Some("Afleverer til review"),
        "mira_update_status" => Some("Opdaterer status"),
        // Step 5 (plan5 C5.13).
        "mira_approve_ticket" => Some("Godkender ticket"),
        "mira_reject_ticket" => Some("Afviser ticket"),
        "mira_assign_ticket" => Some("Tildeler ticket"),
        "mira_unassign_ticket" => Some("Fjerner tildeling"),
        "mira_spawn_agent" => Some("Starter agent"),
        "mira_list_agents" => Some("Læser agenter"),
        "mira_list_projects" => Some("Læser projekter"),
        "mira_list_profiles" => Some("Læser profiler"),
        "mira_get_workspace_rules" => Some("Læser regler"),
        "mira_add_report" => Some("Skriver rapport"),
        "mira_get_report" => Some("Læser rapport"),
        // Step 5c.
        "mira_handoff_ticket" => Some("Giver ticket videre"),
        _ => None,
    }
}

/// Short human-readable summary of what a tool call does (max 120 chars).
/// Empty string when there is nothing sensible to show. The app's own MCP tools get their label
/// instead: their arguments (title, body, summary, note) never reach the status line.
pub fn summarize_tool_input(tool_name: &str, input: Option<&Value>) -> String {
    if tool_name.starts_with(MCP_TOOL_PREFIX) {
        return mira_tool_label(tool_name).unwrap_or_default().to_string();
    }
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
    fn statusline_and_model_switch_fields_parse() {
        let ev = parse(&json!({
            "hook_event_name": "StatusLine", "session_id": "s",
            "model": {"id": "claude-opus-5-5", "display_name": "Opus 5.5"},
            "effort": {"level": "xhigh"}
        }))
        .unwrap();
        assert_eq!(ev.model_id().as_deref(), Some("claude-opus-5-5"));
        assert_eq!(ev.effort_level().as_deref(), Some("xhigh"));
        // Without effort (Haiku).
        let ev = parse(&json!({
            "hook_event_name": "StatusLine", "session_id": "s", "model": {"id": "claude-haiku-4-5"}
        }))
        .unwrap();
        assert_eq!(
            (ev.model_id().as_deref(), ev.effort_level()),
            (Some("claude-haiku-4-5"), None)
        );
        let ev = parse(&json!({
            "hook_event_name": "PostModelSwitch", "session_id": "s",
            "from_model": "claude-haiku-4-5-20251001", "to_model": "claude-sonnet-5-5",
            "requested_model": "sonnet", "source": "resume"
        }))
        .unwrap();
        assert_eq!(ev.model_id().as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(ev.effort_level(), None);
        assert_eq!(ev.source.as_deref(), Some("resume"));
        // SessionStart's `model` string and tool events' effort are not "live" values here.
        let ev = parse(&json!({
            "hook_event_name": "SessionStart", "session_id": "s", "model": "claude-x",
            "effort": {"level": "high"}
        }))
        .unwrap();
        assert_eq!((ev.model_id(), ev.effort_level()), (None, None));
        assert_eq!(ev.model, Some(json!("claude-x")));
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
    fn mcp_server_source_is_parsed_leniently() {
        let base = json!({"hook_event_name":"PreToolUse","session_id":"s",
                          "tool_name":"mcp__mira-bots__mira_list_tickets"});
        let with = |server: Value| {
            let mut v = base.clone();
            v["mcp_server"] = server;
            parse(&v).unwrap().mcp_server_source
        };
        assert_eq!(
            with(json!({"name":"mira-bots","source":"dynamic"})).as_deref(),
            Some("dynamic")
        );
        assert_eq!(
            with(json!({"name":"mira-bots","source":"user"})).as_deref(),
            Some("user")
        );
        assert_eq!(with(json!({"name":"mira-bots"})), None);
        assert_eq!(with(json!("odd")), None);
        assert_eq!(with(Value::Null), None);
        let ev = parse(&base).unwrap();
        assert_eq!(ev.mcp_server_source, None);
        assert!(!ev.extra.contains_key("mcp_server"));
        assert_eq!(parse_fixture(fx::PRE_TOOL_USE).mcp_server_source, None);
    }

    #[test]
    fn own_mcp_tools_get_labels_never_their_arguments() {
        let table = [
            ("mira_create_ticket", "Opretter ticket"),
            ("mira_list_tickets", "Læser tickets"),
            ("mira_get_ticket", "Læser ticket"),
            ("mira_submit_for_review", "Afleverer til review"),
            ("mira_update_status", "Opdaterer status"),
            ("mira_approve_ticket", "Godkender ticket"),
            ("mira_reject_ticket", "Afviser ticket"),
            ("mira_assign_ticket", "Tildeler ticket"),
            ("mira_unassign_ticket", "Fjerner tildeling"),
            ("mira_spawn_agent", "Starter agent"),
            ("mira_list_agents", "Læser agenter"),
            ("mira_list_projects", "Læser projekter"),
            ("mira_list_profiles", "Læser profiler"),
            ("mira_get_workspace_rules", "Læser regler"),
            ("mira_add_report", "Skriver rapport"),
            ("mira_get_report", "Læser rapport"),
            ("mira_handoff_ticket", "Giver ticket videre"),
        ];
        // Every tool of the MCP server has a label (and only those).
        let mut names: Vec<&str> = table.iter().map(|(t, _)| *t).collect();
        let mut want = mira_mcp::tools::TOOL_NAMES.to_vec();
        names.sort_unstable();
        want.sort_unstable();
        assert_eq!(names, want);
        let input =
            json!({"summary":"hemmelig opsummering","title":"x","note":"y","body":"rapport"});
        for (tool, label) in table {
            let name = format!("mcp__mira-bots__{tool}");
            assert_eq!(mira_tool_label(&name), Some(label));
            assert_eq!(summarize_tool_input(&name, Some(&input)), label);
        }
        assert_eq!(mira_tool_label("mcp__mira-bots__other"), None);
        assert_eq!(
            summarize_tool_input("mcp__mira-bots__other", Some(&input)),
            ""
        );
        assert_eq!(mira_tool_label("mira_create_ticket"), None);
        assert_eq!(mira_tool_label("mcp__x__mira_create_ticket"), None);
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
