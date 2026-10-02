//! Persistence of the whole ticket document: [`JsonFileStore`] (`tickets.json`, atomic writes)
//! and [`MemoryStore`] for tests.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::model::TicketDoc;
use crate::agent::now_ms;
use crate::config::TICKETS_SCHEMA_VERSION;

/// Result of [`TicketStore::load`]. `warning` is shown in Diagnostics (e.g. the file was corrupt
/// and renamed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadResult {
    pub doc: TicketDoc,
    pub warning: Option<String>,
}

/// Loads and saves the whole document. Small volume (< a few hundred tickets), so whole-document
/// writes are fine; a different backend can be swapped in behind this trait.
pub trait TicketStore: Send {
    fn load(&self) -> io::Result<LoadResult>;
    fn save(&self, doc: &TicketDoc) -> io::Result<()>;
}

/// `tickets.json` in the app data dir.
#[derive(Clone, Debug)]
pub struct JsonFileStore {
    path: PathBuf,
    /// Set once this store knows what is at `path`: a successful `load` (read, missing or moved
    /// aside as `.broken-*`) or its own successful `save`. Until then `save` refuses to replace
    /// an existing file, so a file that was never read can never be overwritten.
    known: Arc<AtomicBool>,
}

impl JsonFileStore {
    pub fn new(path: PathBuf) -> Self {
        JsonFileStore {
            path,
            known: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn tmp_path(&self) -> PathBuf {
        suffixed(&self.path, ".tmp")
    }

    /// Moves a broken file aside and returns the warning text.
    fn quarantine(&self, reason: &str) -> String {
        let broken = suffixed(&self.path, &format!(".broken-{}", now_ms()));
        let name = broken
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        log::warn!(
            "{} could not be read ({reason}); renaming it to {name}",
            self.path.display()
        );
        if let Err(e) = fs::rename(&self.path, &broken) {
            // The document starts empty anyway; the next save overwrites the broken file.
            log::error!("rename {} -> {name}: {e}", self.path.display());
        }
        format!("tickets.json kunne ikke læses og blev omdøbt til {name}; starter med tom liste")
    }
}

/// `path` with `suffix` appended to its file name (`tickets.json` → `tickets.json.tmp`).
fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Brings a parsed file up to the current schema.
///
/// - version 1: as is;
/// - version missing or 0: treated as 1 when `tickets` is a list;
/// - newer version or wrong structure: error.
///
/// An unknown `state` (or any other value serde cannot read) makes the whole file count as
/// corrupt: losing the list (it is kept as `.broken-*`) is better than guessing a state.
/// Unknown fields are ignored.
pub fn migrate(value: Value) -> Result<TicketDoc, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| "top level is not an object".to_string())?;
    let version = match obj.get("schemaVersion") {
        None | Some(Value::Null) => 0,
        Some(v) => v
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| "schemaVersion is not a number".to_string())?,
    };
    if version > TICKETS_SCHEMA_VERSION {
        return Err(format!("unknown schemaVersion {version}"));
    }
    let tickets = obj
        .get("tickets")
        .filter(|t| t.is_array())
        .ok_or_else(|| "tickets is not a list".to_string())?;
    let tickets = serde_json::from_value(tickets.clone()).map_err(|e| e.to_string())?;
    // Review assignments (step 5) are derived state: missing or unreadable ones are dropped
    // (the tickets' reviewers are cleared at startup anyway, their agents are gone).
    let review_assignments = match obj.get("reviewAssignments") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => serde_json::from_value(v.clone()).unwrap_or_else(|e| {
            log::warn!("tickets: ignoring unreadable reviewAssignments: {e}");
            Vec::new()
        }),
    };
    Ok(TicketDoc {
        schema_version: TICKETS_SCHEMA_VERSION,
        tickets,
        review_assignments,
    })
}

impl TicketStore for JsonFileStore {
    fn load(&self) -> io::Result<LoadResult> {
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.known.store(true, Ordering::SeqCst);
                return Ok(LoadResult {
                    doc: TicketDoc::default(),
                    warning: None,
                });
            }
            // Locked, no permission, a directory, …: the file may hold tickets we could not see.
            // `known` stays false, so `save` will not replace it.
            Err(e) => return Err(e),
        };
        self.known.store(true, Ordering::SeqCst);
        let parsed = serde_json::from_slice::<Value>(&bytes)
            .map_err(|e| e.to_string())
            .and_then(migrate);
        Ok(match parsed {
            Ok(doc) => LoadResult { doc, warning: None },
            Err(reason) => LoadResult {
                doc: TicketDoc::default(),
                warning: Some(self.quarantine(&reason)),
            },
        })
    }

    /// Writes `<path>.tmp` (flushed to disk) and renames it over `<path>`.
    // TODO(windows-verify): std::fs::rename replaces an existing tickets.json on Windows
    // (MoveFileExW with MOVEFILE_REPLACE_EXISTING) and no .tmp is left behind (plan D.34).
    fn save(&self, doc: &TicketDoc) -> io::Result<()> {
        if !self.known.load(Ordering::SeqCst) && !matches!(self.path.try_exists(), Ok(false)) {
            return Err(io::Error::other(
                "tickets.json findes, men blev ikke indlæst; gemmer ikke oven i den",
            ));
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(doc).map_err(io::Error::other)?;
        let tmp = self.tmp_path();
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        self.known.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// In-memory store for tests. Clones share state, so a test can keep a handle after giving one to
/// the service.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct MemoryStore {
    inner: Arc<MemoryInner>,
}

#[derive(Default)]
struct MemoryInner {
    doc: Mutex<Option<TicketDoc>>,
    saves: AtomicUsize,
    fail_next_save: AtomicBool,
    fail_load: AtomicBool,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_doc(doc: TicketDoc) -> Self {
        let s = Self::default();
        *s.lock() = Some(doc);
        s
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<TicketDoc>> {
        self.inner.doc.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Successful saves so far.
    pub fn saves(&self) -> usize {
        self.inner.saves.load(Ordering::SeqCst)
    }

    /// The last saved (or initial) document.
    pub fn doc(&self) -> Option<TicketDoc> {
        self.lock().clone()
    }

    /// Makes every `load` fail with an I/O error (an unreadable file).
    pub fn fail_load(&self) {
        self.inner.fail_load.store(true, Ordering::SeqCst);
    }

    /// Makes the next `save` fail with an I/O error.
    pub fn fail_next_save(&self) {
        self.inner.fail_next_save.store(true, Ordering::SeqCst);
    }
}

impl TicketStore for MemoryStore {
    fn load(&self) -> io::Result<LoadResult> {
        if self.inner.fail_load.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "simulated read failure",
            ));
        }
        Ok(LoadResult {
            doc: self.doc().unwrap_or_default(),
            warning: None,
        })
    }

    fn save(&self, doc: &TicketDoc) -> io::Result<()> {
        if self.inner.fail_next_save.swap(false, Ordering::SeqCst) {
            return Err(io::Error::other("simulated save failure"));
        }
        *self.lock() = Some(doc.clone());
        self.inner.saves.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tickets::model::test_support::ticket;
    use crate::tickets::model::TicketState;
    use serde_json::json;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("mira-tickets-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn sample_doc() -> TicketDoc {
        // Step 6a: a child of the first ticket, blocked by the second.
        let mut child = ticket("33333333-0000-4000-8000-000000000000", TicketState::Backlog);
        child.parent_id = Some("11111111-0000-4000-8000-000000000000".into());
        child.blocked_by = vec!["22222222-0000-4000-8000-000000000000".into()];
        TicketDoc {
            schema_version: 1,
            tickets: vec![
                ticket("11111111-0000-4000-8000-000000000000", TicketState::Backlog),
                ticket("22222222-0000-4000-8000-000000000000", TicketState::Done),
                child,
            ],
            review_assignments: Vec::new(),
        }
    }

    #[test]
    fn missing_file_loads_empty_without_warning() {
        let dir = TempDir::new();
        let store = JsonFileStore::new(dir.0.join("tickets.json"));
        let r = store.load().unwrap();
        assert_eq!(r.doc, TicketDoc::default());
        assert_eq!(r.warning, None);
    }

    #[test]
    fn save_then_load_round_trips_and_leaves_no_tmp() {
        let dir = TempDir::new();
        let path = dir.0.join("sub").join("tickets.json");
        let store = JsonFileStore::new(path.clone());
        store.save(&sample_doc()).unwrap();
        // Saving over an existing file works too.
        store.save(&sample_doc()).unwrap();
        assert_eq!(entries(&dir.0.join("sub")), vec!["tickets.json"]);
        let r = store.load().unwrap();
        assert_eq!(r.doc, sample_doc());
        assert_eq!(r.warning, None);
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"schemaVersion\": 1"),
            "pretty camelCase: {text}"
        );
    }

    fn assert_quarantined(contents: &str) {
        let dir = TempDir::new();
        let path = dir.0.join("tickets.json");
        fs::write(&path, contents).unwrap();
        let r = JsonFileStore::new(path.clone()).load().unwrap();
        assert_eq!(r.doc, TicketDoc::default());
        let w = r.warning.expect("warning");
        assert!(
            w.starts_with("tickets.json kunne ikke læses og blev omdøbt til tickets.json.broken-"),
            "{w}"
        );
        assert!(!path.exists());
        let names = entries(&dir.0);
        assert_eq!(names.len(), 1);
        assert!(names[0].starts_with("tickets.json.broken-"));
        assert!(w.contains(&names[0]));
        assert_eq!(fs::read_to_string(dir.0.join(&names[0])).unwrap(), contents);
    }

    #[test]
    fn corrupt_json_is_renamed_and_starts_empty() {
        assert_quarantined("{ not json");
    }

    #[test]
    fn newer_schema_version_is_treated_as_corrupt() {
        assert_quarantined(r#"{"schemaVersion":99,"tickets":[]}"#);
        assert_quarantined(r#"{"schemaVersion":1,"tickets":{}}"#);
    }

    #[test]
    fn unknown_state_makes_the_whole_file_corrupt() {
        let mut v = serde_json::to_value(sample_doc()).unwrap();
        v["tickets"][1]["state"] = json!("archived");
        assert_quarantined(&v.to_string());
    }

    #[test]
    fn unknown_fields_are_ignored_and_missing_version_is_v1() {
        let mut v = serde_json::to_value(sample_doc()).unwrap();
        v["tickets"][0]["futureField"] = json!(42);
        v["extra"] = json!("x");
        v.as_object_mut().unwrap().remove("schemaVersion");
        let dir = TempDir::new();
        let path = dir.0.join("tickets.json");
        fs::write(&path, v.to_string()).unwrap();
        let r = JsonFileStore::new(path).load().unwrap();
        assert_eq!(r.warning, None);
        assert_eq!(r.doc, sample_doc());
    }

    #[test]
    fn version_1_file_without_project_loads_with_none() {
        // A step 1–5 `tickets.json`: no `project` on any ticket (plan4b punkt 6).
        let mut v = serde_json::to_value(sample_doc()).unwrap();
        for t in v["tickets"].as_array_mut().unwrap() {
            assert!(t.as_object_mut().unwrap().remove("project").is_some());
        }
        assert_eq!(v["schemaVersion"], json!(1));
        let doc = migrate(v).unwrap();
        assert!(!doc.tickets.is_empty());
        assert!(doc.tickets.iter().all(|t| t.project.is_none()));
        assert_eq!(doc, sample_doc());
    }

    #[test]
    fn version_1_file_without_relations_loads() {
        // A step 1–5 `tickets.json`: no `parentId`/`blockedBy` on any ticket (plan6a punkt 4).
        let mut v = serde_json::to_value(sample_doc()).unwrap();
        for t in v["tickets"].as_array_mut().unwrap() {
            let o = t.as_object_mut().unwrap();
            assert!(o.remove("parentId").is_some());
            assert!(o.remove("blockedBy").is_some());
        }
        let dir = TempDir::new();
        let path = dir.0.join("tickets.json");
        fs::write(&path, v.to_string()).unwrap();
        let r = JsonFileStore::new(path).load().unwrap();
        assert_eq!(r.warning, None);
        assert_eq!(r.doc.schema_version, 1);
        assert!(r
            .doc
            .tickets
            .iter()
            .all(|t| t.parent_id.is_none() && t.blocked_by.is_empty()));
        let mut want = sample_doc();
        for t in want.tickets.iter_mut() {
            t.parent_id = None;
            t.blocked_by.clear();
        }
        assert_eq!(r.doc, want);
        // The relations and a waiting ticket survive a save in this build (schema stays 1).
        let mut doc = sample_doc();
        doc.tickets[0].state = TicketState::Waiting;
        doc.tickets[0].assignee_agent_id = Some("k".into());
        let v = serde_json::to_value(&doc).unwrap();
        assert_eq!(v["schemaVersion"], json!(1));
        assert_eq!(v["tickets"][0]["state"], json!("waiting"));
        assert_eq!(v["tickets"][2]["parentId"], json!(doc.tickets[0].id));
        assert_eq!(migrate(v).unwrap(), doc);
    }

    #[test]
    fn unreadable_path_is_an_error_and_never_overwritten() {
        // A directory where the file should be: reading fails with something other than NotFound.
        let dir = TempDir::new();
        let path = dir.0.join("tickets.json");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep.txt"), "x").unwrap();
        let store = JsonFileStore::new(path.clone());
        assert!(store.load().is_err());
        assert!(store.save(&sample_doc()).is_err());
        assert!(path.is_dir());
        assert_eq!(entries(&path), vec!["keep.txt"]);
        assert_eq!(entries(&dir.0), vec!["tickets.json"]);
    }

    #[test]
    fn save_refuses_to_replace_a_file_it_never_read() {
        let dir = TempDir::new();
        let path = dir.0.join("tickets.json");
        fs::write(&path, "precious").unwrap();
        let store = JsonFileStore::new(path.clone());
        let e = store.save(&sample_doc()).unwrap_err();
        assert!(e.to_string().contains("blev ikke indlæst"), "{e}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "precious");
        assert_eq!(entries(&dir.0), vec!["tickets.json"]);
        // After a load (here: corrupt → moved aside) saving is allowed again.
        assert!(store.load().unwrap().warning.is_some());
        store.save(&sample_doc()).unwrap();
        assert_eq!(store.load().unwrap().doc, sample_doc());
    }

    #[test]
    fn memory_store_counts_saves_and_can_fail() {
        let m = MemoryStore::new();
        assert_eq!(m.load().unwrap().doc, TicketDoc::default());
        m.save(&sample_doc()).unwrap();
        assert_eq!(m.saves(), 1);
        m.fail_next_save();
        assert!(m.save(&TicketDoc::default()).is_err());
        assert_eq!(m.saves(), 1);
        assert_eq!(m.doc(), Some(sample_doc()));
        m.save(&TicketDoc::default()).unwrap();
        assert_eq!(m.clone().saves(), 2);
    }
}
