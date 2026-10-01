//! Report files (plan5 A.8): `<app_data>/tickets/<ticketId>/reports/<nn>-<slug>.md`. The
//! metadata lives on the ticket (`Ticket.reports`); this module only touches the files. Report
//! files never go into an agent's folder, and their text is never logged.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Folder name under the ticket's folder, and the first component of `TicketReport.path`.
pub const REPORTS_SUBDIR: &str = "reports";
/// Maximum slug length (chars).
pub const SLUG_MAX_CHARS: usize = 40;
/// Slug when nothing usable is left of the title.
pub const EMPTY_SLUG: &str = "rapport";

/// The report folder root (`<app_data>/tickets`).
#[derive(Clone, Debug)]
pub struct ReportStore {
    root: PathBuf,
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

/// Ticket ids are uuids: only ASCII letters, digits and `-` may become a folder name.
fn check_ticket_id(ticket_id: &str) -> io::Result<()> {
    let ok = !ticket_id.is_empty()
        && ticket_id.len() <= 64
        && ticket_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    if ok {
        Ok(())
    } else {
        Err(invalid("ugyldigt ticket-id"))
    }
}

/// `title` as a file-name part: ASCII `a–z0–9` and `-` (æ/ø/å → ae/oe/aa, runs of anything
/// else → one `-`), at most [`SLUG_MAX_CHARS`], [`EMPTY_SLUG`] when nothing is left.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in title.chars().flat_map(char::to_lowercase) {
        let piece: &str = match c {
            'æ' => "ae",
            'ø' => "oe",
            'å' => "aa",
            'é' | 'è' | 'ê' => "e",
            'ü' => "u",
            'ö' => "o",
            'ä' => "a",
            c if c.is_ascii_alphanumeric() => {
                if dash && !out.is_empty() {
                    out.push('-');
                }
                dash = false;
                out.push(c);
                continue;
            }
            _ => {
                dash = true;
                continue;
            }
        };
        if dash && !out.is_empty() {
            out.push('-');
        }
        dash = false;
        out.push_str(piece);
    }
    let mut s: String = out.chars().take(SLUG_MAX_CHARS).collect();
    while s.ends_with('-') {
        s.pop();
    }
    if s.is_empty() {
        EMPTY_SLUG.to_string()
    } else {
        s
    }
}

impl ReportStore {
    pub fn new(root: PathBuf) -> Self {
        ReportStore { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/<ticketId>/reports` (not created).
    pub fn dir_for(&self, ticket_id: &str) -> io::Result<PathBuf> {
        check_ticket_id(ticket_id)?;
        Ok(self.root.join(ticket_id).join(REPORTS_SUBDIR))
    }

    /// Writes report `seq` and returns its relative path (`reports/01-slug.md`) and size in
    /// bytes. An existing file of the same name is overwritten.
    pub fn write(
        &self,
        ticket_id: &str,
        seq: u32,
        title: &str,
        body: &str,
    ) -> io::Result<(String, u64)> {
        let dir = self.dir_for(ticket_id)?;
        fs::create_dir_all(&dir)?;
        let file = format!("{seq:02}-{}.md", slug(title));
        fs::write(dir.join(&file), body)?;
        Ok((format!("{REPORTS_SUBDIR}/{file}"), body.len() as u64))
    }

    /// Reads a report by its relative path. Only `reports/<file>` without `..`, absolute parts
    /// or backslashes is accepted.
    pub fn read(&self, ticket_id: &str, rel_path: &str) -> io::Result<String> {
        fs::read_to_string(self.resolve(ticket_id, rel_path)?)
    }

    /// Removes a report file (cleanup after a failed metadata save).
    pub fn remove(&self, ticket_id: &str, rel_path: &str) -> io::Result<()> {
        fs::remove_file(self.resolve(ticket_id, rel_path)?)
    }

    fn resolve(&self, ticket_id: &str, rel_path: &str) -> io::Result<PathBuf> {
        check_ticket_id(ticket_id)?;
        let mut parts = rel_path.split('/');
        let ok = parts.next() == Some(REPORTS_SUBDIR)
            && matches!(parts.next(), Some(f) if !f.is_empty()
                && f != "."
                && f != ".."
                && !f.contains(['\\', ':', '\0']))
            && parts.next().is_none();
        if !ok {
            return Err(invalid("ugyldig rapport-sti"));
        }
        Ok(rel_path
            .split('/')
            .fold(self.root.join(ticket_id), |p, c| p.join(c)))
    }

    /// Removes `<root>/<ticketId>` with everything in it; a missing folder is fine.
    pub fn remove_ticket_dir(&self, ticket_id: &str) -> io::Result<()> {
        check_ticket_id(ticket_id)?;
        match fs::remove_dir_all(self.root.join(ticket_id)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (ReportStore, PathBuf) {
        let base = std::env::temp_dir().join(format!("mira-reports-{}", uuid::Uuid::new_v4()));
        (ReportStore::new(base.join("tickets")), base)
    }

    #[test]
    fn slug_table() {
        let table = [
            ("Rapport ved aflevering", "rapport-ved-aflevering"),
            ("  Æbler, øl & ål!  ", "aebler-oel-aal"),
            ("../../etc/passwd", "etc-passwd"),
            ("", "rapport"),
            ("!!!", "rapport"),
            ("Fix #12: Login-fejl", "fix-12-login-fejl"),
        ];
        for (title, want) in table {
            assert_eq!(slug(title), want, "{title:?}");
        }
        let long = slug(&"abc ".repeat(30));
        assert!(long.chars().count() <= SLUG_MAX_CHARS, "{long}");
        assert!(!long.ends_with('-'));
    }

    #[test]
    fn write_read_and_remove() {
        let (s, base) = temp_store();
        let id = "0a1b2c3d-0000-4000-8000-000000000001";
        let (rel, size) = s.write(id, 1, "Første rapport", "# Hej\næøå\n").unwrap();
        assert_eq!(rel, "reports/01-foerste-rapport.md");
        assert_eq!(size, "# Hej\næøå\n".len() as u64);
        assert!(s
            .dir_for(id)
            .unwrap()
            .join("01-foerste-rapport.md")
            .is_file());
        assert_eq!(s.read(id, &rel).unwrap(), "# Hej\næøå\n");
        s.remove_ticket_dir(id).unwrap();
        assert!(!s.root().join(id).exists());
        s.remove_ticket_dir(id).unwrap();
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn get_report_refuses_path_traversal() {
        let (s, base) = temp_store();
        let id = "0a1b2c3d-0000-4000-8000-000000000001";
        s.write(id, 1, "x", "ok").unwrap();
        for bad in [
            "../x",
            "reports/../../x",
            "reports/..",
            "/etc/passwd",
            "reports\\..\\x",
            "reports/a/b",
            "other/01-x.md",
            "reports/",
            "C:/x",
        ] {
            assert!(s.read(id, bad).is_err(), "{bad}");
        }
        for bad_id in ["..", "a/b", "", "a\\b"] {
            assert!(s.read(bad_id, "reports/01-x.md").is_err(), "{bad_id}");
            assert!(s.dir_for(bad_id).is_err());
            assert!(s.remove_ticket_dir(bad_id).is_err());
        }
        assert_eq!(s.read(id, "reports/01-x.md").unwrap(), "ok");
        let _ = fs::remove_dir_all(base);
    }
}
