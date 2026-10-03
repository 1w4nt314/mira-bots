//! What a source hands the inbox (step 6c). B1 has only the data types [`InboxService::apply`]
//! needs; the `Source` trait, status and back-off come with the sources (B2).
//!
//! [`InboxService::apply`]: super::InboxService::apply

use super::ExternalKind;

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
