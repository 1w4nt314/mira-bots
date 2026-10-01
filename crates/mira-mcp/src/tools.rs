//! The five tools (plan4 C4.3): `tools/list` definitions and argument validation before
//! anything is sent to the app. The app validates again (C4.12); this layer gives the model a
//! quick, precise error without a pipe round trip.

use serde_json::{json, Map, Value};

pub const CREATE_TICKET: &str = "mira_create_ticket";
pub const LIST_TICKETS: &str = "mira_list_tickets";
pub const GET_TICKET: &str = "mira_get_ticket";
pub const SUBMIT_FOR_REVIEW: &str = "mira_submit_for_review";
pub const UPDATE_STATUS: &str = "mira_update_status";

pub const TOOL_NAMES: [&str; 5] = [
    CREATE_TICKET,
    LIST_TICKETS,
    GET_TICKET,
    SUBMIT_FOR_REVIEW,
    UPDATE_STATUS,
];

// Step 5 tools (plan5 C5.3). Their definitions and argument validation arrive with batch 2; the
// names are needed now for the role matrix below, which the app also uses for its deny rules.
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

/// Tools every agent has, whatever its roles (plan5 A.2).
pub const COMMON_TOOLS: [&str; 8] = [
    CREATE_TICKET,
    LIST_TICKETS,
    GET_TICKET,
    SUBMIT_FOR_REVIEW,
    UPDATE_STATUS,
    GET_WORKSPACE_RULES,
    ADD_REPORT,
    GET_REPORT,
];

/// Tools only some roles have (the union of [`ROLE_TOOLS`]).
pub const ROLE_BOUND_TOOLS: [&str; 7] = [
    APPROVE_TICKET,
    REJECT_TICKET,
    ASSIGN_TICKET,
    UNASSIGN_TICKET,
    SPAWN_AGENT,
    LIST_AGENTS,
    LIST_PROFILES,
];

/// Every tool name of plan5 C5.3, common ones first.
pub const ALL_TOOL_NAMES: [&str; 15] = [
    CREATE_TICKET,
    LIST_TICKETS,
    GET_TICKET,
    SUBMIT_FOR_REVIEW,
    UPDATE_STATUS,
    GET_WORKSPACE_RULES,
    ADD_REPORT,
    GET_REPORT,
    APPROVE_TICKET,
    REJECT_TICKET,
    ASSIGN_TICKET,
    UNASSIGN_TICKET,
    SPAWN_AGENT,
    LIST_AGENTS,
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
        &[
            ASSIGN_TICKET,
            UNASSIGN_TICKET,
            SPAWN_AGENT,
            LIST_AGENTS,
            LIST_PROFILES,
        ],
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
                    "skipReview": {"type": "boolean", "description": "true: ticketen går direkte til Done når den afleveres"}
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
                "properties": {"filter": {"type": "string", "enum": FILTERS}},
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
                    "ticketId": {"type": "string", "minLength": 1, "maxLength": ID_MAX}
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
    ]
}

pub fn is_known(name: &str) -> bool {
    TOOL_NAMES.contains(&name)
}

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

/// Checks `args` for tool `name` (which must be known) and returns the cleaned object: only the
/// tool's keys, strings trimmed. Errors are Danish, for the model (`isError: true`).
pub fn validate_args(name: &str, args: &Value) -> Result<Value, String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "Argumenterne skal være et objekt".to_string())?;
    let allowed: &[&str] = match name {
        CREATE_TICKET => &["title", "body", "skipReview"],
        LIST_TICKETS => &["filter"],
        GET_TICKET => &["id"],
        SUBMIT_FOR_REVIEW => &["summary", "ticketId"],
        UPDATE_STATUS => &["note"],
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
        }
        LIST_TICKETS => {
            if let Some(v) = obj.get("filter") {
                let f = v.as_str().map(str::trim).unwrap_or_default();
                if !FILTERS.contains(&f) {
                    return Err("Ukendt filter".into());
                }
                out.insert("filter".into(), Value::String(f.to_string()));
            }
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
            take_str(
                obj,
                &mut out,
                &StrRule {
                    key: "ticketId",
                    required: false,
                    min: 1,
                    max: ID_MAX,
                    error: "ticketId skal være en tekst på 1–64 tegn",
                    too_long: None,
                },
            )?;
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
        assert_eq!(tools_for_roles(&["reviewer"]).len(), 10);
        assert_eq!(tools_for_roles(&["coordinator"]).len(), 13);
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
        assert!(TOOL_NAMES.iter().all(|t| COMMON_TOOLS.contains(t)));
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
        assert_eq!(defs.len(), 5);
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
        assert!(all.len() < 8 * 1024, "{} bytes", all.len());
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
}
