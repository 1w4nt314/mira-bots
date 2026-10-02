//! The seventeen tools (plan4 C4.3 + plan5 C5.3 + step 5c's handoff + step 4b's project list;
//! eleven common): `tools/list` definitions, the role matrix and
//! argument validation before anything is sent to the app. The app validates again (C4.12) and
//! enforces the role matrix itself (the security boundary); this layer gives the model a quick,
//! precise error without a pipe round trip and only lists the tools its roles allow.

use serde_json::{json, Map, Value};

pub const CREATE_TICKET: &str = "mira_create_ticket";
pub const LIST_TICKETS: &str = "mira_list_tickets";
pub const GET_TICKET: &str = "mira_get_ticket";
pub const SUBMIT_FOR_REVIEW: &str = "mira_submit_for_review";
pub const UPDATE_STATUS: &str = "mira_update_status";

// Step 5 tools (plan5 C5.3).
pub const APPROVE_TICKET: &str = "mira_approve_ticket";
pub const REJECT_TICKET: &str = "mira_reject_ticket";
pub const ASSIGN_TICKET: &str = "mira_assign_ticket";
pub const UNASSIGN_TICKET: &str = "mira_unassign_ticket";
pub const SPAWN_AGENT: &str = "mira_spawn_agent";
pub const LIST_AGENTS: &str = "mira_list_agents";
pub const LIST_PROFILES: &str = "mira_list_profiles";
pub const GET_WORKSPACE_RULES: &str = "mira_get_workspace_rules";
pub const ADD_REPORT: &str = "mira_add_report";
pub const GET_REPORT: &str = "mira_get_report";
/// Step 5c: the assignee hands its ticket in progress to another agent, or back to the backlog.
pub const HANDOFF_TICKET: &str = "mira_handoff_ticket";
/// Step 4b: the project folders under the projects root (plan4b C4b.5).
pub const LIST_PROJECTS: &str = "mira_list_projects";

/// Tools every agent has, whatever its roles (plan5 A.2).
/// `mira_list_agents` is common since step 5c (read-only): any agent handing a ticket on with
/// `mira_handoff_ticket` must be able to find a free work agent.
/// `mira_list_projects` is common since step 4b (read-only): every ticket needs a project.
pub const COMMON_TOOLS: [&str; 11] = [
    CREATE_TICKET,
    LIST_TICKETS,
    GET_TICKET,
    SUBMIT_FOR_REVIEW,
    UPDATE_STATUS,
    GET_WORKSPACE_RULES,
    ADD_REPORT,
    GET_REPORT,
    HANDOFF_TICKET,
    LIST_AGENTS,
    LIST_PROJECTS,
];

/// Tools only some roles have (the union of [`ROLE_TOOLS`]).
pub const ROLE_BOUND_TOOLS: [&str; 6] = [
    APPROVE_TICKET,
    REJECT_TICKET,
    ASSIGN_TICKET,
    UNASSIGN_TICKET,
    SPAWN_AGENT,
    LIST_PROFILES,
];

/// Every tool name (plan5 C5.8), common ones first; the order of [`definitions`].
pub const TOOL_NAMES: [&str; 17] = ALL_TOOL_NAMES;

/// Every tool name of plan5 C5.3 (+ step 5c's handoff, step 4b's project list), common ones
/// first.
pub const ALL_TOOL_NAMES: [&str; 17] = [
    CREATE_TICKET,
    LIST_TICKETS,
    GET_TICKET,
    SUBMIT_FOR_REVIEW,
    UPDATE_STATUS,
    GET_WORKSPACE_RULES,
    ADD_REPORT,
    GET_REPORT,
    HANDOFF_TICKET,
    LIST_AGENTS,
    LIST_PROJECTS,
    APPROVE_TICKET,
    REJECT_TICKET,
    ASSIGN_TICKET,
    UNASSIGN_TICKET,
    SPAWN_AGENT,
    LIST_PROFILES,
];

/// The role matrix (plan5 A.2): role wire name → the tools that role adds to [`COMMON_TOOLS`].
/// The only copy: the app's tool gate and deny rules use this table too.
pub const ROLE_TOOLS: &[(&str, &[&str])] = &[
    ("coder", &[]),
    ("researcher", &[]),
    ("reviewer", &[APPROVE_TICKET, REJECT_TICKET]),
    (
        "coordinator",
        &[ASSIGN_TICKET, UNASSIGN_TICKET, SPAWN_AGENT, LIST_PROFILES],
    ),
    ("planner", &[]),
    ("debugger", &[]),
];

/// The tools allowed for `roles` (role wire names; unknown names add nothing), in
/// [`ALL_TOOL_NAMES`] order without duplicates. No roles = [`COMMON_TOOLS`].
pub fn tools_for_roles<S: AsRef<str>>(roles: &[S]) -> Vec<&'static str> {
    ALL_TOOL_NAMES
        .iter()
        .copied()
        .filter(|tool| {
            COMMON_TOOLS.contains(tool)
                || ROLE_TOOLS.iter().any(|(role, tools)| {
                    tools.contains(tool) && roles.iter().any(|r| r.as_ref() == *role)
                })
        })
        .collect()
}

/// Whether `tool` is allowed for `roles` (see [`tools_for_roles`]); unknown tools never are.
pub fn is_allowed<S: AsRef<str>>(tool: &str, roles: &[S]) -> bool {
    tools_for_roles(roles).contains(&tool)
}

/// Limits (chars) shared with the app's C4.12.
pub const TITLE_MAX: usize = 200;
pub const BODY_MAX: usize = 20_000;
pub const SUMMARY_MAX: usize = 2_000;
pub const NOTE_MAX: usize = 120;
pub const ID_MAX: usize = 64;
pub const FILTERS: [&str; 3] = ["mine", "backlog", "all"];
/// Step 5 limits (plan5 C5.3).
pub const REVIEW_NOTE_MAX: usize = 2_000;
pub const PROFILE_ID_MAX: usize = 40;
pub const REPORT_TITLE_MAX: usize = 120;
pub const REPORT_BODY_MAX: usize = 20_000;
pub const REPORT_ID_MAX: usize = 8;
pub const SEAT_KINDS: [&str; 2] = ["work", "staff"];
/// Error for a malformed `project` argument (step 4b).
pub const PROJECT_ERROR: &str =
    "project skal være et projekt-id (1–64 tegn) eller {\"new\": \"<navn>\"}";

/// The `project` schema of `mira_create_ticket` and `mira_spawn_agent` (plan4b C4b.5).
fn project_ref_schema(description: &str) -> Value {
    json!({
        "description": description,
        "oneOf": [
            {"type": "string", "minLength": 1, "maxLength": ID_MAX},
            {"type": "object", "properties": {"new": {"type": "string", "minLength": 1, "maxLength": ID_MAX}}, "required": ["new"], "additionalProperties": false}
        ]
    })
}

fn annotations(read_only: bool, idempotent: bool) -> Value {
    json!({
        "readOnlyHint": read_only,
        "destructiveHint": false,
        "idempotentHint": idempotent,
        "openWorldHint": false
    })
}

/// The `tools/list` entries, exactly as in plan4 C4.3.
pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": CREATE_TICKET,
            "description": "Opretter en ny ticket i mira-bots' backlog (ikke tildelt nogen). Brug den til opfølgende opgaver du opdager undervejs. Returnerer id og kort-id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": {"type": "string", "minLength": 1, "maxLength": TITLE_MAX, "description": "Kort titel (én linje)"},
                    "body": {"type": "string", "maxLength": BODY_MAX, "description": "Beskrivelse (markdown)"},
                    "skipReview": {"type": "boolean", "description": "true: ticketen går direkte til Done når den afleveres"},
                    "assignTo": {"type": "string", "minLength": 1, "maxLength": ID_MAX, "description": "Agent-id (kun koordinator): ticketen sættes bagest i agentens kø"},
                    "project": project_ref_schema("Projektet ticketen hører til: et projekt-id fra mira_list_projects, eller {\"new\": \"<mappenavn>\"} for et nyt projekt (kun hvis workspacet tillader det). Udelades: dit eget projekt (arbejdsagent) eller assignTo-agentens.")
                },
                "required": ["title"],
                "additionalProperties": false
            },
            "annotations": annotations(false, false)
        }),
        json!({
            "name": LIST_TICKETS,
            "description": "Lister tickets uden beskrivelse og historik. filter: \"mine\" (dine i kø og i gang, standard), \"backlog\" (ikke tildelte) eller \"all\".",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "filter": {"type": "string", "enum": FILTERS},
                    "project": {"type": "string", "minLength": 1, "maxLength": ID_MAX, "description": "Kun tickets i dette projekt (id); \"none\" = tickets uden projekt"}
                },
                "additionalProperties": false
            },
            "annotations": annotations(true, true)
        }),
        json!({
            "name": GET_TICKET,
            "description": "Henter én ticket med beskrivelse og historik. id kan være det fulde id eller kort-id'et (8 tegn, som i filnavnet).",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string", "minLength": 1, "maxLength": ID_MAX}},
                "required": ["id"],
                "additionalProperties": false
            },
            "annotations": annotations(true, true)
        }),
        json!({
            "name": SUBMIT_FOR_REVIEW,
            "description": "Afleverer den ticket du arbejder på: den går til Review (eller Done hvis den springer review over). KALD DEN når opgaven er færdig, med en kort opsummering af hvad du gjorde. Uden ticketId bruges din igangværende ticket.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "summary": {"type": "string", "minLength": 1, "maxLength": SUMMARY_MAX, "description": "Hvad er gjort, hvad bør brugeren kigge på"},
                    "ticketId": {"type": "string", "minLength": 1, "maxLength": ID_MAX},
                    "report": {"type": "string", "minLength": 1, "maxLength": REPORT_BODY_MAX, "description": "Valgfri rapport (markdown) der gemmes på ticketen sammen med afleveringen"}
                },
                "required": ["summary"],
                "additionalProperties": false
            },
            "annotations": annotations(false, false)
        }),
        json!({
            "name": UPDATE_STATUS,
            "description": "Sætter en kort statuslinje (maks 120 tegn) der vises ved din agent i mira-bots, og noterer den på din igangværende ticket. Kun information; ændrer ingen tilstand.",
            "inputSchema": {
                "type": "object",
                "properties": {"note": {"type": "string", "minLength": 1, "maxLength": NOTE_MAX}},
                "required": ["note"],
                "additionalProperties": false
            },
            "annotations": annotations(false, true)
        }),
        json!({
            "name": GET_WORKSPACE_RULES,
            "description": "Reglerne i dette workspace (mira-bots.workspace.json): lofter for agenter, maks review-runder, om Stop automatisk sender til review, grænser for tickets og rapporter, om agenter må oprette projekter, samt projektroden og projektlisten.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": annotations(true, true)
        }),
        json!({
            "name": ADD_REPORT,
            "description": "Lægger en rapport (markdown, maks 20000 tegn) på en ticket, så brugeren og revieweren kan se hvad der er lavet. Uden ticketId bruges din igangværende ticket. Reviewere kan lægge en review-rapport på tickets de reviewer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ticketId": {"type": "string", "minLength": 1, "maxLength": ID_MAX},
                    "title": {"type": "string", "minLength": 1, "maxLength": REPORT_TITLE_MAX},
                    "body": {"type": "string", "minLength": 1, "maxLength": REPORT_BODY_MAX}
                },
                "required": ["title", "body"],
                "additionalProperties": false
            },
            "annotations": annotations(false, false)
        }),
        json!({
            "name": GET_REPORT,
            "description": "Henter teksten i en rapport på en ticket (rapport-id'erne står i mira_get_ticket).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ticketId": {"type": "string", "minLength": 1, "maxLength": ID_MAX},
                    "reportId": {"type": "string", "minLength": 1, "maxLength": REPORT_ID_MAX}
                },
                "required": ["ticketId", "reportId"],
                "additionalProperties": false
            },
            "annotations": annotations(true, true)
        }),
        json!({
            "name": HANDOFF_TICKET,
            "description": "Giver din igangværende ticket videre: med agentId flytter den bagest i den agents kø (agenten skal køre og må ikke være dig selv); uden agentId lægges den tilbage i backlog. Brug den når ticketen ikke er til dig, og skriv evt. hvorfor med mira_update_status først. Kun din egen ticket i gang; tickets i Review eller Done kan ikke gives videre. Uden ticketId bruges din igangværende ticket.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ticketId": {"type": "string", "minLength": 1, "maxLength": ID_MAX},
                    "agentId": {"type": "string", "minLength": 1, "maxLength": ID_MAX, "description": "Agenten der skal have ticketen; udelad for at lægge den tilbage i backlog"}
                },
                "additionalProperties": false
            },
            "annotations": annotations(false, false)
        }),
        json!({
            "name": LIST_AGENTS,
            "description": "Lister agenterne i appen: id, navn, profil, roller, plads, projekt, status, ticket i gang, kølængde og åbne reviews.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": annotations(true, true)
        }),
        json!({
            "name": LIST_PROJECTS,
            "description": "Lister projekterne (mapperne under projektroden) med sti og antal arbejdsagenter i hvert. Brug id'et som project på tickets og ved start af agenter.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": annotations(true, true)
        }),
        json!({
            "name": APPROVE_TICKET,
            "description": "Godkender en ticket du er reviewer på: den går til Done. Kun tickets i Review, aldrig dine egne afleveringer. Skriv kort hvad du har tjekket i note.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "minLength": 1, "maxLength": ID_MAX},
                    "note": {"type": "string", "maxLength": REVIEW_NOTE_MAX}
                },
                "required": ["id"],
                "additionalProperties": false
            },
            "annotations": annotations(false, false)
        }),
        json!({
            "name": REJECT_TICKET,
            "description": "Afviser en ticket du er reviewer på med en begrundelse: den går tilbage forrest i afsenderens kø (runde +1; efter 3 runder afgør brugeren). Kun tickets i Review, aldrig dine egne.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "minLength": 1, "maxLength": ID_MAX},
                    "note": {"type": "string", "minLength": 1, "maxLength": REVIEW_NOTE_MAX}
                },
                "required": ["id", "note"],
                "additionalProperties": false
            },
            "annotations": annotations(false, false)
        }),
        json!({
            "name": ASSIGN_TICKET,
            "description": "Sætter en ticket fra backlog eller afvist bagest i en kørende agents kø (koordinator). Din egen igangværende ticket kan du også give videre på den måde (den flytter fra dig til agenten). Agent-id'er fås med mira_list_agents.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string", "minLength": 1, "maxLength": ID_MAX},
                    "agentId": {"type": "string", "minLength": 1, "maxLength": ID_MAX}
                },
                "required": ["id", "agentId"],
                "additionalProperties": false
            },
            "annotations": annotations(false, false)
        }),
        json!({
            "name": UNASSIGN_TICKET,
            "description": "Tager en ticket i kø ud af agentens kø og tilbage til backlog (koordinator). Af tickets i gang kan kun din egen lægges tilbage.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string", "minLength": 1, "maxLength": ID_MAX}},
                "required": ["id"],
                "additionalProperties": false
            },
            "annotations": annotations(false, true)
        }),
        json!({
            "name": SPAWN_AGENT,
            "description": "Starter en ny agent fra en profil (koordinator). Samme lofter som i appen; en stabsplads kræver en profil med en stabsrolle (reviewer, koordinator eller planlægger). Med firstTicketId får agenten den ticket som første opgave. En arbejdsplads kræver et projekt (project, eller ticketens projekt når firstTicketId er med).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "profileId": {"type": "string", "minLength": 1, "maxLength": PROFILE_ID_MAX},
                    "seatKind": {"type": "string", "enum": SEAT_KINDS},
                    "firstTicketId": {"type": "string", "minLength": 1, "maxLength": ID_MAX},
                    "project": project_ref_schema("Projektet agenten arbejder i (arbejdsplads): et projekt-id fra mira_list_projects, eller {\"new\": \"<mappenavn>\"} (kun hvis workspacet tillader det). Ticketens projekt vinder.")
                },
                "required": ["profileId"],
                "additionalProperties": false
            },
            "annotations": annotations(false, false)
        }),
        json!({
            "name": LIST_PROFILES,
            "description": "Lister agentprofilerne (id, navn, roller, standardplads, model, effort) til brug for mira_spawn_agent.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": annotations(true, true)
        }),
    ]
}

/// The `tools/list` entries for an agent with `roles` (see [`tools_for_roles`]), in
/// [`TOOL_NAMES`] order.
pub fn definitions_for<S: AsRef<str>>(roles: &[S]) -> Vec<Value> {
    let allowed = tools_for_roles(roles);
    definitions()
        .into_iter()
        .filter(|d| {
            d.get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| allowed.contains(&n))
        })
        .collect()
}

pub fn is_known(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

/// Error text when an agent calls a tool its roles do not allow (same as the app's).
pub const ROLE_DENIED: &str = "Din rolle tillader ikke dette værktøj";

/// What a string argument must look like; `None` max = no upper bound check here.
struct StrRule {
    key: &'static str,
    required: bool,
    min: usize,
    max: usize,
    /// Error when missing, of the wrong type or outside `min..=max`.
    error: &'static str,
    /// Separate error for "too long" (C4.4 texts for title/body); `None` = `error`.
    too_long: Option<&'static str>,
}

/// `project`: a project id (1–64 chars, trimmed) or `{"new": "<name>"}` (1–64 chars); `null`
/// counts as absent. The app validates the folder-name rules again.
fn take_project(args: &Map<String, Value>, out: &mut Map<String, Value>) -> Result<(), String> {
    let id = |v: &Value| -> Result<String, String> {
        let s = v.as_str().ok_or(PROJECT_ERROR)?.trim();
        if s.is_empty() || s.chars().count() > ID_MAX {
            return Err(PROJECT_ERROR.into());
        }
        Ok(s.to_string())
    };
    let v = match args.get("project") {
        None | Some(Value::Null) => return Ok(()),
        Some(v @ Value::String(_)) => Value::String(id(v)?),
        Some(Value::Object(m)) if m.len() == 1 => {
            let new = m.get("new").ok_or(PROJECT_ERROR)?;
            json!({ "new": id(new)? })
        }
        Some(_) => return Err(PROJECT_ERROR.into()),
    };
    out.insert("project".into(), v);
    Ok(())
}

fn take_str(
    args: &Map<String, Value>,
    out: &mut Map<String, Value>,
    r: &StrRule,
) -> Result<(), String> {
    let Some(v) = args.get(r.key) else {
        return if r.required {
            Err(r.error.to_string())
        } else {
            Ok(())
        };
    };
    let s = v.as_str().ok_or_else(|| r.error.to_string())?.trim();
    let n = s.chars().count();
    if n > r.max {
        return Err(r.too_long.unwrap_or(r.error).to_string());
    }
    if n < r.min {
        return Err(r.error.to_string());
    }
    out.insert(r.key.to_string(), Value::String(s.to_string()));
    Ok(())
}

/// An id-like string of 1–`max` chars; the error names the key and the range.
fn id_rule(key: &'static str, required: bool, max: usize) -> StrRule {
    let error: &'static str = match key {
        "profileId" => "profileId skal være en tekst på 1–40 tegn",
        "reportId" => "reportId skal være en tekst på 1–8 tegn",
        "ticketId" => "ticketId skal være en tekst på 1–64 tegn",
        "agentId" => "agentId skal være en tekst på 1–64 tegn",
        "assignTo" => "assignTo skal være en tekst på 1–64 tegn",
        "firstTicketId" => "firstTicketId skal være en tekst på 1–64 tegn",
        _ => "id skal være en tekst på 1–64 tegn",
    };
    StrRule {
        key,
        required,
        min: 1,
        max,
        error,
        too_long: None,
    }
}

/// Checks `args` for tool `name` (which must be known) and returns the cleaned object: only the
/// tool's keys, strings trimmed. Errors are Danish, for the model (`isError: true`).
pub fn validate_args(name: &str, args: &Value) -> Result<Value, String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "Argumenterne skal være et objekt".to_string())?;
    let allowed: &[&str] = match name {
        CREATE_TICKET => &["title", "body", "skipReview", "assignTo", "project"],
        LIST_TICKETS => &["filter", "project"],
        GET_TICKET => &["id"],
        SUBMIT_FOR_REVIEW => &["summary", "ticketId", "report"],
        UPDATE_STATUS => &["note"],
        APPROVE_TICKET | REJECT_TICKET => &["id", "note"],
        ASSIGN_TICKET => &["id", "agentId"],
        UNASSIGN_TICKET => &["id"],
        SPAWN_AGENT => &["profileId", "seatKind", "firstTicketId", "project"],
        LIST_AGENTS | LIST_PROFILES | GET_WORKSPACE_RULES | LIST_PROJECTS => &[],
        ADD_REPORT => &["ticketId", "title", "body"],
        GET_REPORT => &["ticketId", "reportId"],
        HANDOFF_TICKET => &["ticketId", "agentId"],
        other => return Err(format!("Ukendt værktøj: {other}")),
    };
    if let Some(k) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!("Ukendt argument: {k}"));
    }
    let mut out = Map::new();
    match name {
        CREATE_TICKET => {
            take_str(
                obj,
                &mut out,
                &StrRule {
                    key: "title",
                    required: true,
                    min: 1,
                    max: TITLE_MAX,
                    error: "Titel må ikke være tom",
                    too_long: Some("Titlen er for lang (maks 200 tegn)"),
                },
            )?;
            take_str(
                obj,
                &mut out,
                &StrRule {
                    key: "body",
                    required: false,
                    min: 0,
                    max: BODY_MAX,
                    error: "body skal være en tekst",
                    too_long: Some("Teksten er for lang (maks 20000 tegn)"),
                },
            )?;
            if let Some(v) = obj.get("skipReview") {
                let b = v
                    .as_bool()
                    .ok_or_else(|| "skipReview skal være true eller false".to_string())?;
                out.insert("skipReview".into(), Value::Bool(b));
            }
            take_str(obj, &mut out, &id_rule("assignTo", false, ID_MAX))?;
            take_project(obj, &mut out)?;
        }
        LIST_TICKETS => {
            if let Some(v) = obj.get("filter") {
                let f = v.as_str().map(str::trim).unwrap_or_default();
                if !FILTERS.contains(&f) {
                    return Err("Ukendt filter".into());
                }
                out.insert("filter".into(), Value::String(f.to_string()));
            }
            take_str(
                obj,
                &mut out,
                &StrRule {
                    key: "project",
                    required: false,
                    min: 1,
                    max: ID_MAX,
                    error: "project skal være en tekst på 1–64 tegn",
                    too_long: None,
                },
            )?;
        }
        GET_TICKET => take_str(
            obj,
            &mut out,
            &StrRule {
                key: "id",
                required: true,
                min: 1,
                max: ID_MAX,
                error: "id skal være en tekst på 1–64 tegn",
                too_long: None,
            },
        )?,
        SUBMIT_FOR_REVIEW => {
            take_str(
                obj,
                &mut out,
                &StrRule {
                    key: "summary",
                    required: true,
                    min: 1,
                    max: SUMMARY_MAX,
                    error: "summary skal være en tekst på 1–2000 tegn",
                    too_long: None,
                },
            )?;
            take_str(obj, &mut out, &id_rule("ticketId", false, ID_MAX))?;
            take_str(
                obj,
                &mut out,
                &StrRule {
                    key: "report",
                    required: false,
                    min: 1,
                    max: REPORT_BODY_MAX,
                    error: "report skal være en tekst på 1–20000 tegn",
                    too_long: Some("Rapporten er for lang (maks 20000 tegn)"),
                },
            )?;
        }
        APPROVE_TICKET | REJECT_TICKET => {
            take_str(obj, &mut out, &id_rule("id", true, ID_MAX))?;
            let reject = name == REJECT_TICKET;
            take_str(
                obj,
                &mut out,
                &StrRule {
                    key: "note",
                    required: reject,
                    min: usize::from(reject),
                    max: REVIEW_NOTE_MAX,
                    error: if reject {
                        "Afvisning kræver en note"
                    } else {
                        "note skal være en tekst på højst 2000 tegn"
                    },
                    too_long: Some("note må højst være 2000 tegn"),
                },
            )?;
        }
        ASSIGN_TICKET => {
            take_str(obj, &mut out, &id_rule("id", true, ID_MAX))?;
            take_str(obj, &mut out, &id_rule("agentId", true, ID_MAX))?;
        }
        UNASSIGN_TICKET => take_str(obj, &mut out, &id_rule("id", true, ID_MAX))?,
        SPAWN_AGENT => {
            take_str(obj, &mut out, &id_rule("profileId", true, PROFILE_ID_MAX))?;
            if let Some(v) = obj.get("seatKind") {
                let k = v.as_str().map(str::trim).unwrap_or_default();
                if !SEAT_KINDS.contains(&k) {
                    return Err("seatKind skal være \"work\" eller \"staff\"".into());
                }
                out.insert("seatKind".into(), Value::String(k.to_string()));
            }
            take_str(obj, &mut out, &id_rule("firstTicketId", false, ID_MAX))?;
            take_project(obj, &mut out)?;
        }
        LIST_AGENTS | LIST_PROFILES | GET_WORKSPACE_RULES | LIST_PROJECTS => {}
        ADD_REPORT => {
            take_str(obj, &mut out, &id_rule("ticketId", false, ID_MAX))?;
            take_str(
                obj,
                &mut out,
                &StrRule {
                    key: "title",
                    required: true,
                    min: 1,
                    max: REPORT_TITLE_MAX,
                    error: "Titel må ikke være tom",
                    too_long: Some("Titlen er for lang (maks 120 tegn)"),
                },
            )?;
            take_str(
                obj,
                &mut out,
                &StrRule {
                    key: "body",
                    required: true,
                    min: 1,
                    max: REPORT_BODY_MAX,
                    error: "Rapporten må ikke være tom",
                    too_long: Some("Rapporten er for lang (maks 20000 tegn)"),
                },
            )?;
        }
        GET_REPORT => {
            take_str(obj, &mut out, &id_rule("ticketId", true, ID_MAX))?;
            take_str(obj, &mut out, &id_rule("reportId", true, REPORT_ID_MAX))?;
        }
        HANDOFF_TICKET => {
            take_str(obj, &mut out, &id_rule("ticketId", false, ID_MAX))?;
            take_str(obj, &mut out, &id_rule("agentId", false, ID_MAX))?;
        }
        _ => take_str(
            obj,
            &mut out,
            &StrRule {
                key: "note",
                required: true,
                min: 1,
                max: NOTE_MAX,
                error: "note skal være en tekst på 1–120 tegn",
                too_long: None,
            },
        )?,
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_matrix_matches_the_plan() {
        let none: [&str; 0] = [];
        assert_eq!(tools_for_roles(&none), COMMON_TOOLS.to_vec());
        for role in ["coder", "researcher", "planner", "debugger", "nobody"] {
            assert_eq!(tools_for_roles(&[role]), COMMON_TOOLS.to_vec(), "{role}");
        }
        assert_eq!(COMMON_TOOLS.len(), 11);
        assert_eq!(ALL_TOOL_NAMES.len(), 17);
        assert_eq!(tools_for_roles(&["reviewer"]).len(), 13);
        assert_eq!(tools_for_roles(&["coordinator"]).len(), 15);
        // Step 4b: every role sees the projects.
        assert!(is_allowed(LIST_PROJECTS, &none));
        assert!(is_allowed(LIST_PROJECTS, &["coder"]));
        // Step 5c: every role may hand its own ticket on, and find a free agent for it.
        assert!(is_allowed(HANDOFF_TICKET, &["coder"]));
        assert!(is_allowed(HANDOFF_TICKET, &none));
        for roles in [&[][..], &["coder"][..], &["reviewer"][..], &["planner"][..]] {
            assert!(is_allowed(LIST_AGENTS, roles), "{roles:?}");
        }
        assert!(!is_allowed(LIST_PROFILES, &["reviewer"]));
        assert!(!is_allowed(ASSIGN_TICKET, &["planner"]));
        let all = [
            "coder",
            "researcher",
            "reviewer",
            "coordinator",
            "planner",
            "debugger",
        ];
        assert_eq!(tools_for_roles(&all), ALL_TOOL_NAMES.to_vec());
        // Duplicates and order of the input do not matter.
        assert_eq!(
            tools_for_roles(&["coordinator", "reviewer", "reviewer"]),
            ALL_TOOL_NAMES.to_vec()
        );
        assert!(is_allowed(APPROVE_TICKET, &["reviewer"]));
        assert!(!is_allowed(APPROVE_TICKET, &["coordinator"]));
        assert!(is_allowed(SPAWN_AGENT, &["coordinator".to_string()]));
        assert!(!is_allowed("mira_nope", &all));
        // The role-bound set is exactly the union of the matrix, disjoint from the common one.
        let mut bound: Vec<&str> = ROLE_TOOLS
            .iter()
            .flat_map(|(_, t)| t.iter().copied())
            .collect();
        bound.sort_unstable();
        let mut want = ROLE_BOUND_TOOLS.to_vec();
        want.sort_unstable();
        assert_eq!(bound, want);
        assert!(ROLE_BOUND_TOOLS.iter().all(|t| !COMMON_TOOLS.contains(t)));
        assert_eq!(
            COMMON_TOOLS.len() + ROLE_BOUND_TOOLS.len(),
            ALL_TOOL_NAMES.len()
        );
        // Every step-4 tool is a common tool.
        assert!(TOOL_NAMES[..5].iter().all(|t| COMMON_TOOLS.contains(t)));
        assert_eq!(TOOL_NAMES, ALL_TOOL_NAMES);
        let roles: Vec<&str> = ROLE_TOOLS.iter().map(|(r, _)| *r).collect();
        assert_eq!(roles, all);
    }

    fn name_ok(n: &str) -> bool {
        (1..=128).contains(&n.len())
            && n.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    }

    #[test]
    fn definitions_are_well_formed() {
        let defs = definitions();
        assert_eq!(defs.len(), 17);
        let names: Vec<&str> = defs.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert_eq!(names, TOOL_NAMES);
        for d in &defs {
            let name = d["name"].as_str().unwrap();
            assert!(name_ok(name), "{name}");
            let desc = d["description"].as_str().unwrap();
            assert!(!desc.is_empty() && desc.chars().count() <= 400, "{name}");
            let schema = &d["inputSchema"];
            assert_eq!(schema["type"], "object", "{name}");
            assert_eq!(schema["additionalProperties"], false, "{name}");
            let props = schema["properties"].as_object().unwrap();
            for r in schema["required"].as_array().into_iter().flatten() {
                assert!(props.contains_key(r.as_str().unwrap()), "{name}: {r}");
            }
            let ann = d["annotations"].as_object().unwrap();
            for k in [
                "readOnlyHint",
                "destructiveHint",
                "idempotentHint",
                "openWorldHint",
            ] {
                assert!(ann[k].is_boolean(), "{name}: {k}");
            }
        }
        let all = serde_json::to_string(&defs).unwrap();
        assert!(all.len() < 16 * 1024, "{} bytes", all.len());
        assert!(!all.contains('\n'));
    }

    #[test]
    fn definitions_match_the_contract_in_detail() {
        let defs = definitions();
        assert_eq!(defs[0]["inputSchema"]["required"], json!(["title"]));
        assert_eq!(
            defs[0]["inputSchema"]["properties"]["title"]["maxLength"],
            200
        );
        assert_eq!(
            defs[0]["inputSchema"]["properties"]["body"]["maxLength"],
            20000
        );
        assert_eq!(
            defs[1]["inputSchema"]["properties"]["filter"]["enum"],
            json!(["mine", "backlog", "all"])
        );
        assert!(defs[1]["inputSchema"].get("required").is_none());
        assert_eq!(defs[1]["annotations"]["readOnlyHint"], true);
        assert_eq!(defs[2]["inputSchema"]["required"], json!(["id"]));
        assert_eq!(defs[3]["inputSchema"]["required"], json!(["summary"]));
        assert_eq!(
            defs[3]["inputSchema"]["properties"]["summary"]["maxLength"],
            2000
        );
        assert_eq!(
            defs[4]["inputSchema"]["properties"]["note"]["maxLength"],
            120
        );
        assert_eq!(defs[4]["annotations"]["idempotentHint"], true);
        assert!(defs[3]["description"]
            .as_str()
            .unwrap()
            .contains("KALD DEN når opgaven er færdig"));
    }

    #[test]
    fn create_ticket_args() {
        assert_eq!(
            validate_args(
                CREATE_TICKET,
                &json!({"title":"  Ret login ","body":"b","skipReview":true})
            ),
            Ok(json!({"title":"Ret login","body":"b","skipReview":true}))
        );
        assert_eq!(
            validate_args(CREATE_TICKET, &json!({"title":"x"})),
            Ok(json!({"title":"x"}))
        );
        let err = |a: Value| validate_args(CREATE_TICKET, &a).unwrap_err();
        assert_eq!(err(json!({})), "Titel må ikke være tom");
        assert_eq!(err(json!({"title":"   "})), "Titel må ikke være tom");
        assert_eq!(err(json!({"title":5})), "Titel må ikke være tom");
        assert_eq!(
            err(json!({"title":"x".repeat(201)})),
            "Titlen er for lang (maks 200 tegn)"
        );
        assert!(validate_args(CREATE_TICKET, &json!({"title":"æ".repeat(200)})).is_ok());
        assert_eq!(
            err(json!({"title":"x","body":"y".repeat(20_001)})),
            "Teksten er for lang (maks 20000 tegn)"
        );
        assert_eq!(
            err(json!({"title":"x","skipReview":"yes"})),
            "skipReview skal være true eller false"
        );
        assert_eq!(
            err(json!({"title":"x","assignee":"me"})),
            "Ukendt argument: assignee"
        );
        assert_eq!(err(json!(["x"])), "Argumenterne skal være et objekt");
    }

    #[test]
    fn list_and_get_args() {
        assert_eq!(validate_args(LIST_TICKETS, &json!({})), Ok(json!({})));
        assert_eq!(
            validate_args(LIST_TICKETS, &json!({"filter":"backlog"})),
            Ok(json!({"filter":"backlog"}))
        );
        assert_eq!(
            validate_args(LIST_TICKETS, &json!({"filter":"others"})).unwrap_err(),
            "Ukendt filter"
        );
        assert_eq!(
            validate_args(GET_TICKET, &json!({"id":" abcdef01 "})),
            Ok(json!({"id":"abcdef01"}))
        );
        for bad in [
            json!({}),
            json!({"id":""}),
            json!({"id":"x".repeat(65)}),
            json!({"id":1}),
        ] {
            assert_eq!(
                validate_args(GET_TICKET, &bad).unwrap_err(),
                "id skal være en tekst på 1–64 tegn"
            );
        }
    }

    #[test]
    fn submit_args() {
        assert_eq!(
            validate_args(
                SUBMIT_FOR_REVIEW,
                &json!({"summary":" done ","ticketId":"abc"})
            ),
            Ok(json!({"summary":"done","ticketId":"abc"}))
        );
        for bad in [
            json!({}),
            json!({"summary":" "}),
            json!({"summary":"x".repeat(2001)}),
        ] {
            assert_eq!(
                validate_args(SUBMIT_FOR_REVIEW, &bad).unwrap_err(),
                "summary skal være en tekst på 1–2000 tegn"
            );
        }
        assert!(validate_args(SUBMIT_FOR_REVIEW, &json!({"summary":"x".repeat(2000)})).is_ok());
        assert_eq!(
            validate_args(SUBMIT_FOR_REVIEW, &json!({"summary":"ok","ticketId":""})).unwrap_err(),
            "ticketId skal være en tekst på 1–64 tegn"
        );
    }

    #[test]
    fn update_status_args() {
        assert_eq!(
            validate_args(UPDATE_STATUS, &json!({"note":"Kører tests"})),
            Ok(json!({"note":"Kører tests"}))
        );
        assert!(validate_args(UPDATE_STATUS, &json!({"note":"x".repeat(120)})).is_ok());
        for bad in [
            json!({}),
            json!({"note":""}),
            json!({"note":"x".repeat(121)}),
        ] {
            assert_eq!(
                validate_args(UPDATE_STATUS, &bad).unwrap_err(),
                "note skal være en tekst på 1–120 tegn"
            );
        }
    }

    #[test]
    fn unknown_tool_is_rejected() {
        assert!(!is_known("mira_assign"));
        assert!(TOOL_NAMES.iter().all(|n| is_known(n)));
        assert_eq!(
            validate_args("mira_assign", &json!({})).unwrap_err(),
            "Ukendt værktøj: mira_assign"
        );
    }

    fn def<'a>(defs: &'a [Value], name: &str) -> &'a Value {
        defs.iter().find(|d| d["name"] == name).unwrap()
    }

    #[test]
    fn definitions_for_each_role() {
        let names = |roles: &[&str]| -> Vec<String> {
            definitions_for(roles)
                .iter()
                .map(|d| d["name"].as_str().unwrap().to_string())
                .collect()
        };
        for (roles, n) in [
            (&[][..], 11),
            (&["coder"][..], 11),
            (&["researcher"][..], 11),
            (&["planner"][..], 11),
            (&["debugger"][..], 11),
            (&["reviewer"][..], 13),
            (&["coordinator"][..], 15),
            (&["coder", "reviewer", "coordinator"][..], 17),
            (&["nobody"][..], 11),
        ] {
            assert_eq!(names(roles).len(), n, "{roles:?}");
            let want: Vec<String> = tools_for_roles(roles)
                .iter()
                .map(|s| s.to_string())
                .collect();
            assert_eq!(names(roles), want, "{roles:?}");
        }
        assert!(!names(&["coder"]).contains(&APPROVE_TICKET.to_string()));
        assert!(!names(&["reviewer"]).contains(&ASSIGN_TICKET.to_string()));
    }

    #[test]
    fn step5_definitions_match_c5_3() {
        let defs = definitions();
        let create = def(&defs, CREATE_TICKET);
        assert_eq!(
            create["inputSchema"]["properties"]["assignTo"],
            json!({"type":"string","minLength":1,"maxLength":64,"description":"Agent-id (kun koordinator): ticketen sættes bagest i agentens kø"})
        );
        assert_eq!(
            def(&defs, SUBMIT_FOR_REVIEW)["inputSchema"]["properties"]["report"]["maxLength"],
            20000
        );
        assert_eq!(
            def(&defs, APPROVE_TICKET)["inputSchema"],
            json!({"type":"object","properties":{"id":{"type":"string","minLength":1,"maxLength":64},"note":{"type":"string","maxLength":2000}},"required":["id"],"additionalProperties":false})
        );
        assert_eq!(
            def(&defs, REJECT_TICKET)["inputSchema"]["required"],
            json!(["id", "note"])
        );
        assert_eq!(
            def(&defs, ASSIGN_TICKET)["inputSchema"]["required"],
            json!(["id", "agentId"])
        );
        assert_eq!(
            def(&defs, UNASSIGN_TICKET)["annotations"]["idempotentHint"],
            true
        );
        let spawn = &def(&defs, SPAWN_AGENT)["inputSchema"];
        assert_eq!(spawn["properties"]["profileId"]["maxLength"], 40);
        assert_eq!(
            spawn["properties"]["seatKind"]["enum"],
            json!(["work", "staff"])
        );
        assert_eq!(spawn["required"], json!(["profileId"]));
        for t in [LIST_AGENTS, LIST_PROFILES, GET_WORKSPACE_RULES, GET_REPORT] {
            assert_eq!(def(&defs, t)["annotations"]["readOnlyHint"], true, "{t}");
        }
        assert_eq!(
            def(&defs, LIST_AGENTS)["inputSchema"],
            json!({"type":"object","properties":{},"additionalProperties":false})
        );
        let add = &def(&defs, ADD_REPORT)["inputSchema"];
        assert_eq!(add["required"], json!(["title", "body"]));
        assert_eq!(add["properties"]["title"]["maxLength"], 120);
        assert_eq!(add["properties"]["body"]["maxLength"], 20000);
        assert_eq!(
            def(&defs, GET_REPORT)["inputSchema"]["properties"]["reportId"]["maxLength"],
            8
        );
        assert!(def(&defs, REJECT_TICKET)["description"]
            .as_str()
            .unwrap()
            .contains("efter 3 runder afgør brugeren"));
    }

    #[test]
    fn validate_new_tools() {
        let ok = |name: &str, args: Value| validate_args(name, &args).unwrap();
        let err = |name: &str, args: Value| validate_args(name, &args).unwrap_err();
        assert_eq!(
            ok(APPROVE_TICKET, json!({"id":" abcdef01 "})),
            json!({"id":"abcdef01"})
        );
        assert_eq!(
            ok(APPROVE_TICKET, json!({"id":"x","note":""})),
            json!({"id":"x","note":""})
        );
        assert_eq!(
            err(APPROVE_TICKET, json!({"id":"x","note":"n".repeat(2001)})),
            "note må højst være 2000 tegn"
        );
        assert_eq!(
            err(REJECT_TICKET, json!({"id":"x"})),
            "Afvisning kræver en note"
        );
        assert_eq!(
            err(REJECT_TICKET, json!({"id":"x","note":"  "})),
            "Afvisning kræver en note"
        );
        assert_eq!(
            err(APPROVE_TICKET, json!({})),
            "id skal være en tekst på 1–64 tegn"
        );
        assert_eq!(
            err(ASSIGN_TICKET, json!({"id":"x"})),
            "agentId skal være en tekst på 1–64 tegn"
        );
        assert_eq!(
            ok(ASSIGN_TICKET, json!({"id":"x","agentId":"a"})),
            json!({"id":"x","agentId":"a"})
        );
        assert_eq!(
            err(UNASSIGN_TICKET, json!({"id":""})),
            "id skal være en tekst på 1–64 tegn"
        );
        // Step 5c: both arguments optional.
        assert_eq!(ok(HANDOFF_TICKET, json!({})), json!({}));
        assert_eq!(
            ok(
                HANDOFF_TICKET,
                json!({"ticketId":" ab12cd34 ","agentId":"w"})
            ),
            json!({"ticketId":"ab12cd34","agentId":"w"})
        );
        assert_eq!(
            err(HANDOFF_TICKET, json!({"agentId":""})),
            "agentId skal være en tekst på 1–64 tegn"
        );
        assert_eq!(
            err(HANDOFF_TICKET, json!({"id":"x"})),
            "Ukendt argument: id"
        );
        assert_eq!(
            ok(
                SPAWN_AGENT,
                json!({"profileId":"coder","seatKind":"staff","firstTicketId":"t"})
            ),
            json!({"profileId":"coder","seatKind":"staff","firstTicketId":"t"})
        );
        assert_eq!(
            err(SPAWN_AGENT, json!({"profileId":"x".repeat(41)})),
            "profileId skal være en tekst på 1–40 tegn"
        );
        assert_eq!(
            err(SPAWN_AGENT, json!({"profileId":"coder","seatKind":"desk"})),
            "seatKind skal være \"work\" eller \"staff\""
        );
        for t in [
            LIST_AGENTS,
            LIST_PROFILES,
            GET_WORKSPACE_RULES,
            LIST_PROJECTS,
        ] {
            assert_eq!(ok(t, json!({})), json!({}));
            assert_eq!(err(t, json!({"x":1})), "Ukendt argument: x");
        }
        assert_eq!(
            ok(ADD_REPORT, json!({"title":" T ","body":"b"})),
            json!({"title":"T","body":"b"})
        );
        assert_eq!(
            err(ADD_REPORT, json!({"body":"b"})),
            "Titel må ikke være tom"
        );
        assert_eq!(
            err(ADD_REPORT, json!({"title":"t".repeat(121),"body":"b"})),
            "Titlen er for lang (maks 120 tegn)"
        );
        assert_eq!(
            err(ADD_REPORT, json!({"title":"t"})),
            "Rapporten må ikke være tom"
        );
        assert_eq!(
            err(ADD_REPORT, json!({"title":"t","body":"b".repeat(20_001)})),
            "Rapporten er for lang (maks 20000 tegn)"
        );
        assert_eq!(
            err(GET_REPORT, json!({"ticketId":"t","reportId":"123456789"})),
            "reportId skal være en tekst på 1–8 tegn"
        );
        assert_eq!(
            ok(GET_REPORT, json!({"ticketId":"t","reportId":"01"})),
            json!({"ticketId":"t","reportId":"01"})
        );
        assert_eq!(
            ok(CREATE_TICKET, json!({"title":"t","assignTo":"a1"})),
            json!({"title":"t","assignTo":"a1"})
        );
        assert_eq!(
            err(CREATE_TICKET, json!({"title":"t","assignTo":""})),
            "assignTo skal være en tekst på 1–64 tegn"
        );
        assert_eq!(
            ok(SUBMIT_FOR_REVIEW, json!({"summary":"s","report":"# R"})),
            json!({"summary":"s","report":"# R"})
        );
        assert_eq!(
            err(
                SUBMIT_FOR_REVIEW,
                json!({"summary":"s","report":"r".repeat(20_001)})
            ),
            "Rapporten er for lang (maks 20000 tegn)"
        );
    }

    // ---- step 4b ----

    #[test]
    fn validate_project_forms() {
        let ok = |name: &str, args: Value| validate_args(name, &args).unwrap();
        let err = |name: &str, args: Value| validate_args(name, &args).unwrap_err();
        for t in [CREATE_TICKET, SPAWN_AGENT] {
            let base = if t == CREATE_TICKET {
                json!({"title": "t"})
            } else {
                json!({"profileId": "coder"})
            };
            let with = |p: Value| {
                let mut v = base.clone();
                v["project"] = p;
                v
            };
            let mut want = base.clone();
            want["project"] = json!("mira");
            assert_eq!(ok(t, with(json!(" mira "))), want, "{t}");
            want["project"] = json!({"new": "ny"});
            assert_eq!(ok(t, with(json!({"new": " ny "}))), want, "{t}");
            assert_eq!(ok(t, with(Value::Null)), base, "{t}: null = absent");
            for bad in [
                json!({"new": ""}),
                json!({"x": 1}),
                json!({"new": "a", "x": 1}),
                json!(5),
                json!(""),
                json!("p".repeat(65)),
                json!({"new": 5}),
            ] {
                assert_eq!(err(t, with(bad.clone())), PROJECT_ERROR, "{t}: {bad}");
            }
        }
        assert_eq!(
            ok(LIST_TICKETS, json!({"filter": "all", "project": " none "})),
            json!({"filter": "all", "project": "none"})
        );
        assert_eq!(
            err(LIST_TICKETS, json!({"project": ""})),
            "project skal være en tekst på 1–64 tegn"
        );
        assert_eq!(err(LIST_PROJECTS, json!({"x": 1})), "Ukendt argument: x");
    }

    #[test]
    fn step4b_definitions() {
        let defs = definitions();
        let schema = |d: &str| {
            json!({
                "description": d,
                "oneOf": [
                    {"type": "string", "minLength": 1, "maxLength": 64},
                    {"type": "object", "properties": {"new": {"type": "string", "minLength": 1, "maxLength": 64}}, "required": ["new"], "additionalProperties": false}
                ]
            })
        };
        assert_eq!(
            def(&defs, CREATE_TICKET)["inputSchema"]["properties"]["project"],
            schema("Projektet ticketen hører til: et projekt-id fra mira_list_projects, eller {\"new\": \"<mappenavn>\"} for et nyt projekt (kun hvis workspacet tillader det). Udelades: dit eget projekt (arbejdsagent) eller assignTo-agentens.")
        );
        assert_eq!(
            def(&defs, LIST_TICKETS)["inputSchema"]["properties"]["project"],
            json!({"type": "string", "minLength": 1, "maxLength": 64, "description": "Kun tickets i dette projekt (id); \"none\" = tickets uden projekt"})
        );
        let spawn = def(&defs, SPAWN_AGENT);
        assert_eq!(
            spawn["inputSchema"]["properties"]["project"]["oneOf"],
            schema("")["oneOf"]
        );
        assert!(spawn["description"]
            .as_str()
            .unwrap()
            .contains("En arbejdsplads kræver et projekt"));
        assert_eq!(
            def(&defs, LIST_PROJECTS),
            &json!({
                "name": "mira_list_projects",
                "description": "Lister projekterne (mapperne under projektroden) med sti og antal arbejdsagenter i hvert. Brug id'et som project på tickets og ved start af agenter.",
                "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
                "annotations": annotations(true, true)
            })
        );
        assert!(def(&defs, LIST_AGENTS)["description"]
            .as_str()
            .unwrap()
            .contains("projekt"));
        assert!(def(&defs, GET_WORKSPACE_RULES)["description"]
            .as_str()
            .unwrap()
            .contains("projektroden og projektlisten"));
    }
}
