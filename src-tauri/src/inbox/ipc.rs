//! The inbox's command cores on [`TicketsCtx`] (step 6c, plan punkt 11): the Tauri commands in
//! `commands.rs` are thin wrappers. Also the `inbox-changed` payload ([`InboxPayload`]) with the
//! Start dialog's duplicate warning (`duplicateOf`).

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::external::clean_external_title;
use super::refresh::{refresh, RefreshReason};
use super::source::InboxStatus;
use super::start::{start_item, StartRequest};
use super::{DuplicateRef, InboxItem, InboxItemSummary, InboxState};
use crate::events::INBOX_CHANGED;
use crate::tickets::model::{short_id, TicketState, TicketSummary};
use crate::tickets::prompt::one_line;
use crate::tickets::service::norm_title;
use crate::tickets::TicketsCtx;

/// `get_inbox` and the `inbox-changed` event (C6c.2): every item the sources still list
/// (without bodies) and the status per source.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InboxPayload {
    pub items: Vec<InboxItemSummary>,
    pub status: InboxStatus,
}

/// The duplicate key of a project (`""`: none), case-folded like `projects::same_id`.
fn project_key(p: Option<&str>) -> String {
    p.map(str::to_lowercase).unwrap_or_default()
}

/// Fills `duplicate_of` of the `new` items: the oldest open ticket in the same project whose
/// normalised title equals the item's (C6c.5 "Ligner ticket …"; the same rule as
/// [`super::start::duplicate_hint`], one pass over the tickets).
fn fill_duplicates(items: &mut [InboxItemSummary], tickets: &[TicketSummary]) {
    if !items.iter().any(|i| i.state == InboxState::New) {
        return;
    }
    let mut open: HashMap<(String, String), &TicketSummary> = HashMap::new();
    for t in tickets.iter().filter(|t| t.state != TicketState::Done) {
        let key = (
            project_key(t.project.as_ref().map(|p| p.name())),
            norm_title(&t.title),
        );
        open.entry(key)
            .and_modify(|o| {
                if t.created_at < o.created_at {
                    *o = t;
                }
            })
            .or_insert(t);
    }
    for it in items.iter_mut().filter(|i| i.state == InboxState::New) {
        let key = (
            project_key(it.project.as_deref()),
            norm_title(&clean_external_title(&it.title)),
        );
        it.duplicate_of = open.get(&key).map(|t| DuplicateRef {
            short_id: short_id(&t.id),
            title: one_line(&t.title),
        });
    }
}

impl TicketsCtx {
    /// The inbox as the UI sees it. Takes the inbox document's lock, then the service lock
    /// (each briefly, one after the other).
    pub fn inbox_payload(&self) -> InboxPayload {
        let mut items = self.inbox_read(|i| i.list());
        let tickets = self.read(|s| s.list());
        fill_duplicates(&mut items, &tickets);
        InboxPayload {
            items,
            status: self.inbox_rt.status(),
        }
    }

    /// Emits `inbox-changed` with [`Self::inbox_payload`]. Never called with a lock held.
    pub fn emit_inbox(&self) {
        match serde_json::to_value(self.inbox_payload()) {
            Ok(v) => (self.emit)(INBOX_CHANGED, v),
            Err(e) => log::error!("serialize {INBOX_CHANGED}: {e}"),
        }
    }

    /// `get_inbox_item`: one item with its body.
    pub fn inbox_item(&self, id: &str) -> Result<InboxItem, String> {
        self.inbox_read(|i| i.get(id))
            .filter(|i| !i.gone)
            .ok_or_else(|| super::InboxError::Gone.into())
    }

    /// `dismiss_inbox_item` ("Afvis"): under `inbox_lock`, so it never races a Start.
    pub fn inbox_dismiss(&self, id: &str) -> Result<InboxItemSummary, String> {
        let _serial = self.lock_inbox_serial();
        self.inbox_mutate(|i| i.dismiss(id))
            .map(|it| InboxItemSummary::from(&it))
    }

    /// `undismiss_inbox_item` ("Fortryd").
    pub fn inbox_undismiss(&self, id: &str) -> Result<InboxItemSummary, String> {
        let _serial = self.lock_inbox_serial();
        self.inbox_mutate(|i| i.undismiss(id))
            .map(|it| InboxItemSummary::from(&it))
    }

    /// `refresh_inbox`: `Ok(false)` when a refresh is already running.
    pub fn inbox_refresh(self: &Arc<Self>, reason: RefreshReason) -> Result<bool, String> {
        refresh(self, reason)
    }

    /// `start_inbox_item` (blocking: file moves; the command runs it in `spawn_blocking`).
    pub fn inbox_start(self: &Arc<Self>, req: StartRequest) -> Result<TicketSummary, String> {
        start_item(self, req)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{folder_item, github_item, FolderEnv};
    use super::*;
    use crate::projects::ProjectRef;
    use serde_json::json;

    #[test]
    fn payload_lists_items_with_duplicate_of_and_status() {
        let mut gone = folder_item("i3", "c.md");
        gone.gone = true;
        let env = FolderEnv::new(vec![folder_item("i1", "a.md"), github_item("i2", 7), gone]);
        let ctx = &env.t.ctx;
        let web = ProjectRef::Existing("Web".into());
        let older = ctx
            .mutate(|s| s.create_in("fejl  i LOGIN", "", false, Some(web.clone()), None, 1))
            .unwrap();
        ctx.mutate(|s| s.create_in("Fejl i login", "", false, Some(web), None, 2))
            .unwrap();
        // Same title in another project, and a Done ticket, are no duplicates.
        ctx.mutate(|s| s.create("Issue 7", "", false, 1)).unwrap();
        let p = ctx.inbox_payload();
        assert_eq!(p.items.len(), 2, "gone items are not listed");
        assert_eq!(
            p.items[0].duplicate_of,
            Some(DuplicateRef {
                short_id: older.short_id(),
                title: older.title.clone()
            })
        );
        assert_eq!(p.items[1].duplicate_of, None);
        assert_eq!(p.status, InboxStatus::default());
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(
            v["status"],
            json!({"refreshing":false,"lastRefreshAt":null,"sources":[]})
        );
        assert_eq!(
            v["items"][0]["duplicateOf"]["shortId"],
            json!(older.short_id())
        );
    }

    #[test]
    fn item_dismiss_undismiss_emit_inbox_changed() {
        let env = FolderEnv::new(vec![folder_item("i1", "a.md")]);
        let ctx = &env.t.ctx;
        assert_eq!(
            ctx.inbox_item("i1").unwrap().body.as_deref(),
            Some("Trin 1")
        );
        assert_eq!(
            ctx.inbox_item("nej").unwrap_err(),
            "Emnet er ikke længere i indbakken"
        );
        env.t.clear();
        let d = ctx.inbox_dismiss("i1").unwrap();
        assert_eq!(d.state, InboxState::Dismissed);
        let ev = env.t.emitted(INBOX_CHANGED);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0]["items"][0]["state"], json!("dismissed"));
        assert_eq!(ev[0]["status"]["refreshing"], json!(false));
        assert!(ev[0]["status"]["sources"].is_array());
        assert_eq!(ctx.inbox_undismiss("i1").unwrap().state, InboxState::New);
        assert_eq!(
            ctx.inbox_undismiss("i1").unwrap_err(),
            "Emnet er ikke afvist"
        );
        assert_eq!(env.t.emitted(INBOX_CHANGED).len(), 2, "no emit on error");
    }

    #[test]
    fn refresh_and_start_through_the_context() {
        let env = FolderEnv::new(Vec::new());
        env.write("a.md", "# A\nB");
        let ctx = &env.t.ctx;
        assert_eq!(ctx.inbox_refresh(RefreshReason::Manual), Ok(true));
        ctx.join_inbox_threads();
        let item = env.item("a.md").unwrap();
        let s = ctx
            .inbox_start(StartRequest {
                item_id: item.id.clone(),
                kind: Some("bug".into()),
                project: None,
                skip_review: true,
            })
            .unwrap();
        assert_eq!((s.title.as_str(), s.kind.as_deref()), ("A", Some("bug")));
        assert_eq!(ctx.inbox_item(&item.id).unwrap().state, InboxState::Started);
        // The request's wire form (C6c.6).
        let req: StartRequest = serde_json::from_value(
            json!({"itemId":"x","kind":null,"project":"web","skipReview":true}),
        )
        .unwrap();
        assert_eq!(req.project, Some(ProjectRef::Existing("web".into())));
        let r: RefreshReason = serde_json::from_value(json!("manual")).unwrap();
        assert_eq!(r, RefreshReason::Manual);
    }
}
