//! Names and payloads of the Tauri events emitted from Rust to the frontend.
//! All payloads are camelCase on the wire.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hooks::status::AgentStatus;

/// Emits a Tauri event (`name`, JSON payload). In the app this wraps `app.emit`.
/// (Moved here from `pipe::handler`, which re-exports it, so `tickets` can use it too.)
pub type EmitFn = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// Full agent list (`AgentInfo[]`) after any change.
pub const AGENTS_CHANGED: &str = "agents-changed";
/// PTY output chunk (`AgentOutputPayload`).
pub const AGENT_OUTPUT: &str = "agent-output";
/// A permission request awaiting a UI answer (`PermissionRequestInfo`).
pub const PERMISSION_REQUEST: &str = "permission-request";
/// A permission request was answered or expired (`PermissionResolvedPayload`).
pub const PERMISSION_RESOLVED: &str = "permission-resolved";
/// Debug/step-2 feed of hook events (`HookEventPayload`).
pub const HOOK_EVENT: &str = "hook-event";
/// Select an agent in an already open workplace window (payload: agent id string). Only sent to
/// the `workplace` window.
pub const WORKPLACE_SELECT: &str = "workplace-select";
/// Full ticket list (`TicketSummary[]`, without history) after any ticket mutation.
pub const TICKETS_CHANGED: &str = "tickets-changed";
/// Full profile list (`AgentProfile[]`) after a profile was saved, deleted or reset.
pub const PROFILES_CHANGED: &str = "profiles-changed";
/// The inbox (`InboxPayload`: items without bodies + status per source) after any inbox change
/// and at the start and end of a refresh (step 6c).
pub const INBOX_CHANGED: &str = "inbox-changed";

/// Payload of `workplace-select` and the result of `take_workplace_selection`: which agent and/or
/// sidebar tab the workplace window should show (`tab`: "permissions" | "diagnostics" |
/// "tickets"), and optionally a seat kind whose spawn dialog it should open (`spawn`: "work" |
/// "staff").
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkplaceSelection {
    pub agent_id: Option<String>,
    pub tab: Option<String>,
    #[serde(default)]
    pub spawn: Option<String>,
}

/// In-process notification (not a Tauri event) from the pipe handler after a hook frame was
/// matched to an agent and applied: what happened and the status it implied. Lives here, in a
/// module both `pipe` and `tickets` may import, so `pipe` never depends on `tickets`.
/// `prompt` is only set for `UserPromptSubmit` and must never be logged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEvent {
    pub agent_id: String,
    pub hook_event_name: String,
    pub prompt: Option<String>,
    /// `None`: the event left the status unchanged.
    pub status: Option<AgentStatus>,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentOutputPayload {
    pub agent_id: String,
    /// Ring buffer byte counter after this chunk.
    pub seq: u64,
    pub data_base64: String,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionResolvedPayload {
    pub request_id: String,
    /// "allow" | "deny" | "none"
    pub decision: String,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HookEventPayload {
    pub agent_id: Option<String>,
    pub session_id: String,
    pub hook_event_name: String,
    pub tool_name: Option<String>,
    /// Milliseconds since the Unix epoch.
    pub received_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_names_are_kebab_case() {
        assert_eq!(AGENTS_CHANGED, "agents-changed");
        assert_eq!(AGENT_OUTPUT, "agent-output");
        assert_eq!(PERMISSION_REQUEST, "permission-request");
        assert_eq!(PERMISSION_RESOLVED, "permission-resolved");
        assert_eq!(HOOK_EVENT, "hook-event");
        assert_eq!(WORKPLACE_SELECT, "workplace-select");
        assert_eq!(TICKETS_CHANGED, "tickets-changed");
        assert_eq!(PROFILES_CHANGED, "profiles-changed");
        assert_eq!(INBOX_CHANGED, "inbox-changed");
    }

    #[test]
    fn workplace_selection_is_camel_case() {
        let sel = WorkplaceSelection {
            agent_id: Some("a".into()),
            tab: Some("tickets".into()),
            spawn: Some("work".into()),
        };
        let v = serde_json::to_value(&sel).unwrap();
        assert_eq!(v, json!({"agentId":"a","tab":"tickets","spawn":"work"}));
        assert_eq!(
            serde_json::from_value::<WorkplaceSelection>(v).unwrap(),
            sel
        );
        assert_eq!(
            serde_json::to_value(WorkplaceSelection::default()).unwrap(),
            json!({"agentId":null,"tab":null,"spawn":null})
        );
    }

    #[test]
    fn workplace_selection_spawn_is_optional_when_reading() {
        // Old payloads (before `spawn` existed) still parse, with `spawn` = None.
        let old = json!({"agentId":"a","tab":"tickets"});
        let sel = serde_json::from_value::<WorkplaceSelection>(old).unwrap();
        assert_eq!(sel.spawn, None);
        assert_eq!(sel.agent_id.as_deref(), Some("a"));
        let new = json!({"agentId":null,"tab":null,"spawn":"staff"});
        let sel = serde_json::from_value::<WorkplaceSelection>(new).unwrap();
        assert_eq!(sel.spawn.as_deref(), Some("staff"));
    }

    #[test]
    fn payloads_serialize_camel_case() {
        let out = AgentOutputPayload {
            agent_id: "a".into(),
            seq: 7,
            data_base64: "aGk=".into(),
        };
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!({"agentId":"a","seq":7,"dataBase64":"aGk="})
        );
        let res = PermissionResolvedPayload {
            request_id: "r".into(),
            decision: "allow".into(),
        };
        assert_eq!(
            serde_json::to_value(&res).unwrap(),
            json!({"requestId":"r","decision":"allow"})
        );
        let hook = HookEventPayload {
            agent_id: None,
            session_id: "s".into(),
            hook_event_name: "Stop".into(),
            tool_name: None,
            received_at: 1,
        };
        assert_eq!(
            serde_json::to_value(&hook).unwrap(),
            json!({"agentId":null,"sessionId":"s","hookEventName":"Stop","toolName":null,"receivedAt":1})
        );
    }
}
