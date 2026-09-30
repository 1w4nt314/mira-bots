//! PermissionRequests waiting for a UI answer. Lives in `Arc<std::sync::Mutex<PendingPermissions>>`.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::oneshot;

/// The app's answer to one PermissionRequest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    /// No decision: the hook prints nothing and the terminal shows its own dialog.
    None,
}

impl Decision {
    /// Wire value in the pipe reply and in `permission-resolved`.
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::None => "none",
        }
    }
}

/// Wire shape of a pending request (C.2 `PermissionRequestInfo`), camelCase.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequestInfo {
    /// uuid v4; only used between app and UI (the pipe connection itself is the correlation).
    pub request_id: String,
    pub agent_id: String,
    pub agent_name: String,
    pub tool_name: String,
    pub summary: String,
    /// Trimmed tool input as received from the hook.
    pub tool_input: Value,
    /// Unix ms.
    pub created_at: u64,
    /// Unix ms; after this the app answers `none`.
    pub deadline_at: u64,
}

pub struct PendingPermission {
    pub info: PermissionRequestInfo,
    tx: oneshot::Sender<Decision>,
}

#[derive(Default)]
pub struct PendingPermissions {
    map: HashMap<String, PendingPermission>,
    ui_ready: bool,
}

impl PendingPermissions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a request; the receiver yields the decision passed to [`Self::resolve`].
    pub fn insert(&mut self, info: PermissionRequestInfo) -> oneshot::Receiver<Decision> {
        let (tx, rx) = oneshot::channel();
        self.map
            .insert(info.request_id.clone(), PendingPermission { info, tx });
        rx
    }

    /// Removes the request and delivers `decision` to the waiting pipe handler.
    /// `None` if the id is unknown or already resolved/expired.
    pub fn resolve(
        &mut self,
        request_id: &str,
        decision: Decision,
    ) -> Option<PermissionRequestInfo> {
        let p = self.map.remove(request_id)?;
        // The handler may have given up already (deadline); that is fine.
        let _ = p.tx.send(decision);
        Some(p.info)
    }

    /// Pending requests, oldest first.
    pub fn list(&self) -> Vec<PermissionRequestInfo> {
        let mut v: Vec<_> = self.map.values().map(|p| p.info.clone()).collect();
        v.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.request_id.cmp(&b.request_id))
        });
        v
    }

    /// Called by the `ui_ready` command once the island listens for `permission-request`.
    pub fn set_ui_ready(&mut self) {
        self.ui_ready = true;
    }

    pub fn is_ui_ready(&self) -> bool {
        self.ui_ready
    }

    /// Resolves every request of `agent_id` with `Decision::None` (agent stopped/removed).
    /// Returns the resolved requests so the caller can emit `permission-resolved`.
    pub fn remove_for_agent(&mut self, agent_id: &str) -> Vec<PermissionRequestInfo> {
        let ids: Vec<String> = self
            .map
            .values()
            .filter(|p| p.info.agent_id == agent_id)
            .map(|p| p.info.request_id.clone())
            .collect();
        ids.iter()
            .filter_map(|id| self.resolve(id, Decision::None))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info(id: &str, agent: &str, created: u64) -> PermissionRequestInfo {
        PermissionRequestInfo {
            request_id: id.into(),
            agent_id: agent.into(),
            agent_name: "demo".into(),
            tool_name: "Bash".into(),
            summary: "npm test".into(),
            tool_input: json!({"command":"npm test"}),
            created_at: created,
            deadline_at: created + 108_000,
        }
    }

    #[test]
    fn resolve_delivers_decision_once() {
        let mut p = PendingPermissions::new();
        let mut rx = p.insert(info("r1", "a", 1));
        assert_eq!(p.list().len(), 1);
        assert_eq!(p.resolve("r1", Decision::Allow).unwrap().request_id, "r1");
        assert_eq!(rx.try_recv().unwrap(), Decision::Allow);
        assert!(p.resolve("r1", Decision::Deny).is_none());
        assert!(p.list().is_empty());
    }

    #[test]
    fn resolve_after_receiver_dropped_is_harmless() {
        let mut p = PendingPermissions::new();
        drop(p.insert(info("r1", "a", 1)));
        assert!(p.resolve("r1", Decision::Deny).is_some());
    }

    #[test]
    fn ui_ready_flag() {
        let mut p = PendingPermissions::new();
        assert!(!p.is_ui_ready());
        p.set_ui_ready();
        assert!(p.is_ui_ready());
    }

    #[test]
    fn remove_for_agent_resolves_with_none() {
        let mut p = PendingPermissions::new();
        let mut rx1 = p.insert(info("r1", "a", 2));
        let mut rx2 = p.insert(info("r2", "b", 1));
        let mut rx3 = p.insert(info("r3", "a", 3));
        let mut gone: Vec<_> = p
            .remove_for_agent("a")
            .into_iter()
            .map(|i| i.request_id)
            .collect();
        gone.sort();
        assert_eq!(gone, ["r1", "r3"]);
        assert_eq!(rx1.try_recv().unwrap(), Decision::None);
        assert_eq!(rx3.try_recv().unwrap(), Decision::None);
        assert!(rx2.try_recv().is_err());
        assert_eq!(p.list()[0].request_id, "r2");
    }

    #[test]
    fn list_is_oldest_first_and_camel_case() {
        let mut p = PendingPermissions::new();
        let _a = p.insert(info("late", "a", 5));
        let _b = p.insert(info("early", "a", 1));
        let l = p.list();
        assert_eq!(l[0].request_id, "early");
        let v = serde_json::to_value(&l[0]).unwrap();
        for key in [
            "requestId",
            "agentId",
            "agentName",
            "toolName",
            "summary",
            "toolInput",
            "createdAt",
            "deadlineAt",
        ] {
            assert!(v.get(key).is_some(), "{key}");
        }
        assert_eq!(Decision::Allow.as_str(), "allow");
        assert_eq!(Decision::Deny.as_str(), "deny");
        assert_eq!(Decision::None.as_str(), "none");
    }
}
