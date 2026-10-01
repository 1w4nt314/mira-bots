//! Mapping from hook events to an agent's visible status.

use serde::{Deserialize, Serialize};

use super::event::{summarize_tool_input, HookEvent};
use crate::config::MCP_TOOL_PREFIX;

/// Visible agent status. Wire format: `{"kind":"editing"}`, `{"kind":"exited","code":1}`.
/// There is no `Done`: a Stop hook means the turn ended, i.e. `Idle`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AgentStatus {
    Starting,
    Idle,
    Thinking,
    Reading,
    Editing,
    Running,
    WaitingPermission,
    Exited { code: Option<i32> },
}

/// Status implied by a tool call.
///
/// TODO(unverified): the tool-name lists below come from general knowledge of Claude Code's
/// built-in tools, not from research. A wrong entry only means a wrong colour.
///
/// The app's own MCP tools (`mcp__mira-bots__*`) and `ToolSearch` count as thinking: they are
/// bookkeeping, not work on the project (C4.9).
pub fn status_for_tool(tool_name: &str) -> AgentStatus {
    if tool_name.starts_with(MCP_TOOL_PREFIX) {
        return AgentStatus::Thinking;
    }
    match tool_name {
        "Read" | "Glob" | "Grep" | "LS" | "NotebookRead" | "WebFetch" | "WebSearch" => {
            AgentStatus::Reading
        }
        "ToolSearch" => AgentStatus::Thinking,
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => AgentStatus::Editing,
        "TodoWrite" | "AskUserQuestion" | "ExitPlanMode" | "EnterPlanMode" => AgentStatus::Thinking,
        // Bash, PowerShell, Task, Agent, Skill, mcp__* and everything unknown.
        _ => AgentStatus::Running,
    }
}

/// Result of applying a hook event. `status: None` means "leave the status unchanged".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    pub status: Option<AgentStatus>,
    pub detail: Option<String>,
}

impl Transition {
    fn set(status: AgentStatus, detail: Option<String>) -> Self {
        Self {
            status: Some(status),
            detail,
        }
    }

    fn unchanged() -> Self {
        Self {
            status: None,
            detail: None,
        }
    }
}

fn tool_summary(ev: &HookEvent) -> Option<String> {
    let s = summarize_tool_input(
        ev.tool_name.as_deref().unwrap_or_default(),
        ev.tool_input.as_ref(),
    );
    (!s.is_empty()).then_some(s)
}

/// Maps a hook event to a status transition.
pub fn apply(ev: &HookEvent) -> Transition {
    match ev.hook_event_name.as_str() {
        "SessionStart" => Transition::set(AgentStatus::Idle, None),
        "UserPromptSubmit" => Transition::set(AgentStatus::Thinking, None),
        "PreToolUse" => Transition::set(
            status_for_tool(ev.tool_name.as_deref().unwrap_or_default()),
            tool_summary(ev),
        ),
        "PermissionRequest" => Transition::set(AgentStatus::WaitingPermission, tool_summary(ev)),
        // An `isError` result of the app's own tools (e.g. "Du har ingen ticket i gang") is
        // normal flow, not a failure to show.
        // TODO(windows-verify): `PreToolUse` for our tools shows "Tænker" with the Danish label
        // and `PostToolUseFailure` does not show red (plan4 D.47).
        "PostToolUseFailure"
            if ev
                .tool_name
                .as_deref()
                .is_some_and(|n| n.starts_with(MCP_TOOL_PREFIX)) =>
        {
            Transition::unchanged()
        }
        "PermissionDenied" | "PostToolUse" | "PostToolUseFailure" => {
            Transition::set(AgentStatus::Thinking, None)
        }
        "Notification" => match ev.notification_type.as_deref() {
            Some("permission_prompt") => {
                Transition::set(AgentStatus::WaitingPermission, ev.message.clone())
            }
            Some("idle_prompt") => Transition::set(AgentStatus::Idle, None),
            _ => Transition::unchanged(),
        },
        "Stop" | "StopFailure" => Transition::set(AgentStatus::Idle, None),
        // Live model/effort only (plan5 A.4); the handler updates them separately.
        "StatusLine" | "PostModelSwitch" => Transition::unchanged(),
        // SessionEnd: the PTY exit sets Exited. SubagentStart/Stop and the rest: no change.
        _ => Transition::unchanged(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::event::parse;
    use crate::hooks::fixtures as fx;
    use serde_json::Value;

    fn ev(s: &str) -> HookEvent {
        parse(&serde_json::from_str::<Value>(s).unwrap()).unwrap()
    }

    fn t(status: Option<AgentStatus>, detail: Option<&str>) -> Transition {
        Transition {
            status,
            detail: detail.map(str::to_string),
        }
    }

    #[test]
    fn apply_session_start_is_idle() {
        assert_eq!(
            apply(&ev(fx::SESSION_START)),
            t(Some(AgentStatus::Idle), None)
        );
    }

    #[test]
    fn apply_user_prompt_is_thinking() {
        assert_eq!(
            apply(&ev(fx::USER_PROMPT_SUBMIT)),
            t(Some(AgentStatus::Thinking), None)
        );
    }

    #[test]
    fn apply_pre_tool_use_maps_tool_and_summarizes() {
        assert_eq!(
            apply(&ev(fx::PRE_TOOL_USE)),
            t(Some(AgentStatus::Editing), Some("src/main.rs"))
        );
    }

    #[test]
    fn apply_permission_request_waits_with_summary() {
        assert_eq!(
            apply(&ev(fx::PERMISSION_REQUEST)),
            t(Some(AgentStatus::WaitingPermission), Some("npm test"))
        );
    }

    #[test]
    fn apply_permission_denied_and_post_tool_are_thinking() {
        for f in [
            fx::PERMISSION_DENIED,
            fx::POST_TOOL_USE,
            fx::POST_TOOL_USE_FAILURE,
        ] {
            assert_eq!(apply(&ev(f)), t(Some(AgentStatus::Thinking), None));
        }
    }

    #[test]
    fn apply_notification_variants() {
        assert_eq!(
            apply(&ev(fx::NOTIFICATION_PERMISSION)),
            t(
                Some(AgentStatus::WaitingPermission),
                Some("Claude needs your permission to use Bash")
            )
        );
        assert_eq!(
            apply(&ev(fx::NOTIFICATION_IDLE)),
            t(Some(AgentStatus::Idle), None)
        );
        assert_eq!(apply(&ev(fx::NOTIFICATION_OTHER)), t(None, None));
    }

    #[test]
    fn apply_stop_and_stop_failure_are_idle() {
        assert_eq!(apply(&ev(fx::STOP)), t(Some(AgentStatus::Idle), None));
        assert_eq!(
            apply(&ev(fx::STOP_FAILURE)),
            t(Some(AgentStatus::Idle), None)
        );
    }

    #[test]
    fn apply_session_end_and_subagent_events_do_not_change_status() {
        assert_eq!(apply(&ev(fx::SESSION_END)), t(None, None));
        assert_eq!(apply(&ev(fx::SUBAGENT_STOP)), t(None, None));
        let start = r#"{"hook_event_name":"SubagentStart","session_id":"s"}"#;
        assert_eq!(apply(&ev(start)), t(None, None));
    }

    #[test]
    fn statusline_and_model_switch_do_not_change_status() {
        for e in ["StatusLine", "PostModelSwitch"] {
            let ev = parse(&serde_json::json!({
                "hook_event_name": e, "session_id": "s", "model": {"id": "claude-x"},
                "to_model": "claude-x"
            }))
            .unwrap();
            assert_eq!(apply(&ev), t(None, None), "{e}");
        }
    }

    #[test]
    fn notification_without_type_is_unchanged() {
        let n = r#"{"hook_event_name":"Notification","session_id":"s","message":"hi"}"#;
        assert_eq!(apply(&ev(n)), t(None, None));
    }

    #[test]
    fn status_for_tool_known_names() {
        use AgentStatus::*;
        let table = [
            ("Read", Reading),
            ("Glob", Reading),
            ("Grep", Reading),
            ("WebFetch", Reading),
            ("Edit", Editing),
            ("Write", Editing),
            ("MultiEdit", Editing),
            ("NotebookEdit", Editing),
            ("TodoWrite", Thinking),
            ("AskUserQuestion", Thinking),
            ("Bash", Running),
            ("PowerShell", Running),
            ("Task", Running),
            ("Agent", Running),
            ("Skill", Running),
        ];
        for (name, want) in table {
            assert_eq!(status_for_tool(name), want, "tool {name}");
        }
    }

    fn tool_ev(event: &str, tool: &str) -> HookEvent {
        let v = serde_json::json!({"hook_event_name": event, "session_id": "s", "tool_name": tool,
                                   "tool_input": {"summary": "hemmelig", "note": "n"},
                                   "mcp_server": {"name": "mira-bots", "source": "dynamic"}});
        parse(&v).unwrap()
    }

    #[test]
    fn own_mcp_tools_are_thinking_with_a_danish_label() {
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
            ("mira_list_profiles", "Læser profiler"),
            ("mira_get_workspace_rules", "Læser regler"),
            ("mira_add_report", "Skriver rapport"),
            ("mira_get_report", "Læser rapport"),
        ];
        for (tool, label) in table {
            let name = format!("mcp__mira-bots__{tool}");
            assert_eq!(status_for_tool(&name), AgentStatus::Thinking);
            assert_eq!(
                apply(&tool_ev("PreToolUse", &name)),
                t(Some(AgentStatus::Thinking), Some(label))
            );
            assert_eq!(
                apply(&tool_ev("PostToolUse", &name)),
                t(Some(AgentStatus::Thinking), None)
            );
        }
    }

    #[test]
    fn tool_search_is_thinking() {
        assert_eq!(status_for_tool("ToolSearch"), AgentStatus::Thinking);
        let ev = tool_ev("PreToolUse", "ToolSearch");
        assert_eq!(apply(&ev).status, Some(AgentStatus::Thinking));
    }

    #[test]
    fn post_tool_use_failure_of_own_tools_changes_nothing() {
        let ev = tool_ev(
            "PostToolUseFailure",
            "mcp__mira-bots__mira_submit_for_review",
        );
        assert_eq!(apply(&ev), t(None, None));
        let ev = tool_ev("PostToolUseFailure", "mcp__mira-bots__mira_list_tickets");
        assert_eq!(apply(&ev), t(None, None));
        // Everything else as before.
        assert_eq!(
            apply(&tool_ev("PostToolUseFailure", "Bash")),
            t(Some(AgentStatus::Thinking), None)
        );
        assert_eq!(
            apply(&tool_ev("PostToolUseFailure", "mcp__x__y")),
            t(Some(AgentStatus::Thinking), None)
        );
    }

    #[test]
    fn status_for_tool_mcp_and_unknown_are_running() {
        assert_eq!(status_for_tool("mcp__x__y"), AgentStatus::Running);
        assert_eq!(status_for_tool("SomethingNew"), AgentStatus::Running);
        assert_eq!(status_for_tool(""), AgentStatus::Running);
    }

    #[test]
    fn status_serde_roundtrip() {
        let exited = AgentStatus::Exited { code: Some(1) };
        let v = serde_json::to_value(&exited).unwrap();
        assert_eq!(v, serde_json::json!({"kind":"exited","code":1}));
        assert_eq!(serde_json::from_value::<AgentStatus>(v).unwrap(), exited);

        let idle = serde_json::to_value(AgentStatus::Idle).unwrap();
        assert_eq!(idle, serde_json::json!({"kind":"idle"}));

        let w = serde_json::to_value(AgentStatus::WaitingPermission).unwrap();
        assert_eq!(w, serde_json::json!({"kind":"waitingPermission"}));

        let none = serde_json::to_value(AgentStatus::Exited { code: None }).unwrap();
        assert_eq!(none, serde_json::json!({"kind":"exited","code":null}));
    }
}
