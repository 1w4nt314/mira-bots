//! Generation of the app's own hooks.json (passed to claude with `--settings`).
//! `~/.claude/settings.json` is never touched.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::config::{
    DEFAULT_HOOK_TIMEOUT_S, PERMISSION_HOOK_TIMEOUT_S, SESSION_END_HOOK_TIMEOUT_S,
};

/// The events mira-bots listens to. SubagentStart/Stop are left out in step 1.
pub const HOOK_EVENTS: [&str; 11] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PermissionDenied",
    "PostToolUse",
    "PostToolUseFailure",
    "Notification",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

/// hooks.json timeout in seconds for an event.
pub fn timeout_for(event: &str) -> u64 {
    match event {
        "PermissionRequest" => PERMISSION_HOOK_TIMEOUT_S,
        "SessionEnd" => SESSION_END_HOOK_TIMEOUT_S,
        _ => DEFAULT_HOOK_TIMEOUT_S,
    }
}

/// Path as written into hooks.json: lossy string with `\` replaced by `/`, no quotes.
fn command_path(hook_exe: &Path) -> String {
    hook_exe.to_string_lossy().replace('\\', "/")
}

/// Builds the hooks.json document. No `matcher` (= all), no keys other than `hooks`.
///
/// TODO(windows-verify): exec-form hooks (`command` + `args`) with a forward-slash path must work
/// with the installed Claude Code version and fire for the `--settings` file. UNVERIFIED: from which
/// Claude Code version `args` is supported.
pub fn render_hooks_json(hook_exe: &Path) -> Value {
    let command = command_path(hook_exe);
    let mut hooks = Map::new();
    for event in HOOK_EVENTS {
        hooks.insert(
            event.to_string(),
            json!([{
                "hooks": [{
                    "type": "command",
                    "command": command,
                    "args": [event],
                    "timeout": timeout_for(event),
                }]
            }]),
        );
    }
    json!({ "hooks": Value::Object(hooks) })
}

/// Writes `dir/hooks.json` atomically (temp file + rename) and returns its path.
pub fn write_hooks_json(dir: &Path, hook_exe: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let target = dir.join("hooks.json");
    let tmp = dir.join(format!("hooks.json.{}.tmp", std::process::id()));
    let body = serde_json::to_string_pretty(&render_hooks_json(hook_exe))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    fs::write(&tmp, body)?;
    if let Err(e) = fs::rename(&tmp, &target) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_all_eleven_events() {
        let v = render_hooks_json(Path::new("/opt/mira-hook"));
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 11);
        for e in HOOK_EVENTS {
            assert!(hooks.contains_key(e), "missing {e}");
        }
        assert_eq!(v.as_object().unwrap().len(), 1, "only the hooks key");
    }

    #[test]
    fn entries_are_exec_form_commands_with_event_arg() {
        let v = render_hooks_json(Path::new("/opt/mira-hook"));
        for e in HOOK_EVENTS {
            let groups = v["hooks"][e].as_array().unwrap();
            assert_eq!(groups.len(), 1);
            assert!(groups[0].get("matcher").is_none());
            let hooks = groups[0]["hooks"].as_array().unwrap();
            assert_eq!(hooks.len(), 1);
            assert_eq!(hooks[0]["type"], "command");
            assert_eq!(hooks[0]["command"], "/opt/mira-hook");
            assert_eq!(hooks[0]["args"], json!([e]));
        }
    }

    #[test]
    fn timeouts_are_correct() {
        let v = render_hooks_json(Path::new("/x"));
        for e in HOOK_EVENTS {
            let want = match e {
                "PermissionRequest" => 120,
                "SessionEnd" => 1,
                _ => 10,
            };
            assert_eq!(v["hooks"][e][0]["hooks"][0]["timeout"], want, "{e}");
        }
    }

    #[test]
    fn windows_path_uses_forward_slashes_without_quotes() {
        let v = render_hooks_json(Path::new(
            r"C:\Program Files\mira-bots\resources\mira-hook.exe",
        ));
        let cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert_eq!(cmd, "C:/Program Files/mira-bots/resources/mira-hook.exe");
        assert!(!cmd.contains('"') && !cmd.contains('\\'));
    }

    #[test]
    fn write_creates_dir_and_file_atomically() {
        let base = std::env::temp_dir().join(format!("mira-bots-test-{}", uuid::Uuid::new_v4()));
        let dir = base.join("nested");
        let path = write_hooks_json(&dir, Path::new("/opt/mira-hook")).unwrap();
        assert_eq!(path, dir.join("hooks.json"));
        let read: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read, render_hooks_json(Path::new("/opt/mira-hook")));

        // Second write overwrites and leaves no temp file behind.
        write_hooks_json(&dir, Path::new("/other/mira-hook")).unwrap();
        let read: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            read["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/other/mira-hook"
        );
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec!["hooks.json".to_string()]);
        fs::remove_dir_all(&base).unwrap();
    }
}
