//! Agent profiles (plan5 C5.1/C5.2): what a new agent is started with — roles, prompt addition,
//! model, effort, default seat and tool narrowing. Validation, model-id check, the built-in
//! profiles and the derived permission rules (C5.9).

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::agent::roles::{self, Role};
use crate::agent::SeatKind;
use crate::config::{
    BUILTIN_PROFILE_IDS, MCP_TOOL_PREFIX, MODEL_ALIASES, MODEL_ID_MAX_CHARS,
    PROFILE_NAME_MAX_CHARS, PROJECT_FILE, PROMPT_APPEND_MAX_CHARS, WORKSPACE_FILE,
};

/// Maximum number of rules in `extraAllow` / `extraDeny`.
pub const EXTRA_RULES_MAX: usize = 20;
/// Maximum length (chars) of one rule in `extraAllow` / `extraDeny`.
pub const EXTRA_RULE_MAX_CHARS: usize = 200;
/// Maximum length of a profile id (`^[a-z0-9][a-z0-9-]{0,39}$`).
pub const PROFILE_ID_MAX_CHARS: usize = 40;

/// Reasoning effort (`--effort`, `effortLevel`). Wire: `"low"|"medium"|"high"|"xhigh"|"max"`.
/// `max` is only valid as a flag; settings files ignore it (research5 Q2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub const ALL: [Effort; 5] = [
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::Xhigh,
        Effort::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }

    /// Exact wire name → effort.
    pub fn parse(s: &str) -> Option<Effort> {
        Effort::ALL.into_iter().find(|e| e.as_str() == s)
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Effort {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

/// Unknown values fail with the Danish [`ProfileError::UnknownEffort`] text (it reaches the UI
/// through Tauri's argument errors).
impl<'de> Deserialize<'de> for Effort {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Effort::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(ProfileError::UnknownEffort.to_string()))
    }
}

/// Wire: `"builtin"|"custom"`. Always derived from the id ([`kind_for_id`]).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProfileKind {
    Builtin,
    #[default]
    Custom,
}

/// One profile; file `<projects root>/.mira-bots/profiles/<id>.json` (C5.2), camelCase.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentProfile {
    /// `^[a-z0-9][a-z0-9-]{0,39}$`; empty only in a `save_profile` call for a new profile.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: ProfileKind,
    #[serde(default)]
    pub roles: Vec<Role>,
    /// `None`: derived (`roles.len() != 1`), see [`AgentProfile::is_specialist`].
    #[serde(default)]
    pub specialist: Option<bool>,
    #[serde(default)]
    pub prompt_append: String,
    /// Alias or full id; `None` = Claude Code's default.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<Effort>,
    /// The app's own tool names (without prefix) denied on top of the role matrix.
    #[serde(default)]
    pub tool_deny: Vec<String>,
    #[serde(default)]
    pub default_seat: SeatKind,
    /// Raw permission rules added to `permissions.allow`.
    #[serde(default)]
    pub extra_allow: Vec<String>,
    /// Raw permission rules added to `permissions.deny`.
    #[serde(default)]
    pub extra_deny: Vec<String>,
    #[serde(default)]
    pub updated_at: u64,
}

/// Per-spawn overrides of the profile's model and effort (C5.1).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpawnOverrides {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<Effort>,
}

/// What an agent carries from its profile, fixed at spawn (roles never change during a session).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileSnapshot {
    pub profile_id: String,
    pub profile_name: String,
    pub roles: Vec<Role>,
    pub specialist: bool,
    /// The requested model (override, else the profile's); `None` = Claude Code's default.
    pub model: Option<String>,
    pub effort: Option<Effort>,
}

/// Validation errors (Danish, C5.6). They reach the UI as plain strings.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProfileError {
    #[error("Profilen findes ikke")]
    NotFound,
    #[error("Navn skal være 1–60 tegn")]
    BadName,
    #[error("Profil-id må kun indeholde små bogstaver, tal og bindestreg")]
    BadId,
    #[error("Ukendt model: brug et alias (sonnet, opus, haiku, fable, best, opusplan, sonnet[1m], opus[1m]) eller et fuldt id som claude-sonnet-5-5")]
    UnknownModel,
    #[error("Ukendt effort-niveau")]
    UnknownEffort,
    #[error("Prompt-tillægget er for langt (maks 4000 tegn)")]
    PromptTooLong,
    #[error("Ukendt værktøj i toolDeny: {0}")]
    UnknownTool(String),
    /// `field` is `extraAllow` or `extraDeny`.
    #[error("{0} må højst have 20 regler à 200 tegn uden linjeskift")]
    BadExtraRules(&'static str),
    #[error("Indbyggede profiler kan ikke slettes; brug Nulstil")]
    BuiltinNotDeletable,
    #[error("Kun indbyggede profiler kan nulstilles")]
    NotBuiltin,
    #[error("Kunne ikke gemme profilen: {0}")]
    Io(String),
}

impl From<ProfileError> for String {
    fn from(e: ProfileError) -> Self {
        e.to_string()
    }
}

/// `^[a-z0-9][a-z0-9-]{0,39}$`, without a regex crate.
pub fn id_is_valid(id: &str) -> bool {
    let b = id.as_bytes();
    let ok = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    !b.is_empty()
        && b.len() <= PROFILE_ID_MAX_CHARS
        && ok(&b[0])
        && b.iter().all(|c| ok(c) || *c == b'-')
}

/// Whether `id` names a built-in profile.
pub fn is_builtin_id(id: &str) -> bool {
    BUILTIN_PROFILE_IDS.contains(&id)
}

/// `builtin` for the built-in ids, otherwise `custom`.
pub fn kind_for_id(id: &str) -> ProfileKind {
    if is_builtin_id(id) {
        ProfileKind::Builtin
    } else {
        ProfileKind::Custom
    }
}

/// A fresh custom id: `custom-<8 hex>`.
pub fn new_custom_id() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("custom-{}", &hex[..8])
}

/// A model value Claude Code is expected to accept: one of [`MODEL_ALIASES`], or a full id
/// `claude-<a-z0-9->` with an optional `[1m]` suffix, at most [`MODEL_ID_MAX_CHARS`] chars.
/// Whether the account may use the model cannot be checked here (research5 Q1).
// TODO(windows-verify): a profile with `claude-sonnet-5-5` starts; a model the account cannot
// use fails at the first request in the TUI, not in the app (plan5 D.60).
pub fn model_is_valid(s: &str) -> bool {
    if MODEL_ALIASES.contains(&s) {
        return true;
    }
    if s.chars().count() > MODEL_ID_MAX_CHARS {
        return false;
    }
    let base = s.strip_suffix("[1m]").unwrap_or(s);
    let Some(rest) = base.strip_prefix("claude-") else {
        return false;
    };
    !rest.is_empty()
        && rest
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// Text from the UI: CRLF → LF, C0/C1 controls removed except `\n` and `\t`.
fn clean_text(s: &str) -> String {
    s.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

/// Trimmed, empty rules dropped, duplicates removed (first kept); then the limits.
fn clean_rules(rules: &[String], field: &'static str) -> Result<Vec<String>, ProfileError> {
    let mut out: Vec<String> = Vec::new();
    for r in rules {
        if r.contains(['\n', '\r']) {
            return Err(ProfileError::BadExtraRules(field));
        }
        let r = r.trim();
        if r.is_empty() || out.iter().any(|o| o == r) {
            continue;
        }
        if r.chars().count() > EXTRA_RULE_MAX_CHARS {
            return Err(ProfileError::BadExtraRules(field));
        }
        out.push(r.to_string());
    }
    if out.len() > EXTRA_RULES_MAX {
        return Err(ProfileError::BadExtraRules(field));
    }
    Ok(out)
}

impl AgentProfile {
    /// Normalises and validates (C5.6): id format, name trimmed 1–60 chars, roles deduplicated in
    /// canonical order (an empty list is allowed), prompt addition cleaned and ≤ 4000 chars,
    /// model empty → `None` and otherwise [`model_is_valid`], `toolDeny` names known, extra
    /// rules ≤ 20 × 200 chars without line breaks. `kind` is set from the id.
    pub fn validated(mut self) -> Result<Self, ProfileError> {
        if !id_is_valid(&self.id) {
            return Err(ProfileError::BadId);
        }
        self.kind = kind_for_id(&self.id);
        self.name = self.name.trim().to_string();
        let n = self.name.chars().count();
        if n == 0 || n > PROFILE_NAME_MAX_CHARS || self.name.chars().any(char::is_control) {
            return Err(ProfileError::BadName);
        }
        self.roles = roles::normalize(&self.roles);
        self.prompt_append = clean_text(&self.prompt_append).trim().to_string();
        if self.prompt_append.chars().count() > PROMPT_APPEND_MAX_CHARS {
            return Err(ProfileError::PromptTooLong);
        }
        self.model = self
            .model
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        if let Some(m) = &self.model {
            if !model_is_valid(m) {
                return Err(ProfileError::UnknownModel);
            }
        }
        let mut deny: Vec<String> = Vec::new();
        for t in &self.tool_deny {
            let t = t.trim();
            let t = t.strip_prefix(MCP_TOOL_PREFIX).unwrap_or(t);
            if !mira_mcp::tools::ALL_TOOL_NAMES.contains(&t) {
                return Err(ProfileError::UnknownTool(t.to_string()));
            }
            if !deny.iter().any(|d| d == t) {
                deny.push(t.to_string());
            }
        }
        self.tool_deny = deny;
        self.extra_allow = clean_rules(&self.extra_allow, "extraAllow")?;
        self.extra_deny = clean_rules(&self.extra_deny, "extraDeny")?;
        Ok(self)
    }

    /// Explicit `specialist`, else `roles.len() != 1`.
    pub fn is_specialist(&self) -> bool {
        self.specialist
            .unwrap_or(roles::normalize(&self.roles).len() != 1)
    }

    /// `permissions.allow` (C5.9): `mcp__mira-bots__*`, then `extraAllow`.
    pub fn allow_rules(&self) -> Vec<String> {
        let mut v = vec![format!("{MCP_TOOL_PREFIX}*")];
        for r in &self.extra_allow {
            if !v.contains(r) {
                v.push(r.clone());
            }
        }
        v
    }

    /// `permissions.deny` (C5.9): the role-bound tools the roles do not allow plus `toolDeny`,
    /// each as `mcp__mira-bots__<tool>` and sorted, then [`FILE_EDIT_TOOLS`] when the profile
    /// has no work role (5c C.2), then [`WORK_GIT_DENY`] for every profile (step 6b: no push; the
    /// user merges), then with `root` (the projects root) the [`locked_file_rules`] for the
    /// workspace file and the project files, then `extraDeny` as given (without duplicates).
    pub fn deny_rules(&self, root: Option<&Path>) -> Vec<String> {
        let allowed = mira_mcp::tools::tools_for_roles(&roles::wire_names(&self.roles));
        let mut tools: Vec<String> = mira_mcp::tools::ROLE_BOUND_TOOLS
            .iter()
            .filter(|t| !allowed.contains(t))
            .map(|t| t.to_string())
            .chain(self.tool_deny.iter().cloned())
            .map(|t| format!("{MCP_TOOL_PREFIX}{t}"))
            .collect();
        tools.sort();
        tools.dedup();
        if !roles::has_work_role(&self.roles) {
            tools.extend(FILE_EDIT_TOOLS.iter().map(|t| t.to_string()));
        }
        tools.extend(WORK_GIT_DENY.iter().map(|t| t.to_string()));
        if let Some(root) = root {
            tools.extend(locked_file_rules(root));
        }
        for r in &self.extra_deny {
            if !tools.contains(r) {
                tools.push(r.clone());
            }
        }
        tools
    }

    /// A staff seat needs a profile with a staff role (5c B); a work seat takes any profile.
    pub fn check_seat(&self, seat: SeatKind) -> Result<(), String> {
        if seat == SeatKind::Staff && !roles::has_staff_role(&self.roles) {
            return Err(format!(
                "Profilen «{}» har ingen stabsrolle (reviewer, koordinator eller planlægger) og kan ikke stå på en stabsplads",
                self.name
            ));
        }
        Ok(())
    }

    /// The snapshot an agent is spawned with: overrides win over the profile's model/effort.
    pub fn snapshot(&self, overrides: &SpawnOverrides) -> ProfileSnapshot {
        ProfileSnapshot {
            profile_id: self.id.clone(),
            profile_name: self.name.clone(),
            roles: roles::normalize(&self.roles),
            specialist: self.is_specialist(),
            model: overrides.model.clone().or_else(|| self.model.clone()),
            effort: overrides.effort.or(self.effort),
        }
    }
}

/// Validates per-spawn overrides (blank model → `None`).
pub fn validate_overrides(o: SpawnOverrides) -> Result<SpawnOverrides, ProfileError> {
    let model = o
        .model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    if model.as_deref().is_some_and(|m| !model_is_valid(m)) {
        return Err(ProfileError::UnknownModel);
    }
    Ok(SpawnOverrides {
        model,
        effort: o.effort,
    })
}

/// File-editing tools denied to a profile without a work role (coder, researcher, debugger):
/// staff agents distribute work instead of doing it (5c C.2).
pub const FILE_EDIT_TOOLS: [&str; 4] = ["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// Denied to every profile (step 6b, plan6b A.4): the app never lets an agent push; the user
/// merges. A deny rule cannot be overridden by `extraAllow` (deny wins). `gitPush` comes later.
pub const WORK_GIT_DENY: [&str; 2] = ["Bash(git push *)", "Bash(git -C * push *)"];

/// The projects root (or any absolute path) as an absolute permission path (research6b §4.1):
/// `//` + the POSIX form. Unix `/home/x` → `//home/x`; Windows `C:\Users\x` → `//c/Users/x`
/// (drive letter lowercased, `\` → `/`, a `\\?\` or `\\?\UNC\` prefix stripped). No
/// trailing slash, except for the filesystem root (`//`).
pub fn permission_path(p: &Path) -> String {
    permission_path_str(&p.to_string_lossy(), cfg!(windows))
}

/// The pure core of [`permission_path`], testable for both platforms on any host.
// TODO(windows-verify): the drive form `//c/Users/…` is from the docs (research6b §4.1); a UNC
// root (`\\server\share`) becomes `//server/share`, which is unverified (plan6b D.100).
pub fn permission_path_str(s: &str, windows: bool) -> String {
    let posix = if windows {
        let s = s
            .strip_prefix(r"\\?\UNC\")
            .map(|rest| format!(r"\\{rest}"))
            .unwrap_or_else(|| s.strip_prefix(r"\\?\").unwrap_or(s).to_string());
        let s = s.replace('\\', "/");
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(d), Some(':')) if d.is_ascii_alphabetic() => {
                format!(
                    "{}/{}",
                    d.to_ascii_lowercase(),
                    chars.as_str().trim_start_matches('/')
                )
            }
            _ => s,
        }
    } else {
        s.to_string()
    };
    // The filesystem root itself is `//`.
    format!("//{}", posix.trim_matches('/'))
}

/// `Edit` deny rules for the files that belong to the user (step 6b, research6b §4.2 T2/T4/T8):
/// the workspace file in the projects root and every project's `.mira-bots/project.json`, both
/// anchored absolutely (they can lie above the agent's cwd, e.g. from a worktree), plus the
/// cwd-relative form for a copy under the cwd. `Edit` covers every file-editing tool; `Write(…)`
/// path rules are never consulted (T3).
pub fn locked_file_rules(root: &Path) -> Vec<String> {
    let mut base = permission_path(root);
    if !base.ends_with('/') {
        base.push('/');
    }
    vec![
        format!("Edit({base}{WORKSPACE_FILE})"),
        format!("Edit({base}**/{PROJECT_FILE})"),
        format!("Edit(**/{PROJECT_FILE})"),
    ]
}

/// Reviewer: read-only git in other folders (research5 Q6) …
const REVIEWER_ALLOW: [&str; 4] = [
    "Bash(git -C * diff *)",
    "Bash(git -C * log *)",
    "Bash(git -C * status *)",
    "Bash(git -C * show *)",
];
/// … and never commit or push, also not in the `git -C <folder> …` form the review file asks
/// for (`Bash(git commit *)` only matches a command line that starts with `git commit`).
const REVIEWER_DENY: [&str; 4] = [
    "Bash(git commit *)",
    "Bash(git push *)",
    "Bash(git -C * commit *)",
    "Bash(git -C * push *)",
];
/// The specialist's prompt addition (C5.7).
pub const SPECIALIST_PROMPT_APPEND: &str = "Du har flere roller; brug den der passer til ticketen.";

/// The default of built-in profile `id` (`None` for other ids).
pub fn builtin_profile(id: &str) -> Option<AgentProfile> {
    let (name, roles, seat): (&str, Vec<Role>, SeatKind) = match id {
        "coder" => ("Koder", vec![Role::Coder], SeatKind::Work),
        "researcher" => ("Researcher", vec![Role::Researcher], SeatKind::Work),
        "reviewer" => ("Reviewer", vec![Role::Reviewer], SeatKind::Staff),
        "coordinator" => ("Koordinator", vec![Role::Coordinator], SeatKind::Staff),
        // A staff role only (5c batch 2): the planner stands on a staff seat by default.
        "planner" => ("Planlægger", vec![Role::Planner], SeatKind::Staff),
        "debugger" => ("Debugger", vec![Role::Debugger], SeatKind::Work),
        "specialist" => (
            "Specialist (alle roller)",
            Role::ALL.to_vec(),
            SeatKind::Work,
        ),
        _ => return None,
    };
    let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let reviewer = id == "reviewer";
    Some(AgentProfile {
        id: id.to_string(),
        name: name.to_string(),
        kind: ProfileKind::Builtin,
        roles,
        specialist: (id == "specialist").then_some(true),
        prompt_append: if id == "specialist" {
            SPECIALIST_PROMPT_APPEND.to_string()
        } else {
            String::new()
        },
        model: None,
        effort: None,
        tool_deny: Vec::new(),
        default_seat: seat,
        extra_allow: if reviewer {
            owned(&REVIEWER_ALLOW)
        } else {
            Vec::new()
        },
        extra_deny: if reviewer {
            owned(&REVIEWER_DENY)
        } else {
            Vec::new()
        },
        updated_at: 0,
    })
}

/// The seven built-in profiles in [`BUILTIN_PROFILE_IDS`] order.
pub fn builtin_profiles() -> Vec<AgentProfile> {
    BUILTIN_PROFILE_IDS
        .iter()
        .filter_map(|id| builtin_profile(id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn custom() -> AgentProfile {
        AgentProfile {
            id: "custom-0a1b2c3d".into(),
            name: "  Min profil ".into(),
            kind: ProfileKind::Builtin,
            roles: vec![Role::Reviewer, Role::Coder, Role::Coder],
            specialist: None,
            prompt_append: "Skriv kort.\r\n\u{7}Tak".into(),
            model: Some(" sonnet ".into()),
            effort: Some(Effort::High),
            tool_deny: vec!["mcp__mira-bots__mira_add_report".into()],
            default_seat: SeatKind::Work,
            extra_allow: vec![
                "Bash(npm test)".into(),
                " Bash(npm test) ".into(),
                "".into(),
            ],
            extra_deny: vec![],
            updated_at: 0,
        }
    }

    #[test]
    fn builtin_ids_and_roles() {
        let all = builtin_profiles();
        let ids: Vec<&str> = all.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, BUILTIN_PROFILE_IDS);
        for p in &all {
            assert_eq!(p.kind, ProfileKind::Builtin);
            assert_eq!((p.model.as_deref(), p.effort), (None, None), "{}", p.id);
            assert_eq!(
                p.clone().validated().unwrap(),
                *p,
                "{} is valid as is",
                p.id
            );
        }
        let roles_of = |id: &str| builtin_profile(id).unwrap().roles;
        assert_eq!(roles_of("coder"), [Role::Coder]);
        assert_eq!(roles_of("researcher"), [Role::Researcher]);
        assert_eq!(roles_of("reviewer"), [Role::Reviewer]);
        assert_eq!(roles_of("coordinator"), [Role::Coordinator]);
        assert_eq!(roles_of("planner"), [Role::Planner]);
        assert_eq!(roles_of("debugger"), [Role::Debugger]);
        assert_eq!(roles_of("specialist"), Role::ALL);
        let seat = |id: &str| builtin_profile(id).unwrap().default_seat;
        assert_eq!(seat("reviewer"), SeatKind::Staff);
        assert_eq!(seat("coordinator"), SeatKind::Staff);
        assert_eq!(seat("planner"), SeatKind::Staff);
        for id in ["coder", "researcher", "debugger", "specialist"] {
            assert_eq!(seat(id), SeatKind::Work, "{id}");
        }
        let names: Vec<String> = all.iter().map(|p| p.name.clone()).collect();
        assert_eq!(
            names,
            [
                "Koder",
                "Researcher",
                "Reviewer",
                "Koordinator",
                "Planlægger",
                "Debugger",
                "Specialist (alle roller)"
            ]
        );
        assert!(builtin_profile("custom-x").is_none());
        // The reviewer file matches C5.2 exactly.
        assert_eq!(
            serde_json::to_value(builtin_profile("reviewer").unwrap()).unwrap(),
            json!({ "id": "reviewer", "name": "Reviewer", "kind": "builtin", "roles": ["reviewer"],
                "specialist": null, "promptAppend": "", "model": null, "effort": null, "toolDeny": [],
                "defaultSeat": "staff",
                "extraAllow": ["Bash(git -C * diff *)", "Bash(git -C * log *)", "Bash(git -C * status *)", "Bash(git -C * show *)"],
                "extraDeny": ["Bash(git commit *)", "Bash(git push *)", "Bash(git -C * commit *)",
                    "Bash(git -C * push *)"], "updatedAt": 0 })
        );
        let spec = builtin_profile("specialist").unwrap();
        assert_eq!(spec.specialist, Some(true));
        assert_eq!(spec.prompt_append, SPECIALIST_PROMPT_APPEND);
    }

    #[test]
    fn validate_normalises_a_good_profile() {
        let p = custom().validated().unwrap();
        assert_eq!(p.kind, ProfileKind::Custom, "kind follows the id");
        assert_eq!(p.name, "Min profil");
        assert_eq!(p.roles, [Role::Coder, Role::Reviewer]);
        assert_eq!(p.prompt_append, "Skriv kort.\nTak");
        assert_eq!(p.model.as_deref(), Some("sonnet"));
        assert_eq!(p.tool_deny, ["mira_add_report"]);
        assert_eq!(p.extra_allow, ["Bash(npm test)"]);
        let blank_model = AgentProfile {
            model: Some("  ".into()),
            ..custom()
        };
        assert_eq!(blank_model.validated().unwrap().model, None);
        let builtin_id = AgentProfile {
            id: "reviewer".into(),
            ..custom()
        };
        assert_eq!(builtin_id.validated().unwrap().kind, ProfileKind::Builtin);
        let no_roles = AgentProfile {
            roles: vec![],
            ..custom()
        };
        assert!(
            no_roles.validated().is_ok(),
            "an empty role list is allowed"
        );
    }

    #[test]
    fn validate_rejects_each_bad_field() {
        let err = |p: AgentProfile| p.validated().unwrap_err();
        for id in ["", "Custom", "-x", "a b", "æ", "a_b", &"a".repeat(41)] {
            let p = AgentProfile {
                id: id.to_string(),
                ..custom()
            };
            assert_eq!(err(p), ProfileError::BadId, "{id:?}");
        }
        for name in ["", "   ", &"n".repeat(61), "a\u{1}b"] {
            let p = AgentProfile {
                name: name.to_string(),
                ..custom()
            };
            assert_eq!(err(p), ProfileError::BadName, "{name:?}");
        }
        assert!(AgentProfile {
            name: "n".repeat(60),
            ..custom()
        }
        .validated()
        .is_ok());
        let p = AgentProfile {
            prompt_append: "x".repeat(4001),
            ..custom()
        };
        assert_eq!(err(p), ProfileError::PromptTooLong);
        let p = AgentProfile {
            model: Some("bogus".into()),
            ..custom()
        };
        assert_eq!(err(p), ProfileError::UnknownModel);
        let p = AgentProfile {
            tool_deny: vec!["Bash".into()],
            ..custom()
        };
        assert_eq!(err(p), ProfileError::UnknownTool("Bash".into()));
        let p = AgentProfile {
            extra_allow: (0..21).map(|i| format!("Bash(x{i})")).collect(),
            ..custom()
        };
        assert_eq!(err(p), ProfileError::BadExtraRules("extraAllow"));
        let p = AgentProfile {
            extra_deny: vec!["x".repeat(201)],
            ..custom()
        };
        assert_eq!(err(p), ProfileError::BadExtraRules("extraDeny"));
        let p = AgentProfile {
            extra_deny: vec!["Bash(a)\nBash(b)".into()],
            ..custom()
        };
        assert_eq!(err(p), ProfileError::BadExtraRules("extraDeny"));
        // Danish texts (C5.6).
        assert_eq!(
            ProfileError::BadId.to_string(),
            "Profil-id må kun indeholde små bogstaver, tal og bindestreg"
        );
        assert_eq!(
            ProfileError::BadName.to_string(),
            "Navn skal være 1–60 tegn"
        );
        assert_eq!(
            ProfileError::UnknownModel.to_string(),
            "Ukendt model: brug et alias (sonnet, opus, haiku, fable, best, opusplan, sonnet[1m], opus[1m]) eller et fuldt id som claude-sonnet-5-5"
        );
        assert_eq!(
            ProfileError::UnknownTool("x".into()).to_string(),
            "Ukendt værktøj i toolDeny: x"
        );
    }

    #[test]
    fn model_is_valid_table() {
        for ok in MODEL_ALIASES {
            assert!(model_is_valid(ok), "{ok}");
        }
        for ok in [
            "claude-sonnet-5-5",
            "claude-opus-5-5[1m]",
            "claude-haiku-4-5-20251001",
            "claude-x",
        ] {
            assert!(model_is_valid(ok), "{ok}");
        }
        let long = format!("claude-{}", "a".repeat(58));
        assert_eq!(long.len(), 65);
        for bad in [
            "bogus",
            "Claude-x",
            "claude-",
            "claude-[1m]",
            "claude-sonnet 5",
            " sonnet",
            "claude-Sonnet",
            "claude-x[2m]",
            "",
            long.as_str(),
        ] {
            assert!(!model_is_valid(bad), "{bad:?}");
        }
        assert!(model_is_valid(&format!("claude-{}", "a".repeat(57))));
    }

    #[test]
    fn effort_wire_and_danish_error() {
        for (e, s) in Effort::ALL
            .into_iter()
            .zip(["low", "medium", "high", "xhigh", "max"])
        {
            assert_eq!(serde_json::to_value(e).unwrap(), json!(s));
            assert_eq!(serde_json::from_value::<Effort>(json!(s)).unwrap(), e);
        }
        let err = serde_json::from_value::<Effort>(json!("ultra")).unwrap_err();
        assert!(err.to_string().contains("Ukendt effort-niveau"), "{err}");
        assert!(serde_json::from_value::<SpawnOverrides>(json!({"effort":"HIGH"})).is_err());
        assert_eq!(
            serde_json::from_value::<SpawnOverrides>(json!({})).unwrap(),
            SpawnOverrides::default()
        );
    }

    #[test]
    fn specialist_is_derived_when_absent() {
        let with = |roles: Vec<Role>, specialist: Option<bool>| AgentProfile {
            roles,
            specialist,
            ..custom()
        };
        assert!(!with(vec![Role::Coder], None).is_specialist());
        assert!(with(vec![Role::Coder, Role::Reviewer], None).is_specialist());
        assert!(with(vec![], None).is_specialist());
        assert!(with(vec![Role::Coder], Some(true)).is_specialist());
        assert!(!with(vec![], Some(false)).is_specialist());
        assert!(builtin_profile("specialist").unwrap().is_specialist());
        assert!(!builtin_profile("reviewer").unwrap().is_specialist());
    }

    fn mira(tools: &[&str]) -> Vec<String> {
        tools
            .iter()
            .map(|t| format!("mcp__mira-bots__{t}"))
            .collect()
    }

    fn owned(v: &[&str]) -> Vec<String> {
        v.iter().map(|t| t.to_string()).collect()
    }

    #[test]
    fn deny_rules_for_each_builtin() {
        let coordinator_set = mira(&[
            "mira_assign_ticket",
            "mira_list_profiles",
            "mira_spawn_agent",
            "mira_unassign_ticket",
        ]);
        let all_bound = mira(&[
            "mira_approve_ticket",
            "mira_assign_ticket",
            "mira_list_profiles",
            "mira_reject_ticket",
            "mira_spawn_agent",
            "mira_unassign_ticket",
        ]);
        let edit = owned(&["Edit", "Write", "MultiEdit", "NotebookEdit"]);
        let push = owned(&["Bash(git push *)", "Bash(git -C * push *)"]);
        let root = Path::new("/home/x/mira-bots/projects");
        let locked = owned(&[
            "Edit(//home/x/mira-bots/projects/mira-bots.workspace.json)",
            "Edit(//home/x/mira-bots/projects/**/.mira-bots/project.json)",
            "Edit(**/.mira-bots/project.json)",
        ]);
        let deny = |id: &str| builtin_profile(id).unwrap().deny_rules(None);
        // Work roles: the role-bound tools, then the push deny (step 6b).
        for id in ["coder", "researcher", "debugger"] {
            let mut want = all_bound.clone();
            want.extend(push.iter().cloned());
            assert_eq!(deny(id), want, "{id}");
            // With the projects root: the three locked-file rules after the push deny.
            want.extend(locked.iter().cloned());
            assert_eq!(
                builtin_profile(id).unwrap().deny_rules(Some(root)),
                want,
                "{id}"
            );
        }
        // Without a work role: the four file-editing tools after the MCP tools.
        let mut want = all_bound.clone();
        want.extend(edit.iter().cloned());
        want.extend(push.iter().cloned());
        assert_eq!(deny("planner"), want);
        // The reviewer's own push rules are not repeated; commit stays.
        let mut want = coordinator_set.clone();
        want.extend(edit.iter().cloned());
        want.extend(push.iter().cloned());
        want.extend(owned(&["Bash(git commit *)", "Bash(git -C * commit *)"]));
        assert_eq!(deny("reviewer"), want);
        let mut want = mira(&["mira_approve_ticket", "mira_reject_ticket"]);
        want.extend(edit.iter().cloned());
        want.extend(push.iter().cloned());
        assert_eq!(deny("coordinator"), want);
        // The specialist (every role, no narrowing) is no longer empty: push deny, and with the
        // root the three Edit rules.
        assert_eq!(deny("specialist"), push);
        let mut want = push.clone();
        want.extend(locked.iter().cloned());
        assert_eq!(
            builtin_profile("specialist")
                .unwrap()
                .deny_rules(Some(root)),
            want
        );
        for p in builtin_profiles() {
            let rules = p.deny_rules(Some(root));
            let denies_edit = rules.iter().any(|r| r == "Edit");
            let staff_only = ["reviewer", "coordinator", "planner"].contains(&p.id.as_str());
            assert_eq!(denies_edit, staff_only, "{}", p.id);
            for r in push.iter().chain(&locked) {
                assert_eq!(rules.iter().filter(|x| *x == r).count(), 1, "{}: {r}", p.id);
            }
            // Never a Write(<path>) rule (research6b T3: not consulted).
            assert!(!rules.iter().any(|r| r.starts_with("Write(")), "{}", p.id);
        }
        // A custom profile without a work role (and without roles) gets them too, before
        // extraDeny, without duplicates.
        let p = AgentProfile {
            roles: vec![],
            tool_deny: vec![],
            extra_deny: vec!["Write".into(), "Bash(rm *)".into()],
            ..custom()
        };
        let mut want = all_bound.clone();
        want.extend(edit.iter().cloned());
        want.extend(push.iter().cloned());
        want.push("Bash(rm *)".into());
        assert_eq!(p.deny_rules(None), want);
        // One work role is enough.
        let p = AgentProfile {
            roles: vec![Role::Coordinator, Role::Researcher],
            tool_deny: vec![],
            ..custom()
        };
        assert!(!p.deny_rules(None).iter().any(|r| edit.contains(r)));
        // toolDeny is added (sorted in), extraDeny follows.
        let p = AgentProfile {
            roles: Role::ALL.to_vec(),
            tool_deny: vec!["mira_get_report".into(), "mira_add_report".into()],
            extra_deny: vec!["Bash(rm *)".into()],
            ..custom()
        };
        let mut want = mira(&["mira_add_report", "mira_get_report"]);
        want.extend(push.iter().cloned());
        want.push("Bash(rm *)".into());
        assert_eq!(p.deny_rules(None), want);
        assert_eq!(
            builtin_profile("reviewer").unwrap().allow_rules(),
            [
                "mcp__mira-bots__*",
                "Bash(git -C * diff *)",
                "Bash(git -C * log *)",
                "Bash(git -C * status *)",
                "Bash(git -C * show *)"
            ]
        );
        assert_eq!(
            builtin_profile("coder").unwrap().allow_rules(),
            ["mcp__mira-bots__*"]
        );
    }

    #[test]
    fn deny_rules_without_root_have_no_edit_paths() {
        for p in builtin_profiles() {
            let rules = p.deny_rules(None);
            assert!(
                !rules.iter().any(|r| r.starts_with("Edit(")),
                "{}: {rules:?}",
                p.id
            );
            assert!(rules.iter().any(|r| r == "Bash(git push *)"), "{}", p.id);
        }
    }

    #[test]
    fn reviewer_deny_has_no_duplicate_push() {
        let rules = builtin_profile("reviewer")
            .unwrap()
            .deny_rules(Some(Path::new("/r")));
        for r in &rules {
            assert_eq!(rules.iter().filter(|x| *x == r).count(), 1, "{r}");
        }
        assert!(rules.contains(&"Bash(git commit *)".to_string()));
        assert!(rules.contains(&"Bash(git -C * commit *)".to_string()));
        // An extraAllow for push cannot lift the deny (deny wins in Claude Code); the rule
        // stays in the deny list.
        let p = AgentProfile {
            extra_allow: vec!["Bash(git push *)".into()],
            ..builtin_profile("coder").unwrap()
        };
        assert!(p.deny_rules(None).contains(&"Bash(git push *)".to_string()));
    }

    #[test]
    fn permission_path_unix_and_windows_forms() {
        for (input, want) in [
            ("/home/x", "//home/x"),
            ("/home/x/", "//home/x"),
            (
                "/Users/Ann Lee/mira-bots/projects",
                "//Users/Ann Lee/mira-bots/projects",
            ),
            ("/", "//"),
        ] {
            assert_eq!(permission_path_str(input, false), want, "{input}");
        }
        for (input, want) in [
            (r"C:\Users\x", "//c/Users/x"),
            (r"C:\Users\x\", "//c/Users/x"),
            (r"d:\Projekter (x86)\mira", "//d/Projekter (x86)/mira"),
            (r"C:\", "//c"),
            (r"\\?\C:\Users\x", "//c/Users/x"),
            (r"\\?\UNC\server\share\x", "//server/share/x"),
            (r"\\server\share\x", "//server/share/x"),
            ("C:/Users/x", "//c/Users/x"),
        ] {
            assert_eq!(permission_path_str(input, true), want, "{input}");
        }
        assert_eq!(
            permission_path(Path::new(if cfg!(windows) { r"C:\a\b" } else { "/a/b" })),
            if cfg!(windows) { "//c/a/b" } else { "//a/b" }
        );
        // The filesystem root as projects root: still absolute (`//`).
        assert_eq!(
            locked_file_rules(Path::new("/")),
            [
                "Edit(//mira-bots.workspace.json)",
                "Edit(//**/.mira-bots/project.json)",
                "Edit(**/.mira-bots/project.json)"
            ]
        );
    }

    #[test]
    fn check_seat_needs_staff_role_on_staff_seat() {
        for p in builtin_profiles() {
            assert_eq!(p.check_seat(SeatKind::Work), Ok(()), "{}", p.id);
        }
        for id in ["reviewer", "coordinator", "planner", "specialist"] {
            let p = builtin_profile(id).unwrap();
            assert_eq!(p.check_seat(SeatKind::Staff), Ok(()), "{id}");
        }
        for (id, name) in [
            ("coder", "Koder"),
            ("researcher", "Researcher"),
            ("debugger", "Debugger"),
        ] {
            assert_eq!(
                builtin_profile(id).unwrap().check_seat(SeatKind::Staff),
                Err(format!(
                    "Profilen «{name}» har ingen stabsrolle (reviewer, koordinator eller planlægger) og kan ikke stå på en stabsplads"
                )),
                "{id}"
            );
        }
        // A custom profile with defaultSeat staff but no staff role is refused the same way.
        let p = AgentProfile {
            name: "Egen".into(),
            roles: vec![],
            default_seat: SeatKind::Staff,
            ..custom()
        };
        assert!(p
            .check_seat(SeatKind::Staff)
            .unwrap_err()
            .contains("«Egen»"));
    }

    #[test]
    fn snapshot_prefers_overrides() {
        let p = AgentProfile {
            model: Some("opus".into()),
            effort: Some(Effort::Low),
            ..builtin_profile("reviewer").unwrap()
        };
        let s = p.snapshot(&SpawnOverrides::default());
        assert_eq!(
            (s.profile_id.as_str(), s.profile_name.as_str(), s.specialist),
            ("reviewer", "Reviewer", false)
        );
        assert_eq!(
            (s.model.as_deref(), s.effort),
            (Some("opus"), Some(Effort::Low))
        );
        let s = p.snapshot(&SpawnOverrides {
            model: Some("haiku".into()),
            effort: Some(Effort::Max),
        });
        assert_eq!(
            (s.model.as_deref(), s.effort),
            (Some("haiku"), Some(Effort::Max))
        );
        assert_eq!(
            validate_overrides(SpawnOverrides {
                model: Some("bogus".into()),
                effort: None
            }),
            Err(ProfileError::UnknownModel)
        );
        assert_eq!(
            validate_overrides(SpawnOverrides {
                model: Some(" ".into()),
                effort: Some(Effort::High)
            })
            .unwrap(),
            SpawnOverrides {
                model: None,
                effort: Some(Effort::High)
            }
        );
        let id = new_custom_id();
        assert!(
            id.starts_with("custom-") && id.len() == 15 && id_is_valid(&id),
            "{id}"
        );
    }
}
