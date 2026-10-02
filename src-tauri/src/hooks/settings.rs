//! Generation of the app's own Claude Code settings file `settings.json` (passed to claude with
//! `--settings`): the hooks plus `permissions.allow` for the app's own MCP tools. Up to step 3
//! the file was `hooks.json` with only the hooks; writing settings.json removes that file.
//! `~/.claude/settings.json` is never touched.
//!
//! Step 5: every profile gets its own rendered settings file
//! `<app_data>/profiles/<id>/settings.json` ([`write_profile_settings`]), because Claude Code
//! does not merge several `--settings` flags (research5 Q4). The root file is still written
//! (fallback/diagnostics) but no longer passed to claude.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::config::{
    DEFAULT_HOOK_TIMEOUT_S, LEGACY_HOOKS_FILE, MCP_TOOL_PREFIX, PERMISSION_HOOK_TIMEOUT_S,
    PROFILE_FILES_DIR, SESSION_END_HOOK_TIMEOUT_S, SETTINGS_FILE, STATUSLINE_ENABLED,
};
use crate::profiles::model::{AgentProfile, Effort};

/// The events mira-bots listens to. SubagentStart/Stop are left out in step 1; PostModelSwitch
/// (step 5) carries the model after `/model`, a fallback or `--resume`.
// TODO(windows-verify): a Claude Code version that does not know PostModelSwitch (>= 2.1.139)
// starts without an error with this settings file (plan5 D.62).
pub const HOOK_EVENTS: [&str; 12] = [
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
    "PostModelSwitch",
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

/// `permissions` of a profile's settings file; `deny` is left out when empty.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ProfilePermissions {
    pub allow: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

/// `statusLine` of a profile's settings file: the hook exe, called without args.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct StatusLine {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub command: String,
    pub padding: u32,
}

/// A profile's settings file (C5.10). A struct, so the keys keep this order in the file:
/// `hooks`, `permissions`, `model`, `effortLevel`, `statusLine`.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSettings {
    pub hooks: Value,
    pub permissions: ProfilePermissions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort_level: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_line: Option<StatusLine>,
}

/// `statusLine.command`: a shell command string, so a path with spaces is wrapped in `"`.
pub fn statusline_command(hook_exe: &Path) -> String {
    let p = command_path(hook_exe);
    if p.contains(' ') {
        format!("\"{p}\"")
    } else {
        p
    }
}

/// The settings file of `profile` (C5.10): the hooks as in [`render_hooks`], allow
/// (`mcp__mira-bots__*` + `extraAllow`), deny (role-bound tools the roles lack + `toolDeny` +
/// `Edit`/`Write`/`MultiEdit`/`NotebookEdit` without a work role + the push deny + with `root`
/// the `Edit(//<root>/…)` rules for the user's files (step 6b) + `extraDeny`; omitted when
/// empty), `model` when set, `effortLevel` when set and not `max`
/// (settings files ignore `max`; it is only passed as `--effort`), and `statusLine` pointing at
/// the hook exe when [`STATUSLINE_ENABLED`].
///
/// TODO(windows-verify): the statusLine with the quoted path to mira-hook.exe starts without an
/// error in the TUI and the app shows the observed model (plan5 D.52); `permissions.deny` hides
/// the role-bound tools from `/mcp` (D.51).
// TODO(windows-verify): with root `C:\Users\x\mira-bots\projects` the rules read
// `Edit(//c/Users/x/mira-bots/projects/…)` and block a coder (plan6b D.100).
pub fn profile_settings(
    hook_exe: &Path,
    profile: &AgentProfile,
    root: Option<&Path>,
) -> ProfileSettings {
    ProfileSettings {
        hooks: render_hooks(hook_exe),
        permissions: ProfilePermissions {
            allow: profile.allow_rules(),
            deny: profile.deny_rules(root),
        },
        model: profile.model.clone(),
        effort_level: profile
            .effort
            .filter(|e| *e != Effort::Max)
            .map(Effort::as_str),
        status_line: STATUSLINE_ENABLED.then(|| StatusLine {
            kind: "command",
            command: statusline_command(hook_exe),
            padding: 0,
        }),
    }
}

/// [`profile_settings`] as a JSON value.
pub fn render_profile_settings(
    hook_exe: &Path,
    profile: &AgentProfile,
    root: Option<&Path>,
) -> Value {
    serde_json::to_value(profile_settings(hook_exe, profile, root)).unwrap_or(Value::Null)
}

/// Writes `<data_dir>/profiles/<id>/settings.json` (pretty, keys in C5.10 order) atomically and
/// returns its path. Called before every spawn/restart and when a profile is saved; `root` is the
/// projects root (absolute `Edit` deny rules for the user's files, step 6b).
///
/// TODO(windows-verify): %APPDATA%\dk.mira.bots\profiles\<id>\settings.json exists and claude
/// starts with `--settings` on it (plan5 D.50).
pub fn write_profile_settings(
    data_dir: &Path,
    hook_exe: &Path,
    profile: &AgentProfile,
    root: Option<&Path>,
) -> io::Result<PathBuf> {
    let target = data_dir
        .join(PROFILE_FILES_DIR)
        .join(&profile.id)
        .join(SETTINGS_FILE);
    let body = serde_json::to_string_pretty(&profile_settings(hook_exe, profile, root))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_atomic(&target, &body)?;
    Ok(target)
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
    fn contains_all_twelve_events() {
        let v = render_settings_json(Path::new("/opt/mira-hook"));
        let hooks = v["hooks"].as_object().unwrap();
        assert_eq!(hooks.len(), 12);
        assert!(hooks.contains_key("PostModelSwitch"));
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

    use crate::agent::roles::Role;
    use crate::profiles::model::builtin_profile;

    fn profile(id: &str) -> AgentProfile {
        builtin_profile(id).unwrap()
    }

    #[test]
    fn profile_settings_has_twelve_events() {
        for id in ["coder", "reviewer", "specialist"] {
            let v = render_profile_settings(Path::new("/opt/mira-hook"), &profile(id), None);
            assert_eq!(v["hooks"].as_object().unwrap().len(), 12, "{id}");
            assert_eq!(v["hooks"], render_hooks(Path::new("/opt/mira-hook")));
        }
    }

    /// The coder file exactly (C5.10): all role-bound tools denied, push denied and the user's
    /// files locked under the projects root (step 6b), no model/effort keys.
    #[test]
    fn coder_settings_denies_role_bound_tools() {
        let v = render_profile_settings(
            Path::new("/opt/mira-hook"),
            &profile("coder"),
            Some(Path::new("/home/ann/mira-bots/projects")),
        );
        assert_eq!(
            v,
            json!({
                "hooks": render_hooks(Path::new("/opt/mira-hook")),
                "permissions": {
                    "allow": ["mcp__mira-bots__*"],
                    "deny": [
                        "mcp__mira-bots__mira_approve_ticket",
                        "mcp__mira-bots__mira_assign_ticket",
                        "mcp__mira-bots__mira_list_profiles",
                        "mcp__mira-bots__mira_reject_ticket",
                        "mcp__mira-bots__mira_spawn_agent",
                        "mcp__mira-bots__mira_start_playbook",
                        "mcp__mira-bots__mira_unassign_ticket",
                        "Bash(git push *)",
                        "Bash(git -C * push *)",
                        "Edit(//home/ann/mira-bots/projects/mira-bots.workspace.json)",
                        "Edit(//home/ann/mira-bots/projects/**/.mira-bots/project.json)",
                        "Edit(**/.mira-bots/project.json)"
                    ]
                },
                "statusLine": {"type": "command", "command": "/opt/mira-hook", "padding": 0}
            })
        );
    }

    /// The reviewer file exactly: git reads allowed, coordinator tools, file editing (no work
    /// role, 5c C.2) and commit/push denied.
    #[test]
    fn reviewer_settings_allows_git_reads_and_denies_commit_push() {
        let v = render_profile_settings(Path::new("/opt/mira-hook"), &profile("reviewer"), None);
        assert_eq!(
            v,
            json!({
                "hooks": render_hooks(Path::new("/opt/mira-hook")),
                "permissions": {
                    "allow": [
                        "mcp__mira-bots__*",
                        "Bash(git -C * diff *)",
                        "Bash(git -C * log *)",
                        "Bash(git -C * status *)",
                        "Bash(git -C * show *)"
                    ],
                    "deny": [
                        "mcp__mira-bots__mira_assign_ticket",
                        "mcp__mira-bots__mira_list_profiles",
                        "mcp__mira-bots__mira_spawn_agent",
                        "mcp__mira-bots__mira_start_playbook",
                        "mcp__mira-bots__mira_unassign_ticket",
                        "Edit",
                        "Write",
                        "MultiEdit",
                        "NotebookEdit",
                        "Bash(git push *)",
                        "Bash(git -C * push *)",
                        "Bash(git commit *)",
                        "Bash(git -C * commit *)"
                    ]
                },
                "statusLine": {"type": "command", "command": "/opt/mira-hook", "padding": 0}
            })
        );
        // No deny rule has parentheses on an mcp__ name (they would be ignored, research5 Q5).
        for rule in v["permissions"]["deny"].as_array().unwrap() {
            let r = rule.as_str().unwrap();
            assert!(!(r.starts_with("mcp__") && r.contains('(')), "{r}");
        }
    }

    #[test]
    fn model_and_effort_keys_only_when_set() {
        let hook = Path::new("/opt/mira-hook");
        let v = render_profile_settings(hook, &profile("coder"), None);
        assert!(v.get("model").is_none() && v.get("effortLevel").is_none());
        for (effort, want) in [
            (Effort::Low, Some("low")),
            (Effort::Medium, Some("medium")),
            (Effort::High, Some("high")),
            (Effort::Xhigh, Some("xhigh")),
            (Effort::Max, None),
        ] {
            let p = AgentProfile {
                model: Some("sonnet".into()),
                effort: Some(effort),
                ..profile("coder")
            };
            let v = render_profile_settings(hook, &p, None);
            assert_eq!(v["model"], "sonnet");
            assert_eq!(
                v.get("effortLevel").and_then(Value::as_str),
                want,
                "{effort}"
            );
        }
        // Key order in the written file: hooks, permissions, model, effortLevel, statusLine.
        let p = AgentProfile {
            model: Some("claude-opus-5-5[1m]".into()),
            effort: Some(Effort::High),
            ..profile("reviewer")
        };
        let text = serde_json::to_string_pretty(&profile_settings(hook, &p, None)).unwrap();
        let pos = |k: &str| {
            text.find(&format!("\n  \"{k}\":"))
                .unwrap_or_else(|| panic!("{k}"))
        };
        let order = ["hooks", "permissions", "model", "effortLevel", "statusLine"].map(pos);
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{text}");
    }

    #[test]
    fn statusline_points_at_hook_exe() {
        let v = render_profile_settings(Path::new("/opt/mira-hook"), &profile("coder"), None);
        assert_eq!(v["statusLine"]["command"], "/opt/mira-hook");
        let v = render_profile_settings(
            Path::new(r"C:\Program Files\mira-bots\resources\mira-hook.exe"),
            &profile("coder"),
            None,
        );
        assert_eq!(
            v["statusLine"],
            json!({
                "type": "command",
                "command": "\"C:/Program Files/mira-bots/resources/mira-hook.exe\"",
                "padding": 0
            })
        );
        // The hooks themselves keep the unquoted exec form.
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            "C:/Program Files/mira-bots/resources/mira-hook.exe"
        );
        assert_eq!(statusline_command(Path::new(r"C:\x\h.exe")), "C:/x/h.exe");
    }

    /// Step 6b: even the specialist (every role, no narrowing) gets a deny key: the push rules.
    #[test]
    fn specialist_denies_only_push() {
        let v = render_profile_settings(Path::new("/opt/mira-hook"), &profile("specialist"), None);
        assert_eq!(
            v["permissions"],
            json!({"allow": ["mcp__mira-bots__*"],
                   "deny": ["Bash(git push *)", "Bash(git -C * push *)"]})
        );
        // A profile with every role and some toolDeny: the tool first.
        let p = AgentProfile {
            roles: Role::ALL.to_vec(),
            tool_deny: vec!["mira_add_report".into()],
            ..profile("coder")
        };
        let v = render_profile_settings(Path::new("/opt/mira-hook"), &p, None);
        assert_eq!(
            v["permissions"]["deny"],
            json!([
                "mcp__mira-bots__mira_add_report",
                "Bash(git push *)",
                "Bash(git -C * push *)"
            ])
        );
    }

    #[test]
    fn write_profile_settings_goes_to_the_profile_folder() {
        let base = temp_base();
        let p = profile("reviewer");
        let root = Path::new("/home/x/projects");
        let path =
            write_profile_settings(&base, Path::new("/opt/mira-hook"), &p, Some(root)).unwrap();
        assert_eq!(
            path,
            base.join("profiles").join("reviewer").join("settings.json")
        );
        let read: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            read,
            render_profile_settings(Path::new("/opt/mira-hook"), &p, Some(root))
        );
        assert!(read["permissions"]["deny"]
            .as_array()
            .unwrap()
            .contains(&json!("Edit(//home/x/projects/mira-bots.workspace.json)")));
        assert_eq!(file_names(path.parent().unwrap()), ["settings.json"]);
        fs::remove_dir_all(&base).unwrap();
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
