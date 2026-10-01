//! Runs the real binary: JSON-RPC lines in, exactly one line per request out, exit 0 at EOF.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

struct Run {
    code: Option<i32>,
    lines: Vec<Value>,
    took: Duration,
}

fn run_bin(input: &str, env: &[(&str, &str)]) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mira-mcp"));
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for k in [
        "MIRA_BOTS_PIPE",
        "MIRA_AGENT_ID",
        "MIRA_MCP_DEBUG",
        "MIRA_MCP_TIMEOUT_MS",
        "MIRA_AGENT_ROLES",
    ] {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let t = Instant::now();
    let mut child = cmd.spawn().unwrap();
    // Dropping stdin after writing = EOF.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.is_empty() || text.ends_with('\n'), "{text:?}");
    let lines = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| panic!("not JSON: {l:?}")))
        .collect();
    Run {
        code: out.status.code(),
        lines,
        took: t.elapsed(),
    }
}

const INIT: &str = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;

fn call(id: u64, tool: &str, args: Value) -> String {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":tool,"arguments":args}})
        .to_string()
}

#[test]
fn handshake_list_unknown_and_notification_give_three_lines() {
    let input = [
        INIT,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"resources/list"}"#,
    ]
    .join("\n")
        + "\n";
    let r = run_bin(&input, &[]);
    assert_eq!(r.code, Some(0));
    assert_eq!(r.lines.len(), 3, "{:?}", r.lines);
    assert_eq!(r.lines[0]["id"], 0);
    assert_eq!(r.lines[0]["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(r.lines[1]["id"], 1);
    // Without MIRA_AGENT_ROLES: the common tools only.
    assert_eq!(r.lines[1]["result"]["tools"].as_array().unwrap().len(), 9);
    assert_eq!(r.lines[2]["id"], 2);
    assert_eq!(r.lines[2]["error"]["code"], -32601);
    assert!(r.took < Duration::from_secs(5));
}

fn listed_names(roles: Option<&str>) -> Vec<String> {
    let input = format!(
        "{INIT}\n{}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#
    );
    let env: Vec<(&str, &str)> = roles.map(|r| ("MIRA_AGENT_ROLES", r)).into_iter().collect();
    let r = run_bin(&input, &env);
    assert_eq!(r.code, Some(0));
    r.lines[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn tools_list_filtered_by_env() {
    let common = [
        "mira_create_ticket",
        "mira_list_tickets",
        "mira_get_ticket",
        "mira_submit_for_review",
        "mira_update_status",
        "mira_get_workspace_rules",
        "mira_add_report",
        "mira_get_report",
        "mira_handoff_ticket",
    ];
    let with = |extra: &[&str]| -> Vec<String> {
        common.iter().chain(extra).map(|s| s.to_string()).collect()
    };
    assert_eq!(listed_names(None), with(&[]));
    assert_eq!(listed_names(Some("")), with(&[]));
    assert_eq!(listed_names(Some("${MIRA_AGENT_ROLES}")), with(&[]));
    assert_eq!(listed_names(Some("coder")), with(&[]));
    assert_eq!(
        listed_names(Some("reviewer")),
        with(&["mira_approve_ticket", "mira_reject_ticket"])
    );
    assert_eq!(
        listed_names(Some(" coordinator ")),
        with(&[
            "mira_assign_ticket",
            "mira_unassign_ticket",
            "mira_spawn_agent",
            "mira_list_agents",
            "mira_list_profiles"
        ])
    );
    assert_eq!(listed_names(Some("coder,reviewer,coordinator")).len(), 16);
    // A hidden tool is refused without contacting the app (no pipe needed).
    let input = format!(
        "{INIT}\n{}\n",
        call(2, "mira_approve_ticket", json!({"id": "abcdef01"}))
    );
    let r = run_bin(&input, &[("MIRA_AGENT_ROLES", "coder")]);
    assert_eq!(r.lines[1]["result"]["isError"], true);
    assert_eq!(
        r.lines[1]["result"]["content"][0]["text"],
        "Din rolle tillader ikke dette værktøj"
    );
}

#[test]
fn server_discover_is_method_not_found() {
    let r = run_bin(
        "{\"jsonrpc\":\"2.0\",\"id\":\"server-discover-probe-1\",\"method\":\"server/discover\",\"params\":{}}\n",
        &[],
    );
    assert_eq!(r.code, Some(0));
    assert_eq!(r.lines.len(), 1);
    assert_eq!(r.lines[0]["id"], "server-discover-probe-1");
    assert_eq!(r.lines[0]["error"]["code"], -32601);
}

#[test]
fn garbage_gives_one_parse_error_and_exit_zero() {
    let r = run_bin("{{{\n", &[]);
    assert_eq!(r.code, Some(0));
    assert_eq!(
        r.lines,
        vec![json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})]
    );
}

#[test]
fn tool_call_without_pipe_env_is_an_is_error_result() {
    let input = format!("{INIT}\n{}\n", call(1, "mira_list_tickets", json!({})));
    // `${…}` left unexpanded by Claude Code counts as missing.
    let r = run_bin(&input, &[("MIRA_BOTS_PIPE", "${MIRA_BOTS_PIPE}")]);
    assert_eq!(r.code, Some(0));
    assert_eq!(r.lines.len(), 2);
    let res = &r.lines[1]["result"];
    assert_eq!(res["isError"], true);
    let text = res["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("mira-bots kører ikke"), "{text}");
    assert!(r.took < Duration::from_secs(5));
}

#[test]
fn empty_stdin_exits_zero_silently() {
    let r = run_bin("", &[]);
    assert_eq!(r.code, Some(0));
    assert!(r.lines.is_empty());
}

#[cfg(unix)]
mod unix_socket {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;

    fn sock_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("mira-mcp-bin-{}-{tag}.sock", std::process::id()))
    }

    /// A fake app: accepts `n` connections, answers each tool frame with `ok:true` and the frame
    /// it received as the result (or stays silent when `answer` is false).
    fn fake_app(path: &PathBuf, n: usize, answer: bool) -> std::thread::JoinHandle<Vec<Value>> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let mut frames = Vec::new();
            let mut held = Vec::new();
            for _ in 0..n {
                let (s, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(s.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let frame: Value = serde_json::from_str(&line).unwrap();
                if answer {
                    let reply = json!({"v":1,"kind":"tool_result","request_id":frame["request_id"],"ok":true,"result":{"seen":frame}});
                    writeln!(&s, "{reply}").unwrap();
                } else {
                    held.push(s);
                }
                frames.push(frame);
            }
            if !held.is_empty() {
                // Keep the silent connections open well past the client's timeout.
                std::thread::sleep(Duration::from_millis(1500));
            }
            frames
        })
    }

    #[test]
    fn tool_call_round_trip_through_a_socket() {
        let path = sock_path("rt");
        let app = fake_app(&path, 2, true);
        let input = format!(
            "{INIT}\n{}\n{}\n",
            call(1, "mira_create_ticket", json!({"title":"  Ny opgave  "})),
            call(2, "mira_update_status", json!({"note":"Kører tests"}))
        );
        let p = path.to_string_lossy().into_owned();
        let r = run_bin(
            &input,
            &[("MIRA_BOTS_PIPE", &p), ("MIRA_AGENT_ID", "agent-9")],
        );
        assert_eq!(r.code, Some(0));
        assert_eq!(r.lines.len(), 3, "{:?}", r.lines);
        for (i, line) in r.lines[1..].iter().enumerate() {
            assert_eq!(line["id"], i as u64 + 1);
            assert_eq!(line["result"]["isError"], false);
        }
        let first: Value =
            serde_json::from_str(r.lines[1]["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(first["seen"]["args"], json!({"title":"Ny opgave"}));
        let frames = app.join().unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["kind"], "tool");
        assert_eq!(frames[0]["agent_id"], "agent-9");
        assert_eq!(frames[0]["tool"], "mira_create_ticket");
        assert_eq!(frames[1]["tool"], "mira_update_status");
        assert_ne!(frames[0]["request_id"], frames[1]["request_id"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn silent_app_gives_is_error_after_the_timeout() {
        let path = sock_path("to");
        let app = fake_app(&path, 1, false);
        let input = format!(
            "{}\n{}\n",
            call(1, "mira_list_tickets", json!({})),
            r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#
        );
        let p = path.to_string_lossy().into_owned();
        let r = run_bin(
            &input,
            &[
                ("MIRA_BOTS_PIPE", &p),
                ("MIRA_AGENT_ID", "agent-9"),
                ("MIRA_MCP_TIMEOUT_MS", "400"),
            ],
        );
        assert_eq!(r.code, Some(0));
        assert_eq!(r.lines.len(), 2);
        assert_eq!(r.lines[0]["result"]["isError"], true);
        assert_eq!(
            r.lines[0]["result"]["content"][0]["text"],
            "mira-bots svarede ikke inden for 400 ms"
        );
        // The loop goes on after a timeout.
        assert_eq!(r.lines[1], json!({"jsonrpc":"2.0","id":2,"result":{}}));
        assert!(r.took < Duration::from_secs(5), "{:?}", r.took);
        app.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }
}
