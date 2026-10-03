//! The report back to the source when an external ticket is Done (step 6c, plan A.2/A.7,
//! punkt 9–10, C6c.4).
//!
//! The text ([`render_write_back`]) has no paths and no secrets: only the app's branch name and
//! the base, the scrubbed summary ([`scrub_summary`]), the checks state and report titles. The
//! folder source moves the file to `done/` and writes `<name>.result.md` with the same text
//! without the marker ([`folder_write_back`], on the `mira-writeback` thread). GitHub (comment,
//! close) comes in B3.
//!
//! Locks: the write back is claimed under `inbox_lock` (state `none` → `inflight`, saved), the
//! file work runs without any lock, the result is saved afterwards. Done stays Done whatever
//! happens here.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::external::{is_hidden_char, strip_html_comments};
use super::folder::{folder_dir_for, move_with_retry, source_key_of, stem_of, write_result};
use crate::agent::now_ms;
use crate::config::{
    result_written_note, write_back_failed_note, INBOX_DONE_DIR, INBOX_MOVE_FAILED_NOTE,
    INBOX_STARTED_DIR, WRITE_BACK_MAX_CHARS, WRITE_BACK_SUMMARY_MAX_CHARS,
};
use crate::tickets::model::{
    ChecksState, ExternalKind, ExternalRef, GitMode, Ticket, TicketState, WriteBack, WriteBackState,
};
use crate::tickets::prompt::one_line;
use crate::tickets::TicketsCtx;

/// The first line of every write back.
pub const WRITE_BACK_HEADING: &str = "**mira-bots: opgaven er løst og gennemgået**";
/// The summary when the ticket has none.
pub const NO_SUMMARY: &str = "(ingen opsummering)";
/// Replaces a token-like string in the summary.
pub const SCRUBBED: &str = "[fjernet]";
/// Replaces a known folder (projects root, project, worktree) in the summary.
pub const PROJECT_PLACEHOLDER: &str = "<projekt>";
/// Most report titles listed.
pub const REPORT_TITLES_MAX: usize = 10;

/// Prefixes of secrets removed from the summary up to the next whitespace (research §5.2).
const TOKEN_PREFIXES: [&str; 8] = [
    "ghp_",
    "github_pat_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "sk-",
    "AKIA",
];
const PEM_BEGIN: &str = "-----BEGIN";
const PEM_END: &str = "-----END";

/// The hidden marker of the GitHub comment (found again before a retry posts).
pub fn marker(ticket_id: &str) -> String {
    format!("<!-- mira-bots:ticket={ticket_id} -->")
}

/// `s` in a code span (backticks removed, one line).
fn code(s: &str) -> String {
    format!("`{}`", one_line(s).replace('`', ""))
}

/// `{tjek}` of C6c.4: `bestået ({navne})` | `fejlede: {navn}` | `sprunget over` | `ikke kørt`.
/// `names`: the project's check names (none known: just "bestået").
pub fn check_line(t: &Ticket, names: &[String]) -> String {
    match t.checks.as_ref().map(|c| (c.state, c.failed.as_deref())) {
        Some((ChecksState::Passed, _)) if names.is_empty() => "bestået".into(),
        Some((ChecksState::Passed, _)) => {
            let names: Vec<String> = names.iter().map(|n| one_line(n)).collect();
            format!("bestået ({})", names.join(", "))
        }
        Some((ChecksState::Failed, Some(n))) => format!("fejlede: {}", one_line(n)),
        Some((ChecksState::Failed, None)) => "fejlede".into(),
        Some((ChecksState::Skipped, _)) => "sprunget over".into(),
        Some((ChecksState::Pending, _)) | None => "ikke kørt".into(),
    }
}

/// The titles of the ticket's reports, one line each, at most `max`.
pub fn report_titles(t: &Ticket, max: usize) -> Vec<String> {
    t.reports
        .iter()
        .take(max)
        .map(|r| one_line(&r.title))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Removes token-like strings: the [`TOKEN_PREFIXES`] at a word start up to the next
/// whitespace, a PEM block from `-----BEGIN` to the end of its `-----END …-----` line (or the end
/// of the text). Hand-written, no regex.
fn scrub_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut prev: Option<char> = None;
    while i < s.len() {
        let rest = &s[i..];
        let at_word_start = !prev.is_some_and(|c| c.is_alphanumeric() || c == '_');
        if at_word_start && rest.starts_with(PEM_BEGIN) {
            let end = rest[PEM_BEGIN.len()..]
                .find(PEM_END)
                .map(|j| PEM_BEGIN.len() + j + PEM_END.len())
                .and_then(|k| rest[k..].find("-----").map(|m| k + m + 5))
                .unwrap_or(rest.len());
            out.push_str(SCRUBBED);
            i += end;
            prev = Some('-');
            continue;
        }
        if at_word_start && TOKEN_PREFIXES.iter().any(|p| rest.starts_with(p)) {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            out.push_str(SCRUBBED);
            i += end;
            prev = Some('x');
            continue;
        }
        let c = rest.chars().next().expect("not empty");
        out.push(c);
        prev = Some(c);
        i += c.len_utf8();
    }
    out
}

/// The spellings of `p` that may appear in text (as is, with `/` and with `\`); too short
/// ones (a drive or `/`) are left out.
fn spellings(p: &Path) -> Vec<String> {
    let s = p
        .to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .to_string();
    if s.chars().count() < 4 {
        return Vec::new();
    }
    let mut v = vec![s.clone(), s.replace('\\', "/"), s.replace('/', "\\")];
    v.dedup();
    v
}

/// [`scrub_summary`] with an explicit home folder (tests).
pub fn scrub_with(s: &str, roots: &[PathBuf], home: Option<&Path>) -> String {
    let visible: String = s.chars().filter(|c| !is_hidden_char(*c)).collect();
    // No HTML comments: a forged `<!-- mira-bots:ticket=… -->` must not reach the source.
    let (mut text, _) = strip_html_comments(&visible);
    let mut known: Vec<(String, &str)> = roots
        .iter()
        .flat_map(|r| spellings(r))
        .map(|s| (s, PROJECT_PLACEHOLDER))
        .collect();
    known.extend(home.into_iter().flat_map(spellings).map(|s| (s, "~")));
    // Longest first: a project inside the home folder becomes <projekt>, not ~/….
    known.sort_by_key(|(s, _)| std::cmp::Reverse(s.len()));
    for (path, with) in &known {
        text = text.replace(path.as_str(), with);
    }
    scrub_tokens(&text)
}

/// The home folder (`HOME`, else `USERPROFILE`).
fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .find(|p| !p.as_os_str().is_empty())
}

/// Cleans the ticket summary for the source (C6c.4): hidden chars and HTML comments out, the
/// known folders `roots` → `<projekt>`, the home folder → `~`, token-like strings →
/// `[fjernet]`. The summary is agent text the user approved, not read word by word; this is a
/// best effort, which is why GitHub write back is off by default.
pub fn scrub_summary(s: &str, roots: &[PathBuf]) -> String {
    scrub_with(s, roots, home_dir().as_deref())
}

/// [`render_write_back`] with the project's check names and with or without the marker (the
/// folder's `.result.md` has none).
pub fn render_write_back_with(
    t: &Ticket,
    roots: &[PathBuf],
    check_names: &[String],
    with_marker: bool,
) -> String {
    let git = t.git.as_ref().filter(|g| g.mode != GitMode::Off);
    let mut lines = vec![match git {
        Some(g) => format!(
            "- **Branch:** {} (lokal, ikke pushet endnu)",
            code(&g.branch)
        ),
        None => "- **Branch:** ingen (git er slået fra for projektet)".to_string(),
    }];
    if let Some(g) = git {
        lines.push(format!("- **Basis:** {}", code(&g.base)));
    }
    lines.push(format!("- **Tjek:** {}", check_line(t, check_names)));
    let titles = report_titles(t, REPORT_TITLES_MAX);
    if !titles.is_empty() {
        lines.push(format!("- **Rapporter:** {}", titles.join(", ")));
    }
    let mut tail = lines.join("\n");
    if with_marker {
        tail.push_str("\n\n");
        tail.push_str(&marker(&t.id));
    }
    let summary = t
        .summary
        .as_deref()
        .map(|s| scrub_summary(s, roots).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| NO_SUMMARY.to_string());
    // Heading + 2 blank lines + tail + final newline around the summary.
    let fixed = WRITE_BACK_HEADING.chars().count() + tail.chars().count() + 5;
    let budget = WRITE_BACK_SUMMARY_MAX_CHARS.min(WRITE_BACK_MAX_CHARS.saturating_sub(fixed));
    let summary = if summary.chars().count() > budget {
        let cut: String = summary.chars().take(budget.saturating_sub(1)).collect();
        format!("{}…", cut.trim_end())
    } else {
        summary
    };
    let text = format!("{WRITE_BACK_HEADING}\n\n{summary}\n\n{tail}\n");
    text.chars().take(WRITE_BACK_MAX_CHARS).collect()
}

/// The write-back text of C6c.4 with the marker (the GitHub comment), without check names.
pub fn render_write_back(t: &Ticket, roots: &[PathBuf]) -> String {
    render_write_back_with(t, roots, &[], true)
}

/// Whether the Done hook writes back for this external ticket: never tried (`none`), or failed
/// without an attempt. Anything else needs "Prøv igen" (B3).
pub fn wants_write_back(e: &ExternalRef) -> bool {
    match e.write_back.comment {
        WriteBackState::None => true,
        WriteBackState::Failed => e.write_back.attempts == 0,
        WriteBackState::Inflight | WriteBackState::Done => false,
    }
}

/// The folders whose text never reaches a source: the projects root, the ticket's project and
/// its git folders.
fn known_roots(ctx: &TicketsCtx, t: &Ticket) -> Vec<PathBuf> {
    let root = ctx.workspace.root().to_path_buf();
    let mut v = vec![root.clone()];
    if let Some(p) = t
        .project
        .as_ref()
        .and_then(|p| p.id())
        .and_then(|p| crate::projects::find_project(&root, p))
    {
        v.push(PathBuf::from(p.path));
    }
    if let Some(g) = &t.git {
        v.push(PathBuf::from(&g.repo));
        v.extend(g.worktree.as_ref().map(PathBuf::from));
    }
    v
}

/// The project's check names (for "bestået (…)").
fn check_names(ctx: &TicketsCtx, t: &Ticket) -> Vec<String> {
    let Some(dir) = t
        .project
        .as_ref()
        .and_then(|p| p.id())
        .and_then(|p| crate::projects::find_project(ctx.workspace.root(), p))
    else {
        return Vec::new();
    };
    match ctx.project_files.read(Path::new(&dir.path)) {
        Ok(Some(f)) => f.checks.into_iter().map(|c| c.name).collect(),
        _ => Vec::new(),
    }
}

/// Adds a history note by the app (logged when it fails).
fn note(ctx: &TicketsCtx, id: &str, text: &str) {
    let now = now_ms();
    if let Err(e) = ctx.mutate(|s| s.note_by_system(id, text, now)) {
        log::warn!("inbox: note on ticket {} failed: {e}", short(id));
    }
}

fn short(id: &str) -> String {
    crate::tickets::model::short_id(id)
}

/// Where a folder item's file is now: `started/<path>` (after Start), else `<path>` (Start could
/// not move it). `None`: neither exists (the user moved or deleted it).
pub fn locate_folder_file(dir: &Path, path: &str) -> Option<PathBuf> {
    [dir.join(INBOX_STARTED_DIR).join(path), dir.join(path)]
        .into_iter()
        .find(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_file()))
}

/// The folder write back of Done ticket `id` (plan punkt 9; runs on the `mira-writeback`
/// thread). Claimed under `inbox_lock` (`none` → `inflight`, attempts + 1, saved); then without
/// any lock: the file moves to `done/` and `done/<name>.result.md` gets the text without the
/// marker. A failed move is `failed` with the note "filen kunne ikke flyttes …", but the result
/// is written anyway (next to where the file would be). Notes: "resultat skrevet til …" or
/// "kunne ikke melde tilbage: …". `Ok` with the stored state; `Err` when there was nothing to do.
pub fn folder_write_back(ctx: &Arc<TicketsCtx>, id: &str) -> Result<WriteBack, String> {
    let (t, ext, wb) = {
        let _serial = ctx.lock_inbox_serial();
        let t = ctx
            .read(|s| s.get(id))
            .ok_or_else(|| format!("ticket {} findes ikke", short(id)))?;
        let ext = t
            .external
            .clone()
            .filter(|e| e.kind == ExternalKind::Folder)
            .ok_or_else(|| "ingen mappe-kilde".to_string())?;
        if t.state != TicketState::Done || !wants_write_back(&ext) {
            return Err("intet at melde tilbage".into());
        }
        let mut wb = ext.write_back.clone();
        wb.comment = WriteBackState::Inflight;
        wb.attempts += 1;
        let now = now_ms();
        ctx.mutate(|s| s.set_write_back(id, wb.clone(), now))?;
        (t, ext, wb)
    };
    let text = render_write_back_with(&t, &known_roots(ctx, &t), &check_names(ctx, &t), false);
    let path = ext.path.clone().unwrap_or_default();
    let dir =
        source_key_of(&ext.external_id).and_then(|k| folder_dir_for(ctx.workspace.root(), &k));
    let mut wb = wb;
    wb.last_body = Some(text.clone());
    let mut error: Option<String> = None;
    let mut moved: Option<bool> = None;
    let mut result_name = stem_of(&path).to_string();
    match &dir {
        None => error = Some("indbakke-mappen findes ikke længere".into()),
        Some(dir) => {
            let done = dir.join(INBOX_DONE_DIR);
            if let Some(file) = locate_folder_file(dir, &path) {
                match move_with_retry(&file, &done) {
                    Ok(to) => {
                        moved = Some(true);
                        if let Some(n) = to.file_name() {
                            result_name = stem_of(&n.to_string_lossy()).to_string();
                        }
                    }
                    Err(e) => {
                        log::warn!("inbox: moving the file of ticket {} failed: {e}", short(id));
                        moved = Some(false);
                        error = Some(INBOX_MOVE_FAILED_NOTE.to_string());
                    }
                }
            }
            if result_name.is_empty() {
                result_name = short(id);
            }
            match write_result(&done, &result_name, &text) {
                Ok(p) => {
                    let name = p
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let name = name.strip_suffix(".result.md").unwrap_or(&name).to_string();
                    note(ctx, id, &result_written_note(&name));
                }
                Err(e) => {
                    log::warn!("inbox: result of ticket {} not written: {e}", short(id));
                    error = Some(e);
                }
            }
        }
    }
    let now = now_ms();
    match &error {
        None => {
            wb.comment = WriteBackState::Done;
            wb.commented_at = Some(now);
            wb.last_error = None;
        }
        Some(e) => {
            wb.comment = WriteBackState::Failed;
            wb.last_error = Some(e.clone());
            note(
                ctx,
                id,
                &if e == INBOX_MOVE_FAILED_NOTE {
                    e.clone()
                } else {
                    write_back_failed_note(e)
                },
            );
        }
    }
    ctx.mutate(|s| s.set_write_back(id, wb.clone(), now))?;
    if let Some(m) = moved {
        if let Err(e) = ctx.inbox_mutate(|i| i.set_moved(&ext.inbox_item_id, m)) {
            log::debug!("inbox: item of ticket {} not updated: {e}", short(id));
        }
    }
    Ok(wb)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tickets::model::test_support::ticket;
    use crate::tickets::model::{ReportAuthor, TicketChecks, TicketGit, TicketReport};

    fn done_ticket() -> Ticket {
        let mut t = ticket("ab12cd34-0000-0000-0000-000000000000", TicketState::Done);
        t.summary = Some("Rettede login i /home/u/mira/web/src/a.rs".into());
        t
    }

    fn git() -> TicketGit {
        TicketGit {
            mode: GitMode::Worktree,
            branch: "ticket/ab12cd34".into(),
            base: "main".into(),
            repo: "/home/u/mira/web".into(),
            worktree: Some("/home/u/mira/web/.mira-bots/wt/ab12cd34".into()),
        }
    }

    fn report(title: &str) -> TicketReport {
        TicketReport {
            id: "01".into(),
            title: title.into(),
            author: ReportAuthor::user(),
            created_at: 1,
            path: "reports/01-x.md".into(),
            size: 1,
        }
    }

    #[test]
    fn render_matches_contract_with_and_without_git() {
        let mut t = done_ticket();
        t.git = Some(git());
        t.checks = Some(TicketChecks {
            state: ChecksState::Passed,
            failed: None,
            round: 1,
            started_at: 1,
        });
        t.reports = vec![report("Fejlsøgning"), report("Testresultater")];
        let roots = vec![PathBuf::from("/home/u/mira/web")];
        let names = vec!["build".to_string(), "test".to_string()];
        assert_eq!(
            render_write_back_with(&t, &roots, &names, true),
            "**mira-bots: opgaven er løst og gennemgået**\n\n\
             Rettede login i <projekt>/src/a.rs\n\n\
             - **Branch:** `ticket/ab12cd34` (lokal, ikke pushet endnu)\n\
             - **Basis:** `main`\n\
             - **Tjek:** bestået (build, test)\n\
             - **Rapporter:** Fejlsøgning, Testresultater\n\n\
             <!-- mira-bots:ticket=ab12cd34-0000-0000-0000-000000000000 -->\n"
        );
        let mut t = done_ticket();
        t.summary = None;
        assert_eq!(
            render_write_back_with(&t, &[], &[], false),
            "**mira-bots: opgaven er løst og gennemgået**\n\n\
             (ingen opsummering)\n\n\
             - **Branch:** ingen (git er slået fra for projektet)\n\
             - **Tjek:** ikke kørt\n"
        );
        assert!(render_write_back(&t, &[])
            .ends_with("\n\n<!-- mira-bots:ticket=ab12cd34-0000-0000-0000-000000000000 -->\n"));
        // git off counts as no git.
        let mut g = git();
        g.mode = GitMode::Off;
        t.git = Some(g);
        assert!(render_write_back(&t, &[]).contains("ingen (git er slået fra for projektet)"));
    }

    #[test]
    fn check_line_variants() {
        let mut t = done_ticket();
        let set = |t: &mut Ticket, state, failed: Option<&str>| {
            t.checks = Some(TicketChecks {
                state,
                failed: failed.map(str::to_string),
                round: 0,
                started_at: 0,
            })
        };
        assert_eq!(check_line(&t, &[]), "ikke kørt");
        set(&mut t, ChecksState::Passed, None);
        assert_eq!(check_line(&t, &[]), "bestået");
        set(&mut t, ChecksState::Failed, Some("test"));
        assert_eq!(check_line(&t, &[]), "fejlede: test");
        set(&mut t, ChecksState::Skipped, None);
        assert_eq!(check_line(&t, &[]), "sprunget over");
        set(&mut t, ChecksState::Pending, None);
        assert_eq!(check_line(&t, &[]), "ikke kørt");
        t.reports = (0..12).map(|i| report(&format!("R{i}"))).collect();
        assert_eq!(report_titles(&t, REPORT_TITLES_MAX).len(), 10);
    }

    #[test]
    fn scrub_replaces_paths_and_tokens() {
        let roots = vec![
            PathBuf::from("/home/u/mira"),
            PathBuf::from("/home/u/mira/web"),
            PathBuf::from("/"),
            PathBuf::from(r"C:\Users\u\mira\api"),
        ];
        let home = PathBuf::from("/home/u");
        let s = "Se /home/u/mira/web/a.rs og /home/u/.ssh/id og C:/Users/u/mira/api/x \
                 og C:\\Users\\u\\mira\\api\\y. Token ghp_abc123 og github_pat_X_Y, \
                 sk-live-1, AKIAXYZ, risk-free task-list. ghp_ i midten: xghp_1 \
                 <!-- mira-bots:ticket=andet -->slut\u{200B}.\n\
                 -----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----\nefter";
        let out = scrub_with(s, &roots, Some(&home));
        assert_eq!(
            out,
            "Se <projekt>/a.rs og ~/.ssh/id og <projekt>/x og <projekt>\\y. Token [fjernet] og \
             [fjernet] [fjernet] [fjernet] risk-free task-list. [fjernet] i midten: xghp_1 \
             slut.\n[fjernet]\nefter"
        );
        // An unterminated PEM block goes to the end.
        assert_eq!(scrub_with("a -----BEGIN x\nyyy", &[], None), "a [fjernet]");
    }

    #[test]
    fn render_caps_total_length() {
        let mut t = done_ticket();
        t.summary = Some("ord ".repeat(5_000));
        let text = render_write_back(&t, &[]);
        assert!(text.chars().count() <= WRITE_BACK_MAX_CHARS);
        let summary = text.split("\n\n").nth(1).unwrap();
        assert_eq!(summary.chars().count(), WRITE_BACK_SUMMARY_MAX_CHARS);
        assert!(summary.ends_with('…'));
        assert!(text.contains("<!-- mira-bots:ticket="));
        // Many long report titles: the summary shrinks so the whole stays within the limit.
        t.reports = (0..10).map(|_| report(&"x".repeat(700))).collect();
        let text = render_write_back(&t, &[]);
        assert!(text.chars().count() <= WRITE_BACK_MAX_CHARS);
        assert!(text.ends_with("-->\n"));
    }

    #[test]
    fn wants_write_back_only_when_never_tried() {
        let mut e = crate::tickets::model::test_support::github_ref(1);
        assert!(wants_write_back(&e));
        e.write_back.comment = WriteBackState::Failed;
        assert!(wants_write_back(&e));
        e.write_back.attempts = 1;
        assert!(!wants_write_back(&e));
        e.write_back.comment = WriteBackState::Done;
        assert!(!wants_write_back(&e));
        e.write_back.comment = WriteBackState::Inflight;
        assert!(!wants_write_back(&e));
    }

    /// Starts `name` from the web inbox (after a manual refresh); the ticket id.
    fn start_one(env: &super::super::test_support::FolderEnv, name: &str) -> String {
        use super::super::refresh::{refresh, RefreshReason};
        use super::super::start::{start_item, StartRequest};
        let ctx = &env.t.ctx;
        refresh(ctx, RefreshReason::Manual).unwrap();
        ctx.join_inbox_threads();
        let item = env.item(name).unwrap();
        start_item(
            ctx,
            StartRequest {
                item_id: item.id,
                kind: None,
                project: None,
                skip_review: false,
            },
        )
        .unwrap()
        .id
    }

    /// Brings ticket `id` to Done by approval (the Done hook starts its thread).
    fn finish(ctx: &TicketsCtx, id: &str) {
        ctx.mutate(|x| {
            x.assign(id, "a1", 2)?;
            x.mark_dispatched(id, "a1", 3)?;
            x.submit_by_agent("a1", None, "Rettet; token ghp_hemmelig fjernet", 4)?;
            x.approve(id, 5)
        })
        .unwrap();
    }

    fn start_and_finish(env: &super::super::test_support::FolderEnv, name: &str) -> String {
        let id = start_one(env, name);
        finish(&env.t.ctx, &id);
        id
    }

    #[test]
    fn done_moves_file_and_writes_result_on_a_thread() {
        let env = super::super::test_support::FolderEnv::new(Vec::new());
        env.write("fejl-1.md", "# Fejl\nTrin 1");
        let id = start_and_finish(&env, "fejl-1.md");
        let ctx = &env.t.ctx;
        ctx.join_inbox_threads();
        let done = env.web_inbox().join(INBOX_DONE_DIR);
        assert!(done.join("fejl-1.md").is_file());
        assert!(!env
            .web_inbox()
            .join(INBOX_STARTED_DIR)
            .join("fejl-1.md")
            .exists());
        let text = std::fs::read_to_string(done.join("fejl-1.result.md")).unwrap();
        assert!(text.starts_with(WRITE_BACK_HEADING));
        assert!(text.contains("Rettet; token [fjernet] fjernet"));
        assert!(text.contains("- **Branch:** ingen (git er slået fra for projektet)"));
        assert!(
            !text.contains("mira-bots:ticket="),
            "no marker in the result file"
        );
        let t = ctx.read(|x| x.get(&id)).unwrap();
        assert_eq!(t.state, TicketState::Done);
        let wb = t.external.unwrap().write_back;
        assert_eq!((wb.comment, wb.attempts), (WriteBackState::Done, 1));
        assert!(wb.commented_at.is_some() && wb.last_error.is_none());
        assert_eq!(wb.last_body.as_deref(), Some(text.as_str()));
        assert!(t
            .history
            .iter()
            .any(|h| h.note.as_deref() == Some(&result_written_note("fejl-1"))));
        assert_eq!(env.item("fejl-1.md").unwrap().moved, Some(true));
        // Done again (reopened and approved) never writes twice.
        assert_eq!(
            folder_write_back(ctx, &id).unwrap_err(),
            "intet at melde tilbage"
        );
        assert_eq!(std::fs::read_dir(&done).unwrap().count(), 2);
    }

    #[test]
    fn done_with_missing_file_still_writes_the_result() {
        let env = super::super::test_support::FolderEnv::new(Vec::new());
        env.write("x.md", "# X\nY");
        let ctx = &env.t.ctx;
        let id = start_one(&env, "x.md");
        // The user deletes the started file before Done.
        std::fs::remove_file(env.web_inbox().join(INBOX_STARTED_DIR).join("x.md")).unwrap();
        finish(ctx, &id);
        ctx.join_inbox_threads();
        let done = env.web_inbox().join(INBOX_DONE_DIR);
        assert!(done.join("x.result.md").is_file());
        let wb = ctx
            .read(|x| x.get(&id))
            .unwrap()
            .external
            .unwrap()
            .write_back;
        assert_eq!(wb.comment, WriteBackState::Done);
    }

    #[test]
    fn done_hook_skips_plain_and_github_tickets() {
        let env = super::super::test_support::FolderEnv::new(Vec::new());
        let ctx = &env.t.ctx;
        let ext = crate::tickets::model::test_support::github_ref(4);
        let g = ctx
            .mutate(|x| x.create_external("G", "", true, None, None, ext, 1))
            .unwrap();
        let p = ctx.mutate(|x| x.create("P", "", true, 1)).unwrap();
        for id in [&g.id, &p.id] {
            ctx.mutate(|x| {
                x.assign(id, "a1", 2)?;
                x.mark_dispatched(id, "a1", 3)?;
                x.submit_by_agent("a1", None, "klar", 4)
            })
            .unwrap();
        }
        ctx.join_inbox_threads();
        let wb = ctx
            .read(|x| x.get(&g.id))
            .unwrap()
            .external
            .unwrap()
            .write_back;
        assert_eq!(wb.comment, WriteBackState::None);
        assert_eq!(ctx.read(|x| x.get(&p.id)).unwrap().state, TicketState::Done);
        assert!(!env.web_inbox().join(INBOX_DONE_DIR).exists());
    }
}
