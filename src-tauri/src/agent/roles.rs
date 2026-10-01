//! Agent roles (plan5 A.1, C5.1). An agent has 0..6 roles; "no role" is an empty list. The role
//! decides the tools (matrix in `mira_mcp::tools::ROLE_TOOLS`), the role texts of the system
//! prompt, the figure and the default folder prefix.

use serde::{Deserialize, Serialize};

/// Wire: `"coder"|"researcher"|"reviewer"|"coordinator"|"planner"|"debugger"`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Coder,
    Researcher,
    Reviewer,
    Coordinator,
    Planner,
    Debugger,
}

/// Default-folder prefix of a specialist (or of an agent with several roles).
pub const SPECIALIST_PREFIX: &str = "specialist";
/// Default-folder prefix of an agent without roles.
pub const NO_ROLE_PREFIX: &str = "bot";

impl Role {
    /// Every role, in the canonical order (lists, env value, prompt sections).
    pub const ALL: [Role; 6] = [
        Role::Coder,
        Role::Researcher,
        Role::Reviewer,
        Role::Coordinator,
        Role::Planner,
        Role::Debugger,
    ];

    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Coder => "coder",
            Role::Researcher => "researcher",
            Role::Reviewer => "reviewer",
            Role::Coordinator => "coordinator",
            Role::Planner => "planner",
            Role::Debugger => "debugger",
        }
    }

    /// Danish label for the UI and the prompt.
    pub fn label_da(self) -> &'static str {
        match self {
            Role::Coder => "Koder",
            Role::Researcher => "Researcher",
            Role::Reviewer => "Reviewer",
            Role::Coordinator => "Koordinator",
            Role::Planner => "Planlægger",
            Role::Debugger => "Debugger",
        }
    }

    /// Figure file name (`bot-<name>-<state>.svg`) and folder prefix: `koord` for the
    /// coordinator, otherwise the wire name.
    pub fn figure_name(self) -> &'static str {
        match self {
            Role::Coordinator => "koord",
            other => other.as_str(),
        }
    }

    /// Wire name → role (exact, lowercase).
    pub fn parse(s: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|r| r.as_str() == s)
    }
}

/// Roles from a comma-separated list (`"coder, reviewer"`): trimmed, unknown names ignored,
/// duplicates removed, in [`Role::ALL`] order.
pub fn parse_list(s: &str) -> Vec<Role> {
    let named: Vec<Role> = s.split(',').filter_map(|p| Role::parse(p.trim())).collect();
    normalize(&named)
}

/// Duplicates removed, in [`Role::ALL`] order.
pub fn normalize(roles: &[Role]) -> Vec<Role> {
    Role::ALL
        .into_iter()
        .filter(|r| roles.contains(r))
        .collect()
}

/// `coder,reviewer` (no spaces); empty string for no roles. The value of `MIRA_AGENT_ROLES`.
pub fn join_list(roles: &[Role]) -> String {
    normalize(roles)
        .iter()
        .map(|r| r.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Wire names, for `mira_mcp::tools::tools_for_roles`.
pub fn wire_names(roles: &[Role]) -> Vec<&'static str> {
    normalize(roles).iter().map(|r| r.as_str()).collect()
}

/// Prefix of the default folder (`<prefix>-<nn>`): one role and not a specialist → the role's
/// [`Role::figure_name`]; a specialist or several roles → `specialist`; no roles → `bot`.
pub fn prefix_for(roles: &[Role], specialist: bool) -> &'static str {
    let roles = normalize(roles);
    match roles.as_slice() {
        [one] if !specialist => one.figure_name(),
        [] if !specialist => NO_ROLE_PREFIX,
        _ => SPECIALIST_PREFIX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn role_wire_names() {
        let want = [
            "coder",
            "researcher",
            "reviewer",
            "coordinator",
            "planner",
            "debugger",
        ];
        for (role, s) in Role::ALL.into_iter().zip(want) {
            assert_eq!(role.as_str(), s);
            assert_eq!(serde_json::to_value(role).unwrap(), json!(s));
            assert_eq!(serde_json::from_value::<Role>(json!(s)).unwrap(), role);
            assert_eq!(Role::parse(s), Some(role));
        }
        assert!(serde_json::from_value::<Role>(json!("koord")).is_err());
        assert!(serde_json::from_value::<Role>(json!("Coder")).is_err());
        assert!(serde_json::from_value::<Role>(json!("none")).is_err());
        assert_eq!(Role::Coordinator.figure_name(), "koord");
        assert_eq!(Role::Planner.figure_name(), "planner");
        assert_eq!(Role::Coordinator.label_da(), "Koordinator");
        assert_eq!(Role::Planner.label_da(), "Planlægger");
        assert_eq!(Role::Coder.label_da(), "Koder");
        // Same names as the role matrix in mira-mcp.
        let matrix: Vec<&str> = mira_mcp::tools::ROLE_TOOLS
            .iter()
            .map(|(r, _)| *r)
            .collect();
        assert_eq!(matrix, want);
    }

    #[test]
    fn parse_list_dedups_and_orders() {
        assert_eq!(
            parse_list("reviewer, coder,reviewer"),
            [Role::Coder, Role::Reviewer]
        );
        assert_eq!(
            parse_list(" debugger ,nope,, koord,planner"),
            [Role::Planner, Role::Debugger]
        );
        assert!(parse_list("").is_empty());
        assert_eq!(
            join_list(&[Role::Reviewer, Role::Coder, Role::Coder]),
            "coder,reviewer"
        );
        assert_eq!(join_list(&[]), "");
        assert_eq!(parse_list(&join_list(&Role::ALL)), Role::ALL);
        assert_eq!(wire_names(&[Role::Coordinator]), ["coordinator"]);
    }

    #[test]
    fn prefix_for_matrix() {
        for (role, prefix) in Role::ALL.into_iter().zip([
            "coder",
            "researcher",
            "reviewer",
            "koord",
            "planner",
            "debugger",
        ]) {
            assert_eq!(prefix_for(&[role], false), prefix);
            assert_eq!(prefix_for(&[role], true), "specialist");
        }
        assert_eq!(prefix_for(&Role::ALL, true), "specialist");
        assert_eq!(
            prefix_for(&[Role::Coder, Role::Reviewer], false),
            "specialist"
        );
        assert_eq!(prefix_for(&[], false), "bot");
        assert_eq!(prefix_for(&[], true), "specialist");
        // Duplicates do not make a single role "several".
        assert_eq!(prefix_for(&[Role::Coder, Role::Coder], false), "coder");
    }
}
