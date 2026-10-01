//! The agents' MCP server (`mira-mcp`, step 4): the app's `mcp.json` (passed with
//! `--mcp-config`), the system prompt addition `system-prompt.md` (passed with
//! `--append-system-prompt-file`). The exe lookup is `lib.rs::find_mcp_exe`. All files live in the app
//! data dir; the user's own `.mcp.json` and `~/.claude` are never read or written.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::config::{AGENT_ID_ENV, MCP_CONFIG_FILE, MCP_SERVER_NAME, PIPE_ENV, SYSTEM_PROMPT_FILE};
use crate::hooks::settings::{command_path, write_atomic};

/// Per-server tool timeout (ms) in mcp.json: a safety net only; mira-mcp answers within 10 s.
pub const MCP_SERVER_TIMEOUT_MS: u64 = 30_000;

/// `system-prompt.md` (C4.6), appended to every agent's system prompt.
pub const SYSTEM_PROMPT: &str = "Du kører som agent i mira-bots. Dine opgaver kommer som tickets; hver ticket ligger som fil i .mira-bots/tickets/<kort-id>.md i din arbejdsmappe, og appen beder dig om at læse den.

Regler:
- Når en ticket er færdig, SKAL du kalde værktøjet mira_submit_for_review med en kort opsummering (hvad du gjorde, hvad brugeren bør kigge på). Afslut først dit svar bagefter. Uden kaldet står ticketen som \"ikke afleveret\".
- Opdager du opfølgende arbejde, så opret en ny ticket med mira_create_ticket i stedet for at udvide opgaven.
- mira_list_tickets og mira_get_ticket viser dine og andre tickets; mira_update_status sætter en kort statuslinje.
- Rør ikke mappen .mira-bots/ manuelt (ingen filer, ingen redigering); appen ejer den.
";

/// The mcp.json document (C4.5): one stdio server `mira-bots` with the exe's absolute path
/// (forward slashes), no args, the two env vars as `${VAR}` (Claude Code expands them from its
/// own env, which it inherited from the app; inheritance alone would also work on Linux),
/// `alwaysLoad` (otherwise the tools are deferred behind `ToolSearch`) and a timeout.
///
/// TODO(windows-verify): Claude Code starts mira-mcp.exe from a path with spaces and `/` without
/// `cmd /c`; `/mcp` shows `mira-bots` connected and the five tools without `ToolSearch` (plan4
/// D.39). MIRA_BOTS_PIPE and MIRA_AGENT_ID reach mira-mcp.exe (inherited and/or `${VAR}`), also
/// after `/clear` (D.40). No mira-mcp.exe is left after `/exit`, Stop of the agent or quitting
/// the app, and a server crash does not kill the session (D.44). Both start sequences (with and
/// without the `server/discover` probe) end connected (D.46).
pub fn render_mcp_json(mcp_exe: &Path) -> Value {
    let mut server = json!({
        "type": "stdio",
        "command": command_path(mcp_exe),
        "args": [],
        "env": {},
        "alwaysLoad": true,
        "timeout": MCP_SERVER_TIMEOUT_MS,
    });
    for var in [PIPE_ENV, AGENT_ID_ENV] {
        server["env"][var] = Value::String(format!("${{{var}}}"));
    }
    let mut servers = serde_json::Map::new();
    servers.insert(MCP_SERVER_NAME.to_string(), server);
    json!({ "mcpServers": servers })
}

/// Writes `dir/mcp.json` (pretty, UTF-8) atomically and returns its path.
pub fn write_mcp_json(dir: &Path, mcp_exe: &Path) -> io::Result<PathBuf> {
    let target = dir.join(MCP_CONFIG_FILE);
    let body = serde_json::to_string_pretty(&render_mcp_json(mcp_exe))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_atomic(&target, &body)?;
    Ok(target)
}

/// The system prompt addition (C4.6), `\n` line endings.
pub fn render_system_prompt() -> &'static str {
    SYSTEM_PROMPT
}

/// Writes `dir/system-prompt.md` (UTF-8) atomically and returns its path.
///
/// TODO(windows-verify): `--append-system-prompt-file` with æøå works together with
/// `--settings`, `--mcp-config`, `--session-id` and the positional prompt, and survives `/clear`
/// (plan4 D.43).
pub fn write_system_prompt(dir: &Path) -> io::Result<PathBuf> {
    let target = dir.join(SYSTEM_PROMPT_FILE);
    write_atomic(&target, render_system_prompt())?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("mira-mcp-cfg-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn mcp_json_matches_the_contract() {
        let v = render_mcp_json(Path::new(
            r"C:\Program Files\mira-bots\resources\mira-mcp.exe",
        ));
        assert_eq!(
            v,
            json!({
                "mcpServers": {
                    "mira-bots": {
                        "type": "stdio",
                        "command": "C:/Program Files/mira-bots/resources/mira-mcp.exe",
                        "args": [],
                        "env": {
                            "MIRA_BOTS_PIPE": "${MIRA_BOTS_PIPE}",
                            "MIRA_AGENT_ID": "${MIRA_AGENT_ID}"
                        },
                        "alwaysLoad": true,
                        "timeout": 30000
                    }
                }
            })
        );
        let cmd = v["mcpServers"]["mira-bots"]["command"].as_str().unwrap();
        assert!(!cmd.contains('\\') && !cmd.contains('"'));
    }

    #[test]
    fn mcp_json_server_name_matches_the_mcp_crate() {
        let v = render_mcp_json(Path::new("/opt/mira-mcp"));
        assert!(v["mcpServers"].get(mira_mcp::SERVER_NAME).is_some());
        let env = &v["mcpServers"][mira_mcp::SERVER_NAME]["env"];
        assert!(env.get(mira_mcp::PIPE_ENV).is_some());
        assert!(env.get(mira_mcp::AGENT_ID_ENV).is_some());
    }

    #[test]
    fn system_prompt_names_the_tool_and_the_ticket_folder() {
        let p = render_system_prompt();
        assert!(p.starts_with("Du kører som agent i mira-bots."));
        assert!(p.contains("mira_submit_for_review"));
        assert!(p.contains("mira_create_ticket"));
        assert!(p.contains(".mira-bots/tickets/"));
        assert!(p.ends_with('\n') && !p.contains('\r'));
    }

    #[test]
    fn write_both_files_without_temp_leftovers() {
        let dir = temp_dir();
        let mcp = write_mcp_json(&dir, Path::new("/opt/mira-mcp")).unwrap();
        let prompt = write_system_prompt(&dir).unwrap();
        assert_eq!(mcp, dir.join("mcp.json"));
        assert_eq!(prompt, dir.join("system-prompt.md"));
        let read: Value = serde_json::from_str(&fs::read_to_string(&mcp).unwrap()).unwrap();
        assert_eq!(read, render_mcp_json(Path::new("/opt/mira-mcp")));
        assert_eq!(fs::read_to_string(&prompt).unwrap(), SYSTEM_PROMPT);
        // Rewriting is idempotent.
        write_mcp_json(&dir, Path::new("/opt/mira-mcp")).unwrap();
        write_system_prompt(&dir).unwrap();
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["mcp.json", "system-prompt.md"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
