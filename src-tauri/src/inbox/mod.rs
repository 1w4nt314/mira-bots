//! Step 6c: the inbox. Items from external sources (a folder of `.md`/`.txt` files, GitHub
//! issues via `gh`) wait here until the user starts one as a ticket (plan6c A.1).
//!
//! Inbox items are not tickets: they live in their own document `<app_data>/inbox.json`
//! ([`InboxDoc`], same store pattern as `tickets.json`). Dedup is on `(kind, external_id)`.
//! A fetch only changes items in state `new`; the ticket started from an item carries an
//! [`crate::tickets::model::ExternalRef`].
//!
//! Locks (plan A.1): `TicketsCtx::inbox_lock` is taken before, never inside, the service lock;
//! the inbox document's own mutex is held only around one [`InboxService`] call. `gh` and file
//! system work never run under any of them.

pub mod external;
pub mod folder;
pub mod github;
pub mod ipc;
pub mod refresh;
pub mod service;
pub mod source;
pub mod start;
pub mod store;
pub mod write_back;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{INBOX_SCHEMA_VERSION, INBOX_TMP_DIR};
pub use crate::tickets::model::ExternalKind;
pub use ipc::InboxPayload;
pub use refresh::{InboxRuntime, RefreshReason};
pub use service::{Applied, InboxError, InboxService};
pub use source::{InboxStatus, Source, SourceStatus};
pub use start::StartRequest;
pub use store::{load_inbox, InboxStore, JsonInboxStore, MemoryInboxStore};

/// Startup: removes `<app_data>/tmp/wb-*` (write-back body files a crash left behind; the
/// folder itself stays). Returns how many were removed.
pub fn clean_tmp(data_dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(data_dir.join(INBOX_TMP_DIR)) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with("wb-"))
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| std::fs::remove_file(e.path()).is_ok())
        .count()
}

/// Where an inbox item stands. Wire: `"new"|"started"|"dismissed"`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum InboxState {
    New,
    Started,
    Dismissed,
}

/// One external item (C6c.2). All texts are cleaned (`external`) before they are stored.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InboxItem {
    /// uuid v4 (the app's own id).
    pub id: String,
    pub kind: ExternalKind,
    /// `github:owner/name#123` or `folder:<project|_rod>:<relative path>`.
    pub external_id: String,
    /// The source that lists the item (`github:owner/name`, `folder:<project|_rod>`).
    pub source_id: String,
    pub title: String,
    /// Folder items only; a GitHub body is fetched at Start and lives only on the ticket.
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub number: Option<u64>,
    #[serde(default)]
    pub repo: Option<String>,
    /// Path relative to the inbox folder (`/`-separated).
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    /// The project id the item belongs to (`None`: the Start dialog asks).
    #[serde(default)]
    pub project: Option<String>,
    /// Several projects share the repo: the Start dialog offers these.
    #[serde(default)]
    pub candidates: Vec<String>,
    /// The source's own update time (GitHub `updatedAt`).
    #[serde(default)]
    pub updated_at: Option<String>,
    /// Skips re-reading an unchanged file (`"<mtime_ms>:<len>"`) or issue (`updatedAt`).
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// Unix ms when a fetch last listed the item.
    pub seen_at: u64,
    pub state: InboxState,
    #[serde(default)]
    pub ticket_id: Option<String>,
    /// The source no longer lists it (only set by a complete fetch).
    #[serde(default)]
    pub gone: bool,
    /// Folder items: whether the file was moved after Start (`None`: not tried).
    #[serde(default)]
    pub moved: Option<bool>,
    /// Sanitising/parsing notes.
    #[serde(default)]
    pub notes: Vec<String>,
    /// The ticket type a folder file asks for (frontmatter `kind:`, validated: `bug`, `feature`
    /// or a playbook; `None` = task or none). Only preselects the Start dialog (never starts
    /// anything). `kind` is the source kind, hence the name (wire `ticketKind`).
    #[serde(default)]
    pub ticket_kind: Option<String>,
}

/// `inbox.json`: `{"schemaVersion":1,"items":[…]}`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InboxDoc {
    pub schema_version: u32,
    pub items: Vec<InboxItem>,
}

impl Default for InboxDoc {
    fn default() -> Self {
        InboxDoc {
            schema_version: INBOX_SCHEMA_VERSION,
            items: Vec::new(),
        }
    }
}

/// An open ticket the item looks like (the Start dialog's warning).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateRef {
    pub short_id: String,
    pub title: String,
}

/// An item without its body (C6c.6 `InboxItemSummary`; the list in `inbox-changed`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InboxItemSummary {
    pub id: String,
    pub kind: ExternalKind,
    pub external_id: String,
    pub source_id: String,
    pub title: String,
    pub has_body: bool,
    pub labels: Vec<String>,
    pub url: Option<String>,
    pub number: Option<u64>,
    pub repo: Option<String>,
    pub path: Option<String>,
    pub author: Option<String>,
    pub project: Option<String>,
    pub candidates: Vec<String>,
    pub updated_at: Option<String>,
    pub seen_at: u64,
    pub state: InboxState,
    pub ticket_id: Option<String>,
    pub notes: Vec<String>,
    /// Frontmatter `kind:` of a folder file (validated), to preselect the Start dialog.
    pub ticket_kind: Option<String>,
    /// Filled by the caller that knows the tickets (B2); `None` from [`InboxService::list`].
    pub duplicate_of: Option<DuplicateRef>,
}

impl From<&InboxItem> for InboxItemSummary {
    fn from(i: &InboxItem) -> Self {
        InboxItemSummary {
            id: i.id.clone(),
            kind: i.kind,
            external_id: i.external_id.clone(),
            source_id: i.source_id.clone(),
            title: i.title.clone(),
            has_body: i.body.as_deref().is_some_and(|b| !b.trim().is_empty()),
            labels: i.labels.clone(),
            url: i.url.clone(),
            number: i.number,
            repo: i.repo.clone(),
            path: i.path.clone(),
            author: i.author.clone(),
            project: i.project.clone(),
            candidates: i.candidates.clone(),
            updated_at: i.updated_at.clone(),
            seen_at: i.seen_at,
            state: i.state,
            ticket_id: i.ticket_id.clone(),
            notes: i.notes.clone(),
            ticket_kind: i.ticket_kind.clone(),
            duplicate_of: None,
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::agent::AgentManager;
    use crate::tickets::test_support::{test_ctx_with_gh, TestCtx};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// A ticket context whose projects root exists, with project `web` and its inbox folder
    /// `web/.mira-bots/inbox/` (the root's `inbox/` is not created). Removed on drop.
    pub struct FolderEnv {
        pub t: TestCtx,
        pub root: PathBuf,
        pub store: MemoryInboxStore,
    }

    impl FolderEnv {
        pub fn new(items: Vec<InboxItem>) -> Self {
            Self::with_gh(items, Arc::new(crate::gh::fake::FakeGh::new()))
        }

        /// [`Self::new`] with a scripted `gh` (B3).
        pub fn with_gh(items: Vec<InboxItem>, gh: Arc<dyn crate::gh::GhRunner>) -> Self {
            let store = MemoryInboxStore::new();
            let doc = InboxDoc {
                items,
                ..InboxDoc::default()
            };
            let inbox = InboxService::new(Box::new(store.clone()), doc);
            let t = test_ctx_with_gh(Arc::new(Mutex::new(AgentManager::new(5))), gh, inbox);
            let root = t.ctx.workspace.root().to_path_buf();
            std::fs::create_dir_all(folder::project_inbox_dir(&root.join("web"))).unwrap();
            FolderEnv { t, root, store }
        }

        /// Writes `web/.mira-bots/project.json`.
        pub fn project_json(&self, text: &str) {
            let p = crate::checks::project_file_path(&self.root.join("web"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }

        /// `web/.mira-bots/inbox/`.
        pub fn web_inbox(&self) -> PathBuf {
            folder::project_inbox_dir(&self.root.join("web"))
        }

        /// Writes `name` into the web inbox.
        pub fn write(&self, name: &str, text: &str) -> PathBuf {
            let p = self.web_inbox().join(name);
            std::fs::write(&p, text).unwrap();
            p
        }

        /// The item for `folder:web:<name>`.
        pub fn item(&self, name: &str) -> Option<InboxItem> {
            let id = self.t.ctx.inbox_read(|i| {
                i.find_by_external(ExternalKind::Folder, &format!("folder:web:{name}"))
                    .map(|i| i.id.clone())
            })?;
            self.t.ctx.inbox_read(|i| i.get(&id))
        }
    }

    impl Drop for FolderEnv {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A new GitHub item for issue `#n` in `o/r` (project `web`).
    pub fn github_item(id: &str, n: u64) -> InboxItem {
        InboxItem {
            id: id.to_string(),
            kind: ExternalKind::Github,
            external_id: format!("github:o/r#{n}"),
            source_id: "github:o/r".into(),
            title: format!("Issue {n}"),
            body: None,
            labels: vec!["bug".into()],
            url: Some(format!("https://github.com/o/r/issues/{n}")),
            number: Some(n),
            repo: Some("o/r".into()),
            path: None,
            author: Some("alice".into()),
            project: Some("web".into()),
            candidates: Vec::new(),
            updated_at: Some("2026-10-01T10:00:00Z".into()),
            fingerprint: Some("2026-10-01T10:00:00Z".into()),
            seen_at: 1_000,
            state: InboxState::New,
            ticket_id: None,
            gone: false,
            moved: None,
            notes: Vec::new(),
            ticket_kind: None,
        }
    }

    /// A new folder item `path` in project `web` with a body.
    pub fn folder_item(id: &str, path: &str) -> InboxItem {
        InboxItem {
            id: id.to_string(),
            kind: ExternalKind::Folder,
            external_id: format!("folder:web:{path}"),
            source_id: "folder:web".into(),
            title: "Fejl i login".into(),
            body: Some("Trin 1".into()),
            labels: Vec::new(),
            url: None,
            number: None,
            repo: None,
            path: Some(path.to_string()),
            author: None,
            project: Some("web".into()),
            candidates: Vec::new(),
            updated_at: None,
            fingerprint: Some("5:6".into()),
            seen_at: 1_000,
            state: InboxState::New,
            ticket_id: None,
            gone: false,
            moved: None,
            notes: Vec::new(),
            ticket_kind: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use serde_json::json;

    #[test]
    fn item_and_summary_wire_format() {
        let item = github_item("u1", 123);
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(
            v,
            json!({"id":"u1","kind":"github","externalId":"github:o/r#123","sourceId":"github:o/r",
                "title":"Issue 123","body":null,"labels":["bug"],
                "url":"https://github.com/o/r/issues/123","number":123,"repo":"o/r","path":null,
                "author":"alice","project":"web","candidates":[],
                "updatedAt":"2026-10-01T10:00:00Z","fingerprint":"2026-10-01T10:00:00Z",
                "seenAt":1000,"state":"new","ticketId":null,"gone":false,"moved":null,"notes":[],
                "ticketKind":null})
        );
        assert_eq!(serde_json::from_value::<InboxItem>(v).unwrap(), item);
        for (st, wire) in [
            (InboxState::New, "new"),
            (InboxState::Started, "started"),
            (InboxState::Dismissed, "dismissed"),
        ] {
            assert_eq!(serde_json::to_value(st).unwrap(), json!(wire));
        }
        let s = serde_json::to_value(InboxItemSummary::from(&folder_item("u2", "a.md"))).unwrap();
        assert_eq!(s["hasBody"], json!(true));
        assert_eq!(s["duplicateOf"], json!(null));
        assert_eq!(s["kind"], json!("folder"));
        for k in ["body", "fingerprint", "gone", "moved"] {
            assert!(s.get(k).is_none(), "{k}");
        }
        assert_eq!(
            serde_json::to_value(InboxDoc::default()).unwrap(),
            json!({"schemaVersion":1,"items":[]})
        );
    }

    #[test]
    fn clean_tmp_removes_only_write_back_files() {
        let dir = std::env::temp_dir().join(format!("mira-tmp-{}", uuid::Uuid::new_v4()));
        assert_eq!(clean_tmp(&dir), 0);
        let tmp = dir.join(INBOX_TMP_DIR);
        std::fs::create_dir_all(tmp.join("wb-dir")).unwrap();
        std::fs::write(tmp.join("wb-ab12cd34-1.md"), "x").unwrap();
        std::fs::write(tmp.join("andet.md"), "x").unwrap();
        assert_eq!(clean_tmp(&dir), 1);
        assert!(tmp.join("andet.md").exists() && tmp.join("wb-dir").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
