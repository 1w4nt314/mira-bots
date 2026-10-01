//! Generation of the app's own Claude Code settings file `settings.json` (passed to claude with
//! `--settings`): the hooks plus `permissions.allow` for the app's own MCP tools. Up to step 3
//! the file was `hooks.json` with only the hooks; writing settings.json removes that file.
//! `~/.claude/settings.json` is never touched.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::config::{
    DEFAULT_HOOK_TIMEOUT_S, LEGACY_HOOKS_FILE, MCP_TOOL_PREFIX, PERMISSION_HOOK_TIMEOUT_S,
    SESSION_END_HOOK_TIMEOUT_S, SETTINGS_FILE,
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

/// Hook timeout in seconds for an event.
pub fn timeout_for(event: &str) -> u64 {
    match event {
        "PermissionRequest" => PERMISSION_HOOK_TIMEOUT_S,
        "SessionEnd" => SESSION_END_HOOK_TIMEOUT_S,
        _ => DEFAULT_HOOK_TIMEOUT_S,
    }
}

/// Path as written into settings.json/mcp.json: lossy string with `\` replaced by `/`, no quotes.
pub(crate) fn command_path(exe: &Path) -> String {
    exe.to_string_lossy().replace('\\', "/")
}

/// The `hooks` value of settings.json (unchanged since step 1). No `matcher` (= all).
///
/// TODO(windows-verify): exec-form hooks (`command` + `args`) with a forward-slash path must work
/// with the installed Claude Code version and fire for the `--settings` file. UNVERIFIED: from which
/// Claude Code version `args` is supported.
pub fn render_hooks(hook_exe: &Path) -> Value {
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
    Value::Object(hooks)
}

/// The one allow rule: every tool of the app's own MCP server (`mcp__mira-bots__*`).
pub fn allow_rule() -> String {
    format!("{MCP_TOOL_PREFIX}*")
}

/// Builds the settings.json document: exactly the keys `hooks` and `permissions`, the latter
/// with exactly one allow rule. Claude Code merges `permissions.allow` with the user's own rules
/// (research4 Q2), so nothing of the user's is overridden.
///
/// TODO(windows-verify): no permission prompt for `mcp__mira-bots__*` thanks to this rule, and
/// the user's own MCP servers and allow rules still work (plan4 D.41).
pub fn render_settings_json(hook_exe: &Path) -> Value {
    json!({
        "hooks": render_hooks(hook_exe),
        "permissions": { "allow": [allow_rule()] },
    })
}

/// Writes `target` atomically: a temp file in the same directory, then rename. The directory is
/// created if needed; the temp file never survives a failure.
pub(crate) fn write_atomic(target: &Path, body: &str) -> io::Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path without a directory"))?;
    fs::create_dir_all(dir)?;
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
    if let Err(e) = fs::write(&tmp, body) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Writes `dir/settings.json` atomically and returns its path, then removes a step-3
/// `dir/hooks.json` if one is left (a failure there is only logged).
///
/// TODO(windows-verify): after an upgrade, hooks.json is gone and settings.json, mcp.json and
/// system-prompt.md exist in `%APPDATA%\dk.mira.bots\` (plan4 D.48).
pub fn write_settings_json(dir: &Path, hook_exe: &Path) -> io::Result<PathBuf> {
    let target = dir.join(SETTINGS_FILE);
    let body = serde_json::to_string_pretty(&render_settings_json(hook_exe))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_atomic(&target, &body)?;
    let legacy = dir.join(LEGACY_HOOKS_FILE);
    if legacy.exists() {
        match fs::remove_file(&legacy) {
            Ok(()) => log::info!("removed the old {}", legacy.display()),
            Err(e) => log::debug!("could not remove {}: {e}", legacy.display()),
        }
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_all_eleven_events() {
        let v = render_settings_json(Path::new("/opt/mira-hook"));
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 11);
        for e in HOOK_EVENTS {
            assert!(hooks.contains_key(e), "missing {e}");
        }
        assert_eq!(v["hooks"], render_hooks(Path::new("/opt/mira-hook")));
    }

    #[test]
    fn entries_are_exec_form_commands_with_event_arg() {
        let v = render_settings_json(Path::new("/opt/mira-hook"));
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
        let v = render_settings_json(Path::new("/x"));
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
        let v = render_settings_json(Path::new(
            r"C:\Program Files\mira-bots\resources\mira-hook.exe",
        ));
        let cmd = v["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert_eq!(cmd, "C:/Program Files/mira-bots/resources/mira-hook.exe");
        assert!(!cmd.contains('"') && !cmd.contains('\\'));
    }

    #[test]
    fn settings_have_exactly_hooks_and_one_allow_rule() {
        let v = render_settings_json(Path::new("/opt/mira-hook"));
        let mut keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["hooks", "permissions"]);
        assert_eq!(v["permissions"], json!({"allow": ["mcp__mira-bots__*"]}));
        assert_eq!(allow_rule(), "mcp__mira-bots__*");
    }

    fn temp_base() -> PathBuf {
        std::env::temp_dir().join(format!("mira-bots-test-{}", uuid::Uuid::new_v4()))
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn write_creates_dir_and_file_atomically() {
        let base = temp_base();
        let dir = base.join("nested");
        let path = write_settings_json(&dir, Path::new("/opt/mira-hook")).unwrap();
        assert_eq!(path, dir.join("settings.json"));
        let read: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read, render_settings_json(Path::new("/opt/mira-hook")));

        // Second write overwrites and leaves no temp file behind.
        write_settings_json(&dir, Path::new("/other/mira-hook")).unwrap();
        let read: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            read["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/other/mira-hook"
        );
        assert_eq!(file_names(&dir), ["settings.json"]);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn write_removes_the_old_hooks_json() {
        let dir = temp_base();
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("hooks.json"), "{}").unwrap();
        fs::write(dir.join("tickets.json"), "{}").unwrap();
        write_settings_json(&dir, Path::new("/opt/mira-hook")).unwrap();
        assert_eq!(file_names(&dir), ["settings.json", "tickets.json"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_atomic_replaces_and_cleans_up() {
        let dir = temp_base();
        let target = dir.join("a.txt");
        write_atomic(&target, "one").unwrap();
        write_atomic(&target, "two").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "two");
        assert_eq!(file_names(&dir), ["a.txt"]);
        // The target is a directory: the rename fails and no temp file is left.
        fs::create_dir_all(dir.join("sub")).unwrap();
        assert!(write_atomic(&dir.join("sub"), "x").is_err());
        assert_eq!(file_names(&dir), ["a.txt", "sub"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
