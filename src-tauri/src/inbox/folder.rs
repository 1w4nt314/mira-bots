//! The folder source (step 6c, plan punkt 8, research6c §3): `.md`/`.txt` files directly in
//! `<root>/inbox/` and `<project>/.mira-bots/inbox/`.
//!
//! Every file is someone else's text (an agent can write `.mira-bots/inbox/` too): the
//! frontmatter is parsed with `std` only, `kind:` is only checked (never starts a playbook) and
//! `project:` cannot move a file out of its project folder. Only regular files directly in the
//! folder are read (no subfolders, no symlinks: a link to a secret file would put it into a
//! ticket), at most [`INBOX_FILES_PER_DIR_MAX`] per scan, each at most
//! [`INBOX_FILE_MAX_BYTES`]. Files are read and parsed on every scan (they are small); the
//! fingerprint `"<mtime_ms>:<len>"` is stored with the item, `inbox.json` is the truth.
//!
//! Start moves the file to `started/`, Done to `done/` plus `<name>.result.md`
//! ([`move_with_retry`], [`write_result`]); a file is never overwritten or deleted.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use super::source::{Fetched, FetchedItem, Source, SourceError, SourceErrorKind, SourceId};
use crate::config::{
    folder_read_failed_note, foreign_project_note, unknown_key_note, unknown_kind_note, INBOX_DIR,
    INBOX_FILES_PER_DIR_MAX, INBOX_FILE_MAX_BYTES, INBOX_FILE_TOO_BIG_NOTE,
    INBOX_FOLDER_MIN_INTERVAL_MS, INBOX_LABELS_MAX, INBOX_TOO_MANY_FILES_NOTE, PROJECT_INBOX_DIR,
};
use crate::projects::{find_project, same_id, validate_project_id, Project};
use crate::tickets::service::validate_kind;

/// The frontmatter must close within this many lines (research §3.1).
const FRONTMATTER_MAX_LINES: usize = 30;
/// Pauses between the attempts of [`move_with_retry`] (plan A.3).
pub const MOVE_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(150),
    Duration::from_millis(300),
    Duration::from_millis(600),
];

/// A parsed inbox file (C6c.1). Texts are raw: the inbox cleans them before storing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub title: Option<String>,
    pub kind: Option<String>,
    pub project: Option<String>,
    pub labels: Vec<String>,
    pub body: String,
    pub notes: Vec<String>,
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    let b = v.as_bytes();
    let quoted = b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''));
    if quoted {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// `a, b` or `[a, b]` or `["a b", c]`; trimmed, empty ones dropped.
fn split_labels(v: &str) -> Vec<String> {
    let v = v.trim();
    let v = v
        .strip_prefix('[')
        .and_then(|x| x.strip_suffix(']'))
        .unwrap_or(v);
    v.split(',')
        .map(|s| unquote(s).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parses an inbox file (research §3.1): a BOM is dropped and line ends become `\n`; a
/// frontmatter exists only when the first line is exactly `---` and a closing `---` line follows
/// within 30 lines. Known keys (case-insensitive): `title`, `kind`, `project`, `labels`
/// (alias `label`); unknown keys get a note, lines without `:`, comments and indented lines are
/// ignored; values lose surrounding quotes. Without a `title` the first non-empty line (without
/// leading `#`) is the title and leaves the body. The body is trimmed of blank lines around it.
pub fn parse_frontmatter(text: &str) -> Parsed {
    let mut p = Parsed::default();
    let t = text
        .strip_prefix('\u{FEFF}')
        .unwrap_or(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut rest: &str = &t;
    if let Some(after) = t.strip_prefix("---\n") {
        let mut offset = 0usize;
        let mut closed = None;
        for (i, line) in after.split_inclusive('\n').enumerate() {
            if i >= FRONTMATTER_MAX_LINES {
                break;
            }
            if line.trim_end() == "---" {
                closed = Some((offset, offset + line.len()));
                break;
            }
            offset += line.len();
        }
        if let Some((end, after_close)) = closed {
            for line in after[..end].lines() {
                let line = line.trim_end();
                if line.is_empty()
                    || line.starts_with(' ')
                    || line.starts_with('\t')
                    || line.starts_with('#')
                {
                    continue;
                }
                let Some((k, v)) = line.split_once(':') else {
                    continue;
                };
                let key = k.trim().to_lowercase();
                let val = Some(unquote(v)).filter(|s| !s.is_empty());
                match key.as_str() {
                    "title" => p.title = val,
                    "kind" => p.kind = val,
                    "project" => p.project = val,
                    "labels" | "label" => p.labels = split_labels(v),
                    _ => p.notes.push(unknown_key_note(&key)),
                }
            }
            rest = &after[after_close..];
        }
    }
    let mut body = rest.trim_start_matches('\n');
    if p.title.is_none() {
        let (first, after) = body.split_once('\n').unwrap_or((body, ""));
        let candidate = first.trim().trim_start_matches('#').trim();
        if !candidate.is_empty() {
            p.title = Some(candidate.to_string());
            body = after;
        }
    }
    p.body = body.trim_matches('\n').to_string();
    p
}

/// Whether `name` is an inbox file: `.md`/`.txt` (case-insensitive), not hidden, not a backup,
/// temp or swap file, not a result file.
fn is_inbox_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    !(name.starts_with('.')
        || name.ends_with('~')
        || lower.ends_with(".tmp")
        || lower.ends_with(".swp")
        || lower.ends_with(".result.md"))
        && (lower.ends_with(".md") || lower.ends_with(".txt"))
}

fn fingerprint(meta: &fs::Metadata) -> String {
    let ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis());
    format!("{ms}:{}", meta.len())
}

/// The scan of one folder; `kinds`: the workspace's playbook names (`None`: not checked).
/// The projects root is `dir`'s parent for the root folder (`project == None`).
fn scan(dir: &Path, project: Option<&str>, kinds: Option<&[String]>) -> io::Result<Fetched> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        // A folder that went away lists nothing (its new items are gone).
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Fetched {
                complete: true,
                ..Fetched::default()
            })
        }
        Err(e) => return Err(e),
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| is_inbox_name(n))
        .collect();
    names.sort();
    let mut out = Fetched {
        complete: true,
        ..Fetched::default()
    };
    if names.len() > INBOX_FILES_PER_DIR_MAX {
        names.truncate(INBOX_FILES_PER_DIR_MAX);
        out.capped = true;
        out.complete = false;
        out.notes.push(INBOX_TOO_MANY_FILES_NOTE.to_string());
    }
    let source = project.unwrap_or("_rod");
    let root = dir.parent();
    for name in names {
        let path = dir.join(&name);
        // Never follow a symlink; only regular files.
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                log::debug!("inbox: cannot stat a file in {}: {e}", dir.display());
                out.complete = false;
                continue;
            }
        };
        if !meta.file_type().is_file() {
            continue;
        }
        if meta.len() > INBOX_FILE_MAX_BYTES {
            out.notes.push(format!("{name}: {INBOX_FILE_TOO_BIG_NOTE}"));
            continue;
        }
        let text = match fs::read(&path) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => {
                // A locked file is not gone: keep its item as it is.
                log::debug!("inbox: cannot read a file in {}: {e}", dir.display());
                out.complete = false;
                continue;
            }
        };
        let parsed = parse_frontmatter(&text);
        let mut notes = parsed.notes;
        if let (Some(k), Some(kinds)) = (parsed.kind.as_deref(), kinds) {
            if validate_kind(Some(k), kinds).is_err() {
                notes.push(unknown_kind_note(k));
            }
        }
        let item_project = match (project, parsed.project.as_deref()) {
            (Some(own), Some(named)) => {
                if !same_id(own, named) {
                    notes.push(foreign_project_note(named, own));
                }
                Some(own.to_string())
            }
            (Some(own), None) => Some(own.to_string()),
            (None, Some(named)) => validate_project_id(named)
                .ok()
                .zip(root)
                .and_then(|(id, r)| find_project(r, &id))
                .map(|p| p.id),
            (None, None) => None,
        };
        let mut labels = parsed.labels;
        labels.truncate(INBOX_LABELS_MAX);
        out.items.push(FetchedItem {
            external_id: format!("folder:{source}:{name}"),
            title: parsed.title.unwrap_or_default(),
            body: Some(parsed.body),
            labels,
            path: Some(name),
            fingerprint: Some(fingerprint(&meta)),
            project: item_project,
            notes,
            ..FetchedItem::default()
        });
    }
    Ok(out)
}

/// Scans one inbox folder (C6c.1; rules in the module doc). `external_id` =
/// `folder:<project|_rod>:<file name>`. A folder that cannot be read gives an incomplete,
/// empty result with the note "mappen kunne ikke læses: …" (nothing is marked gone).
pub fn scan_dir(dir: &Path, project: Option<&str>) -> Fetched {
    scan(dir, project, None).unwrap_or_else(|e| Fetched {
        notes: vec![folder_read_failed_note(&e.to_string())],
        ..Fetched::default()
    })
}

/// One inbox folder as a [`Source`]: `<root>/inbox/` (`project == None`) or
/// `<project>/.mira-bots/inbox/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderSource {
    pub dir: PathBuf,
    pub project: Option<String>,
    /// The workspace's playbook names (an unknown `kind:` gets a note).
    pub kinds: Vec<String>,
}

impl Source for FolderSource {
    fn id(&self) -> SourceId {
        SourceId::folder(self.project.as_deref())
    }

    fn label(&self) -> String {
        match self.project {
            None => format!("mappen {INBOX_DIR}/"),
            Some(_) => format!("mappen {PROJECT_INBOX_DIR}/"),
        }
    }

    fn min_interval_ms(&self) -> u64 {
        INBOX_FOLDER_MIN_INTERVAL_MS
    }

    fn fetch(&self) -> Result<Fetched, SourceError> {
        scan(&self.dir, self.project.as_deref(), Some(&self.kinds)).map_err(|e| {
            SourceError::new(
                SourceErrorKind::Folder,
                folder_read_failed_note(&e.to_string()),
            )
        })
    }

    fn project(&self) -> Option<String> {
        self.project.clone()
    }
}

/// `dir` is a real folder (not a symlink).
fn is_real_dir(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_dir())
}

/// `<project_dir>/.mira-bots/inbox`, joined component by component.
pub fn project_inbox_dir(project_dir: &Path) -> PathBuf {
    PROJECT_INBOX_DIR
        .split('/')
        .fold(project_dir.to_path_buf(), |p, part| p.join(part))
}

/// The folder sources that exist (plan punkt 8): the root's `inbox/` and every project's
/// `.mira-bots/inbox/`. Missing folders are not created. A "project" named like the root inbox
/// folder is that folder, not a project.
pub fn inbox_dirs(root: &Path, projects: &[Project], kinds: &[String]) -> Vec<FolderSource> {
    let mut out = Vec::new();
    let root_dir = root.join(INBOX_DIR);
    if is_real_dir(&root_dir) {
        out.push(FolderSource {
            dir: root_dir,
            project: None,
            kinds: kinds.to_vec(),
        });
    }
    for p in projects {
        if same_id(&p.id, INBOX_DIR) {
            continue;
        }
        let dir = project_inbox_dir(Path::new(&p.path));
        if is_real_dir(&dir) {
            out.push(FolderSource {
                dir,
                project: Some(p.id.clone()),
                kinds: kinds.to_vec(),
            });
        }
    }
    out
}

/// The inbox folder of the source key `folder:_rod` / `folder:<project>` (`None`: not a folder
/// key, or the project no longer exists).
pub fn folder_dir_for(root: &Path, source_key: &str) -> Option<PathBuf> {
    match source_key.strip_prefix("folder:")? {
        "_rod" => Some(root.join(INBOX_DIR)),
        p => find_project(root, p).map(|p| project_inbox_dir(Path::new(&p.path))),
    }
}

/// The source key of a folder item's external id (`folder:<project|_rod>:<name>` →
/// `folder:<project|_rod>`). Project names cannot contain `:`.
pub fn source_key_of(external_id: &str) -> Option<String> {
    let rest = external_id.strip_prefix("folder:")?;
    let (source, _) = rest.split_once(':')?;
    Some(format!("folder:{source}"))
}

/// `name`, else `stem-2.ext`, `stem-3.ext`, … — the first that does not exist in `dir`.
fn free_name(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if fs::symlink_metadata(&first).is_err() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2u32..)
        .map(|n| dir.join(format!("{stem}-{n}{ext}")))
        .find(|p| fs::symlink_metadata(p).is_err())
        .expect("a free name")
}

/// A rename error that may go away (Windows: the file is open elsewhere).
fn is_retryable(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied || matches!(e.raw_os_error(), Some(5 | 32 | 33))
}

// TODO(windows-verify): a file open in Notepad moves; one locked by Word fails after the
// retries, Start/Done go on with the note and `moved = false` (plan D.111).
/// Moves `from` into `to_dir` (created if missing) under the same name, or with `-2`, `-3`, …
/// when the name is taken (never overwrites). A rename refused because the file is open
/// elsewhere is tried again after 150/300/600 ms; no copy-and-delete. Returns the new path.
pub fn move_with_retry(from: &Path, to_dir: &Path) -> Result<PathBuf, String> {
    move_with_delays(from, to_dir, &MOVE_RETRY_DELAYS)
}

/// [`move_with_retry`] with given pauses (tests).
pub fn move_with_delays(
    from: &Path,
    to_dir: &Path,
    delays: &[Duration],
) -> Result<PathBuf, String> {
    let name = from
        .file_name()
        .ok_or_else(|| "ugyldigt filnavn".to_string())?
        .to_string_lossy()
        .into_owned();
    fs::create_dir_all(to_dir).map_err(|e| e.to_string())?;
    let mut pauses = delays.iter();
    loop {
        let to = free_name(to_dir, &name);
        match fs::rename(from, &to) {
            Ok(()) => return Ok(to),
            Err(e) if is_retryable(&e) => match pauses.next() {
                Some(d) => std::thread::sleep(*d),
                None => return Err(e.to_string()),
            },
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Writes `<done_dir>/<name>.result.md` (`-2`, … when taken; via `.tmp` + rename, never over an
/// existing file). `name`: the file name without its extension. Returns the path.
pub fn write_result(done_dir: &Path, name: &str, text: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(done_dir).map_err(|e| e.to_string())?;
    let target = (1u32..)
        .map(|n| match n {
            1 => done_dir.join(format!("{name}.result.md")),
            n => done_dir.join(format!("{name}-{n}.result.md")),
        })
        .find(|p| fs::symlink_metadata(p).is_err())
        .expect("a free name");
    let mut tmp = target.clone().into_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, text.as_bytes()).map_err(|e| e.to_string())?;
    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(target)
}

/// The file name without its last extension (`fejl-1.md` → `fejl-1`).
pub fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::INBOX_STARTED_DIR;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("mira-inbox-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn frontmatter_full() {
        let p = parse_frontmatter(
            "---\ntitle: Fix \"login\"\nkind: bug\nproject: web\nlabels: a, b\n---\nBody\nline2\n",
        );
        assert_eq!(p.title.as_deref(), Some("Fix \"login\""));
        assert_eq!(p.kind.as_deref(), Some("bug"));
        assert_eq!(p.project.as_deref(), Some("web"));
        assert_eq!(p.labels, vec!["a", "b"]);
        assert_eq!(p.body, "Body\nline2");
        assert!(p.notes.is_empty());
    }

    #[test]
    fn frontmatter_crlf_bom_quotes_and_list() {
        let p = parse_frontmatter(
            "\u{FEFF}---\r\ntitle: 'Hej: verden'\r\nlabels: [\"x y\", z, ]\r\n---\r\nTekst\r\n",
        );
        assert_eq!(p.title.as_deref(), Some("Hej: verden"));
        assert_eq!(p.labels, vec!["x y", "z"]);
        assert_eq!(p.body, "Tekst");
        // A lone \r is a line end too; `label` is an alias.
        let p = parse_frontmatter("---\rLabel: solo\r---\rB\rC");
        assert_eq!(p.labels, vec!["solo".to_string()]);
        assert_eq!((p.title.as_deref(), p.body.as_str()), (Some("B"), "C"));
    }

    #[test]
    fn frontmatter_absent_first_line_is_title() {
        let p = parse_frontmatter("# Crash ved start\n\nTrin 1\n");
        assert_eq!(p.title.as_deref(), Some("Crash ved start"));
        assert_eq!(p.body, "Trin 1");
        let p = parse_frontmatter("\n\n  Bare tekst  \nmere\n\n");
        assert_eq!(p.title.as_deref(), Some("Bare tekst"));
        assert_eq!(p.body, "mere");
        assert_eq!(parse_frontmatter(""), Parsed::default());
    }

    #[test]
    fn frontmatter_unclosed_or_late_dashes_are_body() {
        let p = parse_frontmatter("---\ntitle: x\nBody");
        assert_eq!(p.title.as_deref(), Some("---"));
        assert!(p.body.contains("title: x"));
        let p = parse_frontmatter("Titel\n\n---\nkind: bug\n---\nmere");
        assert_eq!(p.kind, None);
        assert!(p.body.contains("kind: bug"));
        // The closing line must come within 30 lines.
        let long = format!("---\n{}---\nB", "x: 1\n".repeat(31));
        assert_eq!(parse_frontmatter(&long).title.as_deref(), Some("---"));
    }

    #[test]
    fn frontmatter_unknown_keys_noted_others_ignored() {
        let p = parse_frontmatter(
            "---\nFoo: 1\n# kommentar\nuden kolon\n  - indrykket: x\nTITLE: T\nkind:\n---\nB",
        );
        assert_eq!(p.notes, vec![unknown_key_note("foo")]);
        assert_eq!(p.title.as_deref(), Some("T"));
        assert_eq!(p.kind, None);
        let p = parse_frontmatter("---\ntitle: T\n---\n");
        assert_eq!((p.title.as_deref(), p.body.as_str()), (Some("T"), ""));
    }

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, text).unwrap();
        p
    }

    #[test]
    fn scan_skips_symlinks_hidden_big_and_subdirs() {
        let t = TempDir::new();
        let dir = t.0.join("inbox");
        fs::create_dir_all(dir.join("started")).unwrap();
        write(&dir, "a.md", "# A\nbody");
        write(&dir, "b.TXT", "B");
        write(&dir, ".skjult.md", "x");
        write(&dir, "c.md~", "x");
        write(&dir, "d.md.tmp", "x");
        write(&dir, "e.swp", "x");
        write(&dir, "f.result.md", "x");
        write(&dir, "g.pdf", "x");
        write(&dir.join("started"), "h.md", "x");
        fs::write(
            dir.join("big.md"),
            vec![b'x'; INBOX_FILE_MAX_BYTES as usize + 1],
        )
        .unwrap();
        #[cfg(unix)]
        {
            let secret = write(&t.0, "secret.md", "hemmelig");
            std::os::unix::fs::symlink(&secret, dir.join("link.md")).unwrap();
        }
        let f = scan_dir(&dir, None);
        let ids: Vec<&str> = f.items.iter().map(|i| i.external_id.as_str()).collect();
        assert_eq!(ids, vec!["folder:_rod:a.md", "folder:_rod:b.TXT"]);
        assert!(f.complete && !f.capped);
        assert_eq!(f.notes, vec![format!("big.md: {INBOX_FILE_TOO_BIG_NOTE}")]);
        assert!(f
            .items
            .iter()
            .all(|i| i.body.as_deref() != Some("hemmelig")));
        let a = &f.items[0];
        assert_eq!(
            (a.title.as_str(), a.body.as_deref(), a.path.as_deref()),
            ("A", Some("body"), Some("a.md"))
        );
        let meta = fs::metadata(dir.join("a.md")).unwrap();
        assert_eq!(a.fingerprint.as_deref(), Some(fingerprint(&meta).as_str()));
        assert!(a.fingerprint.as_deref().unwrap().ends_with(":8"));
    }

    #[test]
    fn scan_caps_at_200_files_and_is_then_incomplete() {
        let t = TempDir::new();
        for i in 0..(INBOX_FILES_PER_DIR_MAX + 3) {
            write(&t.0, &format!("f{i:03}.md"), "x");
        }
        let f = scan_dir(&t.0, Some("web"));
        assert_eq!(f.items.len(), INBOX_FILES_PER_DIR_MAX);
        assert!(f.capped && !f.complete);
        assert_eq!(f.notes, vec![INBOX_TOO_MANY_FILES_NOTE.to_string()]);
        assert_eq!(f.items[0].external_id, "folder:web:f000.md");
    }

    #[test]
    fn scan_of_missing_or_unreadable_folder() {
        let t = TempDir::new();
        let f = scan_dir(&t.0.join("findes-ikke"), None);
        assert!(f.complete && f.items.is_empty());
        // A file where the folder should be: an error, incomplete (nothing gone).
        let file = write(&t.0, "inbox", "x");
        let f = scan_dir(&file, None);
        assert!(!f.complete && f.items.is_empty());
        assert!(f.notes[0].starts_with("mappen kunne ikke læses: "));
        let src = FolderSource {
            dir: file,
            project: None,
            kinds: Vec::new(),
        };
        let e = src.fetch().unwrap_err();
        assert_eq!(e.kind, SourceErrorKind::Folder);
        assert!(e.text.starts_with("mappen kunne ikke læses: "));
    }

    #[test]
    fn external_id_is_relative_and_prefixed() {
        let t = TempDir::new();
        write(&t.0, "fejl-1.md", "x");
        let f = scan_dir(&t.0, Some("web"));
        assert_eq!(f.items[0].external_id, "folder:web:fejl-1.md");
        assert_eq!(f.items[0].path.as_deref(), Some("fejl-1.md"));
        assert_eq!(
            source_key_of("folder:web:fejl-1.md").as_deref(),
            Some("folder:web")
        );
        assert_eq!(
            source_key_of("folder:_rod:a:b.md").as_deref(),
            Some("folder:_rod")
        );
        assert_eq!(source_key_of("github:o/r#1"), None);
    }

    #[test]
    fn project_frontmatter_cannot_leave_project() {
        let t = TempDir::new();
        let root = &t.0;
        fs::create_dir_all(root.join("web")).unwrap();
        fs::create_dir_all(root.join("api")).unwrap();
        let pdir = project_inbox_dir(&root.join("web"));
        fs::create_dir_all(&pdir).unwrap();
        write(&pdir, "a.md", "---\nproject: api\nkind: docs\n---\nB");
        write(&pdir, "b.md", "---\nproject: WEB\nkind: bug\n---\nB");
        let src = FolderSource {
            dir: pdir,
            project: Some("web".into()),
            kinds: Vec::new(),
        };
        let f = src.fetch().unwrap();
        assert_eq!(f.items[0].project.as_deref(), Some("web"));
        assert_eq!(
            f.items[0].notes,
            vec![
                unknown_kind_note("docs"),
                foreign_project_note("api", "web")
            ]
        );
        assert!(f.items[1].notes.is_empty());
        // Root files: a valid, existing project is used (its on-disk name); else none.
        let rdir = root.join(INBOX_DIR);
        fs::create_dir_all(&rdir).unwrap();
        write(&rdir, "1.md", "---\nproject: Api\n---\nB");
        write(&rdir, "2.md", "---\nproject: findes-ikke\n---\nB");
        write(&rdir, "3.md", "---\nproject: a/b\n---\nB");
        let f = scan_dir(&rdir, None);
        let projects: Vec<Option<&str>> = f.items.iter().map(|i| i.project.as_deref()).collect();
        assert_eq!(projects, vec![Some("api"), None, None]);
    }

    #[test]
    fn inbox_dirs_lists_only_existing_real_folders() {
        let t = TempDir::new();
        let root = &t.0;
        for p in ["web", "api", "inbox"] {
            fs::create_dir_all(root.join(p)).unwrap();
        }
        fs::create_dir_all(project_inbox_dir(&root.join("web"))).unwrap();
        fs::create_dir_all(project_inbox_dir(&root.join("inbox"))).unwrap();
        let projects = crate::projects::list_projects(root);
        let dirs = inbox_dirs(root, &projects, &[]);
        let keys: Vec<String> = dirs.iter().map(|d| d.id().key).collect();
        assert_eq!(keys, vec!["folder:_rod", "folder:web"]);
        assert_eq!(dirs[0].label(), "mappen inbox/");
        assert_eq!(dirs[1].label(), "mappen .mira-bots/inbox/");
        assert_eq!(dirs[1].project().as_deref(), Some("web"));
        assert_eq!(dirs[0].min_interval_ms(), 60_000);
        // Nothing was created.
        assert!(!project_inbox_dir(&root.join("api")).exists());
        assert_eq!(
            folder_dir_for(root, "folder:web"),
            Some(project_inbox_dir(&root.join("web")))
        );
        assert_eq!(
            folder_dir_for(root, "folder:_rod"),
            Some(root.join("inbox"))
        );
        assert_eq!(folder_dir_for(root, "folder:nej"), None);
        assert_eq!(folder_dir_for(root, "github:o/r"), None);
    }

    #[test]
    fn move_adds_suffix_on_collision() {
        let t = TempDir::new();
        let started = t.0.join(INBOX_STARTED_DIR);
        let a = write(&t.0, "fejl.md", "1");
        let to = move_with_retry(&a, &started).unwrap();
        assert_eq!(to, started.join("fejl.md"));
        let a = write(&t.0, "fejl.md", "2");
        let to = move_with_retry(&a, &started).unwrap();
        assert_eq!(to, started.join("fejl-2.md"));
        assert_eq!(fs::read_to_string(started.join("fejl.md")).unwrap(), "1");
        assert!(!a.exists());
    }

    #[test]
    fn move_reports_failure_without_touching_the_file() {
        let t = TempDir::new();
        let a = write(&t.0, "fejl.md", "1");
        // The target folder is a file: no move, the file stays.
        let blocker = write(&t.0, "started", "x");
        assert!(move_with_delays(&a, &blocker, &[]).is_err());
        assert!(a.exists());
        // A missing file is not retried.
        let started = Instant::now();
        assert!(move_with_retry(&t.0.join("nej.md"), &t.0.join("done")).is_err());
        assert!(started.elapsed() < Duration::from_millis(150));
        assert!(is_retryable(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        assert!(is_retryable(&io::Error::from_raw_os_error(32)));
        assert!(!is_retryable(&io::Error::from(io::ErrorKind::NotFound)));
    }

    use std::time::Instant;

    #[test]
    fn write_result_never_overwrites() {
        let t = TempDir::new();
        let done = t.0.join("done");
        let p = write_result(&done, "fejl-1", "tekst").unwrap();
        assert_eq!(p, done.join("fejl-1.result.md"));
        let p2 = write_result(&done, "fejl-1", "igen").unwrap();
        assert_eq!(p2, done.join("fejl-1-2.result.md"));
        assert_eq!(fs::read_to_string(&p).unwrap(), "tekst");
        assert_eq!(fs::read_dir(&done).unwrap().count(), 2);
        assert_eq!(stem_of("fejl-1.md"), "fejl-1");
        assert_eq!(stem_of(".md"), ".md");
        assert_eq!(stem_of("x"), "x");
    }
}
