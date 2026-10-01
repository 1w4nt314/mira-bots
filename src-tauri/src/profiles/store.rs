//! `ProfileStore`: one JSON file per profile in `<agents_root>/.mira-bots/profiles/<id>.json`
//! (pretty, UTF-8, written atomically). Missing built-in profiles are generated on load.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::model::{builtin_profile, is_builtin_id, kind_for_id, AgentProfile, ProfileError};
use crate::config::{BUILTIN_PROFILE_IDS, PROFILES_DIR};
use crate::hooks::settings::write_atomic;

/// `<agents_root>/.mira-bots/profiles` (joined component by component).
pub fn profiles_dir(agents_root: &Path) -> PathBuf {
    PROFILES_DIR
        .split('/')
        .fold(agents_root.to_path_buf(), |p, c| p.join(c))
}

pub struct ProfileStore {
    dir: PathBuf,
    profiles: BTreeMap<String, AgentProfile>,
    /// Set by [`ProfileStore::load`] when files were broken or the folder could not be used.
    warning: Option<String>,
}

fn file_for(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn write_profile(dir: &Path, p: &AgentProfile) -> Result<(), ProfileError> {
    let body = serde_json::to_string_pretty(p).map_err(|e| ProfileError::Io(e.to_string()))?;
    write_atomic(&file_for(dir, &p.id), &body).map_err(|e| ProfileError::Io(e.to_string()))
}

/// Parses and validates one profile file whose stem is `stem`.
fn read_profile(path: &Path, stem: &str) -> Result<AgentProfile, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let p: AgentProfile = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    if p.id != stem {
        return Err(format!("id {:?} matcher ikke filnavnet", p.id));
    }
    p.validated().map_err(|e| e.to_string())
}

impl ProfileStore {
    /// Loads every `*.json` in `dir` (created if needed). A file that cannot be parsed or
    /// validated is renamed to `<name>.broken-<now>` and counted in the warning; missing built-in
    /// profiles are generated and written. A file with a built-in id always gets `kind: builtin`.
    /// Never fails: without a usable folder the built-ins live in memory only (warning set).
    // TODO(windows-verify): on first start %USERPROFILE%\mira-bots\agents\.mira-bots\profiles\
    // holds the seven built-in profiles (plan5 D.49).
    pub fn load(dir: PathBuf, now: u64) -> Self {
        let mut store = Self {
            dir,
            profiles: BTreeMap::new(),
            warning: None,
        };
        let mut problems: Vec<String> = Vec::new();
        if let Err(e) = fs::create_dir_all(&store.dir) {
            problems.push(format!("Profilmappen kunne ikke oprettes: {e}"));
        }
        let mut broken = 0usize;
        if let Ok(entries) = fs::read_dir(&store.dir) {
            let mut paths: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
                .collect();
            paths.sort();
            for path in paths {
                let stem = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match read_profile(&path, &stem) {
                    Ok(p) => {
                        store.profiles.insert(p.id.clone(), p);
                    }
                    Err(e) => {
                        broken += 1;
                        log::warn!("profile file {} is broken: {e}", path.display());
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let target = path.with_file_name(format!("{name}.broken-{now}"));
                        if let Err(e) = fs::rename(&path, &target) {
                            log::warn!("could not rename {}: {e}", path.display());
                        }
                    }
                }
            }
        }
        if broken > 0 {
            problems.push(format!(
                "{broken} profilfil(er) kunne ikke læses og er omdøbt til .broken-{now}"
            ));
        }
        for id in BUILTIN_PROFILE_IDS {
            if store.profiles.contains_key(id) {
                continue;
            }
            let Some(p) = builtin_profile(id) else {
                continue;
            };
            if let Err(e) = write_profile(&store.dir, &p) {
                log::warn!("could not write built-in profile {id}: {e}");
            }
            store.profiles.insert(p.id.clone(), p);
        }
        if !problems.is_empty() {
            store.warning = Some(problems.join("; "));
        }
        store
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<AgentProfile> {
        self.profiles.get(id).cloned()
    }

    /// Built-in profiles first in [`BUILTIN_PROFILE_IDS`] order, then the custom ones by name
    /// (case-insensitive), then id.
    pub fn list(&self) -> Vec<AgentProfile> {
        let mut builtins: Vec<AgentProfile> = BUILTIN_PROFILE_IDS
            .iter()
            .filter_map(|id| self.profiles.get(*id).cloned())
            .collect();
        let mut custom: Vec<AgentProfile> = self
            .profiles
            .values()
            .filter(|p| !is_builtin_id(&p.id))
            .cloned()
            .collect();
        custom.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        builtins.append(&mut custom);
        builtins
    }

    /// Validates (C5.6), sets `kind` from the id and `updatedAt = now`, writes the file and keeps
    /// the profile. The caller gives new profiles their `custom-<8 hex>` id first.
    pub fn save(&mut self, profile: AgentProfile, now: u64) -> Result<AgentProfile, ProfileError> {
        let mut p = profile.validated()?;
        p.kind = kind_for_id(&p.id);
        p.updated_at = now;
        write_profile(&self.dir, &p)?;
        self.profiles.insert(p.id.clone(), p.clone());
        Ok(p)
    }

    /// Only custom profiles; the file is removed (a missing file is fine).
    pub fn delete(&mut self, id: &str) -> Result<(), ProfileError> {
        if is_builtin_id(id) {
            return Err(ProfileError::BuiltinNotDeletable);
        }
        if !self.profiles.contains_key(id) {
            return Err(ProfileError::NotFound);
        }
        match fs::remove_file(file_for(&self.dir, id)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(ProfileError::Io(e.to_string())),
        }
        self.profiles.remove(id);
        Ok(())
    }

    /// Writes the built-in default of `id` again (`updatedAt = now`).
    pub fn reset_builtin(&mut self, id: &str, now: u64) -> Result<AgentProfile, ProfileError> {
        let mut p = builtin_profile(id).ok_or(ProfileError::NotBuiltin)?;
        p.updated_at = now;
        write_profile(&self.dir, &p)?;
        self.profiles.insert(p.id.clone(), p.clone());
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::roles::Role;
    use crate::profiles::model::ProfileKind;

    fn temp_dir() -> PathBuf {
        profiles_dir(
            &std::env::temp_dir()
                .join(format!("mira-profiles-{}", uuid::Uuid::new_v4()))
                .join("agents"),
        )
    }

    fn files(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    fn cleanup(dir: &Path) {
        // <tmp>/mira-profiles-<uuid>/agents/.mira-bots/profiles → remove <tmp>/mira-profiles-<uuid>.
        let base = dir.ancestors().nth(3).unwrap();
        fs::remove_dir_all(base).unwrap();
    }

    fn custom(id: &str, name: &str) -> AgentProfile {
        AgentProfile {
            id: id.into(),
            name: name.into(),
            roles: vec![Role::Planner, Role::Debugger],
            ..builtin_profile("coder").unwrap()
        }
    }

    #[test]
    fn profiles_dir_is_under_the_agents_root() {
        assert_eq!(
            profiles_dir(Path::new("/h/mira-bots/agents")),
            Path::new("/h/mira-bots/agents/.mira-bots/profiles")
        );
    }

    #[test]
    fn first_load_generates_seven_builtins() {
        let dir = temp_dir();
        let s = ProfileStore::load(dir.clone(), 5);
        assert_eq!(s.len(), 7);
        assert_eq!(s.warning(), None);
        let mut want: Vec<String> = BUILTIN_PROFILE_IDS
            .iter()
            .map(|id| format!("{id}.json"))
            .collect();
        want.sort();
        assert_eq!(files(&dir), want);
        let text = fs::read_to_string(dir.join("reviewer.json")).unwrap();
        assert!(
            text.contains("\n  \"name\": \"Reviewer\""),
            "pretty: {text}"
        );
        let read: AgentProfile = serde_json::from_str(&text).unwrap();
        assert_eq!(read, builtin_profile("reviewer").unwrap());
        // A second load reads the files instead of regenerating them.
        let s2 = ProfileStore::load(dir.clone(), 6);
        assert_eq!(s2.list(), s.list());
        cleanup(&dir);
    }

    #[test]
    fn save_custom_roundtrip() {
        let dir = temp_dir();
        let mut s = ProfileStore::load(dir.clone(), 1);
        let saved = s.save(custom("custom-0000abcd", " Mix "), 42).unwrap();
        assert_eq!(saved.name, "Mix");
        assert_eq!(saved.kind, ProfileKind::Custom);
        assert_eq!(saved.updated_at, 42);
        assert_eq!(s.get("custom-0000abcd"), Some(saved.clone()));
        assert!(dir.join("custom-0000abcd.json").is_file());
        let again = ProfileStore::load(dir.clone(), 2);
        assert_eq!(again.get("custom-0000abcd"), Some(saved));
        assert_eq!(again.len(), 8);
        // Invalid profiles are refused and nothing is written.
        let bad = AgentProfile {
            model: Some("bogus".into()),
            ..custom("custom-0000ffff", "x")
        };
        assert_eq!(s.save(bad, 3), Err(ProfileError::UnknownModel));
        assert!(!dir.join("custom-0000ffff.json").exists());
        // A custom profile can be deleted.
        s.delete("custom-0000abcd").unwrap();
        assert!(s.get("custom-0000abcd").is_none());
        assert!(!dir.join("custom-0000abcd.json").exists());
        assert_eq!(s.delete("custom-0000abcd"), Err(ProfileError::NotFound));
        cleanup(&dir);
    }

    #[test]
    fn delete_builtin_refused() {
        let dir = temp_dir();
        let mut s = ProfileStore::load(dir.clone(), 1);
        assert_eq!(s.delete("reviewer"), Err(ProfileError::BuiltinNotDeletable));
        assert_eq!(
            ProfileError::BuiltinNotDeletable.to_string(),
            "Indbyggede profiler kan ikke slettes; brug Nulstil"
        );
        assert!(s.get("reviewer").is_some());
        assert!(dir.join("reviewer.json").is_file());
        cleanup(&dir);
    }

    #[test]
    fn reset_builtin_restores_defaults() {
        let dir = temp_dir();
        let mut s = ProfileStore::load(dir.clone(), 1);
        let edited = AgentProfile {
            name: "Min reviewer".into(),
            model: Some("opus".into()),
            ..s.get("reviewer").unwrap()
        };
        s.save(edited, 10).unwrap();
        assert_eq!(
            ProfileStore::load(dir.clone(), 2)
                .get("reviewer")
                .unwrap()
                .name,
            "Min reviewer"
        );
        let reset = s.reset_builtin("reviewer", 20).unwrap();
        assert_eq!(
            reset,
            AgentProfile {
                updated_at: 20,
                ..builtin_profile("reviewer").unwrap()
            }
        );
        assert_eq!(
            ProfileStore::load(dir.clone(), 3).get("reviewer"),
            Some(reset)
        );
        s.save(custom("custom-0000abcd", "Mix"), 4).unwrap();
        assert_eq!(
            s.reset_builtin("custom-0000abcd", 5),
            Err(ProfileError::NotBuiltin)
        );
        assert_eq!(
            ProfileError::NotBuiltin.to_string(),
            "Kun indbyggede profiler kan nulstilles"
        );
        cleanup(&dir);
    }

    #[test]
    fn broken_file_is_renamed_and_reported() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("custom-0000dead.json"), "{ not json").unwrap();
        // Valid JSON, invalid profile (bad model).
        let mut bad = serde_json::to_value(custom("custom-0000beef", "x")).unwrap();
        bad["model"] = "bogus".into();
        fs::write(dir.join("custom-0000beef.json"), bad.to_string()).unwrap();
        // The id must match the file name.
        let other = serde_json::to_string(&custom("custom-0000aaaa", "x")).unwrap();
        fs::write(dir.join("custom-0000bbbb.json"), other).unwrap();
        // A broken built-in is regenerated.
        fs::write(dir.join("coder.json"), "[]").unwrap();
        let s = ProfileStore::load(dir.clone(), 77);
        assert_eq!(s.len(), 7);
        let w = s.warning().unwrap();
        assert!(
            w.contains("4 profilfil(er)") && w.contains(".broken-77"),
            "{w}"
        );
        let names = files(&dir);
        for n in [
            "coder.json.broken-77",
            "custom-0000dead.json.broken-77",
            "custom-0000beef.json.broken-77",
            "custom-0000bbbb.json.broken-77",
            "coder.json",
        ] {
            assert!(names.contains(&n.to_string()), "{n} in {names:?}");
        }
        assert_eq!(s.get("coder"), builtin_profile("coder"));
        cleanup(&dir);
    }

    #[test]
    fn builtin_id_forces_builtin_kind() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).unwrap();
        let mut p = serde_json::to_value(builtin_profile("planner").unwrap()).unwrap();
        p["kind"] = "custom".into();
        p["name"] = "Min planlægger".into();
        fs::write(dir.join("planner.json"), p.to_string()).unwrap();
        let mut c = serde_json::to_value(custom("custom-00001111", "C")).unwrap();
        c["kind"] = "builtin".into();
        fs::write(dir.join("custom-00001111.json"), c.to_string()).unwrap();
        let mut s = ProfileStore::load(dir.clone(), 1);
        let planner = s.get("planner").unwrap();
        assert_eq!(
            (planner.kind, planner.name.as_str()),
            (ProfileKind::Builtin, "Min planlægger")
        );
        assert_eq!(s.get("custom-00001111").unwrap().kind, ProfileKind::Custom);
        let saved = s
            .save(
                AgentProfile {
                    kind: ProfileKind::Custom,
                    ..planner
                },
                2,
            )
            .unwrap();
        assert_eq!(saved.kind, ProfileKind::Builtin);
        cleanup(&dir);
    }

    #[test]
    fn list_order() {
        let dir = temp_dir();
        let mut s = ProfileStore::load(dir.clone(), 1);
        s.save(custom("custom-00000002", "beta"), 1).unwrap();
        s.save(custom("custom-00000001", "Alfa"), 1).unwrap();
        s.save(custom("custom-00000003", "alfa"), 1).unwrap();
        let ids: Vec<String> = s.list().into_iter().map(|p| p.id).collect();
        let mut want: Vec<String> = BUILTIN_PROFILE_IDS.iter().map(|s| s.to_string()).collect();
        want.extend(["custom-00000001", "custom-00000003", "custom-00000002"].map(String::from));
        assert_eq!(ids, want);
        cleanup(&dir);
    }
}
