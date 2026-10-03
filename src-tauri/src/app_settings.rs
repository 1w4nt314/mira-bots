//! The app's own settings, `<data_dir>/app-settings.json` (plan4b A.1, 6d A.7): the projects
//! root (applies after a restart) and the watch/notice preferences (live, step 6d). Read once at
//! startup into `AppState.settings`; every change goes through `AppState::update_settings`
//! (read-modify-write + [`save`]), so no field is ever overwritten with its default by another.

use std::io;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::APP_SETTINGS_FILE;
use crate::hooks::settings::write_atomic;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    /// `None` (or blank) → `<home>/mira-bots/projects`.
    pub projects_root: Option<String>,
    /// Step 6d (A.7): the watch is paused for every project ("Stop vagten").
    #[serde(default)]
    pub watch_paused: bool,
    /// Step 6d (A.7): project ids whose watch is paused ("Hold vagt"); the user's `project.json`
    /// is never written.
    #[serde(default)]
    pub watch_off: Vec<String>,
    /// Step 6d (A.8): notice kinds (camelCase names) the user opted out of.
    #[serde(default)]
    pub notify_off: Vec<String>,
}

impl AppSettings {
    /// The configured projects root, when set and not blank.
    pub fn projects_root(&self) -> Option<PathBuf> {
        self.projects_root
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    }
}

/// `<data_dir>/app-settings.json`.
pub fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join(APP_SETTINGS_FILE)
}

/// Reads the settings; a missing file gives the defaults, an unreadable/invalid one the defaults
/// and a warning in the log. Field by field (6d A.7): a field of the wrong type gets its default
/// and a warning, the other fields keep their values.
pub fn load(data_dir: &Path) -> AppSettings {
    let path = settings_path(data_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return AppSettings::default(),
        Err(e) => {
            log::warn!("app settings: cannot read {}: {e}", path.display());
            return AppSettings::default();
        }
    };
    parse(&text).unwrap_or_else(|e| {
        log::warn!(
            "app settings: invalid {}: {e}; using defaults",
            path.display()
        );
        AppSettings::default()
    })
}

/// [`load`] without the file: `Err` only when `text` is not a JSON object; a bad field is a
/// warning and that field's default.
pub fn parse(text: &str) -> Result<AppSettings, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let Value::Object(mut map) = value else {
        return Err("skal være et JSON-objekt".into());
    };
    Ok(AppSettings {
        projects_root: field(&mut map, "projectsRoot"),
        watch_paused: field(&mut map, "watchPaused"),
        watch_off: field(&mut map, "watchOff"),
        notify_off: field(&mut map, "notifyOff"),
    })
}

/// One field of the settings object: missing → default; wrong type → warning + default.
fn field<T: DeserializeOwned + Default>(map: &mut serde_json::Map<String, Value>, key: &str) -> T {
    match map.remove(key) {
        None => T::default(),
        Some(v) => serde_json::from_value(v).unwrap_or_else(|e| {
            log::warn!("app settings: {key} ignoreres ({e}); standardværdien bruges");
            T::default()
        }),
    }
}

/// Writes the settings atomically and returns the file's path.
pub fn save(data_dir: &Path, settings: &AppSettings) -> io::Result<PathBuf> {
    let path = settings_path(data_dir);
    let body = serde_json::to_string_pretty(settings).map_err(io::Error::other)?;
    write_atomic(&path, &body)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("mira-app-settings-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn round_trip_in_a_temp_dir() {
        let dir = temp_dir();
        assert_eq!(load(&dir), AppSettings::default(), "missing file");
        let s = AppSettings {
            projects_root: Some(r"D:\arbejde\projekter".into()),
            ..AppSettings::default()
        };
        let path = save(&dir, &s).unwrap();
        assert_eq!(path, dir.join("app-settings.json"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"projectsRoot\""), "{text}");
        assert_eq!(load(&dir), s);
        assert_eq!(
            s.projects_root(),
            Some(PathBuf::from(r"D:\arbejde\projekter"))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn watch_fields_are_camel_case_and_round_trip() {
        let s = AppSettings {
            projects_root: None,
            watch_paused: true,
            watch_off: vec!["shop".into()],
            notify_off: vec!["escalated".into(), "budgetReached".into()],
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(
            v,
            json!({
                "projectsRoot": null,
                "watchPaused": true,
                "watchOff": ["shop"],
                "notifyOff": ["escalated", "budgetReached"]
            })
        );
        assert_eq!(parse(&v.to_string()).unwrap(), s);
        // An older file (before 6d) has only the root: the new fields default.
        let old = parse(r#"{"projectsRoot": "/r"}"#).unwrap();
        assert_eq!(
            old,
            AppSettings {
                projects_root: Some("/r".into()),
                ..AppSettings::default()
            }
        );
        let dir = temp_dir();
        save(&dir, &s).unwrap();
        assert_eq!(load(&dir), s);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn invalid_file_gives_the_defaults() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(settings_path(&dir), "{ nope").unwrap();
        assert_eq!(load(&dir), AppSettings::default());
        std::fs::write(settings_path(&dir), "[1, 2]").unwrap();
        assert_eq!(load(&dir), AppSettings::default());
        std::fs::write(settings_path(&dir), r#"{"projectsRoot": 5}"#).unwrap();
        assert_eq!(load(&dir), AppSettings::default());
        // Unknown keys are ignored; a blank root counts as unset.
        std::fs::write(
            settings_path(&dir),
            r#"{"projectsRoot": "  ", "other": true}"#,
        )
        .unwrap();
        let s = load(&dir);
        assert_eq!(s.projects_root.as_deref(), Some("  "));
        assert_eq!(s.projects_root(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_tolerates_bad_field() {
        // 6d A.7: an invalid `watchPaused` does not reset `projectsRoot` (and the other way
        // round); only that field falls back to its default.
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            settings_path(&dir),
            r#"{"projectsRoot": "/r", "watchPaused": "ja", "watchOff": ["a"], "notifyOff": 7}"#,
        )
        .unwrap();
        assert_eq!(
            load(&dir),
            AppSettings {
                projects_root: Some("/r".into()),
                watch_paused: false,
                watch_off: vec!["a".into()],
                notify_off: Vec::new(),
            }
        );
        std::fs::write(
            settings_path(&dir),
            r#"{"projectsRoot": ["x"], "watchPaused": true, "watchOff": [1], "notifyOff": null}"#,
        )
        .unwrap();
        assert_eq!(
            load(&dir),
            AppSettings {
                projects_root: None,
                watch_paused: true,
                watch_off: Vec::new(),
                notify_off: Vec::new(),
            }
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
