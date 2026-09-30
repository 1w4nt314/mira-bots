//! Names and payloads of the Tauri events emitted from Rust to the frontend.
//! All payloads are camelCase on the wire.

use serde::Serialize;

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
