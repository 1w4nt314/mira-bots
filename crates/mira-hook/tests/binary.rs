//! Runs the real binary: it must exit 0 with empty stdout when the app is absent.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn run_bin(input: &str, pipe: Option<&str>) -> (Option<i32>, Vec<u8>, Duration) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mira-hook"));
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd.env_remove("MIRA_BOTS_PIPE")
        .env_remove("MIRA_HOOK_DEBUG");
    if let Some(p) = pipe {
        cmd.env("MIRA_BOTS_PIPE", p);
    }
    let t = Instant::now();
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.code(), out.stdout, t.elapsed())
}

#[test]
fn missing_pipe_env_exits_zero_silently() {
    let (code, stdout, took) = run_bin(
        r#"{"hook_event_name":"PermissionRequest","session_id":"x"}"#,
        None,
    );
    assert_eq!(code, Some(0));
    assert!(stdout.is_empty());
    assert!(took < Duration::from_millis(2500));
}

#[test]
fn unreachable_pipe_exits_zero_silently() {
    let nope = std::env::temp_dir().join("mira-bots-nope.sock");
    let (code, stdout, took) = run_bin(
        r#"{"hook_event_name":"Stop","session_id":"x"}"#,
        nope.to_str(),
    );
    assert_eq!(code, Some(0));
    assert!(stdout.is_empty());
    assert!(took < Duration::from_millis(2500));
}

#[test]
fn garbage_stdin_exits_zero_silently() {
    let (code, stdout, _) = run_bin("{{{", Some("whatever"));
    assert_eq!(code, Some(0));
    assert!(stdout.is_empty());
}
