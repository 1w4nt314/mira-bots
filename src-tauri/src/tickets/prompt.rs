//! What the agent receives for a ticket: a file in its cwd (`.mira-bots/tickets/<short>.md`) and
//! one typed line pointing at it (plan C3.5/C3.6). The body never goes to the PTY.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::model::Ticket;
use crate::config::{TICKET_DIR, TICKET_LINE_TITLE_MAX_CHARS};

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

/// "Bed om aflevering" (C4.7): typed like a ticket line (one write, `\r` separately). It starts
/// with "Du", never with "Ticket", so the dispatcher can never take it for a ticket delivery.
pub fn request_submission_line(short: &str) -> String {
    format!(
        "Du afsluttede uden at aflevere ticket {short}. Kald mira_submit_for_review med en kort opsummering når opgaven er færdig; ellers fortsæt arbejdet."
    )
}

/// The line for a ticket (sanitises the title).
pub fn line_for(t: &Ticket) -> String {
    render_line(&t.short_id(), &sanitize_title(&t.title))
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

/// Content of `<cwd>/.mira-bots/tickets/<short>.md` (plan C3.6, rules from plan4 C4.7).
pub fn render_file(t: &Ticket, now_ms: u64) -> String {
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
         - review: {review}\n\n\
         ## Opgave\n\n\
         {body}\n\n",
        title = one_line(&t.title),
        id = t.id,
        created = iso_utc(t.created_at),
        updated = iso_utc(now_ms),
        state = t.state.as_str(),
    );
    if let Some(note) = &t.rejection_note {
        out.push_str(&format!(
            "## Afvist: {}\n\
             Ret det ovenstående og afslut dit svar igen, så ticketen kommer til review på ny.\n\n",
            one_line(note)
        ));
    }
    out.push_str(
        "## Regler\n\
         - Opgaven er en ticket fra mira-bots. Når den er løst, kald værktøjet mira_submit_for_review med en kort opsummering, og afslut så dit svar.\n\
         - Opret opfølgende opgaver med mira_create_ticket. Opret eller redigér ikke selv filer i .mira-bots/.\n",
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
pub fn write_ticket_file(cwd: &Path, t: &Ticket, now_ms: u64) -> io::Result<PathBuf> {
    let dir = ticket_dir(cwd);
    fs::create_dir_all(&dir)?;
    if let Some(root) = dir.parent() {
        let gitignore = root.join(".gitignore");
        if !gitignore.exists() {
            fs::write(&gitignore, "*\n")?;
        }
    }
    let path = dir.join(format!("{}.md", t.short_id()));
    fs::write(&path, render_file(t, now_ms))?;
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
            let line = line_for(&t);
            assert_line_safe(&line);
            assert!(line.starts_with("Ticket abcdef01: "));
            assert!(!line.contains('\u{200B}') && !line.contains('@'));
        }
        t.title = "z".repeat(300);
        assert!(line_for(&t).chars().count() < 300);
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
        let f = render_file(&t, 1_700_000_000_000);
        assert!(f.starts_with("# Ticket abcdef01: Ret login\n\n"), "{f}");
        assert!(f.contains(&format!("- id: {ID}\n")));
        assert!(f.contains("- kort-id: abcdef01\n"));
        assert!(f.contains("- oprettet: 1970-01-01T00:00:01Z\n"));
        assert!(f.contains("- opdateret: 2023-11-14T22:13:20Z\n"));
        assert!(f.contains("- status: assigned\n"));
        assert!(f.contains("- review: ja\n"));
        assert!(f.contains("## Opgave\n\nLinje 1\n\n- punkt @x /y\n\n## Regler\n"));
        assert!(!f.contains('\r'));
        assert!(!f.contains("## Afvist:"));
        assert!(f.ends_with(
            "## Regler\n\
             - Opgaven er en ticket fra mira-bots. Når den er løst, kald værktøjet mira_submit_for_review med en kort opsummering, og afslut så dit svar.\n\
             - Opret opfølgende opgaver med mira_create_ticket. Opret eller redigér ikke selv filer i .mira-bots/.\n"
        ));
        assert!(f.contains("mira_submit_for_review"));

        t.skip_review = true;
        t.body = String::new();
        t.rejection_note = Some("Mangler\ntest".into());
        let f = render_file(&t, 0);
        assert!(f.contains("- review: springes over\n"));
        assert!(f.contains("## Opgave\n\n(ingen beskrivelse)\n\n"));
        assert!(f.contains(
            "## Afvist: Mangler test\nRet det ovenstående og afslut dit svar igen, så ticketen kommer til review på ny.\n\n## Regler\n"
        ));
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
        let path = write_ticket_file(&cwd, &t, 0).unwrap();
        assert_eq!(
            path,
            cwd.join(".mira-bots").join("tickets").join("abcdef01.md")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), render_file(&t, 0));
        let gi = cwd.join(".mira-bots").join(".gitignore");
        assert_eq!(fs::read_to_string(&gi).unwrap(), "*\n");

        // Existing .gitignore is kept; the ticket file is overwritten.
        fs::write(&gi, "custom\n").unwrap();
        let mut t2 = t.clone();
        t2.title = "Ny titel".into();
        write_ticket_file(&cwd, &t2, 0).unwrap();
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
}
