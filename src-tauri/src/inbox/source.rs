//! Inbox sources (step 6c, plan punkt 7): what a source hands the inbox ([`Fetched`]), the
//! [`Source`] trait (folder: [`super::folder::FolderSource`]; GitHub: B3), each source's status
//! as the UI sees it ([`SourceStatus`], [`InboxStatus`]) and the back-off after failed fetches
//! ([`next_retry`]).
//!
//! [`InboxService::apply`]: super::InboxService::apply

use serde::{Deserialize, Serialize};

use super::ExternalKind;
use crate::config::INBOX_BACKOFF_MS;

/// A source of inbox items: `folder:_rod`, `folder:<project>`, `github:<owner/name lowercase>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SourceId {
    pub kind: ExternalKind,
    /// The whole id (also stored as the items' `sourceId`).
    pub key: String,
}

impl SourceId {
    /// The inbox folder of `project` (`None`: the projects root).
    pub fn folder(project: Option<&str>) -> Self {
        SourceId {
            kind: ExternalKind::Folder,
            key: format!("folder:{}", project.unwrap_or("_rod")),
        }
    }

    /// The issues of `repo` (`owner/name`, compared lowercase).
    pub fn github(repo: &str) -> Self {
        SourceId {
            kind: ExternalKind::Github,
            key: format!("github:{}", repo.to_ascii_lowercase()),
        }
    }
}

/// One item as a source lists it. Texts may be raw: [`super::InboxService::apply`] cleans
/// them again before storing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FetchedItem {
    pub external_id: String,
    pub title: String,
    /// Folder items only (GitHub lists have no body, plan A.1).
    pub body: Option<String>,
    pub labels: Vec<String>,
    pub url: Option<String>,
    pub number: Option<u64>,
    /// `owner/name` (GitHub; B1 addition to the plan's field list).
    pub repo: Option<String>,
    /// Path relative to the inbox folder (folder; B1 addition to the plan's field list).
    pub path: Option<String>,
    pub author: Option<String>,
    pub updated_at: Option<String>,
    pub fingerprint: Option<String>,
    pub project: Option<String>,
    pub candidates: Vec<String>,
    pub notes: Vec<String>,
}

/// The result of one fetch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fetched {
    pub items: Vec<FetchedItem>,
    /// The list is the whole source: items it does not contain are gone.
    pub complete: bool,
    /// The source had more than it returned (GitHub limit 100, folder 200 files).
    pub capped: bool,
    pub notes: Vec<String>,
}

/// A source of inbox items (C6c.1). `fetch` runs on the refresh thread without any lock held.
pub trait Source: Send + Sync {
    fn id(&self) -> SourceId;
    /// Shown in the status list ("mappen inbox/", "owner/name").
    fn label(&self) -> String;
    /// Smallest interval between two fetches (except a manual refresh).
    fn min_interval_ms(&self) -> u64;
    fn fetch(&self) -> Result<Fetched, SourceError>;
    /// The project the source belongs to (`None`: the projects root, or several projects).
    fn project(&self) -> Option<String> {
        None
    }
}

/// Why a fetch failed. Wire (`errorKind`): camelCase.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SourceErrorKind {
    /// The inbox folder could not be read.
    Folder,
    GhMissing,
    NotLoggedIn,
    BadCredentials,
    RepoNotFound,
    RateLimited,
    IssuesDisabled,
    Network,
    Timeout,
    TooLarge,
    BadJson,
    Other,
    /// The refresh itself failed (a panic, or `inbox.json` could not be saved).
    Internal,
}

/// When a failed source is tried again (plan A.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retry {
    /// Automatically, after the back-off ([`INBOX_BACKOFF_MS`]).
    Backoff,
    /// Only after a manual refresh ("Opdatér"): the user has to act first.
    Manual,
}

impl SourceErrorKind {
    /// `NotLoggedIn`/`GhMissing`/`BadCredentials`/`RepoNotFound` need the user (plan A.5);
    /// everything else backs off.
    pub fn retry(self) -> Retry {
        match self {
            SourceErrorKind::GhMissing
            | SourceErrorKind::NotLoggedIn
            | SourceErrorKind::BadCredentials
            | SourceErrorKind::RepoNotFound => Retry::Manual,
            _ => Retry::Backoff,
        }
    }
}

/// A failed fetch: the kind, the Danish text for the status list (C6c.5) and when to retry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceError {
    pub kind: SourceErrorKind,
    pub text: String,
    pub retry: Retry,
}

impl SourceError {
    pub fn new(kind: SourceErrorKind, text: impl Into<String>) -> Self {
        SourceError {
            kind,
            text: text.into(),
            retry: kind.retry(),
        }
    }
}

/// When a source that failed `fails` times in a row (≥ 1) may be fetched again: `None` for
/// the kinds that wait for a manual refresh; `RateLimited` waits the longest step at once;
/// otherwise 120 → 240 → 480 → 900 s ([`INBOX_BACKOFF_MS`]).
pub fn next_retry(fails: u32, now: u64, kind: SourceErrorKind) -> Option<u64> {
    if kind.retry() == Retry::Manual {
        return None;
    }
    let last = INBOX_BACKOFF_MS.len() - 1;
    let step = if kind == SourceErrorKind::RateLimited {
        last
    } else {
        (fails.max(1) as usize - 1).min(last)
    };
    Some(now + INBOX_BACKOFF_MS[step])
}

/// One source in the inbox status (C6c.2 `InboxSourceStatus`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SourceStatus {
    pub id: String,
    pub kind: ExternalKind,
    pub label: String,
    pub project: Option<String>,
    /// Unix ms of the last successful fetch.
    pub last_fetch_at: Option<u64>,
    pub ok: bool,
    pub error: Option<String>,
    pub error_kind: Option<SourceErrorKind>,
    /// Unix ms before which the timer and focus do not fetch (back-off).
    pub next_retry_at: Option<u64>,
    /// Items the last successful fetch listed.
    pub items: usize,
    /// The source had more than it returned (folder: 200 files; GitHub: 100 issues).
    pub capped: bool,
    /// The last fetch's notes (B2 addition to C6c.2: a skipped big file, the file cap).
    #[serde(default)]
    pub notes: Vec<String>,
    /// Failed fetches in a row (back-off step).
    #[serde(skip)]
    pub fails: u32,
    /// Unix ms of the last fetch attempt (the minimum interval).
    #[serde(skip)]
    pub last_attempt_at: Option<u64>,
}

impl SourceStatus {
    /// A source not fetched yet.
    pub fn new(id: &SourceId, label: String, project: Option<String>) -> Self {
        SourceStatus {
            id: id.key.clone(),
            kind: id.kind,
            label,
            project,
            last_fetch_at: None,
            ok: true,
            error: None,
            error_kind: None,
            next_retry_at: None,
            items: 0,
            capped: false,
            notes: Vec::new(),
            fails: 0,
            last_attempt_at: None,
        }
    }

    /// Whether the source is fetched now (plan A.5): a manual refresh always; otherwise not
    /// while it waits for the user (a [`Retry::Manual`] error), not before `next_retry_at`
    /// (back-off) and not within `min_interval_ms` of the last attempt.
    pub fn due(&self, now: u64, min_interval_ms: u64, manual: bool) -> bool {
        if manual {
            return true;
        }
        if !self.ok && self.error_kind.map(SourceErrorKind::retry) == Some(Retry::Manual) {
            return false;
        }
        if self.next_retry_at.is_some_and(|t| now < t) {
            return false;
        }
        !self
            .last_attempt_at
            .is_some_and(|a| now < a.saturating_add(min_interval_ms))
    }

    /// A successful fetch at `now`.
    pub fn succeeded(&mut self, now: u64, fetched: &Fetched) {
        self.last_fetch_at = Some(now);
        self.ok = true;
        self.error = None;
        self.error_kind = None;
        self.next_retry_at = None;
        self.items = fetched.items.len();
        self.capped = fetched.capped;
        self.notes = fetched.notes.clone();
        self.fails = 0;
    }

    /// A failed fetch at `now` (the back-off grows with each failure in a row).
    pub fn failed(&mut self, now: u64, e: &SourceError) {
        self.fails = self.fails.saturating_add(1);
        self.ok = false;
        self.error = Some(e.text.clone());
        self.error_kind = Some(e.kind);
        self.next_retry_at = next_retry(self.fails, now, e.kind);
    }
}

/// The inbox status (C6c.2 `InboxPayload.status`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InboxStatus {
    pub refreshing: bool,
    /// Unix ms when the last refresh finished.
    pub last_refresh_at: Option<u64>,
    pub sources: Vec<SourceStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn backoff_table() {
        let k = SourceErrorKind::Network;
        assert_eq!(next_retry(1, 1_000, k), Some(121_000));
        assert_eq!(next_retry(2, 1_000, k), Some(241_000));
        assert_eq!(next_retry(3, 1_000, k), Some(481_000));
        assert_eq!(next_retry(4, 1_000, k), Some(901_000));
        assert_eq!(next_retry(9, 1_000, k), Some(901_000));
        assert_eq!(next_retry(0, 1_000, k), Some(121_000));
        assert_eq!(
            next_retry(1, 1_000, SourceErrorKind::RateLimited),
            Some(901_000)
        );
        assert_eq!(
            next_retry(1, 0, SourceErrorKind::Folder),
            Some(INBOX_BACKOFF_MS[0])
        );
    }

    #[test]
    fn manual_retry_kinds_never_schedule() {
        for k in [
            SourceErrorKind::GhMissing,
            SourceErrorKind::NotLoggedIn,
            SourceErrorKind::BadCredentials,
            SourceErrorKind::RepoNotFound,
        ] {
            assert_eq!(k.retry(), Retry::Manual);
            assert_eq!(next_retry(1, 5, k), None);
            let id = SourceId::github("o/r");
            let mut st = SourceStatus::new(&id, "o/r".into(), None);
            st.failed(5, &SourceError::new(k, "x"));
            assert!(!st.due(10_000_000, 120_000, false), "{k:?}");
            assert!(st.due(10_000_000, 120_000, true), "{k:?}");
        }
        assert_eq!(SourceErrorKind::Timeout.retry(), Retry::Backoff);
    }

    #[test]
    fn status_due_min_interval_and_backoff() {
        let id = SourceId::folder(None);
        let mut st = SourceStatus::new(&id, "mappen inbox/".into(), None);
        assert!(st.due(0, 60_000, false));
        st.last_attempt_at = Some(1_000);
        st.succeeded(1_000, &Fetched::default());
        assert!(!st.due(60_999, 60_000, false));
        assert!(st.due(61_000, 60_000, false));
        assert!(st.due(2_000, 60_000, true));
        st.failed(61_000, &SourceError::new(SourceErrorKind::Folder, "nej"));
        st.last_attempt_at = Some(61_000);
        assert_eq!(st.next_retry_at, Some(181_000));
        assert!(!st.due(130_000, 60_000, false));
        assert!(st.due(181_000, 60_000, false));
        st.failed(181_000, &SourceError::new(SourceErrorKind::Folder, "nej"));
        assert_eq!((st.fails, st.next_retry_at), (2, Some(421_000)));
        st.succeeded(500_000, &Fetched::default());
        assert_eq!((st.fails, st.next_retry_at, st.ok), (0, None, true));
    }

    #[test]
    fn status_wire_format() {
        let id = SourceId::github("Owner/Name");
        let mut st = SourceStatus::new(&id, "Owner/Name".into(), Some("web".into()));
        st.failed(
            10,
            &SourceError::new(SourceErrorKind::NotLoggedIn, "ikke logget ind"),
        );
        let v = serde_json::to_value(InboxStatus {
            refreshing: false,
            last_refresh_at: None,
            sources: vec![st],
        })
        .unwrap();
        assert_eq!(
            v,
            json!({"refreshing":false,"lastRefreshAt":null,"sources":[{
                "id":"github:owner/name","kind":"github","label":"Owner/Name","project":"web",
                "lastFetchAt":null,"ok":false,"error":"ikke logget ind","errorKind":"notLoggedIn",
                "nextRetryAt":null,"items":0,"capped":false,"notes":[]}]})
        );
    }
}
