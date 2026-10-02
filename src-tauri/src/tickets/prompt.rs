//! What the agent receives for a ticket: a file in its cwd (`.mira-bots/tickets/<short>.md`) and
//! one typed line pointing at it (plan C3.5/C3.6). The body never goes to the PTY.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::model::{Ticket, TicketState};
use crate::agent::roles::{self, Role};
use crate::agent::SeatKind;
use crate::config::{MAX_REVIEW_ROUNDS, REVIEW_DIR, TICKET_DIR, TICKET_LINE_TITLE_MAX_CHARS};

/// Title used when nothing is left after sanitising.
pub const EMPTY_TITLE: &str = "(uden titel)";
/// Body text in the file when the ticket has none.
pub const EMPTY_BODY: &str = "(ingen beskrivelse)";
/// Chars that must not start the sanitised title (TUI meaning: command, shell mode, file mention,
/// emoji shortcode, shortcut help, web session, CLI flag; research3 §6).
pub const FORBIDDEN_FIRST: &[char] = &['/', '!', '@', ':', '?', '&', '-'];

/// Invisible chars Claude Code strips on Enter (which then sends nothing; research3 §1b) plus
/// all C0/C1 controls except `\t`, `\r`, `\n` (those become spaces later). ZWJ/ZWNJ are kept.
fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{200B}' | '\u{2060}' | '\u{FEFF}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2066}'..='\u{2069}'
        | '\u{E0000}'..='\u{E007F}')
        || (c.is_control() && !matches!(c, '\t' | '\r' | '\n'))
}

/// Inserts `-` after "ultra" in every case-insensitive `ultrathink` (keeps the original case).
fn defuse_ultrathink(s: &str) -> String {
    const WORD: &str = "ultrathink";
    let lower = s.to_ascii_lowercase(); // same byte offsets as `s`
    let mut out = String::with_capacity(s.len() + 4);
    let mut rest = 0;
    while let Some(i) = lower[rest..].find(WORD) {
        let start = rest + i;
        out.push_str(&s[rest..start + 5]);
        out.push('-');
        out.push_str(&s[start + 5..start + WORD.len()]);
        rest = start + WORD.len();
    }
    out.push_str(&s[rest..]);
    out
}

/// Makes a ticket title safe to type into Claude Code's prompt (plan B.5, steps a–i).
pub fn sanitize_title(raw: &str) -> String {
    // (a) invisible chars and controls.
    let s: String = raw.chars().filter(|c| !is_invisible(*c)).collect();
    // (b) line breaks and tabs → space; (c) collapse whitespace runs (also Unicode line
    // separators), trim.
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    // (d) `@` opens the file-mention picker.
    let s = s.replace('@', "(at)");
    // (e) `:` + non-space can open emoji shortcode suggestions.
    let mut e = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        e.push(c);
        if c == ':' && chars.peek().is_some_and(|n| *n != ' ') {
            e.push(' ');
        }
    }
    // (f) a word starting with `/` would open command autocomplete: use U+2215 instead.
    let s = e
        .split(' ')
        .map(|w| match w.strip_prefix('/') {
            Some(rest) => format!("\u{2215}{rest}"),
            None => w.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    // (g) keyword that raises the effort level.
    let s = defuse_ultrathink(&s);
    // First-char rule: drop leading chars from FORBIDDEN_FIRST (and the spaces they leave).
    let s = s.trim_start_matches(|c: char| FORBIDDEN_FIRST.contains(&c) || c == ' ');
    // (h) truncate (chars), `…` marks the cut.
    let s = if s.chars().count() > TICKET_LINE_TITLE_MAX_CHARS {
        let mut t: String = s.chars().take(TICKET_LINE_TITLE_MAX_CHARS - 1).collect();
        t = t.trim_end().to_string();
        t.push('…');
        t
    } else {
        s.to_string()
    };
    // (i) nothing left.
    if s.is_empty() {
        EMPTY_TITLE.to_string()
    } else {
        s
    }
}

/// The one line typed into the agent's terminal (no `\r`/`\n`; always starts with `Ticket `).
/// `title` must already be sanitised.
// TODO(windows-verify): the agent reads the file via the `/`-separated relative path without a
// permission prompt (plan D.30).
pub fn render_line(short: &str, title: &str) -> String {
    format!(
        "Ticket {short}: {title}. Læs filen {TICKET_DIR}/{short}.md og udfør opgaven. Afslut dit svar når opgaven er færdig."
    )
}

/// Start of a coordination line: ASCII only (review 5c N1), so the typed line and the hook's
/// `prompt` stay byte-identical whatever the terminal does to non-ASCII input.
pub const COORDINATION_LINE_PREFIX: &str = "Koordiner ticket ";

/// The line for a ticket delivered as a coordination task (5c C.1; a staff seat or a profile
/// without a work role): it distributes the work instead of doing it. Like [`render_line`] one
/// line that never starts with "Du"; the dispatcher confirms it with
/// [`is_coordination_line_for`].
pub fn render_coordination_line(short: &str, title: &str) -> String {
    format!(
        "{COORDINATION_LINE_PREFIX}{short}: {title}. Læs filen {TICKET_DIR}/{short}.md og fordel opgaven; udfør den ikke selv."
    )
}

/// Whether `prompt` is the coordination line for the ticket `short`: tolerant of the first
/// word's spelling ("Koordiner", "Koordinér", a lost or decomposed accent), strict about
/// "ticket <short>" after it (review 5c N1).
pub fn is_coordination_line_for(prompt: &str, short: &str) -> bool {
    prompt.trim_start().starts_with("Koordin")
        && prompt
            .trim_start()
            .split_once(' ')
            .is_some_and(|(_, rest)| rest.starts_with(&format!("ticket {short}")))
}

/// What an agent on a staff seat is asked to do with a ticket (5c C.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoordinationKind {
    /// Has the coordinator role: hand the ticket to a work agent (or split it up).
    Distribute,
    /// Reviewer/planner without the coordinator role: split it into backlog tickets (or, when
    /// a coordinator runs, write the plan as a report; step 6a).
    Plan,
}

/// One child of the delivered ticket for the file's `## Del-tickets` (step 6a, C6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildLine {
    pub short: String,
    /// The raw title (made one-line when rendered).
    pub title: String,
    pub state: TicketState,
    /// Short ids of its open blockers.
    pub open_blockers: Vec<String>,
}

/// One child of a parent in review, for the review file (step 6a, C6.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildReview {
    pub short: String,
    /// The raw title (made one-line when rendered).
    pub title: String,
    pub state: TicketState,
    /// The child's summary (agent text: one line, cut to [`CHILD_SUMMARY_MAX_CHARS`]).
    pub summary: Option<String>,
}

/// A child's summary in a parent's review file is cut to this many characters (then "…").
pub const CHILD_SUMMARY_MAX_CHARS: usize = 300;

/// Other work agents in the same project folder (plan4b A.5): the `## Delt projekt` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedProject {
    pub project: String,
    /// Names of the other live work agents in the project.
    pub others: Vec<String>,
}

/// The project list of a coordination task (plan4b A.6): it changes while the agent runs, so it
/// goes in the file, not in the system prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectList {
    pub ids: Vec<String>,
    /// `agentsMayCreateProjects` from the workspace file.
    pub may_create: bool,
}

/// How a ticket is delivered: as work (the default) or as a coordination task (the assignee sits
/// on a staff seat), plus the step 4b file sections.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TicketDelivery {
    pub coordination: Option<CoordinationKind>,
    /// `## Delt projekt` (only for real work deliveries).
    pub shared: Option<SharedProject>,
    /// "Projekter lige nu: …" (only for coordination tasks).
    pub projects: Option<ProjectList>,
    /// A live agent has the coordinator role, or the ticket's parent belongs to one (step 6a):
    /// a [`CoordinationKind::Plan`] task then gets
    /// [`COORDINATION_PLAN_WITH_COORDINATOR_TEXT`].
    pub coordinator_available: bool,
    /// The ticket's children (`## Del-tickets`, step 6a); empty = no section.
    pub children: Vec<ChildLine>,
}

impl TicketDelivery {
    /// A plain work delivery (unchanged line).
    pub fn work() -> TicketDelivery {
        TicketDelivery::default()
    }

    /// Whether this is a real work delivery (not a coordination task).
    pub fn is_work(&self) -> bool {
        self.coordination.is_none()
    }

    /// Adds the `## Delt projekt` section (plan4b A.5); ignored for a coordination task or
    /// without other agents.
    pub fn with_shared(mut self, project: &str, others: Vec<String>) -> Self {
        if self.is_work() && !others.is_empty() {
            self.shared = Some(SharedProject {
                project: project.to_string(),
                others,
            });
        }
        self
    }

    /// Adds the project list line (plan4b A.6); ignored for a work delivery.
    pub fn with_projects(mut self, ids: Vec<String>, may_create: bool) -> Self {
        if !self.is_work() {
            self.projects = Some(ProjectList { ids, may_create });
        }
        self
    }

    /// Adds the ticket's children (`## Del-tickets`, step 6a).
    pub fn with_children(mut self, children: Vec<ChildLine>) -> Self {
        self.children = children;
        self
    }

    /// Sets [`Self::coordinator_available`] (step 6a).
    pub fn with_coordinator(mut self, available: bool) -> Self {
        self.coordinator_available = available;
        self
    }

    /// The delivery for an agent on `seat` with `roles` (review 5c W1: the role decides what
    /// the agent may do, the seat which tickets it gets). The work delivery only for a work
    /// seat AND a work role (coder/researcher/debugger); a staff seat, or a profile without a
    /// work role (it may not edit files), gets a coordination task:
    /// [`CoordinationKind::Distribute`] with the coordinator role, else
    /// [`CoordinationKind::Plan`].
    pub fn for_agent(seat: SeatKind, roles: &[Role]) -> TicketDelivery {
        let coordination = if seat == SeatKind::Work && roles::has_work_role(roles) {
            None
        } else if roles.contains(&Role::Coordinator) {
            Some(CoordinationKind::Distribute)
        } else {
            Some(CoordinationKind::Plan)
        };
        TicketDelivery {
            coordination,
            ..TicketDelivery::default()
        }
    }
}

/// `## Delt projekt` (plan4b C4b.6); `{project}` and `{others}` are filled in.
// TODO(windows-verify): with two work agents in one project the second agent's ticket file has
// the "Delt projekt" section naming the first, the project name on the desk shows "⚠" until a
// coordinator runs, and maxAgentsPerProject refuses the next agent (plan4b D.84).
pub const SHARED_PROJECT_TEXT: &str = "Andre agenter arbejder i samme mappe (projekt «{project}»): {others}. Hold dig til de filer din ticket handler om. Brug `git add <stier>` på netop dine filer — aldrig `git add -A` eller `git add .`. Kør ikke `git reset`, `git checkout -- <fil>`, `git stash` eller andet, der rører de andres ændringer; opdager du ændringer, du ikke selv har lavet, så lad dem stå. Skriv i din rapport, hvilke filer du har rørt.";

/// The `## Delt projekt` text for `shared`.
pub fn shared_project_text(shared: &SharedProject) -> String {
    let others: Vec<String> = shared.others.iter().map(|n| one_line(n)).collect();
    SHARED_PROJECT_TEXT
        .replace("{project}", &one_line(&shared.project))
        .replace("{others}", &others.join(", "))
}

/// The project line of a coordination task (plan4b C4b.6).
// TODO(windows-verify): the staff agent (in the projects root) can read and `ls`/`git -C`
// `projects\<p>\…` without extra directory flags or permission prompts, mira_list_projects lists the
// folders and the coordination file shows "Projekter lige nu: …" (plan4b D.80).
pub fn project_list_text(list: &ProjectList) -> String {
    let ids = if list.ids.is_empty() {
        "ingen".to_string()
    } else {
        list.ids.join(", ")
    };
    let create = if list.may_create {
        "kan oprettes med {\"new\": …}"
    } else {
        "skal brugeren oprette (agentsMayCreateProjects er slået fra)"
    };
    format!(
        "Projekter lige nu: {ids}. Angiv `project` på hver ticket du opretter; nye projekter {create}. Mangler ticketen et projekt, angiv `project` når du giver den videre (mira_assign_ticket/mira_handoff_ticket)."
    )
}

/// The `- projekt:` header line value of a ticket file.
fn project_header(t: &Ticket) -> String {
    match &t.project {
        Some(crate::projects::ProjectRef::Existing(id)) => one_line(id),
        Some(crate::projects::ProjectRef::New { new }) => {
            format!("{} (oprettes ved tildeling)", one_line(new))
        }
        None => "ingen".to_string(),
    }
}

/// `## Koordineringsopgave` text for an agent with the coordinator role.
pub const COORDINATION_DISTRIBUTE_TEXT: &str = "Du sidder på en stabsplads eller har ingen arbejdsrolle: udfør IKKE opgaven selv (skriv ingen kode og ingen filer; brug heller ikke Bash til at skrive eller ændre filer (ingen `>`/heredoc/sed -i)). Tjek først `mira_list_tickets all` for del-tickets og dubletter, der allerede findes. Kan én ledig arbejdsagent (mira_list_agents) tage hele opgaven, så giv den ticketen med mira_assign_ticket (den flytter fra dig; arbejd så ikke videre på den). Ellers opret del-tickets med mira_create_ticket: `parentId` er som standard denne ticket, `assignTo` giver dem direkte til en agent, og `blockedBy` lader en del-ticket vente på en anden — fx byg-ticketen til koderen med det samme med `blockedBy: [plan-ticketens id]`. Få, større del-tickets er bedre end mange små (højst 20 oprettelser i timen). Aflever så denne ticket med mira_submit_for_review og en kort fordelingsplan: den venter automatisk, til del-ticketsene er godkendt, og du får besked efter hver — tildel næste del-ticket eller aflever igen. Er der ingen ledig arbejdsagent, start en fra en profil med mira_spawn_agent (mira_list_profiles) hvis der er en fri arbejdsplads; ellers skriv hvorfor med mira_update_status og læg ticketen tilbage i backlog med mira_unassign_ticket.";
/// `## Koordineringsopgave` text for an agent without the coordinator role (reviewer, planner,
/// no roles): hand the ticket to a free work agent (review 5c W2), else split it up.
pub const COORDINATION_PLAN_TEXT: &str = "Du sidder på en stabsplads eller har ingen arbejdsrolle: udfør IKKE opgaven selv (skriv ingen kode og ingen filer; brug heller ikke Bash til at skrive eller ændre filer (ingen `>`/heredoc/sed -i)). Find en ledig arbejdsagent (arbejdsplads, rollen koder, researcher eller debugger, ingen ticket i gang) med mira_list_agents og giv ticketen videre med mira_handoff_ticket(ticketId, agentId); arbejd så ikke videre på den. Er der ingen ledig arbejdsagent, så del opgaven op i del-tickets med mira_create_ticket (`parentId` = denne ticket, så de hænger sammen; de lander i backlog), og aflever denne ticket med planen med mira_submit_for_review — eller læg den tilbage i backlog med mira_handoff_ticket uden agentId.";
/// `## Koordineringsopgave` text for an agent without the coordinator role while a coordinator
/// runs, or when the ticket's parent belongs to one (step 6a, C6.3): only the plan, as a report.
pub const COORDINATION_PLAN_WITH_COORDINATOR_TEXT: &str = "Du sidder på en stabsplads eller har ingen arbejdsrolle: udfør IKKE opgaven selv (skriv ingen kode og ingen filer; brug heller ikke Bash til at skrive eller ændre filer (ingen `>`/heredoc/sed -i)). Der er en koordinator i staben: skriv planen som rapport (mira_add_report, eller `report` i mira_submit_for_review) med små, ordnede del-opgaver og klare acceptkriterier, og aflever ticketen. Opret IKKE selv del-tickets og tildel ingen; det gør koordinatoren ud fra din plan.";

/// Intro of a ticket file's `## Del-tickets` section (step 6a, C6.3).
pub const CHILDREN_INTRO: &str = "Denne ticket har del-tickets; tjek dem før du opretter nye:";
/// The extra rule in a parent's review file (step 6a, C6.3).
pub const PARENT_REVIEW_RULE: &str = "- Del-ticketsene er allerede reviewet hver for sig; vurdér helheden: hænger delene sammen, og er ticketens mål nået? Åbne del-tickets taler imod godkendelse.";
/// Children named in a wake line; the rest are counted (" og {k} til").
pub const WAKE_LINE_MAX_CHILDREN: usize = 3;

/// What a wake line says (step 6a, plan A.2): the waiting parent, the children that became Done
/// since it was last in progress (Done order; raw titles) and how many are still open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WakeInfo {
    pub parent_short: String,
    /// `(short id, raw title)` of the newly done children.
    pub newly_done: Vec<(String, String)>,
    pub open_left: usize,
}

/// `a`, `a og b`, `a, b og c` (Danish list).
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} og {last}", init.join(", ")),
    }
}

/// The wake line typed into a waiting parent's assignee (step 6a, C6.3). Starts with "Du har
/// fået besked:" — never with "Ticket", "Koordin…" or "Review af ticket", so the dispatcher never
/// takes it for a delivery ([`super::dispatcher`]'s `DeliveryKind::confirms`). Only short ids and
/// sanitised titles; never a child's summary (another agent's text, research §3.2). At most
/// [`WAKE_LINE_MAX_CHILDREN`] children are named. One line, no `\r`/`\n`.
pub fn wake_line(w: &WakeInfo) -> String {
    let p = &w.parent_short;
    let named: Vec<String> = w
        .newly_done
        .iter()
        .take(WAKE_LINE_MAX_CHILDREN)
        .map(|(short, title)| format!("{short} ({})", sanitize_title(title)))
        .collect();
    let more = w.newly_done.len().saturating_sub(WAKE_LINE_MAX_CHILDREN);
    if w.open_left == 0 {
        return match w.newly_done.last() {
            Some((short, title)) => format!(
                "Du har fået besked: alle del-tickets til ticket {p} er afsluttet (sidst {short}: {}). Læs opsummeringerne med mira_get_ticket, og aflever ticket {p} med mira_submit_for_review (ticketId {p}) med en samlet opsummering.",
                sanitize_title(title)
            ),
            None => format!(
                "Du har fået besked: alle del-tickets til ticket {p} er afsluttet. Aflever ticket {p} med mira_submit_for_review (ticketId {p})."
            ),
        };
    }
    let m = w.open_left;
    let tail = format!(
        "Tildel næste del-ticket hvis der mangler én, og aflever så ticket {p} igen med mira_submit_for_review (ticketId {p})."
    );
    match w.newly_done.as_slice() {
        [(short, _)] => format!(
            "Du har fået besked: del-ticket {} til ticket {p} er godkendt; {m} del-ticket(s) mangler stadig. Læs opsummeringen med mira_get_ticket {short}. {tail}",
            named[0]
        ),
        _ => {
            let mut list = and_list(&named);
            if more > 0 {
                list = format!("{} og {more} mere", named.join(", "));
            }
            format!(
                "Du har fået besked: del-tickets {list} til ticket {p} er godkendt; {m} del-ticket(s) mangler stadig. Læs opsummeringerne med mira_get_ticket. {tail}"
            )
        }
    }
}

/// "Bed om aflevering" (C4.7): typed like a ticket line (one write, `\r` separately). It starts
/// with "Du", never with "Ticket", so the dispatcher can never take it for a ticket delivery.
pub fn request_submission_line(short: &str) -> String {
    format!(
        "Du afsluttede uden at aflevere ticket {short}. Kald mira_submit_for_review med en kort opsummering når opgaven er færdig; ellers fortsæt arbejdet."
    )
}

/// "Stop working" (review 5c W4): typed into an idle agent whose ticket in progress someone
/// else (the user) handed to `to_name` (`None`: put back in the backlog). Starts with "Du",
/// so the dispatcher never takes it for a ticket delivery; the name is sanitised like a title.
pub fn handed_over_line(short: &str, to_name: Option<&str>) -> String {
    let whereto = match to_name {
        Some(name) => format!("den er givet videre til {}", sanitize_title(name)),
        None => "den er lagt tilbage i backlog".to_string(),
    };
    format!("Du skal stoppe arbejdet på ticket {short}: {whereto}. Afslut dit svar.")
}

/// Detail text of an agent whose ticket in progress left it (review 5c W4).
pub fn handed_over_detail(short: &str, to_agent: bool) -> String {
    if to_agent {
        format!("Ticket {short} givet videre")
    } else {
        format!("Ticket {short} lagt tilbage")
    }
}

/// Whether `detail` is a [`handed_over_detail`] text (cleared like the other dispatcher hints).
pub fn is_handed_over_detail(detail: &str) -> bool {
    detail.starts_with("Ticket ")
        && (detail.ends_with(" givet videre") || detail.ends_with(" lagt tilbage"))
}

/// The line for a ticket (sanitises the title): [`render_line`], or
/// [`render_coordination_line`] for a coordination task.
pub fn line_for(t: &Ticket, delivery: &TicketDelivery) -> String {
    let (short, title) = (t.short_id(), sanitize_title(&t.title));
    match delivery.coordination {
        None => render_line(&short, &title),
        Some(_) => render_coordination_line(&short, &title),
    }
}

/// One-line form for headings and agent-supplied titles/notes: CRLF/CR/LF/tab → space, other
/// controls removed, trimmed.
pub(crate) fn one_line(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\r' | '\n' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Body text from an agent (`mira_create_ticket`): CRLF → LF; C0/C1 controls removed except
/// `\n`, `\t` and `\r`.
pub fn clean_body(s: &str) -> String {
    s.replace("\r\n", "\n")
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t' | '\r'))
        .collect()
}

/// Content of `<cwd>/.mira-bots/tickets/<short>.md` (plan C3.6, rules from plan4 C4.7); a
/// coordination task gets `## Koordineringsopgave` before `## Regler` (5c C.1).
pub fn render_file(t: &Ticket, now_ms: u64, delivery: &TicketDelivery) -> String {
    let short = t.short_id();
    let body = t.body.replace("\r\n", "\n");
    let body = body.trim_end_matches('\n');
    let body = if body.trim().is_empty() {
        EMPTY_BODY
    } else {
        body
    };
    let review = if t.skip_review { "springes over" } else { "ja" };
    let mut out = format!(
        "# Ticket {short}: {title}\n\n\
         - id: {id}\n\
         - kort-id: {short}\n\
         - oprettet: {created}\n\
         - opdateret: {updated}\n\
         - status: {state}\n\
         - review: {review}\n\
         - projekt: {project}\n\n\
         ## Opgave\n\n\
         {body}\n\n",
        title = one_line(&t.title),
        id = t.id,
        created = iso_utc(t.created_at),
        updated = iso_utc(now_ms),
        state = t.state.as_str(),
        project = project_header(t),
    );
    if let Some(note) = &t.rejection_note {
        out.push_str(&format!(
            "## Afvist: {}\n\
             Ret det ovenstående og afslut dit svar igen, så ticketen kommer til review på ny.\n\n",
            one_line(note)
        ));
    }
    if let Some(kind) = delivery.coordination {
        let text = match kind {
            CoordinationKind::Distribute => COORDINATION_DISTRIBUTE_TEXT,
            CoordinationKind::Plan if delivery.coordinator_available => {
                COORDINATION_PLAN_WITH_COORDINATOR_TEXT
            }
            CoordinationKind::Plan => COORDINATION_PLAN_TEXT,
        };
        out.push_str(&format!("## Koordineringsopgave\n{text}\n"));
        if let Some(list) = &delivery.projects {
            out.push_str(&project_list_text(list));
            out.push('\n');
        }
        out.push('\n');
    }
    if !delivery.children.is_empty() {
        out.push_str(&format!("## Del-tickets\n{CHILDREN_INTRO}\n"));
        for c in &delivery.children {
            out.push_str(&format!(
                "- {} {} — {}",
                c.short,
                one_line(&c.title),
                c.state.label_da()
            ));
            if !c.open_blockers.is_empty() {
                out.push_str(&format!(" — venter på {}", c.open_blockers.join(", ")));
            }
            out.push('\n');
        }
        out.push('\n');
    }
    if let Some(shared) = delivery.shared.as_ref().filter(|_| delivery.is_work()) {
        out.push_str(&format!(
            "## Delt projekt\n{}\n\n",
            shared_project_text(shared)
        ));
    }
    out.push_str(
        "## Regler\n\
         - Opgaven er en ticket fra mira-bots. Når den er løst, kald værktøjet mira_submit_for_review med en kort opsummering, og afslut så dit svar.\n\
         - Opret opfølgende opgaver med mira_create_ticket. Opret eller redigér ikke selv filer i .mira-bots/.\n\
         - Læg en rapport på ticketen med mira_add_report (eller `report` i mira_submit_for_review) når du har lavet noget brugeren skal kunne læse om.\n",
    );
    out
}

/// `<cwd>/.mira-bots/tickets`, joined component by component.
pub fn ticket_dir(cwd: &Path) -> PathBuf {
    TICKET_DIR
        .split('/')
        .fold(cwd.to_path_buf(), |p, part| p.join(part))
}

/// Writes the ticket file (overwriting) and, if missing, `<cwd>/.mira-bots/.gitignore` with `*`.
/// Returns the file's path.
pub fn write_ticket_file(
    cwd: &Path,
    t: &Ticket,
    now_ms: u64,
    delivery: &TicketDelivery,
) -> io::Result<PathBuf> {
    let dir = ticket_dir(cwd);
    fs::create_dir_all(&dir)?;
    if let Some(root) = dir.parent() {
        let gitignore = root.join(".gitignore");
        if !gitignore.exists() {
            fs::write(&gitignore, "*\n")?;
        }
    }
    let path = dir.join(format!("{}.md", t.short_id()));
    fs::write(&path, render_file(t, now_ms, delivery))?;
    Ok(path)
}

// ---- review deliveries (plan5 C5.12) ----

/// Text in the review line and file when the sender's agent no longer exists.
pub const SENDER_DIR_UNKNOWN: &str = "afsenderens mappe kendes ikke længere";

/// A folder path for the typed review line: one line, invisible chars removed, `@` (file-mention
/// picker) as `(at)`. Separators, drive colons and spaces stay (it must remain a usable path).
// TODO(windows-verify): a typed `C:\Users\…` path (colon + backslash) opens no emoji or command
// suggestion in the TUI (plan5 D.54).
fn line_safe_path(path: &str) -> String {
    let s: String = path.chars().filter(|c| !is_invisible(*c)).collect();
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('@', "(at)")
}

/// The review line typed into the reviewer's terminal (C5.12; one line, always starts with
/// "Review af ticket"). `title` must already be sanitised; `sender_cwd` `None` when the sender
/// is gone; the report count is left out at 0.
pub fn render_review_line(
    short: &str,
    title: &str,
    sender_cwd: Option<&str>,
    report_count: usize,
) -> String {
    let work = match sender_cwd {
        Some(cwd) => format!("afsenderens arbejde ligger i {}", line_safe_path(cwd)),
        None => SENDER_DIR_UNKNOWN.to_string(),
    };
    let fetch = if report_count == 0 {
        format!("opsummering fås med mira_get_ticket {short}")
    } else {
        format!(
            "opsummering og {report_count} rapport(er) fås med mira_get_ticket {short} og mira_get_report"
        )
    };
    format!(
        "Review af ticket {short}: {title}. Læs {REVIEW_DIR}/{short}.md i din mappe; {work}, og {fetch}. Kald mira_approve_ticket eller mira_reject_ticket med en note."
    )
}

/// The review line for a ticket (sanitises the title).
pub fn review_line_for(t: &Ticket, sender_cwd: Option<&str>) -> String {
    render_review_line(
        &t.short_id(),
        &sanitize_title(&t.title),
        sender_cwd,
        t.reports.len(),
    )
}

/// Who sent the ticket to review, for the review file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSender {
    pub name: String,
    pub cwd: String,
}

/// When the ticket last went into review (history), else `updated_at`.
fn submitted_at(t: &Ticket) -> u64 {
    t.history
        .iter()
        .rev()
        .find(|h| h.to == TicketState::Review && h.from != Some(TicketState::Review))
        .map_or(t.updated_at, |h| h.at)
}

/// A child's summary for a parent's review file: one line, cut to [`CHILD_SUMMARY_MAX_CHARS`]
/// with "…"; "(ingen opsummering)" when there is none.
fn child_summary(summary: Option<&str>) -> String {
    let s = summary.map(one_line).unwrap_or_default();
    if s.is_empty() {
        return "(ingen opsummering)".to_string();
    }
    if s.chars().count() <= CHILD_SUMMARY_MAX_CHARS {
        return s;
    }
    let mut out: String = s.chars().take(CHILD_SUMMARY_MAX_CHARS).collect();
    out.push('…');
    out
}

/// Content of `<reviewer cwd>/.mira-bots/reviews/<short>.md` (C5.12). `author_name` turns a
/// report author into a display name. A parent (`children` not empty, step 6a) gets
/// `## Del-tickets (allerede reviewet)` before `## Opgaven` and [`PARENT_REVIEW_RULE`].
pub fn render_review_file(
    t: &Ticket,
    sender: Option<&ReviewSender>,
    author_name: &dyn Fn(&super::model::ReportAuthor) -> String,
    children: &[ChildReview],
) -> String {
    let short = t.short_id();
    let (sender_line, git_dir) = match sender {
        Some(s) => (
            format!("{} ({})", one_line(&s.name), one_line(&s.cwd)),
            one_line(&s.cwd),
        ),
        None => (SENDER_DIR_UNKNOWN.to_string(), "<afsenderens mappe>".into()),
    };
    let summary = t
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("(ingen)");
    let body = t.body.replace("\r\n", "\n");
    let body = body.trim_end_matches('\n');
    let body = if body.trim().is_empty() {
        EMPTY_BODY
    } else {
        body
    };
    let mut out = format!(
        "# Review af ticket {short}: {title}\n\
         Afsender: {sender_line}   Runde: {round} af {MAX_REVIEW_ROUNDS}   Afleveret: {at}\n\
         ## Opsummering fra afsenderen\n\
         {summary}\n\
         ## Rapporter\n",
        title = one_line(&t.title),
        // Capped like the card: a hand-picked reviewer of an escalated ticket (review_round = 3)
        // reads "3 af 3", not "4 af 3" (review5 N5).
        round = (t.review_round + 1).min(MAX_REVIEW_ROUNDS),
        at = iso_utc(submitted_at(t)),
    );
    if t.reports.is_empty() {
        out.push_str("(ingen)\n");
    }
    for r in &t.reports {
        out.push_str(&format!(
            "- {id} {title} ({author}, {at}) → mira_get_report {short} {id}\n",
            id = r.id,
            title = one_line(&r.title),
            author = one_line(&author_name(&r.author)),
            at = iso_utc(r.created_at),
        ));
    }
    if !children.is_empty() {
        out.push_str("## Del-tickets (allerede reviewet)\n");
        for c in children {
            out.push_str(&format!(
                "- {} {} — {}: {}\n",
                c.short,
                one_line(&c.title),
                c.state.label_da(),
                child_summary(c.summary.as_deref())
            ));
        }
    }
    out.push_str(&format!(
        "## Opgaven\n\
         {body}\n\
         ## Regler\n\
         - Læs ændringerne med git -C \"{git_dir}\" diff/log/status/show; ret ikke selv i afsenderens mappe, og commit/push aldrig.\n\
         - Afgør med mira_approve_ticket {short} (note: hvad du tjekkede) eller mira_reject_ticket {short} (note: hvad der mangler, konkret).\n\
         - Læg gerne en review-rapport med mira_add_report før du afgør.\n"
    ));
    if !children.is_empty() {
        out.push_str(PARENT_REVIEW_RULE);
        out.push('\n');
    }
    out
}

/// `<cwd>/.mira-bots/reviews`, joined component by component.
pub fn review_dir(cwd: &Path) -> PathBuf {
    REVIEW_DIR
        .split('/')
        .fold(cwd.to_path_buf(), |p, part| p.join(part))
}

/// Writes the review file in the reviewer's folder (overwriting) and, if missing,
/// `<cwd>/.mira-bots/.gitignore` with `*`. Returns the file's path.
pub fn write_review_file(
    cwd: &Path,
    t: &Ticket,
    sender: Option<&ReviewSender>,
    author_name: &dyn Fn(&super::model::ReportAuthor) -> String,
    children: &[ChildReview],
) -> io::Result<PathBuf> {
    let dir = review_dir(cwd);
    fs::create_dir_all(&dir)?;
    if let Some(root) = dir.parent() {
        let gitignore = root.join(".gitignore");
        if !gitignore.exists() {
            fs::write(&gitignore, "*\n")?;
        }
    }
    let path = dir.join(format!("{}.md", t.short_id()));
    fs::write(&path, render_review_file(t, sender, author_name, children))?;
    Ok(path)
}

/// `YYYY-MM-DDTHH:MM:SSZ` for Unix ms (UTC; no chrono).
pub fn iso_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tickets::model::test_support::ticket;
    use crate::tickets::model::TicketState;

    const ID: &str = "ABCDEF01-2345-4678-9abc-def012345678";

    fn assert_line_safe(line: &str) {
        assert!(line.starts_with("Ticket "), "{line}");
        assert!(!line.chars().any(|c| c.is_control()), "{line:?}");
    }

    #[test]
    fn a_strips_invisible_and_control_chars_but_keeps_zwj() {
        assert_eq!(
            sanitize_title(
                "a\u{200B}b\u{2060}c\u{FEFF}d\u{202E}e\u{2068}f\u{E0041}g\u{1b}h\u{85}i\u{7f}j"
            ),
            "abcdefghij"
        );
        assert_eq!(sanitize_title("x\u{200D}y\u{200C}z"), "x\u{200D}y\u{200C}z");
    }

    #[test]
    fn b_c_newlines_tabs_become_single_spaces() {
        assert_eq!(
            sanitize_title("  one\r\ntwo\nthree\rfour\tfive  "),
            "one two three four five"
        );
        assert_eq!(sanitize_title("a    b\u{2028}c"), "a b c");
    }

    #[test]
    fn d_at_sign_is_spelled_out() {
        assert_eq!(sanitize_title("mail me@x.dk"), "mail me(at)x.dk");
    }

    #[test]
    fn e_colon_gets_a_space() {
        assert_eq!(sanitize_title("fix:smile: now: ok"), "fix: smile: now: ok");
        assert_eq!(sanitize_title("ends with:"), "ends with:");
    }

    #[test]
    fn f_word_initial_slash_is_replaced() {
        assert_eq!(
            sanitize_title("run /compact in src/main.rs"),
            "run \u{2215}compact in src/main.rs"
        );
    }

    #[test]
    fn g_ultrathink_is_defused_case_insensitively() {
        assert_eq!(
            sanitize_title("please ULTRATHINK and ultrathink"),
            "please ULTRA-THINK and ultra-think"
        );
    }

    #[test]
    fn h_truncates_by_chars_with_ellipsis() {
        let long = "æ".repeat(300);
        let s = sanitize_title(&long);
        assert_eq!(s.chars().count(), TICKET_LINE_TITLE_MAX_CHARS);
        assert!(s.ends_with('…'));
        let exact = "b".repeat(TICKET_LINE_TITLE_MAX_CHARS);
        assert_eq!(sanitize_title(&exact), exact);
    }

    #[test]
    fn i_empty_title_gets_a_placeholder() {
        assert_eq!(sanitize_title(""), EMPTY_TITLE);
        assert_eq!(sanitize_title(" \u{200B}\n\t "), EMPTY_TITLE);
        assert_eq!(sanitize_title("!?&-"), EMPTY_TITLE);
    }

    #[test]
    fn forbidden_first_chars_are_dropped() {
        for raw in [
            "/cmd x", "!ls", "@file", ":smile:", "?", "& bg", "-flag", "- - !x", "  ?? hi",
        ] {
            let s = sanitize_title(raw);
            let first = s.chars().next().unwrap();
            assert!(!FORBIDDEN_FIRST.contains(&first), "{raw:?} -> {s:?}");
        }
        assert_eq!(sanitize_title("-flag"), "flag");
        assert_eq!(sanitize_title("!ls -la"), "ls -la");
        assert_eq!(sanitize_title("? help"), "help");
    }

    #[test]
    fn combined_case() {
        assert_eq!(
            sanitize_title("\u{FEFF}-  @bob:\n/deploy\u{200B} ULTRAthink\r\n"),
            "(at)bob: \u{2215}deploy ULTRA-think"
        );
    }

    #[test]
    fn render_line_matches_the_contract() {
        assert_eq!(
            render_line("abcdef01", "Fix it"),
            "Ticket abcdef01: Fix it. Læs filen .mira-bots/tickets/abcdef01.md og udfør opgaven. Afslut dit svar når opgaven er færdig."
        );
        let mut t = ticket(ID, TicketState::Assigned);
        for raw in [
            "line\nbreak",
            "zero\u{200B}width",
            "a@b",
            "x:y",
            "/cmd",
            &"z".repeat(300),
        ] {
            t.title = raw.to_string();
            let line = line_for(&t, &TicketDelivery::work());
            assert_line_safe(&line);
            assert!(line.starts_with("Ticket abcdef01: "));
            assert!(!line.contains('\u{200B}') && !line.contains('@'));
        }
        t.title = "z".repeat(300);
        assert!(line_for(&t, &TicketDelivery::work()).chars().count() < 300);
    }

    #[test]
    fn request_submission_line_matches_the_contract() {
        let l = request_submission_line("3f2a9c1e");
        assert_eq!(
            l,
            "Du afsluttede uden at aflevere ticket 3f2a9c1e. Kald mira_submit_for_review med en kort opsummering når opgaven er færdig; ellers fortsæt arbejdet."
        );
        assert!(!l.starts_with("Ticket"));
        assert!(!l.contains(['\r', '\n', '@']));
        assert!(!FORBIDDEN_FIRST.contains(&l.chars().next().unwrap()));
    }

    #[test]
    fn render_file_has_header_body_and_rules() {
        let mut t = ticket(ID, TicketState::Assigned);
        t.title = "Ret\nlogin".into();
        t.body = "Linje 1\r\n\r\n- punkt @x /y\n".into();
        let f = render_file(&t, 1_700_000_000_000, &TicketDelivery::work());
        assert!(f.starts_with("# Ticket abcdef01: Ret login\n\n"), "{f}");
        assert!(f.contains(&format!("- id: {ID}\n")));
        assert!(f.contains("- kort-id: abcdef01\n"));
        assert!(f.contains("- oprettet: 1970-01-01T00:00:01Z\n"));
        assert!(f.contains("- opdateret: 2023-11-14T22:13:20Z\n"));
        assert!(f.contains("- status: assigned\n"));
        assert!(f.contains("- review: ja\n- projekt: ingen\n\n## Opgave"));
        assert!(f.contains("## Opgave\n\nLinje 1\n\n- punkt @x /y\n\n## Regler\n"));
        assert!(!f.contains('\r'));
        assert!(!f.contains("## Afvist:"));
        assert!(f.ends_with(
            "## Regler\n\
             - Opgaven er en ticket fra mira-bots. Når den er løst, kald værktøjet mira_submit_for_review med en kort opsummering, og afslut så dit svar.\n\
             - Opret opfølgende opgaver med mira_create_ticket. Opret eller redigér ikke selv filer i .mira-bots/.\n\
             - Læg en rapport på ticketen med mira_add_report (eller `report` i mira_submit_for_review) når du har lavet noget brugeren skal kunne læse om.\n"
        ));
        assert!(f.contains("mira_submit_for_review"));

        t.skip_review = true;
        t.body = String::new();
        t.rejection_note = Some("Mangler\ntest".into());
        let f = render_file(&t, 0, &TicketDelivery::default());
        assert!(f.contains("- review: springes over\n"));
        assert!(f.contains("## Opgave\n\n(ingen beskrivelse)\n\n"));
        assert!(f.contains(
            "## Afvist: Mangler test\nRet det ovenstående og afslut dit svar igen, så ticketen kommer til review på ny.\n\n## Regler\n"
        ));
    }

    #[test]
    fn delivery_for_agent_by_seat_and_roles() {
        let d = |seat, roles: &[Role]| TicketDelivery::for_agent(seat, roles).coordination;
        // Review 5c W1: a work seat gives the plain delivery only with a work role.
        for roles in [
            &[Role::Coder][..],
            &[Role::Researcher],
            &[Role::Debugger],
            &[Role::Reviewer, Role::Coder],
            &Role::ALL,
        ] {
            assert_eq!(d(SeatKind::Work, roles), None, "{roles:?}");
        }
        // A work seat without a work role (it may not edit files): a coordination task.
        assert_eq!(d(SeatKind::Work, &[]), Some(CoordinationKind::Plan));
        assert_eq!(
            d(SeatKind::Work, &[Role::Reviewer]),
            Some(CoordinationKind::Plan)
        );
        assert_eq!(
            d(SeatKind::Work, &[Role::Planner]),
            Some(CoordinationKind::Plan)
        );
        assert_eq!(
            d(SeatKind::Work, &[Role::Coordinator]),
            Some(CoordinationKind::Distribute)
        );
        // A staff seat: always a coordination task, also with a work role.
        assert_eq!(
            d(SeatKind::Staff, &[Role::Reviewer, Role::Coder]),
            Some(CoordinationKind::Plan)
        );
        assert_eq!(
            d(SeatKind::Staff, &[Role::Coordinator]),
            Some(CoordinationKind::Distribute)
        );
        assert_eq!(
            d(SeatKind::Staff, &Role::ALL),
            Some(CoordinationKind::Distribute)
        );
        assert_eq!(
            d(SeatKind::Staff, &[Role::Reviewer]),
            Some(CoordinationKind::Plan)
        );
        assert_eq!(
            d(SeatKind::Staff, &[Role::Planner, Role::Reviewer]),
            Some(CoordinationKind::Plan)
        );
        assert_eq!(TicketDelivery::default(), TicketDelivery::work());
    }

    #[test]
    fn coordination_line_and_file() {
        let mut t = ticket(ID, TicketState::Assigned);
        t.title = "Lav @en side".into();
        t.rejection_note = Some("Prøv igen".into());
        let work = render_file(&t, 0, &TicketDelivery::work());
        let distribute = TicketDelivery {
            coordination: Some(CoordinationKind::Distribute),
            ..TicketDelivery::default()
        };
        let plan = TicketDelivery {
            coordination: Some(CoordinationKind::Plan),
            ..TicketDelivery::default()
        };
        // The work delivery is unchanged: no coordination section, the plain line.
        assert!(!work.contains("Koordineringsopgave"));
        assert!(line_for(&t, &TicketDelivery::work()).starts_with("Ticket abcdef01: "));
        for (d, text, other) in [
            (
                distribute,
                COORDINATION_DISTRIBUTE_TEXT,
                COORDINATION_PLAN_TEXT,
            ),
            (plan, COORDINATION_PLAN_TEXT, COORDINATION_DISTRIBUTE_TEXT),
        ] {
            let line = line_for(&t, &d);
            assert_eq!(
                line,
                "Koordiner ticket abcdef01: Lav (at)en side. Læs filen .mira-bots/tickets/abcdef01.md og fordel opgaven; udfør den ikke selv."
            );
            assert!(!line.starts_with("Du") && !line.starts_with("Ticket"));
            assert!(!line.chars().any(|c| c.is_control()));
            // Review 5c N1: the prefix is ASCII.
            assert!(
                line.starts_with(COORDINATION_LINE_PREFIX) && COORDINATION_LINE_PREFIX.is_ascii()
            );
            assert!(is_coordination_line_for(&line, "abcdef01"));
            let f = render_file(&t, 0, &d);
            let section = format!("## Koordineringsopgave\n{text}\n\n## Regler\n");
            assert!(f.contains(&section), "{f}");
            assert!(!f.contains(other));
            // After the rejection note, before the rules; the rest is the work file.
            let afvist = f.find("## Afvist:").unwrap();
            assert!(afvist < f.find("## Koordineringsopgave").unwrap());
            assert_eq!(
                f.replace(&format!("## Koordineringsopgave\n{text}\n\n"), ""),
                work
            );
        }
        assert!(COORDINATION_DISTRIBUTE_TEXT.contains("mira_assign_ticket"));
        // Step 5c: both tools work on the coordinator's own ticket in progress.
        assert!(COORDINATION_DISTRIBUTE_TEXT.contains("den flytter fra dig"));
        assert!(COORDINATION_DISTRIBUTE_TEXT.contains("mira_unassign_ticket"));
        assert!(COORDINATION_DISTRIBUTE_TEXT.contains("mira_list_agents"));
        assert!(COORDINATION_PLAN_TEXT.contains("mira_create_ticket"));
        assert!(!COORDINATION_PLAN_TEXT.contains("mira_assign_ticket"));
        // Review 5c W2: without the coordinator role, hand it on with the common tools.
        assert!(COORDINATION_PLAN_TEXT.contains("mira_list_agents"));
        assert!(COORDINATION_PLAN_TEXT.contains("mira_handoff_ticket(ticketId, agentId)"));
        assert!(COORDINATION_PLAN_TEXT.contains("mira_handoff_ticket uden agentId"));
        assert!(COORDINATION_PLAN_TEXT.contains("mira_submit_for_review"));
        // Review 5c W3: Bash is not denied, so both texts ask for it.
        for text in [COORDINATION_DISTRIBUTE_TEXT, COORDINATION_PLAN_TEXT] {
            assert!(text.contains(
                "brug heller ikke Bash til at skrive eller ændre filer (ingen `>`/heredoc/sed -i)"
            ));
        }
    }

    // Review 5c N1: the confirmation tolerates the first word's spelling, not another ticket.
    #[test]
    fn coordination_line_match_is_tolerant() {
        for p in [
            "Koordiner ticket abcdef01: x",
            "Koordinér ticket abcdef01: x",
            "Koordine\u{301}r ticket abcdef01: x",
            "  Koordinr ticket abcdef01",
        ] {
            assert!(is_coordination_line_for(p, "abcdef01"), "{p}");
        }
        for p in [
            "Koordiner ticket 11111111: x",
            "Ticket abcdef01: x",
            "Koordiner abcdef01",
            "Du skal stoppe arbejdet på ticket abcdef01",
            "koordiner ticket abcdef01",
        ] {
            assert!(!is_coordination_line_for(p, "abcdef01"), "{p}");
        }
    }

    // Review 5c W4.
    #[test]
    fn handed_over_line_and_detail() {
        assert_eq!(
            handed_over_line("abcdef01", Some("Koder 2")),
            "Du skal stoppe arbejdet på ticket abcdef01: den er givet videre til Koder 2. Afslut dit svar."
        );
        assert_eq!(
            handed_over_line("abcdef01", None),
            "Du skal stoppe arbejdet på ticket abcdef01: den er lagt tilbage i backlog. Afslut dit svar."
        );
        let l = handed_over_line("abcdef01", Some("@bob\n/x"));
        assert!(
            l.starts_with("Du ") && !l.contains(['\r', '\n', '@']),
            "{l}"
        );
        assert_eq!(
            handed_over_detail("abcdef01", true),
            "Ticket abcdef01 givet videre"
        );
        assert_eq!(
            handed_over_detail("abcdef01", false),
            "Ticket abcdef01 lagt tilbage"
        );
        assert!(is_handed_over_detail(&handed_over_detail("abcdef01", true)));
        assert!(is_handed_over_detail(&handed_over_detail(
            "abcdef01", false
        )));
        assert!(!is_handed_over_detail("Kører tests"));
    }

    #[test]
    fn clean_body_and_one_line_strip_controls() {
        assert_eq!(
            clean_body("a\u{0}b\r\nc\td\re\u{7}\u{9b}f\n"),
            "ab\nc\td\ref\n"
        );
        assert_eq!(clean_body("æøå **md**"), "æøå **md**");
        assert_eq!(one_line("  Ret\r\nlogin\u{0}\tnu  "), "Ret  login nu");
    }

    #[test]
    fn write_ticket_file_creates_dir_file_and_gitignore() {
        let cwd = std::env::temp_dir().join(format!("mira-prompt-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&cwd).unwrap();
        let t = ticket(ID, TicketState::Assigned);
        let path = write_ticket_file(&cwd, &t, 0, &TicketDelivery::work()).unwrap();
        assert_eq!(
            path,
            cwd.join(".mira-bots").join("tickets").join("abcdef01.md")
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            render_file(&t, 0, &TicketDelivery::work())
        );
        let gi = cwd.join(".mira-bots").join(".gitignore");
        assert_eq!(fs::read_to_string(&gi).unwrap(), "*\n");

        // Existing .gitignore is kept; the ticket file is overwritten.
        fs::write(&gi, "custom\n").unwrap();
        let mut t2 = t.clone();
        t2.title = "Ny titel".into();
        write_ticket_file(&cwd, &t2, 0, &TicketDelivery::work()).unwrap();
        assert_eq!(fs::read_to_string(&gi).unwrap(), "custom\n");
        assert!(fs::read_to_string(&path).unwrap().contains("Ny titel"));
        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn iso_utc_known_values() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_700_000_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(iso_utc(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(iso_utc(1_791_158_399_999), "2026-10-04T23:59:59Z");
    }

    fn review_ticket() -> Ticket {
        let mut t = ticket(ID, TicketState::Review);
        t.title = "Ret @login /nu".into();
        t.summary = Some("Rettet og testet".into());
        t.review_round = 1;
        t.history.push(crate::tickets::model::TicketHistoryEntry {
            at: 1_700_000_000_000,
            from: Some(TicketState::InProgress),
            to: TicketState::Review,
            by: crate::tickets::model::TicketActor::Agent,
            note: None,
        });
        t
    }

    #[test]
    fn review_line_text_matches_contract() {
        let t = review_ticket();
        assert_eq!(
            review_line_for(&t, Some("C:/Users/x/mira-bots/agents/coder-01")),
            "Review af ticket abcdef01: Ret (at)login \u{2215}nu. Læs .mira-bots/reviews/abcdef01.md i din mappe; afsenderens arbejde ligger i C:/Users/x/mira-bots/agents/coder-01, og opsummering fås med mira_get_ticket abcdef01. Kald mira_approve_ticket eller mira_reject_ticket med en note."
        );
        assert_eq!(
            render_review_line("abcdef01", "T", None, 2),
            "Review af ticket abcdef01: T. Læs .mira-bots/reviews/abcdef01.md i din mappe; afsenderens mappe kendes ikke længere, og opsummering og 2 rapport(er) fås med mira_get_ticket abcdef01 og mira_get_report. Kald mira_approve_ticket eller mira_reject_ticket med en note."
        );
        let line = render_review_line("abcdef01", "T", Some("/w/a@b\n c"), 0);
        assert!(line.contains("ligger i /w/a(at)b c,"), "{line}");
        assert!(!line.contains('\n') && !line.contains('\r'));
        assert!(line.starts_with("Review af ticket "));
    }

    #[test]
    fn review_file_follows_the_contract() {
        let mut t = review_ticket();
        t.reports.push(crate::tickets::model::TicketReport {
            id: "01".into(),
            title: "Ændringer".into(),
            author: crate::tickets::model::ReportAuthor::agent("a1"),
            created_at: 1_000,
            path: "reports/01-aendringer.md".into(),
            size: 4,
        });
        let sender = ReviewSender {
            name: "coder-01".into(),
            cwd: "/w/coder-01".into(),
        };
        let f = render_review_file(
            &t,
            Some(&sender),
            &|a| a.agent_id.clone().unwrap_or_else(|| "dig".into()),
            &[],
        );
        assert!(f.starts_with("# Review af ticket abcdef01: Ret @login /nu\n"));
        assert!(f.contains(
            "Afsender: coder-01 (/w/coder-01)   Runde: 2 af 3   Afleveret: 2023-11-14T22:13:20Z\n"
        ));
        assert!(f.contains("## Opsummering fra afsenderen\nRettet og testet\n## Rapporter\n"));
        assert!(f.contains(
            "- 01 Ændringer (a1, 1970-01-01T00:00:01Z) → mira_get_report abcdef01 01\n## Opgaven\n"
        ));
        assert!(f.contains("- Læs ændringerne med git -C \"/w/coder-01\" diff/log/status/show;"));
        assert!(f.ends_with("- Læg gerne en review-rapport med mira_add_report før du afgør.\n"));
        let f = render_review_file(
            &ticket(ID, TicketState::Review),
            None,
            &|_| String::new(),
            &[],
        );
        assert!(f.contains("Afsender: afsenderens mappe kendes ikke længere   Runde: 1 af 3"));
        assert!(f.contains("## Opsummering fra afsenderen\n(ingen)\n## Rapporter\n(ingen)\n"));
        // An escalated ticket with a hand-picked reviewer: capped at the last round.
        let mut t = ticket(ID, TicketState::Review);
        t.review_round = MAX_REVIEW_ROUNDS;
        let f = render_review_file(&t, None, &|_| String::new(), &[]);
        assert!(f.contains("Runde: 3 af 3   "), "{f}");
    }

    #[test]
    fn review_file_written_in_reviewer_cwd() {
        let dir = std::env::temp_dir().join(format!("mira-review-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = review_ticket();
        let p = write_review_file(&dir, &t, None, &|_| String::new(), &[]).unwrap();
        assert_eq!(
            p,
            dir.join(".mira-bots").join("reviews").join("abcdef01.md")
        );
        assert!(p.is_file());
        assert_eq!(
            std::fs::read_to_string(dir.join(".mira-bots").join(".gitignore")).unwrap(),
            "*\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ---- step 4b ----

    #[test]
    fn render_file_shows_the_project() {
        use crate::projects::ProjectRef;
        let mut t = ticket(ID, TicketState::Assigned);
        t.project = Some(ProjectRef::Existing("mira".into()));
        let f = render_file(&t, 0, &TicketDelivery::work());
        assert!(f.contains("- review: ja\n- projekt: mira\n\n"), "{f}");
        t.project = Some(ProjectRef::New { new: "ny".into() });
        let f = render_file(&t, 0, &TicketDelivery::work());
        assert!(
            f.contains("- projekt: ny (oprettes ved tildeling)\n"),
            "{f}"
        );
    }

    #[test]
    fn shared_project_section_lists_the_others() {
        let t = ticket(ID, TicketState::Assigned);
        let d = TicketDelivery::work().with_shared("p", vec!["coder-01".into(), "coder-02".into()]);
        let f = render_file(&t, 0, &d);
        let section = f.find("## Delt projekt\n").expect("section");
        assert!(section < f.find("## Regler").unwrap());
        assert!(f.contains("(projekt «p»): coder-01, coder-02."), "{f}");
        assert!(f.contains("aldrig `git add -A` eller `git add .`"));
        assert!(f.contains("`git stash`"));
        // No others, or a coordination task: no section.
        let none = TicketDelivery::work().with_shared("p", Vec::new());
        assert!(!render_file(&t, 0, &none).contains("Delt projekt"));
        let coord = TicketDelivery::for_agent(SeatKind::Work, &[Role::Reviewer])
            .with_shared("p", vec!["coder-01".into()]);
        assert_eq!(coord.shared, None);
        assert!(!render_file(&t, 0, &coord).contains("Delt projekt"));
        // The line is the plain work line.
        assert!(line_for(&t, &d).starts_with("Ticket abcdef01: "));
    }

    #[test]
    fn coordination_file_lists_projects() {
        let t = ticket(ID, TicketState::Assigned);
        let d = TicketDelivery::for_agent(SeatKind::Staff, &[Role::Coordinator])
            .with_projects(vec!["a".into(), "b".into()], false);
        let f = render_file(&t, 0, &d);
        let line = "Projekter lige nu: a, b. Angiv `project` på hver ticket du opretter; nye projekter skal brugeren oprette (agentsMayCreateProjects er slået fra). Mangler ticketen et projekt, angiv `project` når du giver den videre (mira_assign_ticket/mira_handoff_ticket).\n\n## Regler";
        assert!(
            f.contains(&format!("{COORDINATION_DISTRIBUTE_TEXT}\n{line}")),
            "{f}"
        );
        let d = TicketDelivery::for_agent(SeatKind::Staff, &[Role::Planner])
            .with_projects(Vec::new(), true);
        let f = render_file(&t, 0, &d);
        assert!(f.contains(
            "Projekter lige nu: ingen. Angiv `project` på hver ticket du opretter; nye projekter kan oprettes med {\"new\": …}. Mangler ticketen et projekt, angiv `project` når du giver den videre"
        ));
        // A work delivery ignores the list.
        let w = TicketDelivery::work().with_projects(vec!["a".into()], true);
        assert_eq!(w.projects, None);
    }

    // ---- step 6a ----

    fn wake(parent: &str, done: &[(&str, &str)], open_left: usize) -> String {
        wake_line(&WakeInfo {
            parent_short: parent.into(),
            newly_done: done
                .iter()
                .map(|(s, t)| (s.to_string(), t.to_string()))
                .collect(),
            open_left,
        })
    }

    #[test]
    fn wake_line_starts_with_du_and_never_ticket() {
        let one = wake("pppppppp", &[("cccccccc", "Plan")], 1);
        assert_eq!(
            one,
            "Du har fået besked: del-ticket cccccccc (Plan) til ticket pppppppp er godkendt; 1 del-ticket(s) mangler stadig. Læs opsummeringen med mira_get_ticket cccccccc. Tildel næste del-ticket hvis der mangler én, og aflever så ticket pppppppp igen med mira_submit_for_review (ticketId pppppppp)."
        );
        let last = wake("pppppppp", &[("cccccccc", "Byg")], 0);
        assert_eq!(
            last,
            "Du har fået besked: alle del-tickets til ticket pppppppp er afsluttet (sidst cccccccc: Byg). Læs opsummeringerne med mira_get_ticket, og aflever ticket pppppppp med mira_submit_for_review (ticketId pppppppp) med en samlet opsummering."
        );
        let gone = wake("pppppppp", &[], 0);
        assert_eq!(
            gone,
            "Du har fået besked: alle del-tickets til ticket pppppppp er afsluttet. Aflever ticket pppppppp med mira_submit_for_review (ticketId pppppppp)."
        );
        let two = wake("pppppppp", &[("aaaaaaaa", "A"), ("bbbbbbbb", "B")], 2);
        assert!(
            two.starts_with(
                "Du har fået besked: del-tickets aaaaaaaa (A) og bbbbbbbb (B) til ticket pppppppp er godkendt; 2 del-ticket(s) mangler stadig. Læs opsummeringerne med mira_get_ticket. "
            ),
            "{two}"
        );
        for l in [&one, &last, &gone, &two] {
            assert!(l.starts_with("Du har fået besked"), "{l}");
            assert!(!l.contains(['\r', '\n', '@']), "{l:?}");
            assert!(!l.starts_with("Ticket ") && !l.starts_with("Koordin"));
            assert!(!l.starts_with("Review af ticket"));
            assert!(!is_coordination_line_for(l, "pppppppp"));
            assert!(!l.chars().any(|c| c.is_control()));
        }
    }

    #[test]
    fn wake_line_sanitises_titles_and_caps_children() {
        let l = wake(
            "pppppppp",
            &[
                ("aaaaaaaa", "@evil\n/compact ultrathink"),
                ("bbbbbbbb", "B"),
                ("cccccccc", "C"),
                ("dddddddd", "D"),
            ],
            1,
        );
        assert!(
            l.contains(
                "del-tickets aaaaaaaa ((at)evil \u{2215}compact ultra-think), bbbbbbbb (B), cccccccc (C) og 1 mere til ticket pppppppp"
            ),
            "{l}"
        );
        assert!(!l.contains("dddddddd") && !l.contains('@') && !l.contains('\n'));
        // Exactly three named.
        let l = wake(
            "pppppppp",
            &[("aaaaaaaa", "A"), ("bbbbbbbb", "B"), ("cccccccc", "C")],
            1,
        );
        assert!(
            l.contains("del-tickets aaaaaaaa (A), bbbbbbbb (B) og cccccccc (C) til ticket"),
            "{l}"
        );
        // The last-child title is sanitised too.
        let l = wake("pppppppp", &[("aaaaaaaa", "x\r\ny:z")], 0);
        assert!(l.contains("(sidst aaaaaaaa: x y: z)"), "{l}");
    }

    #[test]
    fn step6a_texts_match_c6_3() {
        assert_eq!(
            COORDINATION_DISTRIBUTE_TEXT,
            "Du sidder på en stabsplads eller har ingen arbejdsrolle: udfør IKKE opgaven selv (skriv ingen kode og ingen filer; brug heller ikke Bash til at skrive eller ændre filer (ingen `>`/heredoc/sed -i)). Tjek først `mira_list_tickets all` for del-tickets og dubletter, der allerede findes. Kan én ledig arbejdsagent (mira_list_agents) tage hele opgaven, så giv den ticketen med mira_assign_ticket (den flytter fra dig; arbejd så ikke videre på den). Ellers opret del-tickets med mira_create_ticket: `parentId` er som standard denne ticket, `assignTo` giver dem direkte til en agent, og `blockedBy` lader en del-ticket vente på en anden — fx byg-ticketen til koderen med det samme med `blockedBy: [plan-ticketens id]`. Få, større del-tickets er bedre end mange små (højst 20 oprettelser i timen). Aflever så denne ticket med mira_submit_for_review og en kort fordelingsplan: den venter automatisk, til del-ticketsene er godkendt, og du får besked efter hver — tildel næste del-ticket eller aflever igen. Er der ingen ledig arbejdsagent, start en fra en profil med mira_spawn_agent (mira_list_profiles) hvis der er en fri arbejdsplads; ellers skriv hvorfor med mira_update_status og læg ticketen tilbage i backlog med mira_unassign_ticket."
        );
        assert!(COORDINATION_PLAN_TEXT.ends_with(
            "Er der ingen ledig arbejdsagent, så del opgaven op i del-tickets med mira_create_ticket (`parentId` = denne ticket, så de hænger sammen; de lander i backlog), og aflever denne ticket med planen med mira_submit_for_review — eller læg den tilbage i backlog med mira_handoff_ticket uden agentId."
        ));
        assert!(COORDINATION_PLAN_TEXT.starts_with(
            "Du sidder på en stabsplads eller har ingen arbejdsrolle: udfør IKKE opgaven selv (skriv ingen kode og ingen filer; brug heller ikke Bash til at skrive eller ændre filer (ingen `>`/heredoc/sed -i)). Find en ledig arbejdsagent (arbejdsplads, rollen koder, researcher eller debugger, ingen ticket i gang) med mira_list_agents og giv ticketen videre med mira_handoff_ticket(ticketId, agentId); arbejd så ikke videre på den. Er der ingen"
        ));
        assert_eq!(
            COORDINATION_PLAN_WITH_COORDINATOR_TEXT,
            "Du sidder på en stabsplads eller har ingen arbejdsrolle: udfør IKKE opgaven selv (skriv ingen kode og ingen filer; brug heller ikke Bash til at skrive eller ændre filer (ingen `>`/heredoc/sed -i)). Der er en koordinator i staben: skriv planen som rapport (mira_add_report, eller `report` i mira_submit_for_review) med små, ordnede del-opgaver og klare acceptkriterier, og aflever ticketen. Opret IKKE selv del-tickets og tildel ingen; det gør koordinatoren ud fra din plan."
        );
        assert_eq!(
            PARENT_REVIEW_RULE,
            "- Del-ticketsene er allerede reviewet hver for sig; vurdér helheden: hænger delene sammen, og er ticketens mål nået? Åbne del-tickets taler imod godkendelse."
        );
        assert_eq!(
            CHILDREN_INTRO,
            "Denne ticket har del-tickets; tjek dem før du opretter nye:"
        );
    }

    fn child(short: &str, title: &str, state: TicketState, blockers: &[&str]) -> ChildLine {
        ChildLine {
            short: short.into(),
            title: title.into(),
            state,
            open_blockers: blockers.iter().map(|b| b.to_string()).collect(),
        }
    }

    #[test]
    fn render_file_lists_children_with_states_and_blockers() {
        let t = ticket(ID, TicketState::Assigned);
        let d = TicketDelivery::for_agent(SeatKind::Staff, &[Role::Coordinator])
            .with_projects(vec!["p".into()], false)
            .with_children(vec![
                child("aaaaaaaa", "Plan\nspil", TicketState::Done, &[]),
                child(
                    "bbbbbbbb",
                    "Byg",
                    TicketState::Assigned,
                    &["aaaaaaaa", "cccccccc"],
                ),
            ]);
        let f = render_file(&t, 0, &d);
        let section = "## Del-tickets\nDenne ticket har del-tickets; tjek dem før du opretter nye:\n- aaaaaaaa Plan spil — Done\n- bbbbbbbb Byg — I kø — venter på aaaaaaaa, cccccccc\n\n";
        assert!(f.contains(section), "{f}");
        let at = f.find("## Del-tickets").unwrap();
        assert!(f.find("## Koordineringsopgave").unwrap() < at);
        assert!(at < f.find("## Regler").unwrap());
        // A work delivery: after nothing, before `## Delt projekt`.
        let w = TicketDelivery::work()
            .with_shared("p", vec!["coder-02".into()])
            .with_children(vec![child("aaaaaaaa", "A", TicketState::Review, &[])]);
        let f = render_file(&t, 0, &w);
        let at = f.find("## Del-tickets").unwrap();
        assert!(at < f.find("## Delt projekt").unwrap());
        assert!(f.contains("- aaaaaaaa A — Review\n"));
    }

    #[test]
    fn render_file_without_children_has_no_section() {
        let t = ticket(ID, TicketState::Assigned);
        for d in [
            TicketDelivery::work(),
            TicketDelivery::for_agent(SeatKind::Staff, &[Role::Coordinator]),
            TicketDelivery::for_agent(SeatKind::Staff, &[Role::Planner]).with_children(Vec::new()),
        ] {
            assert!(!render_file(&t, 0, &d).contains("Del-tickets"));
        }
    }

    #[test]
    fn plan_text_depends_on_coordinator() {
        let t = ticket(ID, TicketState::Assigned);
        let plan = TicketDelivery::for_agent(SeatKind::Staff, &[Role::Planner]);
        let f = render_file(&t, 0, &plan);
        assert!(f.contains(&format!(
            "## Koordineringsopgave\n{COORDINATION_PLAN_TEXT}\n"
        )));
        assert!(!f.contains("Der er en koordinator i staben"));
        let f = render_file(&t, 0, &plan.clone().with_coordinator(true));
        assert!(f.contains(&format!(
            "## Koordineringsopgave\n{COORDINATION_PLAN_WITH_COORDINATOR_TEXT}\n"
        )));
        assert!(!f.contains(COORDINATION_PLAN_TEXT));
        // The coordinator's own text and a work delivery ignore the flag.
        let dist =
            TicketDelivery::for_agent(SeatKind::Staff, &[Role::Coordinator]).with_coordinator(true);
        assert!(render_file(&t, 0, &dist).contains(COORDINATION_DISTRIBUTE_TEXT));
        let w = TicketDelivery::work().with_coordinator(true);
        assert!(!render_file(&t, 0, &w).contains("Koordineringsopgave"));
        // The line is the same coordination line either way.
        assert_eq!(
            line_for(&t, &plan),
            line_for(&t, &plan.clone().with_coordinator(true))
        );
    }

    #[test]
    fn review_file_for_parent_lists_children_and_rule() {
        let t = review_ticket();
        let long = "æ".repeat(400);
        let children = vec![
            ChildReview {
                short: "aaaaaaaa".into(),
                title: "Plan\nspil".into(),
                state: TicketState::Done,
                summary: Some("Planen\r\ner skrevet".into()),
            },
            ChildReview {
                short: "bbbbbbbb".into(),
                title: "Byg".into(),
                state: TicketState::Done,
                summary: Some(long),
            },
            ChildReview {
                short: "cccccccc".into(),
                title: "Test".into(),
                state: TicketState::Assigned,
                summary: None,
            },
        ];
        let f = render_review_file(&t, None, &|_| String::new(), &children);
        let cut = format!("{}…", "æ".repeat(CHILD_SUMMARY_MAX_CHARS));
        let section = format!(
            "## Del-tickets (allerede reviewet)\n- aaaaaaaa Plan spil — Done: Planen  er skrevet\n- bbbbbbbb Byg — Done: {cut}\n- cccccccc Test — I kø: (ingen opsummering)\n## Opgaven\n"
        );
        assert!(f.contains(&section), "{f}");
        assert!(f.ends_with(&format!("{PARENT_REVIEW_RULE}\n")), "{f}");
        let rules = f.find("## Regler").unwrap();
        assert!(f.find(PARENT_REVIEW_RULE).unwrap() > rules);
        // Without children: unchanged.
        let f = render_review_file(&t, None, &|_| String::new(), &[]);
        assert!(!f.contains("Del-tickets") && !f.contains(PARENT_REVIEW_RULE));
    }
}
