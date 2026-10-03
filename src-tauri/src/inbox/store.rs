//! Persistence of the inbox document: [`JsonInboxStore`] (`inbox.json`, atomic writes,
//! quarantine of an unreadable file) and [`MemoryInboxStore`] for tests. Same pattern as
//! `tickets::store`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::{InboxDoc, InboxService};
use crate::agent::now_ms;
use crate::config::INBOX_SCHEMA_VERSION;
use crate::tickets::store::suffixed;

/// Result of [`InboxStore::load`]; `warning` goes to Diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxLoad {
    pub doc: InboxDoc,
    pub warning: Option<String>,
}

/// Loads and saves the whole inbox document.
pub trait InboxStore: Send {
    fn load(&self) -> io::Result<InboxLoad>;
    fn save(&self, doc: &InboxDoc) -> io::Result<()>;
}

/// `inbox.json` in the app data dir.
#[derive(Clone, Debug)]
pub struct JsonInboxStore {
    path: PathBuf,
    /// Set once this store knows what is at `path` (a successful `load` or its own `save`);
    /// until then `save` never replaces an existing file.
    known: Arc<AtomicBool>,
}

impl JsonInboxStore {
    pub fn new(path: PathBuf) -> Self {
        JsonInboxStore {
            path,
            known: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
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
            log::error!("rename {} -> {name}: {e}", self.path.display());
        }
        format!("inbox.json kunne ikke læses og blev omdøbt til {name}; starter med tom indbakke")
    }
}

/// Brings a parsed file up to the current schema: version 1 as is; missing or 0 counts as 1
/// when `items` is a list; a newer version, a wrong structure or any value serde cannot read
/// (an unknown `state` or `kind`) is an error, and the file is quarantined. Unknown fields are
/// ignored.
pub fn migrate(value: Value) -> Result<InboxDoc, String> {
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
    if version > INBOX_SCHEMA_VERSION {
        return Err(format!("unknown schemaVersion {version}"));
    }
    let items = obj
        .get("items")
        .filter(|t| t.is_array())
        .ok_or_else(|| "items is not a list".to_string())?;
    let items = serde_json::from_value(items.clone()).map_err(|e| e.to_string())?;
    Ok(InboxDoc {
        schema_version: INBOX_SCHEMA_VERSION,
        items,
    })
}

impl InboxStore for JsonInboxStore {
    fn load(&self) -> io::Result<InboxLoad> {
        let bytes = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.known.store(true, Ordering::SeqCst);
                return Ok(InboxLoad {
                    doc: InboxDoc::default(),
                    warning: None,
                });
            }
            Err(e) => return Err(e),
        };
        self.known.store(true, Ordering::SeqCst);
        let parsed = serde_json::from_slice::<Value>(&bytes)
            .map_err(|e| e.to_string())
            .and_then(migrate);
        Ok(match parsed {
            Ok(doc) => InboxLoad { doc, warning: None },
            Err(reason) => InboxLoad {
                doc: InboxDoc::default(),
                warning: Some(self.quarantine(&reason)),
            },
        })
    }

    /// Writes `<path>.tmp` (flushed) and renames it over `<path>`.
    // TODO(windows-verify): rename replaces an existing inbox.json and leaves no .tmp (as
    // tickets.json, plan D.34).
    fn save(&self, doc: &InboxDoc) -> io::Result<()> {
        if !self.known.load(Ordering::SeqCst) && !matches!(self.path.try_exists(), Ok(false)) {
            return Err(io::Error::other(
                "inbox.json findes, men blev ikke indlæst; gemmer ikke oven i den",
            ));
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(doc).map_err(io::Error::other)?;
        let tmp = suffixed(&self.path, ".tmp");
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

/// Startup: loads `path` into an [`InboxService`] (warning: quarantined or unreadable file).
pub fn load_inbox(path: PathBuf) -> (InboxService, Option<String>) {
    let (service, warning) = InboxService::load(Box::new(JsonInboxStore::new(path.clone())));
    log::info!(
        "inbox: {} items loaded from {}",
        service.len(),
        path.display()
    );
    if let Some(w) = &warning {
        log::warn!("inbox: {w}");
    }
    (service, warning)
}

/// In-memory store for tests; clones share state.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct MemoryInboxStore {
    inner: Arc<MemoryInner>,
}

#[derive(Default)]
struct MemoryInner {
    doc: Mutex<Option<InboxDoc>>,
    saves: AtomicUsize,
    fail_next_save: AtomicBool,
    fail_load: AtomicBool,
}

impl MemoryInboxStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_doc(doc: InboxDoc) -> Self {
        let s = Self::default();
        *s.lock() = Some(doc);
        s
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<InboxDoc>> {
        self.inner.doc.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Successful saves so far.
    pub fn saves(&self) -> usize {
        self.inner.saves.load(Ordering::SeqCst)
    }

    /// The last saved (or initial) document.
    pub fn doc(&self) -> Option<InboxDoc> {
        self.lock().clone()
    }

    /// Makes every `load` fail with an I/O error.
    pub fn fail_load(&self) {
        self.inner.fail_load.store(true, Ordering::SeqCst);
    }

    /// Makes the next `save` fail.
    pub fn fail_next_save(&self) {
        self.inner.fail_next_save.store(true, Ordering::SeqCst);
    }
}

impl InboxStore for MemoryInboxStore {
    fn load(&self) -> io::Result<InboxLoad> {
        if self.inner.fail_load.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "simulated read failure",
            ));
        }
        Ok(InboxLoad {
            doc: self.doc().unwrap_or_default(),
            warning: None,
        })
    }

    fn save(&self, doc: &InboxDoc) -> io::Result<()> {
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
    use super::super::test_support::{folder_item, github_item};
    use super::*;
    use serde_json::json;

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

    fn entries(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn sample_doc() -> InboxDoc {
        let mut started = folder_item("u2", "fejl-1.md");
        started.state = super::super::InboxState::Started;
        started.ticket_id = Some("t1".into());
        started.moved = Some(false);
        InboxDoc {
            schema_version: 1,
            items: vec![github_item("u1", 7), started],
        }
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = TempDir::new();
        let r = JsonInboxStore::new(dir.0.join("inbox.json"))
            .load()
            .unwrap();
        assert_eq!((r.doc, r.warning), (InboxDoc::default(), None));
    }

    #[test]
    fn save_then_load_round_trips_and_leaves_no_tmp() {
        let dir = TempDir::new();
        let path = dir.0.join("sub").join("inbox.json");
        let store = JsonInboxStore::new(path.clone());
        store.save(&sample_doc()).unwrap();
        store.save(&sample_doc()).unwrap();
        assert_eq!(entries(&dir.0.join("sub")), vec!["inbox.json"]);
        let r = store.load().unwrap();
        assert_eq!((r.doc, r.warning), (sample_doc(), None));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"schemaVersion\": 1"), "{text}");
        assert!(text.contains("\"externalId\": \"github:o/r#7\""), "{text}");
    }

    fn assert_quarantined(contents: &str) {
        let dir = TempDir::new();
        let path = dir.0.join("inbox.json");
        fs::write(&path, contents).unwrap();
        let r = JsonInboxStore::new(path.clone()).load().unwrap();
        assert_eq!(r.doc, InboxDoc::default());
        let w = r.warning.expect("warning");
        assert!(
            w.starts_with("inbox.json kunne ikke læses og blev omdøbt til inbox.json.broken-"),
            "{w}"
        );
        assert!(w.ends_with("; starter med tom indbakke"), "{w}");
        assert!(!path.exists());
        let names = entries(&dir.0);
        assert_eq!(names.len(), 1);
        assert!(names[0].starts_with("inbox.json.broken-"));
        assert!(w.contains(&names[0]));
        assert_eq!(fs::read_to_string(dir.0.join(&names[0])).unwrap(), contents);
    }

    #[test]
    fn quarantine_on_corrupt_file() {
        assert_quarantined("{ not json");
        assert_quarantined(r#"{"schemaVersion":1,"items":{}}"#);
        assert_quarantined("[]");
    }

    #[test]
    fn unknown_version_is_quarantined() {
        assert_quarantined(r#"{"schemaVersion":2,"items":[]}"#);
        assert_quarantined(r#"{"schemaVersion":"1","items":[]}"#);
    }

    #[test]
    fn unknown_enum_value_is_quarantined() {
        let mut v = serde_json::to_value(sample_doc()).unwrap();
        v["items"][0]["state"] = json!("archived");
        assert_quarantined(&v.to_string());
        let mut v = serde_json::to_value(sample_doc()).unwrap();
        v["items"][1]["kind"] = json!("rubrik");
        assert_quarantined(&v.to_string());
    }

    #[test]
    fn unknown_fields_are_ignored_and_missing_version_is_v1() {
        let mut v = serde_json::to_value(sample_doc()).unwrap();
        v["items"][0]["futureField"] = json!(1);
        v.as_object_mut().unwrap().remove("schemaVersion");
        assert_eq!(migrate(v).unwrap(), sample_doc());
        // Optional fields may be absent.
        let min = json!({"schemaVersion":1,"items":[{"id":"u","kind":"folder",
            "externalId":"folder:_rod:a.md","sourceId":"folder:_rod","title":"T","seenAt":1,
            "state":"dismissed"}]});
        let doc = migrate(min).unwrap();
        let i = &doc.items[0];
        assert_eq!((i.gone, i.moved, i.body.as_deref()), (false, None, None));
        assert!(i.labels.is_empty() && i.notes.is_empty());
    }

    #[test]
    fn unreadable_path_is_an_error_and_never_overwritten() {
        let dir = TempDir::new();
        let path = dir.0.join("inbox.json");
        fs::create_dir(&path).unwrap();
        let store = JsonInboxStore::new(path.clone());
        assert!(store.load().is_err());
        assert!(store.save(&sample_doc()).is_err());
        assert!(path.is_dir());
    }

    #[test]
    fn save_refuses_to_replace_a_file_it_never_read() {
        let dir = TempDir::new();
        let path = dir.0.join("inbox.json");
        fs::write(&path, "precious").unwrap();
        let store = JsonInboxStore::new(path.clone());
        let e = store.save(&sample_doc()).unwrap_err();
        assert!(e.to_string().contains("blev ikke indlæst"), "{e}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "precious");
    }

    #[test]
    fn load_inbox_reports_the_warning_and_starts_empty() {
        let dir = TempDir::new();
        let path = dir.0.join("inbox.json");
        fs::write(&path, "nope").unwrap();
        let (svc, w) = load_inbox(path.clone());
        assert!(w.unwrap().contains("inbox.json.broken-"));
        assert_eq!(svc.len(), 0);
        let (svc, w) = load_inbox(dir.0.join("missing.json"));
        assert_eq!((svc.len(), w), (0, None));
    }
}
