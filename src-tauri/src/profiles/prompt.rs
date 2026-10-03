//! Per-profile system prompt addition `<app_data>/profiles/<id>/system-prompt.md` (plan5 C5.7),
//! passed with `--append-system-prompt-file`: the common part, one text per role, the profile's
//! own addition and the rules of the workspace.

use std::io;
use std::path::{Path, PathBuf};

use super::model::AgentProfile;
use crate::agent::roles::{self, Role};
use crate::config::{PROFILE_FILES_DIR, SYSTEM_PROMPT_FILE};
use crate::hooks::settings::write_atomic;
use crate::tickets::model::WorkspaceRules;

/// The common part (C4.6 with the step-5 tool line), for every agent.
pub const COMMON_PROMPT: &str = "Du kører som agent i mira-bots. Dine opgaver kommer som tickets; hver ticket ligger som fil i .mira-bots/tickets/<kort-id>.md i din arbejdsmappe, og appen beder dig om at læse den.

Regler:
- Når en ticket er færdig, SKAL du kalde værktøjet mira_submit_for_review med en kort opsummering (hvad du gjorde, hvad brugeren bør kigge på). Afslut først dit svar bagefter. Uden kaldet står ticketen som \"ikke afleveret\".
- Opdager du opfølgende arbejde, så opret en ny ticket med mira_create_ticket i stedet for at udvide opgaven.
- Er din igangværende ticket ikke til dig, så giv den videre med mira_handoff_ticket (med agentId til en anden agent; uden agentId tilbage i backlog) og afslut dit svar.
- mira_list_tickets og mira_get_ticket viser dine og andre tickets; mira_update_status sætter en kort statuslinje; mira_add_report lægger en rapport (markdown) på ticketen, så brugeren og revieweren kan se hvad du har lavet — gør det ved større opgaver, gerne som `report` i mira_submit_for_review; mira_get_workspace_rules viser reglerne; mira_list_projects viser projekterne (mapperne under projektroden).
- Rør ikke mappen .mira-bots/ manuelt (ingen filer, ingen redigering); appen ejer den. Undtagelsen er din egen worktree under .mira-bots/wt/, når din ticket peger dertil.
- Filerne mira-bots.workspace.json (projektroden) og .mira-bots/project.json (projektet) er brugerens; de er låst for dig — bed brugeren om ændringer i stedet for at forsøge at redigere dem.
";

/// The role texts (C5.7), verbatim.
pub fn role_text(role: Role) -> &'static str {
    match role {
        Role::Coder => "Du er koder: du implementerer tickets i din arbejdsmappe, kører tests og afleverer med en rapport der beskriver ændringerne.",
        Role::Researcher => "Du er researcher: du undersøger og dokumenterer; dine afleveringer er tekst (rapport), ikke kodeændringer, medmindre ticketen siger andet.",
        Role::Reviewer => "Du er reviewer: appen beder dig reviewe andres tickets som kritisk modpart. Læs review-filen, rapporterne («Ændringer», «Tjek», afsenderens rapport) og selve diffen (`git -C <mappe> diff <base>...<branch>`; du må ikke committe eller pushe). Rapportér fund som CRITICAL / WARNING / NICE-TO-HAVE med fil:linje, scenarie og rettelse; mindst ét CRITICAL eller WARNING betyder mira_reject_ticket med listen som note (læg den fulde liste som rapport først), ellers mira_approve_ticket med én linje om hvad du tjekkede. Vurdér aldrig på afsenderens opsummering alene. Reviewer du en forældre-ticket, er del-ticketsene allerede reviewet; vurdér helheden.",
        Role::Coordinator => "Du er koordinator: du splitter opgaver i tickets (mira_create_ticket, gerne med assignTo), tildeler og fjerner tildelinger (mira_assign_ticket/mira_unassign_ticket), starter agenter fra profiler når der mangler kapacitet (mira_list_profiles, mira_spawn_agent; lofterne gælder) og holder øje med fremdrift (mira_list_agents, mira_list_tickets all). Hver ticket skal have et projekt (`project`: et id fra mira_list_projects); en arbejdsagent kan kun få tickets fra sit eget projekt, så vælg agent efter projekt eller start en ny i det rigtige projekt (mira_spawn_agent med project). Del-tickets opretter du med `parentId` (standard: din igangværende ticket) og `blockedBy` for rækkefølge; afleverer du en ticket med åbne del-tickets, venter den automatisk, og du får besked i terminalen, når en del-ticket er godkendt. Du godkender ikke tickets selv; det gør reviewere eller brugeren. En ticket du får som koordineringsopgave, giver du videre med mira_assign_ticket eller deler op. Har ticketen en type med et forløb (fx feature eller bug), kan du i stedet starte forløbet med mira_start_playbook: appen opretter del-ticketsene i rækkefølge og tildeler dem efter rolle.",
        Role::Planner => "Du er planlægger: du nedbryder større mål i små, ordnede del-opgaver med klare acceptkriterier. Er der en koordinator i staben, afleverer du planen som rapport (mira_add_report) og opretter ikke tickets selv; ellers opretter du del-ticketsene med mira_create_ticket (med parentId), men tildeler dem ikke.",
        Role::Debugger => "Du er debugger: du reproducerer fejl, finder årsagen og retter eller dokumenterer den; skriv altid reproduktion og årsag i rapporten.",
    }
}

/// Added after the role texts when the profile has no work role (coder/researcher/debugger):
/// the file tools are denied for such a profile (review 5c W1/W3), Bash is not.
pub const NO_WORK_ROLE_TEXT: &str = "Du udfører aldrig selve arbejdet: du ændrer ikke kode eller filer (filværktøjerne er slået fra for din profil), og du bruger heller ikke Bash til at skrive eller ændre filer (ingen `>`/heredoc/sed -i). En arbejdsopgave giver du videre til en arbejdsagent (mira_handoff_ticket) eller deler op i tickets.";

/// Heading of the role section.
pub const ROLE_HEADING: &str = "## Din rolle";
/// Heading of the rules section (always last).
pub const RULES_HEADING: &str = "## Regler i dette workspace";

/// The project rule lines of [`rules_section`] (plan4b C4b.6).
fn project_rules(r: &WorkspaceRules) -> String {
    let mut out = String::from(
        "- Projekter er mapper under projektroden; en ticket skal have et projekt (`project`), før den kan tildeles en arbejdsagent. Angiv `project` på hver ticket du opretter (eksisterende id, eller `{\"new\": \"<navn>\"}`).\n",
    );
    out.push_str(if r.agents_may_create_projects {
        "- Agenter må oprette nye projekter; brug `{\"new\": …}` sparsomt.\n"
    } else {
        "- Agenter må ikke oprette nye projekter; bed brugeren om det.\n"
    });
    if r.max_agents_per_project > 0 {
        out.push_str(&format!(
            "- Højst {} agenter pr. projekt.\n",
            r.max_agents_per_project
        ));
    }
    out
}

/// The rules section: limits, review rounds, projects, report limits (the effective rules of
/// the workspace file, read when the agent starts).
fn rules_section(r: &WorkspaceRules) -> String {
    let stop = if r.auto_review_on_stop {
        "Når din turn slutter, sendes din igangværende ticket automatisk til review."
    } else {
        "Når din turn slutter uden mira_submit_for_review, står ticketen som \"ikke afleveret\"."
    };
    format!(
        "{RULES_HEADING}\n\
         - Højst {work} arbejdsagenter og {staff} stabsagenter kører samtidig.\n\
         - En ticket kan afvises i review højst {rounds} gange; derefter afgør brugeren.\n\
         - {stop}\n\
         {projects}\
         - Du kan oprette højst {rate} tickets i timen; en ticket-tekst må højst være {body} tegn.\n\
         - Højst {reports} rapporter pr. ticket, hver højst {report_body} tegn.\n",
        work = r.max_work_agents,
        staff = r.max_staff_agents,
        rounds = r.max_review_rounds,
        projects = project_rules(r),
        rate = r.create_ticket_rate_limit,
        body = r.ticket_body_max_chars,
        reports = r.reports_per_ticket_max,
        report_body = r.report_body_max_chars,
    )
}

/// The whole prompt: [`COMMON_PROMPT`], then (only with roles) "Din rolle" with one paragraph
/// per role in [`Role::ALL`] order, then the profile's `promptAppend` (if not blank), then the
/// rules. `\n` line endings, ends with `\n`.
pub fn render_profile_prompt(profile: &AgentProfile, rules: &WorkspaceRules) -> String {
    let mut out = String::from(COMMON_PROMPT);
    let roles = roles::normalize(&profile.roles);
    if !roles.is_empty() {
        out.push('\n');
        out.push_str(ROLE_HEADING);
        out.push('\n');
        for role in &roles {
            out.push_str(role_text(*role));
            out.push('\n');
        }
    }
    // Review 5c W5: also for a profile without any role (its file tools are denied too).
    if !roles::has_work_role(&roles) {
        if roles.is_empty() {
            out.push('\n');
        }
        out.push_str(NO_WORK_ROLE_TEXT);
        out.push('\n');
    }
    let append = profile.prompt_append.replace("\r\n", "\n");
    let append = append.trim();
    if !append.is_empty() {
        out.push('\n');
        out.push_str(append);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&rules_section(rules));
    out
}

/// `<data_dir>/profiles/<id>`.
pub fn profile_files_dir(data_dir: &Path, profile_id: &str) -> PathBuf {
    data_dir.join(PROFILE_FILES_DIR).join(profile_id)
}

/// Writes `<data_dir>/profiles/<id>/system-prompt.md` atomically and returns its path.
///
/// TODO(windows-verify): the per-profile file under %APPDATA%\dk.mira.bots\profiles\<id>\ with
/// æøå reaches claude via `--append-system-prompt-file` (plan5 D.50).
pub fn write_profile_prompt(
    data_dir: &Path,
    profile: &AgentProfile,
    rules: &WorkspaceRules,
) -> io::Result<PathBuf> {
    let target = profile_files_dir(data_dir, &profile.id).join(SYSTEM_PROMPT_FILE);
    write_atomic(&target, &render_profile_prompt(profile, rules))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::model::{builtin_profile, builtin_profiles};

    fn rules() -> WorkspaceRules {
        WorkspaceRules::defaults()
    }

    #[test]
    fn prompt_contains_each_role_text_once() {
        for p in builtin_profiles() {
            let text = render_profile_prompt(&p, &rules());
            assert!(text.starts_with(COMMON_PROMPT), "{}", p.id);
            assert_eq!(text.matches(ROLE_HEADING).count(), 1, "{}", p.id);
            for role in Role::ALL {
                let want = usize::from(p.roles.contains(&role));
                assert_eq!(
                    text.matches(role_text(role)).count(),
                    want,
                    "{} {role:?}",
                    p.id
                );
            }
        }
        let spec = render_profile_prompt(&builtin_profile("specialist").unwrap(), &rules());
        let positions: Vec<usize> = Role::ALL
            .iter()
            .map(|r| spec.find(role_text(*r)).unwrap())
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "Role::ALL order");
    }

    /// Review 5c W1/W3: the "do not do the work" paragraph follows the roles, not the seat, and
    /// only when the profile has no work role (a specialist with coder may code).
    #[test]
    fn no_work_role_paragraph_only_without_a_work_role() {
        for id in ["reviewer", "planner", "coordinator"] {
            let text = render_profile_prompt(&builtin_profile(id).unwrap(), &rules());
            assert_eq!(text.matches(NO_WORK_ROLE_TEXT).count(), 1, "{id}");
            let r = text.find(ROLE_HEADING).unwrap();
            let n = text.find(NO_WORK_ROLE_TEXT).unwrap();
            let rules_at = text.find(RULES_HEADING).unwrap();
            assert!(r < n && n < rules_at, "{id}");
        }
        for id in ["coder", "researcher", "debugger", "specialist"] {
            let text = render_profile_prompt(&builtin_profile(id).unwrap(), &rules());
            assert!(!text.contains(NO_WORK_ROLE_TEXT), "{id}");
            assert!(!text.contains("ændrer ikke"), "{id}");
        }
        // A custom profile without any role has no role section but still gets the paragraph.
        let none = AgentProfile {
            roles: vec![],
            ..builtin_profile("coder").unwrap()
        };
        let text = render_profile_prompt(&none, &rules());
        assert!(!text.contains(ROLE_HEADING));
        assert_eq!(text.matches(NO_WORK_ROLE_TEXT).count(), 1);
        assert!(text.find(NO_WORK_ROLE_TEXT).unwrap() < text.find(RULES_HEADING).unwrap());
        for role in Role::ALL {
            assert!(!role_text(role).contains("Bash til at skrive"), "{role:?}");
        }
        assert!(role_text(Role::Coordinator).contains("koordineringsopgave, giver du videre"));
    }

    #[test]
    fn prompt_without_roles_has_no_role_section() {
        let p = AgentProfile {
            roles: vec![],
            ..builtin_profile("coder").unwrap()
        };
        let text = render_profile_prompt(&p, &rules());
        assert!(!text.contains(ROLE_HEADING));
        assert!(Role::ALL.iter().all(|r| !text.contains(role_text(*r))));
        assert!(text.contains(RULES_HEADING));
    }

    #[test]
    fn prompt_append_is_last_before_rules() {
        let p = AgentProfile {
            prompt_append: "  Svar altid på dansk.\r\nBrug korte sætninger.  ".into(),
            ..builtin_profile("debugger").unwrap()
        };
        let text = render_profile_prompt(&p, &rules());
        let append = "Svar altid på dansk.\nBrug korte sætninger.\n";
        let a = text.find(append).unwrap();
        let r = text.find(RULES_HEADING).unwrap();
        assert!(text.find(role_text(Role::Debugger)).unwrap() < a);
        assert!(a < r);
        assert_eq!(&text[a + append.len()..r], "\n");
        assert!(text.ends_with('\n') && !text.contains('\r'));
        // The specialist's built-in addition sits in the same place.
        let spec = render_profile_prompt(&builtin_profile("specialist").unwrap(), &rules());
        assert!(
            spec.contains("Du har flere roller; brug den der passer til ticketen.\n\n## Regler")
        );
    }

    #[test]
    fn prompt_mentions_reports_and_rules() {
        let text = render_profile_prompt(&builtin_profile("coder").unwrap(), &rules());
        for needle in [
            "mira_submit_for_review",
            "mira_create_ticket",
            ".mira-bots/tickets/",
            "mira_add_report",
            "`report` i mira_submit_for_review",
            "mira_get_workspace_rules",
            "mira_handoff_ticket",
            "Højst 5 arbejdsagenter og 3 stabsagenter",
            "højst 3 gange",
            "Højst 20 rapporter pr. ticket, hver højst 20000 tegn.",
            "højst 20 tickets i timen",
            "mira_list_projects",
            "skal have et projekt",
            "må ikke oprette nye projekter",
        ] {
            assert!(text.contains(needle), "{needle}");
        }
        assert!(!text.contains("agenter pr. projekt"));
        assert!(text.trim_end().ends_with("20000 tegn."));
    }

    #[test]
    fn project_rules_follow_the_workspace() {
        let r = WorkspaceRules {
            agents_may_create_projects: true,
            max_agents_per_project: 2,
            ..rules()
        };
        let text = render_profile_prompt(&builtin_profile("coordinator").unwrap(), &r);
        assert!(
            text.contains("- Agenter må oprette nye projekter; brug `{\"new\": …}` sparsomt.\n")
        );
        assert!(!text.contains("må ikke oprette"));
        assert!(text.contains("- Højst 2 agenter pr. projekt.\n"));
        let rules_at = text.find(RULES_HEADING).unwrap();
        assert!(
            text.find("en ticket skal have et projekt (`project`), før")
                .unwrap()
                > rules_at
        );
        // The coordinator text names project before its last sentences.
        let coord = role_text(Role::Coordinator);
        assert!(coord.contains("mira_spawn_agent med project"));
        assert!(
            coord.find("Hver ticket skal have et projekt").unwrap()
                < coord.find("Du godkender ikke tickets selv").unwrap()
        );
    }

    #[test]
    fn write_profile_prompt_goes_to_the_profile_folder() {
        let dir = std::env::temp_dir().join(format!("mira-pprompt-{}", uuid::Uuid::new_v4()));
        let p = builtin_profile("reviewer").unwrap();
        let path = write_profile_prompt(&dir, &p, &rules()).unwrap();
        assert_eq!(
            path,
            dir.join("profiles")
                .join("reviewer")
                .join("system-prompt.md")
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            render_profile_prompt(&p, &rules())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---- step 6a (C6.3) ----

    #[test]
    fn common_prompt_mentions_locked_files() {
        let last = COMMON_PROMPT.trim_end().lines().last().unwrap();
        assert_eq!(
            last,
            "- Filerne mira-bots.workspace.json (projektroden) og .mira-bots/project.json (projektet) er brugerens; de er låst for dig — bed brugeren om ændringer i stedet for at forsøge at redigere dem."
        );
        for id in ["coder", "reviewer", "specialist"] {
            let p = crate::profiles::model::builtin_profile(id).unwrap();
            let text = render_profile_prompt(&p, &rules());
            assert_eq!(text.matches(last).count(), 1, "{id}");
        }
    }

    #[test]
    fn planner_text_mentions_report_and_parent_id() {
        assert_eq!(
            role_text(Role::Planner),
            "Du er planlægger: du nedbryder større mål i små, ordnede del-opgaver med klare acceptkriterier. Er der en koordinator i staben, afleverer du planen som rapport (mira_add_report) og opretter ikke tickets selv; ellers opretter du del-ticketsene med mira_create_ticket (med parentId), men tildeler dem ikke."
        );
        let coord = role_text(Role::Coordinator);
        let added = "Del-tickets opretter du med `parentId` (standard: din igangværende ticket) og `blockedBy` for rækkefølge; afleverer du en ticket med åbne del-tickets, venter den automatisk, og du får besked i terminalen, når en del-ticket er godkendt. ";
        let at = coord.find(added).expect("coordinator sentence");
        assert_eq!(
            at + added.len(),
            coord.find("Du godkender ikke tickets selv").unwrap()
        );
        assert!(coord.find("Hver ticket skal have et projekt").unwrap() < at);
        assert!(coord.ends_with(
            "En ticket du får som koordineringsopgave, giver du videre med mira_assign_ticket eller deler op. Har ticketen en type med et forløb (fx feature eller bug), kan du i stedet starte forløbet med mira_start_playbook: appen opretter del-ticketsene i rækkefølge og tildeler dem efter rolle."
        ));
    }

    // ---- step 6b (C6b.5) ----

    #[test]
    fn reviewer_text_mentions_critical_warning_and_reject() {
        assert_eq!(
            role_text(Role::Reviewer),
            "Du er reviewer: appen beder dig reviewe andres tickets som kritisk modpart. Læs review-filen, rapporterne («Ændringer», «Tjek», afsenderens rapport) og selve diffen (`git -C <mappe> diff <base>...<branch>`; du må ikke committe eller pushe). Rapportér fund som CRITICAL / WARNING / NICE-TO-HAVE med fil:linje, scenarie og rettelse; mindst ét CRITICAL eller WARNING betyder mira_reject_ticket med listen som note (læg den fulde liste som rapport først), ellers mira_approve_ticket med én linje om hvad du tjekkede. Vurdér aldrig på afsenderens opsummering alene. Reviewer du en forældre-ticket, er del-ticketsene allerede reviewet; vurdér helheden."
        );
        // Only the coordinator is told about mira_start_playbook (its tool, plan6b C6b.3).
        for role in Role::ALL {
            assert_eq!(
                role_text(role).contains("mira_start_playbook"),
                role == Role::Coordinator,
                "{role:?}"
            );
        }
    }
}
