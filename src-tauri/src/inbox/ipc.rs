//! The inbox's command cores on [`TicketsCtx`] (step 6c, plan punkt 11): the Tauri commands in
//! `commands.rs` are thin wrappers. Also the `inbox-changed` payload ([`InboxPayload`]) with the
//! Start dialog's duplicate warning (`duplicateOf`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::external::clean_external_title;
use super::folder::inbox_dirs;
use super::github::build_github_sources;
use super::refresh::{refresh, RefreshReason};
use super::source::InboxStatus;
use super::start::{start_item, StartRequest};
use super::write_back::write_back;
use super::Source;
use super::{DuplicateRef, InboxItem, InboxItemSummary, InboxState};
use crate::checks::GITHUB_IGNORED_PREFIX;
use crate::config::INBOX_NO_URL;
use crate::diagnostics::InboxSourceDiag;
use crate::events::INBOX_CHANGED;
use crate::gh::{check_auth, issue_url_ok, GhAuthResult};
use crate::tickets::model::{short_id, TicketState, TicketSummary};
use crate::tickets::model::{ExternalKind, WriteBack};
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

    /// `start_inbox_item` (blocking: file moves, `gh issue view`; the command runs it in
    /// `spawn_blocking`).
    pub fn inbox_start(self: &Arc<Self>, req: StartRequest) -> Result<TicketSummary, String> {
        start_item(self, req)
    }

    /// `retry_write_back` ("Prøv igen"; blocking: `gh`/file work).
    pub fn inbox_retry_write_back(self: &Arc<Self>, ticket_id: &str) -> Result<WriteBack, String> {
        write_back(self, ticket_id)
    }

    /// `open_inbox_url`: the GitHub address stored on inbox item `id`, else on ticket `id`'s
    /// `external` — only when it is exactly the issue's `https://github.com/<repo>/issues/<n>`.
    /// Never a URL from the caller.
    pub fn inbox_url(&self, id: &str) -> Result<String, String> {
        let from_item = self
            .inbox_read(|i| i.get(id))
            .and_then(|it| Some((it.url?, it.repo?, it.number?)));
        let found = from_item.or_else(|| {
            self.read(|s| s.get(id))
                .and_then(|t| t.external)
                .and_then(|e| Some((e.url?, e.repo?, e.number?)))
        });
        match found {
            Some((url, repo, n)) if issue_url_ok(&url, &repo, n) => Ok(url),
            _ => Err(INBOX_NO_URL.into()),
        }
    }

    /// `check_gh_auth` (blocking, 15 s; only on a click).
    pub fn inbox_check_gh_auth(&self) -> GhAuthResult {
        check_auth(&*self.gh)
    }

    /// Diagnostik's `inboxSources`: every folder source and every GitHub source of the
    /// projects (one row per project), with the last fetch's status; a project whose
    /// `project.json` `github` is ignored gets a row with that note as its error. Reads folders
    /// and (cached) project files; never runs `gh`.
    pub fn inbox_sources_diag(&self) -> Vec<InboxSourceDiag> {
        let status = self.inbox_rt.status();
        let row = |id: &str, kind, label: String, project: Option<String>| {
            let st = status.sources.iter().find(|s| s.id == id);
            InboxSourceDiag {
                project,
                kind,
                label,
                last_fetch_at: st.and_then(|s| s.last_fetch_at),
                error: st.and_then(|s| s.error.clone()),
                items: st.map_or(0, |s| s.items),
            }
        };
        let root = self.workspace.root();
        let projects = crate::projects::list_projects(root);
        let kinds = self.workspace.config().playbook_kinds();
        let mut out: Vec<InboxSourceDiag> = inbox_dirs(root, &projects, &kinds)
            .iter()
            .map(|f| row(&f.id().key, ExternalKind::Folder, f.label(), f.project()))
            .collect();
        let mut github = Vec::new();
        for p in &projects {
            if let Ok(Some(f)) = self.project_files.read(Path::new(&p.path)) {
                for n in f
                    .notes
                    .iter()
                    .filter(|n| n.starts_with(GITHUB_IGNORED_PREFIX))
                {
                    out.push(InboxSourceDiag {
                        project: Some(p.id.clone()),
                        kind: ExternalKind::Github,
                        label: "project.json".into(),
                        last_fetch_at: None,
                        error: Some(n.clone()),
                        items: 0,
                    });
                }
                if let Some(g) = f.github {
                    github.push((p.id.clone(), g));
                }
            }
        }
        for s in build_github_sources(&self.gh, &github) {
            let id = s.id().key;
            for p in &s.projects {
                out.push(row(&id, ExternalKind::Github, s.label(), Some(p.clone())));
            }
        }
        out
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

    #[test]
    fn open_inbox_url_rejects_foreign_host() {
        let mut evil = github_item("g2", 8);
        evil.url = Some("https://evil.example/o/r/issues/8".into());
        let mut other = github_item("g3", 9);
        other.url = Some("https://github.com/o/r/issues/10".into());
        let env = FolderEnv::new(vec![
            github_item("g1", 7),
            evil,
            other,
            folder_item("f1", "a.md"),
        ]);
        let ctx = &env.t.ctx;
        assert_eq!(
            ctx.inbox_url("g1").unwrap(),
            "https://github.com/o/r/issues/7"
        );
        for id in ["g2", "g3", "f1", "findes-ikke"] {
            assert_eq!(ctx.inbox_url(id).unwrap_err(), INBOX_NO_URL, "{id}");
        }
        // A ticket from GitHub: its stored address; a forged one is refused.
        let t = ctx
            .mutate(|s| {
                s.create_external(
                    "G",
                    "",
                    true,
                    None,
                    None,
                    crate::tickets::model::test_support::github_ref(4),
                    1,
                )
            })
            .unwrap();
        assert_eq!(
            ctx.inbox_url(&t.id).unwrap(),
            "https://github.com/o/r/issues/4"
        );
        let mut forged = crate::tickets::model::test_support::github_ref(5);
        forged.external_id = "github:o/r#5".into();
        forged.url = Some("javascript:alert(1)".into());
        let t = ctx
            .mutate(|s| s.create_external("H", "", true, None, None, forged, 1))
            .unwrap();
        assert_eq!(ctx.inbox_url(&t.id).unwrap_err(), INBOX_NO_URL);
    }

    #[test]
    fn diag_lists_sources_per_project_with_ignored_github() {
        let env = FolderEnv::new(Vec::new());
        env.project_json(r#"{"github": {"repo": "o/r"}}"#);
        let api = env.root.join("api");
        let pj = crate::checks::project_file_path(&api);
        std::fs::create_dir_all(pj.parent().unwrap()).unwrap();
        std::fs::write(&pj, r#"{"github": {"repo": "ikke gyldig"}}"#).unwrap();
        let rows = env.t.ctx.inbox_sources_diag();
        let v = serde_json::to_value(&rows).unwrap();
        assert_eq!(
            v,
            json!([
                {"project": "web", "kind": "folder", "label": rows[0].label, "lastFetchAt": null,
                 "error": null, "items": 0},
                {"project": "api", "kind": "github", "label": "project.json", "lastFetchAt": null,
                 "error": "project.json: github ignoreres: repo «ikke gyldig» skal have formen ejer/navn",
                 "items": 0},
                {"project": "web", "kind": "github", "label": "o/r", "lastFetchAt": null,
                 "error": null, "items": 0}
            ])
        );
        assert!(env
            .t
            .ctx
            .inbox_check_gh_auth()
            .text
            .starts_with("gh: FakeGh"));
    }
}
