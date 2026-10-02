//! The app's own settings, `<data_dir>/app-settings.json` (plan4b A.1): currently only the
//! projects root. Read once at startup; a change (Diagnostik) takes effect after a restart.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::APP_SETTINGS_FILE;
use crate::hooks::settings::write_atomic;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct AppSettings {
    /// `None` (or blank) → `<home>/mira-bots/projects`.
    pub projects_root: Option<String>,
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
/// and a warning in the log.
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
    serde_json::from_str(&text).unwrap_or_else(|e| {
        log::warn!(
            "app settings: invalid {}: {e}; using defaults",
            path.display()
        );
        AppSettings::default()
    })
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

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("mira-app-settings-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn round_trip_in_a_temp_dir() {
        let dir = temp_dir();
        assert_eq!(load(&dir), AppSettings::default(), "missing file");
        let s = AppSettings {
            projects_root: Some(r"D:\arbejde\projekter".into()),
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
    fn invalid_file_gives_the_defaults() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(settings_path(&dir), "{ nope").unwrap();
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
}
