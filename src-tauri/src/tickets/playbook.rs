//! Playbooks (step 6b, plan6b A.2): a ticket's `kind` names a playbook whose steps become child
//! tickets. This file holds the playbook types, the two built-in playbooks (`feature`, `bug`,
//! C6b.2 verbatim) and the validation of the workspace file's `playbooks` object. The rollout
//! itself (`start_playbook`) comes in batch 2.
//!
//! Validation never rejects the workspace file: a malformed playbook or an unknown role gives a
//! note and drops only that playbook (the built-in one of the same name, if any, stays).

use std::collections::BTreeMap;

use serde_json::Value;

use crate::agent::roles::Role;
use crate::config::{PLAYBOOK_STEPS_MAX, TICKET_BODY_MAX_CHARS};

/// Most chars of a step title (one line).
pub const PLAYBOOK_TITLE_MAX_CHARS: usize = 200;
/// Most chars of a playbook name (`^[a-z0-9_-]{1,32}$`, the same rule as a ticket's `kind`).
pub const PLAYBOOK_NAME_MAX_CHARS: usize = 32;
/// The reserved kind of a plain ticket; never a playbook name.
pub const TASK_KIND: &str = "task";

/// A playbook: its steps in order (1..=[`PLAYBOOK_STEPS_MAX`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Playbook {
    pub steps: Vec<PlaybookStep>,
}

/// One step: a child ticket for an agent with `role`. `title`/`body` are templates with
/// `{title}`, `{body}` and `{parent}`. `blocked_by_previous` is always false for the first step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaybookStep {
    pub role: Role,
    pub title: String,
    pub body: String,
    pub blocked_by_previous: bool,
}

/// `^[a-z0-9_-]{1,32}$`.
pub fn is_playbook_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= PLAYBOOK_NAME_MAX_CHARS
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn step(role: Role, title: &str, body: &str, blocked_by_previous: bool) -> PlaybookStep {
    PlaybookStep {
        role,
        title: title.to_string(),
        body: body.to_string(),
        blocked_by_previous,
    }
}

/// The built-in playbooks `feature` (planner → coder) and `bug` (debugger → coder), C6b.2.
pub fn builtin_playbooks() -> BTreeMap<String, Playbook> {
    let feature = Playbook {
        steps: vec![
            step(
                Role::Planner,
                "Plan: {title}",
                "Lav en plan for «{title}» (forældre-ticket {parent}). Nedbryd i små, ordnede \
                 del-opgaver med acceptkriterier, og læg planen som rapport (mira_add_report). \
                 Opret ikke tickets selv.\n\n{body}",
                false,
            ),
            step(
                Role::Coder,
                "Byg: {title}",
                "Implementér «{title}» efter planen i rapporterne på ticket {parent} \
                 (mira_get_ticket {parent}, mira_get_report). Kør projektets tjek, commit på \
                 ticketens branch og aflever med en rapport over ændringerne.\n\n{body}",
                true,
            ),
        ],
    };
    let bug = Playbook {
        steps: vec![
            step(
                Role::Debugger,
                "Find årsag: {title}",
                "Reproducér fejlen «{title}» (forældre-ticket {parent}), find årsagen og skriv \
                 reproduktion, årsag og foreslået rettelse som rapport (mira_add_report). Ret \
                 kun hvis rettelsen er lille og sikker.\n\n{body}",
                false,
            ),
            step(
                Role::Coder,
                "Ret: {title}",
                "Ret fejlen «{title}» ud fra debuggerens rapport på ticket {parent} \
                 (mira_get_ticket {parent}, mira_get_report). Tilføj en test der fanger den, kør \
                 projektets tjek, commit på ticketens branch og aflever med en rapport.\n\n{body}",
                true,
            ),
        ],
    };
    BTreeMap::from([("bug".to_string(), bug), ("feature".to_string(), feature)])
}

/// Why one playbook value is not usable (the text after "playbooks.{k} …").
enum Invalid {
    /// Note "playbooks.{k}: ukendt rolle «{r}»".
    UnknownRole(String),
    /// Note "playbooks.{k} ignoreres: {reason}".
    Shape(String),
}

fn parse_step(i: usize, v: &Value) -> Result<PlaybookStep, Invalid> {
    let n = i + 1;
    let o = v
        .as_object()
        .ok_or_else(|| Invalid::Shape(format!("trin {n} skal være et objekt")))?;
    let role = match o.get("role") {
        Some(Value::String(r)) => Role::parse(&r.trim().to_ascii_lowercase())
            .ok_or_else(|| Invalid::UnknownRole(r.clone()))?,
        _ => return Err(Invalid::Shape(format!("trin {n} mangler role"))),
    };
    let title = match o.get("title") {
        Some(Value::String(t)) => t.trim(),
        _ => return Err(Invalid::Shape(format!("trin {n} mangler title"))),
    };
    if title.is_empty()
        || title.chars().count() > PLAYBOOK_TITLE_MAX_CHARS
        || title.contains(['\n', '\r'])
    {
        return Err(Invalid::Shape(format!(
            "trin {n}: title skal være 1–{PLAYBOOK_TITLE_MAX_CHARS} tegn på én linje"
        )));
    }
    let body = match o.get("body") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(b)) if b.chars().count() <= TICKET_BODY_MAX_CHARS => b.clone(),
        Some(_) => {
            return Err(Invalid::Shape(format!(
                "trin {n}: body skal være en tekst på højst {TICKET_BODY_MAX_CHARS} tegn"
            )))
        }
    };
    let blocked = match o.get("blockedByPrevious") {
        None | Some(Value::Null) => i > 0,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(Invalid::Shape(format!(
                "trin {n}: blockedByPrevious skal være true eller false"
            )))
        }
    };
    Ok(PlaybookStep {
        role,
        title: title.to_string(),
        body,
        // The first step has nothing before it.
        blocked_by_previous: i > 0 && blocked,
    })
}

fn parse_playbook(v: &Value) -> Result<Playbook, Invalid> {
    let steps = v
        .as_object()
        .and_then(|o| o.get("steps"))
        .and_then(Value::as_array)
        .filter(|s| (1..=PLAYBOOK_STEPS_MAX).contains(&s.len()))
        .ok_or_else(|| {
            Invalid::Shape(format!(
                "steps skal være en liste med 1–{PLAYBOOK_STEPS_MAX} trin"
            ))
        })?;
    let steps = steps
        .iter()
        .enumerate()
        .map(|(i, s)| parse_step(i, s))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Playbook { steps })
}

/// The workspace file's `playbooks` value → the valid playbooks in it (C6b.2), with a note per
/// dropped one: not an object → "playbooks ignoreres: skal være et objekt"; a bad name (not
/// `^[a-z0-9_-]{1,32}$`, or the reserved `task`) → "playbooks.{k} ignoreres: ugyldigt navn"; an
/// unknown role → "playbooks.{k}: ukendt rolle «{r}»"; any other shape error →
/// "playbooks.{k} ignoreres: {reason}". Roles are matched trimmed and ASCII-case-insensitively.
/// The caller merges the result over [`builtin_playbooks`].
pub fn validate_playbooks(v: &Value, notes: &mut Vec<String>) -> BTreeMap<String, Playbook> {
    let mut out = BTreeMap::new();
    let Some(map) = v.as_object() else {
        notes.push("playbooks ignoreres: skal være et objekt".to_string());
        return out;
    };
    for (k, pv) in map {
        if !is_playbook_name(k) || k == TASK_KIND {
            notes.push(format!("playbooks.{k} ignoreres: ugyldigt navn"));
            continue;
        }
        match parse_playbook(pv) {
            Ok(p) => {
                out.insert(k.clone(), p);
            }
            Err(Invalid::UnknownRole(r)) => {
                notes.push(format!("playbooks.{k}: ukendt rolle «{r}»"))
            }
            Err(Invalid::Shape(why)) => notes.push(format!("playbooks.{k} ignoreres: {why}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builtin_playbooks_are_feature_and_bug() {
        let b = builtin_playbooks();
        assert_eq!(b.keys().collect::<Vec<_>>(), ["bug", "feature"]);
        let roles = |k: &str| b[k].steps.iter().map(|s| s.role).collect::<Vec<_>>();
        assert_eq!(roles("feature"), [Role::Planner, Role::Coder]);
        assert_eq!(roles("bug"), [Role::Debugger, Role::Coder]);
        for p in b.values() {
            let blocked: Vec<bool> = p.steps.iter().map(|s| s.blocked_by_previous).collect();
            assert_eq!(blocked, [false, true]);
            assert!(p.steps.len() <= PLAYBOOK_STEPS_MAX);
            for s in &p.steps {
                assert!(s.title.contains("{title}") && !s.title.contains('\n'));
                assert!(s.body.ends_with("\n\n{body}") && s.body.contains("{parent}"));
            }
        }
        let f = &b["feature"].steps;
        assert_eq!(f[0].title, "Plan: {title}");
        assert_eq!(
            f[0].body,
            "Lav en plan for «{title}» (forældre-ticket {parent}). Nedbryd i små, ordnede \
             del-opgaver med acceptkriterier, og læg planen som rapport (mira_add_report). Opret \
             ikke tickets selv.\n\n{body}"
        );
        assert_eq!(f[1].title, "Byg: {title}");
        assert_eq!(
            f[1].body,
            "Implementér «{title}» efter planen i rapporterne på ticket {parent} (mira_get_ticket \
             {parent}, mira_get_report). Kør projektets tjek, commit på ticketens branch og \
             aflever med en rapport over ændringerne.\n\n{body}"
        );
        let g = &b["bug"].steps;
        assert_eq!(g[0].title, "Find årsag: {title}");
        assert_eq!(
            g[0].body,
            "Reproducér fejlen «{title}» (forældre-ticket {parent}), find årsagen og skriv \
             reproduktion, årsag og foreslået rettelse som rapport (mira_add_report). Ret kun \
             hvis rettelsen er lille og sikker.\n\n{body}"
        );
        assert_eq!(g[1].title, "Ret: {title}");
        assert_eq!(
            g[1].body,
            "Ret fejlen «{title}» ud fra debuggerens rapport på ticket {parent} (mira_get_ticket \
             {parent}, mira_get_report). Tilføj en test der fanger den, kør projektets tjek, \
             commit på ticketens branch og aflever med en rapport.\n\n{body}"
        );
    }

    #[test]
    fn playbook_names() {
        for ok in ["docs", "a", "x_1-y", &"a".repeat(32)] {
            assert!(is_playbook_name(ok), "{ok}");
        }
        for bad in ["", "Docs", "a b", "æ", "a.b", &"a".repeat(33)] {
            assert!(!is_playbook_name(bad), "{bad}");
        }
    }

    fn one(v: Value) -> (BTreeMap<String, Playbook>, Vec<String>) {
        let mut notes = Vec::new();
        let m = validate_playbooks(&v, &mut notes);
        (m, notes)
    }

    #[test]
    fn validate_playbooks_table() {
        // A valid playbook: defaults for body and blockedByPrevious, role case-insensitive.
        let (m, notes) = one(json!({"docs": {"steps": [
            {"role": "researcher", "title": " Find kilder: {title} "},
            {"role": "Coder", "title": "Skriv: {title}", "body": "B {body}"},
            {"role": "reviewer", "title": "Læs", "blockedByPrevious": false}
        ]}}));
        assert!(notes.is_empty(), "{notes:?}");
        let s = &m["docs"].steps;
        assert_eq!(
            s.iter().map(|x| x.role).collect::<Vec<_>>(),
            [Role::Researcher, Role::Coder, Role::Reviewer]
        );
        assert_eq!(s[0].title, "Find kilder: {title}");
        assert_eq!((s[0].body.as_str(), s[1].body.as_str()), ("", "B {body}"));
        assert_eq!(
            s.iter().map(|x| x.blocked_by_previous).collect::<Vec<_>>(),
            [false, true, false]
        );
        // blockedByPrevious on the first step is ignored.
        let (m, _) = one(
            json!({"x": {"steps": [{"role": "coder", "title": "t", "blockedByPrevious": true}]}}),
        );
        assert!(!m["x"].steps[0].blocked_by_previous);

        // Each invalid case: exactly one note, the playbook is dropped.
        let step = json!({"role": "coder", "title": "t"});
        let seven: Vec<Value> = (0..7).map(|_| step.clone()).collect();
        let table: Vec<(Value, &str)> = vec![
            (json!([1, 2]), "playbooks ignoreres: skal være et objekt"),
            (json!("feature"), "playbooks ignoreres: skal være et objekt"),
            (
                json!({"Docs": {"steps": [step]}}),
                "playbooks.Docs ignoreres: ugyldigt navn",
            ),
            (
                json!({"a b": {"steps": [step]}}),
                "playbooks.a b ignoreres: ugyldigt navn",
            ),
            (
                json!({"docs": {"steps": [{"role": "tester", "title": "t"}]}}),
                "playbooks.docs: ukendt rolle «tester»",
            ),
            (
                json!({"docs": {"steps": [step, {"role": "boss", "title": "t"}]}}),
                "playbooks.docs: ukendt rolle «boss»",
            ),
            (
                json!({"docs": []}),
                "playbooks.docs ignoreres: steps skal være en liste med 1–6 trin",
            ),
            (
                json!({"docs": {"steps": []}}),
                "playbooks.docs ignoreres: steps skal være en liste med 1–6 trin",
            ),
            (
                json!({"docs": {"steps": seven}}),
                "playbooks.docs ignoreres: steps skal være en liste med 1–6 trin",
            ),
            (
                json!({"docs": {"steps": "coder"}}),
                "playbooks.docs ignoreres: steps skal være en liste med 1–6 trin",
            ),
            (
                json!({"docs": {"steps": [step, 3]}}),
                "playbooks.docs ignoreres: trin 2 skal være et objekt",
            ),
            (
                json!({"docs": {"steps": [{"title": "t"}]}}),
                "playbooks.docs ignoreres: trin 1 mangler role",
            ),
            (
                json!({"docs": {"steps": [{"role": 1, "title": "t"}]}}),
                "playbooks.docs ignoreres: trin 1 mangler role",
            ),
            (
                json!({"docs": {"steps": [{"role": "coder"}]}}),
                "playbooks.docs ignoreres: trin 1 mangler title",
            ),
            (
                json!({"docs": {"steps": [{"role": "coder", "title": "  "}]}}),
                "playbooks.docs ignoreres: trin 1: title skal være 1–200 tegn på én linje",
            ),
            (
                json!({"docs": {"steps": [{"role": "coder", "title": "a\nb"}]}}),
                "playbooks.docs ignoreres: trin 1: title skal være 1–200 tegn på én linje",
            ),
            (
                json!({"docs": {"steps": [{"role": "coder", "title": "x".repeat(201)}]}}),
                "playbooks.docs ignoreres: trin 1: title skal være 1–200 tegn på én linje",
            ),
            (
                json!({"docs": {"steps": [{"role": "coder", "title": "t", "body": 5}]}}),
                "playbooks.docs ignoreres: trin 1: body skal være en tekst på højst 20000 tegn",
            ),
            (
                json!({"docs": {"steps": [{"role": "coder", "title": "t",
                    "body": "x".repeat(20_001)}]}}),
                "playbooks.docs ignoreres: trin 1: body skal være en tekst på højst 20000 tegn",
            ),
            (
                json!({"docs": {"steps": [{"role": "coder", "title": "t",
                    "blockedByPrevious": "yes"}]}}),
                "playbooks.docs ignoreres: trin 1: blockedByPrevious skal være true eller false",
            ),
        ];
        for (v, note) in table {
            let (m, notes) = one(v.clone());
            assert!(m.is_empty(), "{v}");
            assert_eq!(notes, [note], "{v}");
        }
        // A 200-char title and a 20000-char body are fine.
        let (m, notes) = one(json!({"docs": {"steps": [{"role": "coder",
            "title": "x".repeat(200), "body": "y".repeat(20_000)}]}}));
        assert!(notes.is_empty() && m.len() == 1, "{notes:?}");
        // Bad and good side by side: only the bad one is dropped.
        let (m, notes) = one(json!({
            "docs": {"steps": [step]},
            "bad": {"steps": [{"role": "x", "title": "t"}]}
        }));
        assert_eq!(m.keys().collect::<Vec<_>>(), ["docs"]);
        assert_eq!(notes, ["playbooks.bad: ukendt rolle «x»"]);
    }
}
