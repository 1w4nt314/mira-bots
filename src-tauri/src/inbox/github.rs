//! The GitHub issues source (step 6c, plan punkt 14, C6c.3): `gh issue list` without bodies
//! (one page, at most 100; a body is fetched at Start with `gh issue view`), parsed into
//! [`FetchedItem`]s. Every text from GitHub is foreign: titles, labels and logins are cleaned,
//! a URL is kept only when it is exactly the issue's `https://github.com/…` address, and issues
//! that are not `OPEN` are dropped. Projects sharing a repo (and labels) share one source and one
//! call; their items get no project but the projects as candidates.

use std::sync::Arc;

use serde_json::Value;

use super::external::{clean_external_title, clean_label, clean_login};
use super::source::{Fetched, FetchedItem, Source, SourceError, SourceErrorKind, SourceId};
use crate::checks::GithubConfig;
use crate::config::{INBOX_GITHUB_LIMIT, INBOX_GITHUB_MIN_INTERVAL_MS, INBOX_LABELS_MAX};
use crate::gh::{error_text, issue_url_ok, valid_repo, GhCall, GhError, GhRunner};

/// The fields of the list (C6c.3; no `body`).
pub const LIST_FIELDS: &str = "number,title,labels,url,updatedAt,state,author";
/// The fields of one issue at Start.
pub const VIEW_FIELDS: &str =
    "number,title,body,labels,url,updatedAt,state,stateReason,closedAt,author";

/// The issues of one repo (with labels: all of them) for one or more projects.
pub struct GithubSource {
    pub gh: Arc<dyn GhRunner>,
    /// `owner/name` as the first project wrote it (validated).
    pub repo: String,
    /// Sorted, without duplicates.
    pub labels: Vec<String>,
    /// Every project with this repo and labels.
    pub projects: Vec<String>,
}

/// The source id: `github:<owner/name lowercase>`, with labels `github:<…>[a,b]` (two
/// projects with different labels on one repo are two sources).
pub fn source_id(repo: &str, labels: &[String]) -> SourceId {
    let mut id = SourceId::github(repo);
    if !labels.is_empty() {
        id.key = format!("{}[{}]", id.key, labels.join(","));
    }
    id
}

/// `github:<owner/name lowercase>#<n>`.
pub fn external_id(repo: &str, number: u64) -> String {
    format!("github:{}#{number}", repo.to_ascii_lowercase())
}

/// The `gh issue list` arguments (C6c.3, verbatim).
pub fn list_args(repo: &str, labels: &[String]) -> Vec<String> {
    let mut a: Vec<String> = ["issue", "list", "--repo", repo, "--state", "open"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for l in labels {
        a.push("--label".into());
        a.push(l.clone());
    }
    a.extend([
        "--limit".to_string(),
        INBOX_GITHUB_LIMIT.to_string(),
        "--json".into(),
        LIST_FIELDS.into(),
    ]);
    a
}

/// The `gh issue view` arguments of one issue.
pub fn view_args(repo: &str, number: u64, fields: &str) -> Vec<String> {
    vec![
        "issue".into(),
        "view".into(),
        number.to_string(),
        "--repo".into(),
        repo.into(),
        "--json".into(),
        fields.into(),
    ]
}

fn str_field<'a>(o: &'a serde_json::Map<String, Value>, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Value::as_str)
}

/// `labels` as gh exports it: a flat list of `{name, …}`; cleaned, at most
/// [`INBOX_LABELS_MAX`].
fn labels_of(o: &serde_json::Map<String, Value>) -> Vec<String> {
    o.get("labels")
        .and_then(Value::as_array)
        .map(|l| {
            l.iter()
                .filter_map(|x| x.get("name").and_then(Value::as_str))
                .filter_map(clean_label)
                .take(INBOX_LABELS_MAX)
                .collect()
        })
        .unwrap_or_default()
}

/// `author.login` (a deleted account is `null`): cleaned.
fn author_of(o: &serde_json::Map<String, Value>) -> Option<String> {
    o.get("author")
        .and_then(|a| a.get("login"))
        .and_then(Value::as_str)
        .map(clean_login)
}

fn url_of(o: &serde_json::Map<String, Value>, repo: &str, n: u64) -> Option<String> {
    str_field(o, "url")
        .filter(|u| issue_url_ok(u, repo, n))
        .map(str::to_string)
}

/// Parses `gh issue list --json …` (C6c.1): a JSON list; entries without a positive `number`
/// and issues whose `state` is not `OPEN` are skipped. Not a list → `BadJson`.
pub fn parse_issue_list(stdout: &str, repo: &str) -> Result<Vec<FetchedItem>, GhError> {
    let v: Value =
        serde_json::from_str(stdout.trim()).map_err(|e| GhError::BadJson(e.to_string()))?;
    let Value::Array(list) = v else {
        return Err(GhError::BadJson("not a list".into()));
    };
    Ok(list
        .iter()
        .filter_map(Value::as_object)
        .filter(|o| str_field(o, "state") == Some("OPEN"))
        .filter_map(|o| {
            let n = o.get("number").and_then(Value::as_u64).filter(|n| *n > 0)?;
            let updated = str_field(o, "updatedAt").map(str::to_string);
            Some(FetchedItem {
                external_id: external_id(repo, n),
                title: clean_external_title(str_field(o, "title").unwrap_or_default()),
                body: None,
                labels: labels_of(o),
                url: url_of(o, repo, n),
                number: Some(n),
                repo: Some(repo.to_string()),
                path: None,
                author: author_of(o),
                fingerprint: updated.clone(),
                updated_at: updated,
                ..FetchedItem::default()
            })
        })
        .collect())
}

/// One issue at Start (`gh issue view`; texts cleaned except the body, which the caller
/// sanitises).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IssueView {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub url: Option<String>,
    /// `OPEN` | `CLOSED`.
    pub state: String,
    pub author: Option<String>,
    pub updated_at: Option<String>,
}

/// `gh issue view <n> --repo <repo> --json …` (C6c.3).
pub fn fetch_issue(gh: &dyn GhRunner, repo: &str, n: u64) -> Result<IssueView, GhError> {
    if n == 0 || !valid_repo(repo) {
        return Err(GhError::Other("ugyldigt issue".into()));
    }
    let out = gh.run(&GhCall::new(view_args(repo, n, VIEW_FIELDS)))?;
    let v: Value =
        serde_json::from_str(out.stdout.trim()).map_err(|e| GhError::BadJson(e.to_string()))?;
    let Value::Object(o) = v else {
        return Err(GhError::BadJson("not an object".into()));
    };
    Ok(IssueView {
        title: clean_external_title(str_field(&o, "title").unwrap_or_default()),
        body: str_field(&o, "body").unwrap_or_default().to_string(),
        labels: labels_of(&o),
        url: url_of(&o, repo, n),
        state: str_field(&o, "state").unwrap_or_default().to_string(),
        author: author_of(&o),
        updated_at: str_field(&o, "updatedAt").map(str::to_string),
    })
}

/// Before a retry posts again (plan A.7): the URL (or `""`) of an existing comment on issue `n`
/// whose body contains `marker`, from `gh issue view <n> --repo <repo> --json comments` (the
/// first 100 comments). `Ok(None)`: not there.
pub fn find_marker_comment(
    gh: &dyn GhRunner,
    repo: &str,
    n: u64,
    marker: &str,
) -> Result<Option<String>, GhError> {
    let out = gh.run(&GhCall::new(view_args(repo, n, "comments")))?;
    let v: Value =
        serde_json::from_str(out.stdout.trim()).map_err(|e| GhError::BadJson(e.to_string()))?;
    let comments = v
        .get("comments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(comments
        .iter()
        .find(|c| {
            c.get("body")
                .and_then(Value::as_str)
                .is_some_and(|b| b.contains(marker))
        })
        .map(|c| {
            c.get("url")
                .and_then(Value::as_str)
                .filter(|u| u.starts_with("https://github.com/"))
                .unwrap_or_default()
                .to_string()
        }))
}

/// A failed fetch as the status list shows it (C6c.5). RateLimited gets its clock time when
/// the back-off is known ([`super::source::SourceStatus::failed`]).
pub fn source_error(e: &GhError, repo: &str) -> SourceError {
    let kind = match e {
        GhError::GhMissing => SourceErrorKind::GhMissing,
        GhError::Timeout => SourceErrorKind::Timeout,
        GhError::NotLoggedIn => SourceErrorKind::NotLoggedIn,
        GhError::BadCredentials => SourceErrorKind::BadCredentials,
        GhError::RepoNotFound => SourceErrorKind::RepoNotFound,
        GhError::RateLimited => SourceErrorKind::RateLimited,
        GhError::IssuesDisabled => SourceErrorKind::IssuesDisabled,
        GhError::Network => SourceErrorKind::Network,
        GhError::TooLarge => SourceErrorKind::TooLarge,
        GhError::BadJson(_) => SourceErrorKind::BadJson,
        GhError::Other(_) => SourceErrorKind::Other,
    };
    SourceError::new(kind, error_text(e, repo))
}

impl Source for GithubSource {
    fn id(&self) -> SourceId {
        source_id(&self.repo, &self.labels)
    }

    fn label(&self) -> String {
        if self.labels.is_empty() {
            self.repo.clone()
        } else {
            format!("{} ({})", self.repo, self.labels.join(", "))
        }
    }

    fn min_interval_ms(&self) -> u64 {
        INBOX_GITHUB_MIN_INTERVAL_MS
    }

    fn project(&self) -> Option<String> {
        match self.projects.as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        }
    }

    /// `gh issue list` (C6c.3). Exactly 100 issues: there may be more, so the list is not
    /// complete (nothing is marked gone).
    fn fetch(&self) -> Result<Fetched, SourceError> {
        let call = GhCall::new(list_args(&self.repo, &self.labels));
        let out = self
            .gh
            .run(&call)
            .map_err(|e| source_error(&e, &self.repo))?;
        let mut items =
            parse_issue_list(&out.stdout, &self.repo).map_err(|e| source_error(&e, &self.repo))?;
        let project = self.project();
        let candidates = if project.is_none() {
            self.projects.clone()
        } else {
            Vec::new()
        };
        for it in &mut items {
            it.project = project.clone();
            it.candidates = candidates.clone();
        }
        // Counted before the state filter would be better, but gh lists only open issues.
        let capped = items.len() >= INBOX_GITHUB_LIMIT;
        Ok(Fetched {
            items,
            complete: !capped,
            capped,
            notes: Vec::new(),
        })
    }
}

/// One GitHub source per `(repo lowercase, labels sorted)` over the projects' `github` (C6c.1
/// `build_github_sources`); the first project's spelling of the repo is used.
pub fn build_github_sources(
    gh: &Arc<dyn GhRunner>,
    projects: &[(String, GithubConfig)],
) -> Vec<GithubSource> {
    let mut out: Vec<GithubSource> = Vec::new();
    for (project, cfg) in projects {
        if !valid_repo(&cfg.repo) {
            continue;
        }
        let mut labels = cfg.labels.clone();
        labels.sort();
        labels.dedup();
        match out
            .iter_mut()
            .find(|s| s.repo.eq_ignore_ascii_case(&cfg.repo) && s.labels == labels)
        {
            Some(s) => {
                if !s.projects.iter().any(|p| p == project) {
                    s.projects.push(project.clone());
                }
            }
            None => out.push(GithubSource {
                gh: Arc::clone(gh),
                repo: cfg.repo.clone(),
                labels,
                projects: vec![project.clone()],
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::WriteBackConfig;
    use crate::gh::fake::{FakeGh, NOT_LOGGED_IN, RATE_LIMIT, REPO_MISSING};
    use crate::inbox::source::Retry;
    use crate::proc::Captured;

    /// Research6c §1.2 / bilag C: two issues, one with `labels: []` and `author: null`, one with
    /// CRLF and a tag char in the title; a closed one and a foreign URL.
    pub const LIST: &str = r#"[
      {"author":null,"labels":[],"number":7,"state":"OPEN","title":"Crash on start <b>x</b>",
       "updatedAt":"2026-10-01T10:00:00Z","url":"https://github.com/o/r/issues/7"},
      {"author":{"id":"U1","is_bot":false,"login":"alice","name":"Alice"},
       "labels":[{"id":"L1","name":"bug","description":"","color":"d73a4a"},{"id":"L2","name":"ÆØÅ\u000b","color":"x"}],
       "number":8,"state":"OPEN","title":"Line\r\nbreak󠁁 here",
       "updatedAt":"2026-10-02T10:00:00Z","url":"https://evil.example/o/r/issues/8"},
      {"author":{"login":"bob"},"labels":[],"number":9,"state":"CLOSED","title":"old",
       "updatedAt":"2026-09-01T10:00:00Z","url":"https://github.com/o/r/issues/9"}
    ]"#;

    fn source(gh: &Arc<FakeGh>, projects: &[&str]) -> GithubSource {
        GithubSource {
            gh: gh.clone(),
            repo: "O/R".into(),
            labels: vec!["bug".into()],
            projects: projects.iter().map(|p| p.to_string()).collect(),
        }
    }

    #[test]
    fn list_args_are_verbatim() {
        assert_eq!(
            list_args("o/r", &["bug".into(), "help wanted".into()]),
            [
                "issue",
                "list",
                "--repo",
                "o/r",
                "--state",
                "open",
                "--label",
                "bug",
                "--label",
                "help wanted",
                "--limit",
                "100",
                "--json",
                "number,title,labels,url,updatedAt,state,author"
            ]
        );
        assert_eq!(
            list_args("o/r", &[]),
            [
                "issue",
                "list",
                "--repo",
                "o/r",
                "--state",
                "open",
                "--limit",
                "100",
                "--json",
                "number,title,labels,url,updatedAt,state,author"
            ]
        );
        assert!(!list_args("o/r", &[]).iter().any(|a| a.contains("body")));
        assert_eq!(
            view_args("o/r", 7, VIEW_FIELDS),
            [
                "issue",
                "view",
                "7",
                "--repo",
                "o/r",
                "--json",
                "number,title,body,labels,url,updatedAt,state,stateReason,closedAt,author"
            ]
        );
    }

    #[test]
    fn list_parses_two_items_with_null_author_and_crlf() {
        let items = parse_issue_list(LIST, "o/r").unwrap();
        assert_eq!(items.len(), 2, "the closed issue is dropped");
        let a = &items[0];
        assert_eq!(a.external_id, "github:o/r#7");
        assert_eq!(a.title, "Crash on start <b>x</b>");
        assert_eq!((a.labels.len(), a.author.as_deref()), (0, None));
        assert_eq!(a.url.as_deref(), Some("https://github.com/o/r/issues/7"));
        assert_eq!(a.number, Some(7));
        assert_eq!(a.repo.as_deref(), Some("o/r"));
        assert_eq!(a.body, None, "no body in the list");
        assert_eq!(a.fingerprint.as_deref(), Some("2026-10-01T10:00:00Z"));
        let b = &items[1];
        assert_eq!(b.title, "Line break here");
        assert_eq!(b.labels, ["bug", "ÆØÅ"]);
        assert_eq!(b.author.as_deref(), Some("alice"));
        assert_eq!(b.url, None, "url_from_other_host_is_dropped");
        assert!(matches!(
            parse_issue_list("{\"a\":1}", "o/r"),
            Err(GhError::BadJson(_))
        ));
        assert!(matches!(
            parse_issue_list("[{\"number\":", "o/r"),
            Err(GhError::BadJson(_))
        ));
    }

    #[test]
    fn list_empty_is_complete() {
        let gh = Arc::new(FakeGh::new());
        gh.reply(&["issue", "list"], 0, "[]\n", "");
        let f = source(&gh, &["web"]).fetch().unwrap();
        assert_eq!((f.items.len(), f.complete, f.capped), (0, true, false));
        assert_eq!(
            gh.calls(),
            vec![list_args("O/R", &["bug".to_string()])],
            "one call with the configured spelling"
        );
    }

    #[test]
    fn list_capped_at_100_is_incomplete() {
        let many: Vec<String> = (1..=100)
            .map(|n| {
                format!(
                    r#"{{"number":{n},"state":"OPEN","title":"t{n}","labels":[],"author":null,"url":"https://github.com/o/r/issues/{n}","updatedAt":"x"}}"#
                )
            })
            .collect();
        let gh = Arc::new(FakeGh::new());
        gh.reply(&["issue", "list"], 0, &format!("[{}]", many.join(",")), "");
        let f = source(&gh, &["web"]).fetch().unwrap();
        assert_eq!((f.items.len(), f.complete, f.capped), (100, false, true));
        assert!(f.items.iter().all(|i| i.project.as_deref() == Some("web")));
    }

    #[test]
    fn not_logged_in_is_manual_retry() {
        let gh = Arc::new(FakeGh::new());
        gh.reply(&["issue", "list"], 4, "", NOT_LOGGED_IN);
        let e = source(&gh, &["web"]).fetch().unwrap_err();
        assert_eq!(e.kind, SourceErrorKind::NotLoggedIn);
        assert_eq!(e.retry, Retry::Manual);
        assert_eq!(
            e.text,
            "gh er ikke logget ind — kør gh auth login i en terminal"
        );
        let gh = Arc::new(FakeGh::new());
        gh.reply(&["issue", "list"], 1, "", REPO_MISSING);
        let e = source(&gh, &["web"]).fetch().unwrap_err();
        assert_eq!(
            (e.kind, e.retry),
            (SourceErrorKind::RepoNotFound, Retry::Manual)
        );
        assert_eq!(e.text, "repoet «O/R» findes ikke, eller gh har ikke adgang");
        let gh = Arc::new(FakeGh::new());
        gh.fail(&["issue"]);
        let e = source(&gh, &["web"]).fetch().unwrap_err();
        assert_eq!(
            (e.kind, e.retry),
            (SourceErrorKind::GhMissing, Retry::Manual)
        );
    }

    #[test]
    fn rate_limited_is_backoff() {
        let gh = Arc::new(FakeGh::new());
        gh.reply(&["issue", "list"], 1, "", RATE_LIMIT);
        let e = source(&gh, &["web"]).fetch().unwrap_err();
        assert_eq!(
            (e.kind, e.retry),
            (SourceErrorKind::RateLimited, Retry::Backoff)
        );
    }

    #[test]
    fn clipped_is_too_large() {
        let gh = Arc::new(FakeGh::new());
        gh.captured(
            &["issue", "list"],
            Captured {
                code: Some(0),
                clipped: true,
                stdout: "\"title\":\"x\"}]".into(),
                ..Captured::default()
            },
        );
        let e = source(&gh, &["web"]).fetch().unwrap_err();
        assert_eq!(e.kind, SourceErrorKind::TooLarge);
        assert_eq!(e.text, "svaret fra gh var for stort");
        let gh = Arc::new(FakeGh::new());
        gh.captured(
            &["issue", "list"],
            Captured {
                timed_out: true,
                ..Captured::default()
            },
        );
        let e = source(&gh, &["web"]).fetch().unwrap_err();
        assert_eq!(e.kind, SourceErrorKind::Timeout);
    }

    fn cfg(repo: &str, labels: &[&str]) -> GithubConfig {
        GithubConfig {
            repo: repo.into(),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            state: "open".into(),
            write_back: WriteBackConfig::default(),
        }
    }

    #[test]
    fn same_repo_in_two_projects_gives_one_source_and_candidates() {
        let fake = Arc::new(FakeGh::new());
        let gh: Arc<dyn GhRunner> = fake.clone();
        let sources = build_github_sources(
            &gh,
            &[
                ("web".into(), cfg("o/r", &["b", "a"])),
                ("api".into(), cfg("O/R", &["a", "b", "a"])),
                ("docs".into(), cfg("o/r", &[])),
                ("bad".into(), cfg("not a repo", &[])),
            ],
        );
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].projects, ["web", "api"]);
        assert_eq!(sources[0].labels, ["a", "b"]);
        assert_eq!(sources[0].id().key, "github:o/r[a,b]");
        assert_eq!(sources[0].label(), "o/r (a, b)");
        assert_eq!(sources[0].project(), None);
        assert_eq!(sources[1].id().key, "github:o/r");
        assert_eq!(sources[1].project().as_deref(), Some("docs"));
        assert_eq!(sources[1].min_interval_ms(), 120_000);
        fake.reply(&["issue", "list"], 0, LIST, "");
        let f = sources[0].fetch().unwrap();
        assert!(f
            .items
            .iter()
            .all(|i| i.project.is_none() && i.candidates == ["web", "api"]));
        assert_eq!(fake.calls().len(), 1, "one gh call for both projects");
    }

    #[test]
    fn fetch_issue_and_marker() {
        let gh = FakeGh::new();
        gh.reply(
            &["issue", "view", "7"],
            0,
            r#"{"author":{"login":"alice"},"body":"Trin\r\n<!-- x -->1","closedAt":null,
               "labels":[{"name":"bug"}],"number":7,"state":"OPEN","stateReason":"",
               "title":"Crash","updatedAt":"2026-10-01T10:00:00Z","url":"https://github.com/o/r/issues/7"}"#,
            "",
        );
        let v = fetch_issue(&gh, "o/r", 7).unwrap();
        assert_eq!(
            v.body, "Trin\r\n<!-- x -->1",
            "the caller sanitises the body"
        );
        assert_eq!((v.state.as_str(), v.title.as_str()), ("OPEN", "Crash"));
        assert_eq!(v.labels, ["bug"]);
        assert_eq!(v.author.as_deref(), Some("alice"));
        assert_eq!(gh.calls()[0], view_args("o/r", 7, VIEW_FIELDS));
        assert!(fetch_issue(&gh, "o/r", 0).is_err());
        assert!(fetch_issue(&gh, "bad repo", 7).is_err());
        assert_eq!(gh.calls().len(), 1, "invalid input never reaches gh");

        let gh = FakeGh::new();
        gh.reply(
            &["issue", "view", "7", "--repo", "o/r", "--json", "comments"],
            0,
            r#"{"comments":[{"body":"andet","url":"https://github.com/o/r/issues/7#issuecomment-1"},
               {"body":"x\n<!-- mira-bots:ticket=T1 -->","url":"https://github.com/o/r/issues/7#issuecomment-2"}]}"#,
            "",
        );
        assert_eq!(
            find_marker_comment(&gh, "o/r", 7, "<!-- mira-bots:ticket=T1 -->").unwrap(),
            Some("https://github.com/o/r/issues/7#issuecomment-2".into())
        );
        assert_eq!(
            find_marker_comment(&gh, "o/r", 7, "<!-- mira-bots:ticket=T2 -->").unwrap(),
            None
        );
        let gh = FakeGh::new();
        gh.reply(&["issue", "view"], 0, r#"{"comments":null}"#, "");
        assert_eq!(find_marker_comment(&gh, "o/r", 7, "m").unwrap(), None);
    }
}
