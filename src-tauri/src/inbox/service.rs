//! [`InboxService`]: the inbox document in memory; every mutation saves atomically and rolls
//! back on a failed save (plan6c punkt 3). Meant to sit behind a `std::sync::Mutex`
//! (`TicketsCtx::inbox`).
//!
//! Rules (plan A.1, research risk 11):
//! - dedup on `(kind, external_id)`;
//! - a fetch changes the content only of items in state `new`; it refreshes `seenAt` of every
//!   listed item and, when the fetch is `complete`, marks unlisted `new`/`dismissed` items of
//!   that source `gone` (a listed `gone` item comes back);
//! - `started` items are never changed by a fetch (the file moved to `started/`, the issue is
//!   the ticket's now) and never pruned;
//! - [`InboxService::prune`]: `gone` + `new` at once, `gone` + `dismissed` after 30 days.

use std::collections::HashSet;
use std::io;

use super::external::{clean_external_title, clean_label, clean_login, sanitize_external_body};
use super::source::{Fetched, FetchedItem, SourceId};
use super::store::InboxStore;
use super::{ExternalKind, InboxDoc, InboxItem, InboxItemSummary, InboxState};
use crate::config::{
    INBOX_BODY_MAX_CHARS, INBOX_DISMISSED_KEEP_MS, INBOX_ITEM_GONE, INBOX_LABELS_MAX,
};

/// Inbox errors (Danish: commands pass them to the UI).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InboxError {
    /// Unknown id, or the source no longer lists the item.
    #[error("{INBOX_ITEM_GONE}")]
    Gone,
    /// The item is not in the state the action needs.
    #[error("Emnet er allerede startet eller afvist")]
    NotNew,
    #[error("Emnet er ikke afvist")]
    NotDismissed,
    #[error(
        "Indbakke-filen kunne ikke læses ved opstart; ændringer er slået fra. Genstart appen."
    )]
    ReadOnly,
    #[error("Kunne ikke gemme indbakken: {0}")]
    Io(String),
}

impl From<InboxError> for String {
    fn from(e: InboxError) -> Self {
        e.to_string()
    }
}

/// What [`InboxService::apply`] changed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub added: usize,
    pub updated: usize,
    pub gone: usize,
}

pub struct InboxService {
    store: Box<dyn InboxStore>,
    doc: InboxDoc,
    /// The store could not be read at startup: nothing is saved over the unread file.
    read_only: bool,
}

/// The cleaned content of a fetched item: `(title, body, labels, author, notes)`.
type Cleaned = (
    String,
    Option<String>,
    Vec<String>,
    Option<String>,
    Vec<String>,
);

/// Cleans a fetched item's texts again (sources clean too; cleaning is idempotent, so notes
/// only appear when something was left).
fn clean(f: &FetchedItem) -> Cleaned {
    let mut notes = f.notes.clone();
    let body = f.body.as_deref().map(|b| {
        let s = sanitize_external_body(b, INBOX_BODY_MAX_CHARS);
        notes.extend(s.notes);
        s.text
    });
    let labels = f
        .labels
        .iter()
        .filter_map(|l| clean_label(l))
        .take(INBOX_LABELS_MAX)
        .collect();
    (
        clean_external_title(&f.title),
        body,
        labels,
        f.author.as_deref().map(clean_login),
        notes,
    )
}

/// Overwrites the content of `it` with `f`; `true` when anything changed.
fn update_from(it: &mut InboxItem, f: &FetchedItem) -> bool {
    let (title, body, labels, author, notes) = clean(f);
    let before = it.clone();
    it.title = title;
    it.body = body;
    it.labels = labels;
    it.author = author;
    it.notes = notes;
    it.url = f.url.clone();
    it.number = f.number;
    it.repo = f.repo.clone();
    it.path = f.path.clone();
    it.project = f.project.clone();
    it.candidates = f.candidates.clone();
    it.updated_at = f.updated_at.clone();
    it.fingerprint = f.fingerprint.clone();
    *it != before
}

fn new_item(source: &SourceId, f: &FetchedItem, now: u64) -> InboxItem {
    let mut it = InboxItem {
        id: uuid::Uuid::new_v4().to_string(),
        kind: source.kind,
        external_id: f.external_id.clone(),
        source_id: source.key.clone(),
        title: String::new(),
        body: None,
        labels: Vec::new(),
        url: None,
        number: None,
        repo: None,
        path: None,
        author: None,
        project: None,
        candidates: Vec::new(),
        updated_at: None,
        fingerprint: None,
        seen_at: now,
        state: InboxState::New,
        ticket_id: None,
        gone: false,
        moved: None,
        notes: Vec::new(),
    };
    update_from(&mut it, f);
    it
}

fn find_mut<'a>(doc: &'a mut InboxDoc, id: &str) -> Result<&'a mut InboxItem, InboxError> {
    doc.items
        .iter_mut()
        .find(|i| i.id == id)
        .ok_or(InboxError::Gone)
}

impl InboxService {
    pub fn new(store: Box<dyn InboxStore>, doc: InboxDoc) -> Self {
        InboxService {
            store,
            doc,
            read_only: false,
        }
    }

    /// Loads the store. A missing file is an empty inbox; a corrupt or unknown-version file was
    /// quarantined by the store (warning); any other read error starts read-only (warning).
    pub fn load(store: Box<dyn InboxStore>) -> (Self, Option<String>) {
        match store.load() {
            Ok(r) => (InboxService::new(store, r.doc), r.warning),
            Err(e) => {
                log::error!("inbox: load failed, starting read-only: {e}");
                let mut svc = InboxService::new(store, InboxDoc::default());
                svc.read_only = true;
                let warning = format!(
                    "inbox.json kunne ikke læses ved opstart ({e}); ændringer er slået fra. \
                     Genstart appen."
                );
                (svc, Some(warning))
            }
        }
    }

    /// Runs `f` on the document and saves; any error restores the document.
    fn commit<T>(
        &mut self,
        f: impl FnOnce(&mut InboxDoc) -> Result<T, InboxError>,
    ) -> Result<T, InboxError> {
        if self.read_only {
            return Err(InboxError::ReadOnly);
        }
        let before = self.doc.clone();
        let result = f(&mut self.doc).and_then(|v| {
            self.store
                .save(&self.doc)
                .map(|()| v)
                .map_err(|e: io::Error| {
                    log::error!("inbox: save failed: {e}");
                    InboxError::Io(e.to_string())
                })
        });
        if result.is_err() {
            self.doc = before;
        }
        result
    }

    // ---- reading ----

    pub fn len(&self) -> usize {
        self.doc.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.doc.items.is_empty()
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// One item with its body.
    pub fn get(&self, id: &str) -> Option<InboxItem> {
        self.doc.items.iter().find(|i| i.id == id).cloned()
    }

    /// The item for `(kind, external_id)`.
    pub fn find_by_external(&self, kind: ExternalKind, external_id: &str) -> Option<&InboxItem> {
        self.doc
            .items
            .iter()
            .find(|i| i.kind == kind && i.external_id == external_id)
    }

    /// Every item the sources still list (not `gone`), without bodies, in insertion order.
    pub fn list(&self) -> Vec<InboxItemSummary> {
        self.doc
            .items
            .iter()
            .filter(|i| !i.gone)
            .map(InboxItemSummary::from)
            .collect()
    }

    /// Items not started, not dismissed and not gone (Diagnostics' "Nye emner").
    pub fn new_count(&self) -> usize {
        self.doc
            .items
            .iter()
            .filter(|i| i.state == InboxState::New && !i.gone)
            .count()
    }

    // ---- mutations ----

    /// Merges one fetch of `source` (see the module rules). Items of another kind than the
    /// source are ignored.
    pub fn apply(
        &mut self,
        source: &SourceId,
        fetched: Fetched,
        now: u64,
    ) -> Result<Applied, InboxError> {
        self.commit(|doc| {
            let mut applied = Applied::default();
            let mut listed: HashSet<String> = HashSet::new();
            for f in &fetched.items {
                if !listed.insert(f.external_id.clone()) {
                    continue; // the same item twice in one fetch: the first wins
                }
                let existing = doc
                    .items
                    .iter_mut()
                    .find(|i| i.kind == source.kind && i.external_id == f.external_id);
                match existing {
                    Some(it) => {
                        it.seen_at = now;
                        match it.state {
                            InboxState::New => {
                                let came_back = std::mem::take(&mut it.gone);
                                if update_from(it, f) || came_back {
                                    applied.updated += 1;
                                }
                            }
                            InboxState::Dismissed => it.gone = false,
                            InboxState::Started => {}
                        }
                    }
                    None => {
                        doc.items.push(new_item(source, f, now));
                        applied.added += 1;
                    }
                }
            }
            if fetched.complete {
                for it in doc.items.iter_mut().filter(|i| {
                    i.source_id == source.key
                        && !i.gone
                        && i.state != InboxState::Started
                        && !listed.contains(&i.external_id)
                }) {
                    it.gone = true;
                    applied.gone += 1;
                }
            }
            Ok(applied)
        })
    }

    /// Start (plan punkt 5): a `new` item becomes `started` with its ticket. Idempotent for the
    /// same ticket.
    pub fn mark_started(&mut self, id: &str, ticket_id: &str) -> Result<InboxItem, InboxError> {
        self.commit(|doc| {
            let it = find_mut(doc, id)?;
            match (it.state, it.ticket_id.as_deref()) {
                (InboxState::Started, Some(t)) if t == ticket_id => {}
                (InboxState::New, _) if !it.gone => {
                    it.state = InboxState::Started;
                    it.ticket_id = Some(ticket_id.to_string());
                }
                _ => return Err(InboxError::NotNew),
            }
            Ok(it.clone())
        })
    }

    /// "Afvis": a `new` item becomes `dismissed` (idempotent).
    pub fn dismiss(&mut self, id: &str) -> Result<InboxItem, InboxError> {
        self.commit(|doc| {
            let it = find_mut(doc, id)?;
            match it.state {
                InboxState::New => it.state = InboxState::Dismissed,
                InboxState::Dismissed => {}
                InboxState::Started => return Err(InboxError::NotNew),
            }
            Ok(it.clone())
        })
    }

    /// "Fortryd": a dismissed item the source still lists becomes `new` again.
    pub fn undismiss(&mut self, id: &str) -> Result<InboxItem, InboxError> {
        self.commit(|doc| {
            let it = find_mut(doc, id)?;
            if it.gone {
                return Err(InboxError::Gone);
            }
            if it.state != InboxState::Dismissed {
                return Err(InboxError::NotDismissed);
            }
            it.state = InboxState::New;
            Ok(it.clone())
        })
    }

    /// Whether the started/done file was moved (folder items).
    pub fn set_moved(&mut self, id: &str, moved: bool) -> Result<InboxItem, InboxError> {
        self.commit(|doc| {
            let it = find_mut(doc, id)?;
            it.moved = Some(moved);
            Ok(it.clone())
        })
    }

    /// Repairs items whose ticket exists (Start saved the ticket but not the inbox, plan A.1):
    /// every item with one of the `external_id`s becomes `started` with that ticket. Saves only
    /// when something changed; returns how many items changed.
    pub fn reconcile(&mut self, started: &[(String, String)]) -> Result<usize, InboxError> {
        let due = |i: &InboxItem| {
            started.iter().find(|(ext, ticket)| {
                *ext == i.external_id
                    && (i.state != InboxState::Started || i.ticket_id.as_ref() != Some(ticket))
            })
        };
        if !self.doc.items.iter().any(|i| due(i).is_some()) {
            return Ok(0);
        }
        self.commit(|doc| {
            let mut n = 0;
            for it in doc.items.iter_mut() {
                if let Some((_, ticket)) = due(it) {
                    it.state = InboxState::Started;
                    it.ticket_id = Some(ticket.clone());
                    n += 1;
                }
            }
            Ok(n)
        })
    }

    /// Forgets `gone` items: `new` at once, `dismissed` when last listed more than 30 days ago.
    /// Saves only when something was removed; returns how many.
    pub fn prune(&mut self, now: u64) -> Result<usize, InboxError> {
        let drop = |i: &InboxItem| {
            i.gone
                && match i.state {
                    InboxState::New => true,
                    InboxState::Dismissed => {
                        now.saturating_sub(i.seen_at) > INBOX_DISMISSED_KEEP_MS
                    }
                    InboxState::Started => false,
                }
        };
        let n = self.doc.items.iter().filter(|i| drop(i)).count();
        if n == 0 {
            return Ok(0);
        }
        self.commit(|doc| {
            doc.items.retain(|i| !drop(i));
            Ok(n)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::MemoryInboxStore;
    use super::super::test_support::github_item;
    use super::*;

    fn svc() -> (InboxService, MemoryInboxStore) {
        let store = MemoryInboxStore::new();
        (
            InboxService::new(Box::new(store.clone()), InboxDoc::default()),
            store,
        )
    }

    fn gh(n: u64, title: &str) -> FetchedItem {
        FetchedItem {
            external_id: format!("github:o/r#{n}"),
            title: title.into(),
            labels: vec!["bug".into()],
            url: Some(format!("https://github.com/o/r/issues/{n}")),
            number: Some(n),
            repo: Some("o/r".into()),
            author: Some("alice".into()),
            project: Some("web".into()),
            ..FetchedItem::default()
        }
    }

    fn fetched(items: Vec<FetchedItem>, complete: bool) -> Fetched {
        Fetched {
            items,
            complete,
            ..Fetched::default()
        }
    }

    fn id_of(s: &InboxService, ext: &str) -> String {
        s.doc
            .items
            .iter()
            .find(|i| i.external_id == ext)
            .unwrap()
            .id
            .clone()
    }

    #[test]
    fn apply_dedups_on_kind_and_external_id() {
        let (mut s, store) = svc();
        let src = SourceId::github("O/R");
        assert_eq!(src.key, "github:o/r");
        let a = s
            .apply(
                &src,
                fetched(vec![gh(1, "A"), gh(2, "B"), gh(1, "dup")], true),
                10,
            )
            .unwrap();
        assert_eq!(
            a,
            Applied {
                added: 2,
                updated: 0,
                gone: 0
            }
        );
        let a = s
            .apply(&src, fetched(vec![gh(1, "A"), gh(2, "B")], true), 20)
            .unwrap();
        assert_eq!(a, Applied::default());
        assert_eq!(s.len(), 2);
        assert_eq!(store.doc().unwrap().items.len(), 2);
        let it = s.get(&id_of(&s, "github:o/r#1")).unwrap();
        assert_eq!(
            (it.title.as_str(), it.seen_at, it.source_id.as_str()),
            ("A", 20, "github:o/r")
        );
        // The same external id from a folder source is another item (other kind).
        let folder = SourceId::folder(Some("web"));
        let mut f = gh(1, "file");
        f.number = None;
        s.apply(&folder, fetched(vec![f], true), 30).unwrap();
        assert_eq!(s.len(), 3);
        assert!(s
            .find_by_external(ExternalKind::Folder, "github:o/r#1")
            .is_some());
    }

    #[test]
    fn apply_cleans_texts_again() {
        let (mut s, _) = svc();
        let mut f = gh(1, "Fix\u{200B}\nlogin");
        f.labels = vec!["".into(), "a\u{E0041}".into()];
        f.author = Some("bad login".into());
        f.body = Some("x<!-- hide -->y".into());
        s.apply(&SourceId::folder(None), fetched(vec![f], true), 1)
            .unwrap();
        let it = &s.doc.items[0];
        assert_eq!(it.title, "Fix login");
        assert_eq!(it.labels, vec!["a".to_string()]);
        assert_eq!(it.author.as_deref(), Some("ukendt"));
        assert_eq!(it.body.as_deref(), Some("xy"));
        assert_eq!(it.notes, vec!["1 HTML-kommentar(er) fjernet".to_string()]);
        assert_eq!(it.source_id, "folder:_rod");
    }

    #[test]
    fn apply_updates_only_new_items() {
        let (mut s, _) = svc();
        let src = SourceId::github("o/r");
        s.apply(
            &src,
            fetched(vec![gh(1, "A"), gh(2, "B"), gh(3, "C")], true),
            1,
        )
        .unwrap();
        let (i2, i3) = (id_of(&s, "github:o/r#2"), id_of(&s, "github:o/r#3"));
        s.mark_started(&i2, "t2").unwrap();
        s.dismiss(&i3).unwrap();
        let a = s
            .apply(
                &src,
                fetched(vec![gh(1, "A2"), gh(2, "B2"), gh(3, "C2")], true),
                5,
            )
            .unwrap();
        assert_eq!(
            a,
            Applied {
                added: 0,
                updated: 1,
                gone: 0
            }
        );
        assert_eq!(s.get(&id_of(&s, "github:o/r#1")).unwrap().title, "A2");
        let started = s.get(&i2).unwrap();
        assert_eq!(
            (started.title.as_str(), started.state),
            ("B", InboxState::Started)
        );
        let dismissed = s.get(&i3).unwrap();
        assert_eq!((dismissed.title.as_str(), dismissed.seen_at), ("C", 5));
    }

    #[test]
    fn apply_marks_gone_only_when_complete() {
        let (mut s, _) = svc();
        let src = SourceId::github("o/r");
        s.apply(
            &src,
            fetched(vec![gh(1, "A"), gh(2, "B"), gh(3, "C")], true),
            1,
        )
        .unwrap();
        let (i2, i3) = (id_of(&s, "github:o/r#2"), id_of(&s, "github:o/r#3"));
        s.mark_started(&i2, "t2").unwrap();
        s.dismiss(&i3).unwrap();
        // Incomplete (capped) fetch without #1–#3: nothing is gone.
        let a = s.apply(&src, fetched(vec![], false), 2).unwrap();
        assert_eq!(a.gone, 0);
        // Another source's complete fetch does not touch this source's items.
        s.apply(&SourceId::github("x/y"), fetched(vec![], true), 3)
            .unwrap();
        assert_eq!(s.list().len(), 3);
        // Complete fetch: the new and the dismissed item are gone, the started one is kept.
        let a = s.apply(&src, fetched(vec![], true), 4).unwrap();
        assert_eq!(a.gone, 2);
        let ids: Vec<String> = s.list().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![i2.clone()]);
        assert!(s.get(&i3).unwrap().gone);
        // Listed again: back (the new one with an update).
        let a = s
            .apply(&src, fetched(vec![gh(1, "A"), gh(3, "C")], true), 5)
            .unwrap();
        assert_eq!(
            a,
            Applied {
                added: 0,
                updated: 1,
                gone: 0
            }
        );
        assert_eq!(s.list().len(), 3);
        assert_eq!(s.new_count(), 1);
    }

    #[test]
    fn prune_rules() {
        let (mut s, store) = svc();
        let src = SourceId::github("o/r");
        s.apply(
            &src,
            fetched(vec![gh(1, "A"), gh(2, "B"), gh(3, "C")], true),
            1_000,
        )
        .unwrap();
        let (i1, i2, i3) = (
            id_of(&s, "github:o/r#1"),
            id_of(&s, "github:o/r#2"),
            id_of(&s, "github:o/r#3"),
        );
        s.mark_started(&i2, "t2").unwrap();
        s.dismiss(&i3).unwrap();
        // Nothing gone: nothing pruned, nothing saved.
        let saves = store.saves();
        assert_eq!(s.prune(10_000_000_000).unwrap(), 0);
        assert_eq!(store.saves(), saves);
        s.apply(&src, fetched(vec![], true), 2_000).unwrap();
        // gone + new: at once; gone + dismissed: kept for 30 days; started: never.
        assert_eq!(s.prune(2_000).unwrap(), 1);
        assert!(s.get(&i1).is_none());
        // The dismissed item was last listed at 1 000.
        assert_eq!(s.prune(1_000 + INBOX_DISMISSED_KEEP_MS).unwrap(), 0);
        assert_eq!(s.prune(1_001 + INBOX_DISMISSED_KEEP_MS).unwrap(), 1);
        assert!(s.get(&i3).is_none());
        assert!(s.get(&i2).is_some());
        assert_eq!(store.doc().unwrap().items.len(), 1);
    }

    #[test]
    fn undismiss_refused_when_gone() {
        let (mut s, _) = svc();
        let src = SourceId::github("o/r");
        s.apply(&src, fetched(vec![gh(1, "A")], true), 1).unwrap();
        let i1 = id_of(&s, "github:o/r#1");
        assert_eq!(s.undismiss(&i1), Err(InboxError::NotDismissed));
        s.dismiss(&i1).unwrap();
        s.dismiss(&i1).unwrap();
        assert_eq!(s.undismiss(&i1).unwrap().state, InboxState::New);
        s.dismiss(&i1).unwrap();
        s.apply(&src, fetched(vec![], true), 2).unwrap();
        assert_eq!(s.undismiss(&i1), Err(InboxError::Gone));
        assert_eq!(
            String::from(s.undismiss(&i1).unwrap_err()),
            "Emnet er ikke længere i indbakken"
        );
        assert_eq!(s.dismiss("nope"), Err(InboxError::Gone));
    }

    #[test]
    fn mark_started_set_moved_and_reconcile() {
        let (mut s, store) = svc();
        let src = SourceId::github("o/r");
        s.apply(&src, fetched(vec![gh(1, "A"), gh(2, "B")], true), 1)
            .unwrap();
        let (i1, i2) = (id_of(&s, "github:o/r#1"), id_of(&s, "github:o/r#2"));
        assert_eq!(
            s.mark_started(&i1, "t1").unwrap().ticket_id.as_deref(),
            Some("t1")
        );
        assert!(s.mark_started(&i1, "t1").is_ok(), "idempotent");
        assert_eq!(s.mark_started(&i1, "t9"), Err(InboxError::NotNew));
        assert_eq!(s.dismiss(&i1), Err(InboxError::NotNew));
        assert_eq!(s.set_moved(&i1, false).unwrap().moved, Some(false));
        // Reconcile: #2 has a ticket already (an interrupted Start); #1 is consistent.
        let saves = store.saves();
        let pairs = vec![
            ("github:o/r#1".to_string(), "t1".to_string()),
            ("github:o/r#2".to_string(), "t2".to_string()),
        ];
        assert_eq!(s.reconcile(&pairs).unwrap(), 1);
        let it = s.get(&i2).unwrap();
        assert_eq!(
            (it.state, it.ticket_id.as_deref()),
            (InboxState::Started, Some("t2"))
        );
        assert_eq!(s.reconcile(&pairs).unwrap(), 0);
        assert_eq!(store.saves(), saves + 1);
    }

    #[test]
    fn failed_save_rolls_back_and_read_only_never_saves() {
        let (mut s, store) = svc();
        let src = SourceId::github("o/r");
        store.fail_next_save();
        assert!(matches!(
            s.apply(&src, fetched(vec![gh(1, "A")], true), 1),
            Err(InboxError::Io(_))
        ));
        assert_eq!(s.len(), 0);
        let ro = MemoryInboxStore::new();
        ro.fail_load();
        let (mut s, w) = InboxService::load(Box::new(ro.clone()));
        assert!(s.is_read_only());
        assert!(w.unwrap().contains("ændringer er slået fra"));
        assert_eq!(
            s.apply(&src, fetched(vec![gh(1, "A")], true), 1),
            Err(InboxError::ReadOnly)
        );
        assert_eq!(ro.saves(), 0);
    }

    #[test]
    fn list_hides_gone_and_bodies() {
        let mut doc = InboxDoc::default();
        let mut gone = github_item("g", 9);
        gone.gone = true;
        doc.items = vec![github_item("a", 1), gone];
        let s = InboxService::new(Box::new(MemoryInboxStore::new()), doc);
        let l = s.list();
        assert_eq!(l.len(), 1);
        assert_eq!((l[0].id.as_str(), l[0].has_body), ("a", false));
        assert_eq!(s.new_count(), 1);
    }
}
