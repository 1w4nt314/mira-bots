//! Projects: folders directly under the projects root (plan4b A.1–A.2). Windows naming rules
//! are enforced on every host so a ProjectId means the same everywhere (research4b §5).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::SeatKind;
use crate::config::{PROJECT_ID_MAX_CHARS, PROJECT_PATH_MAX_CHARS};
use crate::tickets::model::TicketError;

/// A project id: the folder name, validated by [`validate_project_id`].
pub type ProjectId = String;

/// Wire: `"<id>"` (existing) or `{"new": "<name>"}` (created when the ticket is assigned/spawned).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub enum ProjectRef {
    Existing(ProjectId),
    New { new: String },
}

impl ProjectRef {
    /// The id of an existing project; `None` for a project still to be created.
    pub fn id(&self) -> Option<&str> {
        match self {
            ProjectRef::Existing(id) => Some(id),
            ProjectRef::New { .. } => None,
        }
    }

    /// The id or the name of the project to create.
    pub fn name(&self) -> &str {
        match self {
            ProjectRef::Existing(id) => id,
            ProjectRef::New { new } => new,
        }
    }
}

/// A project folder (`list_projects` result, wire camelCase).
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: ProjectId,
    pub path: String,
}

/// Project errors; the messages are user-facing (Danish).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProjectError {
    #[error("Projektnavnet «{0}» er ugyldigt: {1}")]
    InvalidName(String, &'static str),
    #[error("Projektet «{0}» findes allerede")]
    Exists(String),
    #[error("Projektet «{0}» findes ikke")]
    NotFound(String),
    #[error("Stien til projektet bliver for lang (over {PROJECT_PATH_MAX_CHARS} tegn)")]
    PathTooLong,
    #[error("Agenter må ikke oprette projekter i dette workspace (agentsMayCreateProjects) — bed brugeren oprette «{0}»")]
    AgentsMayNotCreate(String),
    #[error("Kunne ikke oprette projektmappen: {0}")]
    Io(String),
}

impl From<ProjectError> for String {
    fn from(e: ProjectError) -> Self {
        e.to_string()
    }
}

impl From<ProjectError> for TicketError {
    fn from(e: ProjectError) -> Self {
        TicketError::Validation(e.to_string())
    }
}

/// Reasons of [`ProjectError::InvalidName`].
const EMPTY: &str = "tomt";
const TOO_LONG: &str = "må højst være 64 tegn";
const BAD_CHAR: &str = "indeholder et ugyldigt tegn (< > : \" / \\ | ? *)";
const CONTROL: &str = "indeholder et kontroltegn";
const LEADING_DOT: &str = "må ikke begynde med punktum";
const EDGE_SPACE: &str = "må ikke begynde eller slutte med mellemrum";
const TRAILING_DOT: &str = "må ikke slutte med punktum";
const RESERVED: &str = "er et reserveret navn i Windows";
const ONLY_DOTS: &str = "må ikke kun bestå af punktummer";

/// Device names Windows reserves in every folder, also with an extension (`NUL.txt`).
pub const RESERVED_STEMS: [&str; 28] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "com¹", "com²",
    "com³", "lpt¹", "lpt²", "lpt³",
];

const INVALID_CHARS: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Checks a project name against the Windows folder-name rules (research4b §5) on every host.
/// Returns the name unchanged (no normalisation).
// TODO(windows-verify): CON, aux.txt, "foo.", "foo ", a:b and a 65-char name are refused before
// anything is created; æøå-projekt works (plan4b D.78).
pub fn validate_project_id(raw: &str) -> Result<ProjectId, ProjectError> {
    let bad = |reason| Err(ProjectError::InvalidName(raw.to_string(), reason));
    if raw.is_empty() {
        return bad(EMPTY);
    }
    if raw.chars().count() > PROJECT_ID_MAX_CHARS {
        return bad(TOO_LONG);
    }
    if raw.chars().any(|c| INVALID_CHARS.contains(&c)) {
        return bad(BAD_CHAR);
    }
    if raw.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7F) {
        return bad(CONTROL);
    }
    if raw.chars().all(|c| c == '.') {
        return bad(ONLY_DOTS);
    }
    if raw.starts_with('.') {
        return bad(LEADING_DOT);
    }
    if raw.starts_with(' ') || raw.ends_with(' ') {
        return bad(EDGE_SPACE);
    }
    if raw.ends_with('.') {
        return bad(TRAILING_DOT);
    }
    let stem = raw
        .split('.')
        .next()
        .unwrap_or(raw)
        .trim_end_matches(' ')
        .to_lowercase();
    if RESERVED_STEMS.contains(&stem.as_str()) {
        return bad(RESERVED);
    }
    Ok(raw.to_string())
}

/// Project ids compare ASCII-case-insensitively (NTFS).
pub fn same_id(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// `<root>/<id>`.
pub fn project_dir(root: &Path, id: &str) -> PathBuf {
    root.join(id)
}

/// The project folders directly under `root`, sorted case-insensitively. Hidden folders (`.`
/// prefix, e.g. `.mira-bots`), files and folders whose names are not valid project ids are
/// skipped. A missing root gives an empty list.
pub fn list_projects(root: &Path) -> Vec<Project> {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!("projects: cannot read {}: {e}", root.display());
            }
            return Vec::new();
        }
    };
    let mut out: Vec<Project> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            if let Err(err) = validate_project_id(&name) {
                log::debug!("projects: skipping folder: {err}");
                return None;
            }
            Some(Project {
                path: project_dir(root, &name).to_string_lossy().into_owned(),
                id: name,
            })
        })
        .collect();
    out.sort_by_key(|p| p.id.to_ascii_lowercase());
    out
}

/// The project named `name` (case-insensitive), with the name as it is on disk.
pub fn find_project(root: &Path, name: &str) -> Option<Project> {
    list_projects(root)
        .into_iter()
        .find(|p| same_id(&p.id, name))
}

/// Creates `<root>/<name>` (and the root if missing). Refuses invalid names, existing projects
/// (case-insensitive) and paths longer than [`PROJECT_PATH_MAX_CHARS`].
pub fn create_project(root: &Path, name: &str) -> Result<Project, ProjectError> {
    let id = validate_project_id(name)?;
    std::fs::create_dir_all(root).map_err(|e| ProjectError::Io(e.to_string()))?;
    if let Some(existing) = find_project(root, &id) {
        return Err(ProjectError::Exists(existing.id));
    }
    let dir = project_dir(root, &id);
    if dir.to_string_lossy().chars().count() > PROJECT_PATH_MAX_CHARS {
        return Err(ProjectError::PathTooLong);
    }
    match std::fs::create_dir(&dir) {
        Ok(()) => Ok(Project {
            id,
            path: dir.to_string_lossy().into_owned(),
        }),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(ProjectError::Exists(id)),
        Err(e) => Err(ProjectError::Io(e.to_string())),
    }
}

/// Makes `r` an existing project: `Existing(id)` must exist; `New { new }` is found when it
/// already exists (case-insensitive; nothing is created), otherwise created when `may_create`.
pub fn realize(root: &Path, r: &ProjectRef, may_create: bool) -> Result<Project, ProjectError> {
    match r {
        ProjectRef::Existing(id) => {
            find_project(root, id).ok_or_else(|| ProjectError::NotFound(id.clone()))
        }
        ProjectRef::New { new } => {
            let name = validate_project_id(new)?;
            if let Some(p) = find_project(root, &name) {
                return Ok(p);
            }
            if !may_create {
                return Err(ProjectError::AgentsMayNotCreate(name));
            }
            create_project(root, &name)
        }
    }
}

/// Whether the ticket's project is the existing project `id`.
pub fn matches(ticket: Option<&ProjectRef>, id: &str) -> bool {
    matches!(ticket, Some(ProjectRef::Existing(x)) if same_id(x, id))
}

/// What an assignment to `agent` means for the ticket's project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssignmentProject {
    Unchanged,
    /// The ticket's project becomes this existing project (a `New` matching the agent's).
    Set(ProjectId),
}

/// Whether a ticket with project `ticket` may go to an agent on `seat` in `agent_project`
/// (plan4b A.2, C4b.1). Staff seats take any ticket; work seats need the agent's project.
pub fn assignment_target(
    ticket: Option<&ProjectRef>,
    seat: SeatKind,
    agent_project: Option<&str>,
    agent_name: &str,
) -> Result<AssignmentProject, TicketError> {
    if seat == SeatKind::Staff {
        return Ok(AssignmentProject::Unchanged);
    }
    let Some(agent_project) = agent_project else {
        return Err(TicketError::Validation("Agenten har intet projekt".into()));
    };
    let wrong = |ticket_project: &str| TicketError::WrongProject {
        agent: agent_name.to_string(),
        agent_project: agent_project.to_string(),
        ticket_project: ticket_project.to_string(),
    };
    match ticket {
        None => Err(TicketError::ProjectRequired),
        Some(ProjectRef::Existing(id)) if same_id(id, agent_project) => {
            Ok(AssignmentProject::Unchanged)
        }
        Some(ProjectRef::Existing(id)) => Err(wrong(id)),
        Some(ProjectRef::New { new }) if same_id(new, agent_project) => {
            Ok(AssignmentProject::Set(agent_project.to_string()))
        }
        Some(ProjectRef::New { new }) => Err(wrong(new)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("mira-projects-{}", uuid::Uuid::new_v4()))
    }

    fn reason(name: &str) -> &'static str {
        match validate_project_id(name) {
            Err(ProjectError::InvalidName(n, r)) => {
                assert_eq!(n, name);
                r
            }
            other => panic!("{name:?}: {other:?}"),
        }
    }

    #[test]
    fn valid_names_are_returned_unchanged() {
        for name in [
            "a",
            "mira-bots",
            "æøå ok",
            "æøå-projekt",
            "a.b",
            "a_b",
            "Foo",
            "con-app",
            "console",
            "com10",
            "lpt0",
            "x.con",
            "a b",
            &"x".repeat(64),
        ] {
            assert_eq!(validate_project_id(name).unwrap(), name, "{name}");
        }
    }

    #[test]
    fn invalid_names_give_the_danish_reason() {
        let table: [(&str, &str); 22] = [
            ("", EMPTY),
            (&"x".repeat(65), TOO_LONG),
            ("a:b", BAD_CHAR),
            ("a<b", BAD_CHAR),
            ("a>b", BAD_CHAR),
            ("a\"b", BAD_CHAR),
            ("a/b", BAD_CHAR),
            ("a\\b", BAD_CHAR),
            ("a|b", BAD_CHAR),
            ("a?b", BAD_CHAR),
            ("a*b", BAD_CHAR),
            ("a\tb", CONTROL),
            ("a\u{7f}", CONTROL),
            (".git", LEADING_DOT),
            (".mira-bots", LEADING_DOT),
            (" x", EDGE_SPACE),
            ("x ", EDGE_SPACE),
            ("x.", TRAILING_DOT),
            ("...", ONLY_DOTS),
            ("CON", RESERVED),
            ("Nul.txt", RESERVED),
            ("COM¹", RESERVED),
        ];
        for (name, want) in table {
            assert_eq!(reason(name), want, "{name:?}");
        }
        for name in [
            "con",
            "prn",
            "aux",
            "nul",
            "com1",
            "com9",
            "lpt1",
            "lpt9",
            "aux.txt",
            "nul.tar.gz",
            "con .txt",
            "LPT³",
        ] {
            assert_eq!(reason(name), RESERVED, "{name:?}");
        }
        assert_eq!(
            validate_project_id("a:b").unwrap_err().to_string(),
            "Projektnavnet «a:b» er ugyldigt: indeholder et ugyldigt tegn (< > : \" / \\ | ? *)"
        );
    }

    #[test]
    fn errors_are_danish() {
        let table = [
            (
                ProjectError::Exists("a".into()),
                "Projektet «a» findes allerede",
            ),
            (ProjectError::NotFound("a".into()), "Projektet «a» findes ikke"),
            (
                ProjectError::PathTooLong,
                "Stien til projektet bliver for lang (over 200 tegn)",
            ),
            (
                ProjectError::AgentsMayNotCreate("x".into()),
                "Agenter må ikke oprette projekter i dette workspace (agentsMayCreateProjects) — bed brugeren oprette «x»",
            ),
            (
                ProjectError::Io("nej".into()),
                "Kunne ikke oprette projektmappen: nej",
            ),
        ];
        for (e, text) in table {
            assert_eq!(String::from(e.clone()), text);
            assert_eq!(TicketError::from(e), TicketError::Validation(text.into()));
        }
    }

    #[test]
    fn project_ref_wire_format() {
        let e: ProjectRef = serde_json::from_value(json!("mira-bots")).unwrap();
        assert_eq!(e, ProjectRef::Existing("mira-bots".into()));
        assert_eq!(serde_json::to_value(&e).unwrap(), json!("mira-bots"));
        let n: ProjectRef = serde_json::from_value(json!({"new": "x"})).unwrap();
        assert_eq!(n, ProjectRef::New { new: "x".into() });
        assert_eq!(serde_json::to_value(&n).unwrap(), json!({"new": "x"}));
        let none: Option<ProjectRef> = serde_json::from_value(json!(null)).unwrap();
        assert_eq!(none, None);
        assert!(serde_json::from_value::<ProjectRef>(json!(5)).is_err());
        assert!(serde_json::from_value::<ProjectRef>(json!({"x": 1})).is_err());
        assert_eq!((e.id(), e.name()), (Some("mira-bots"), "mira-bots"));
        assert_eq!((n.id(), n.name()), (None, "x"));
    }

    #[test]
    fn same_id_and_matches_ignore_ascii_case() {
        assert!(same_id("Mira", "mIRA"));
        assert!(!same_id("a", "b"));
        let a = ProjectRef::Existing("A".into());
        assert!(matches(Some(&a), "a"));
        assert!(!matches(Some(&a), "b"));
        assert!(!matches(Some(&ProjectRef::New { new: "a".into() }), "a"));
        assert!(!matches(None, "a"));
        assert_eq!(project_dir(Path::new("/r"), "p"), Path::new("/r/p"));
    }

    #[test]
    fn list_create_and_find_in_a_temp_root() {
        let root = temp_root();
        assert!(list_projects(&root).is_empty(), "missing root → empty");
        let a = create_project(&root, "a").unwrap();
        assert_eq!(a.id, "a");
        assert_eq!(a.path, root.join("a").to_string_lossy());
        assert!(root.join("a").is_dir());
        std::fs::create_dir(root.join(".hidden")).unwrap();
        std::fs::create_dir(root.join(".mira-bots")).unwrap();
        std::fs::write(root.join("f.txt"), "x").unwrap();
        std::fs::create_dir(root.join("CON")).unwrap();
        std::fs::create_dir(root.join("Beta")).unwrap();
        let ids: Vec<_> = list_projects(&root).into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["a", "Beta"]);

        assert_eq!(
            create_project(&root, "A"),
            Err(ProjectError::Exists("a".into()))
        );
        assert!(matches!(
            create_project(&root, "x."),
            Err(ProjectError::InvalidName(_, TRAILING_DOT))
        ));
        assert!(!root.join("x").exists());
        assert_eq!(find_project(&root, "beta").unwrap().id, "Beta");
        assert_eq!(find_project(&root, "nope"), None);
        assert_eq!(find_project(&root, "f.txt"), None, "files are not projects");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn create_refuses_too_long_paths() {
        let root = temp_root().join("r".repeat(190));
        assert_eq!(
            create_project(&root, &"p".repeat(20)),
            Err(ProjectError::PathTooLong)
        );
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    #[test]
    fn realize_existing_and_new() {
        let root = temp_root();
        create_project(&root, "a").unwrap();
        assert_eq!(
            realize(&root, &ProjectRef::Existing("A".into()), false)
                .unwrap()
                .id,
            "a"
        );
        assert_eq!(
            realize(&root, &ProjectRef::Existing("b".into()), true),
            Err(ProjectError::NotFound("b".into()))
        );
        let new_b = ProjectRef::New { new: "b".into() };
        assert_eq!(
            realize(&root, &new_b, false),
            Err(ProjectError::AgentsMayNotCreate("b".into()))
        );
        assert!(!root.join("b").exists());
        assert_eq!(realize(&root, &new_b, true).unwrap().id, "b");
        assert!(root.join("b").is_dir());
        // A `New` that exists by now is found, nothing is created (no permission needed).
        assert_eq!(
            realize(&root, &ProjectRef::New { new: "B".into() }, false)
                .unwrap()
                .id,
            "b"
        );
        assert!(matches!(
            realize(&root, &ProjectRef::New { new: "CON".into() }, true),
            Err(ProjectError::InvalidName(_, RESERVED))
        ));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn assignment_target_truth_table() {
        let a = ProjectRef::Existing("a".into());
        let big_a = ProjectRef::Existing("A".into());
        let new_a = ProjectRef::New { new: "a".into() };
        let new_c = ProjectRef::New { new: "c".into() };
        let wrong = |ticket_project: &str, agent_project: &str| {
            Err(TicketError::WrongProject {
                agent: "k1".into(),
                agent_project: agent_project.into(),
                ticket_project: ticket_project.into(),
            })
        };
        use AssignmentProject::*;
        use SeatKind::*;
        let table: [(Option<&ProjectRef>, SeatKind, Option<&str>, Result<_, _>); 11] = [
            (None, Staff, None, Ok(Unchanged)),
            (Some(&a), Staff, None, Ok(Unchanged)),
            (Some(&new_c), Staff, None, Ok(Unchanged)),
            (None, Work, Some("a"), Err(TicketError::ProjectRequired)),
            (Some(&a), Work, Some("a"), Ok(Unchanged)),
            (Some(&big_a), Work, Some("a"), Ok(Unchanged)),
            (Some(&a), Work, Some("b"), wrong("a", "b")),
            (Some(&new_a), Work, Some("A"), Ok(Set("A".into()))),
            (Some(&new_c), Work, Some("a"), wrong("c", "a")),
            (
                None,
                Work,
                None,
                Err(TicketError::Validation("Agenten har intet projekt".into())),
            ),
            (
                Some(&a),
                Work,
                None,
                Err(TicketError::Validation("Agenten har intet projekt".into())),
            ),
        ];
        for (i, (ticket, seat, agent_project, want)) in table.into_iter().enumerate() {
            assert_eq!(
                assignment_target(ticket, seat, agent_project, "k1"),
                want,
                "row {i}"
            );
        }
    }
}
