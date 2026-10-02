//! PATH of the user's login shell, read once. GUI apps started from Finder/Dock inherit
//! launchd's minimal PATH (`/usr/bin:/bin:/usr/sbin:/sbin`), so `claude` and the agent's tools
//! would not be found otherwise (research7 §7, the fix-path-env pattern with a timeout).
//!
//! The shell runs as `$SHELL -ilc '<print PATH between delimiters>'`: interactive + login, because
//! PATH is often set in `.zshrc`, which a plain login shell does not read. Only the text between
//! the two delimiters is used, so banners, MOTDs and prompt noise are ignored. stdin is null and
//! the shell gets its own session (no controlling terminal), so nothing can wait for input; after
//! `LOGIN_SHELL_TIMEOUT` the whole session is killed and the process PATH is used. The result is
//! cached in a `OnceLock`; nothing calls `std::env::set_var` — children get the PATH through
//! `process::spawn_env_extra`.
// TODO(macos-verify): `claude` is found when the app is started from Finder/Dock/Launchpad (native
// installer, Homebrew, npm); the log shows "login shell PATH: login-shell"; a slow `.zshrc`
// (sleep 10) gives "process" after 5 s and the app still starts (plan7 M.7).

use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use crate::config::{LOGIN_PATH_DELIMITER, LOGIN_SHELL_TIMEOUT};

/// [`source`] when the login shell answered.
pub const SOURCE_LOGIN_SHELL: &str = "login-shell";
/// [`source`] when the process PATH is used (Windows, shell failed or timed out, before `init`).
pub const SOURCE_PROCESS: &str = "process";

struct LoginEnv {
    path: Option<OsString>,
    source: &'static str,
}

static LOGIN_ENV: OnceLock<LoginEnv> = OnceLock::new();

/// Pure: text between the first two `delimiter`s, ANSI escapes removed, trimmed. `None` when
/// fewer than two delimiters are present.
pub fn parse_delimited(output: &str, delimiter: &str) -> Option<String> {
    if delimiter.is_empty() {
        return None;
    }
    let clean = strip_ansi(output);
    let start = clean.find(delimiter)? + delimiter.len();
    let rest = &clean[start..];
    let end = rest.find(delimiter)?;
    Some(rest[..end].trim().to_string())
}

/// Removes ANSI escape sequences: CSI (`ESC [ … final`), OSC (`ESC ] … BEL` or `ESC ] … ESC \`)
/// and two-byte `ESC x` sequences.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                // Parameters/intermediates until a final byte in 0x40..=0x7e.
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{07}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // `ESC x` (or a lone ESC at the end): drop both.
            _ => {}
        }
    }
    out
}

/// Pure: `login` entries first, then entries of `current` not already present; empty entries
/// dropped; the `join_paths` result. `login == None` → `current` unchanged (deduplicated).
pub fn merge_paths(login: Option<&str>, current: Option<&OsStr>) -> OsString {
    let mut entries: Vec<PathBuf> = Vec::new();
    let login_entries = login.into_iter().flat_map(std::env::split_paths);
    let current_entries = current.into_iter().flat_map(std::env::split_paths);
    for p in login_entries.chain(current_entries) {
        if !p.as_os_str().is_empty() && !entries.contains(&p) {
            entries.push(p);
        }
    }
    std::env::join_paths(&entries).unwrap_or_else(|e| {
        log::warn!("could not join the merged PATH ({e}); using the process PATH");
        current.map(OsStr::to_os_string).unwrap_or_default()
    })
}

#[cfg(unix)]
/// `shell -ilc 'printf "%s" "<D>"; printf "%s" "$PATH"; printf "%s" "<D>"'` with stdin null,
/// stderr null, stdout piped, cwd = $HOME (if set), env DISABLE_AUTO_UPDATE=1; polls
/// try_wait every 50 ms, kills on `timeout`. Returns the parsed PATH or None (warn-logged).
///
/// The shell is started in its own session (`setsid`): an interactive shell then has no
/// controlling terminal to wait for, and on timeout `killpg` also ends anything its profile
/// started (e.g. a `sleep` in `.zshrc`). The answer is taken as soon as both delimiters have
/// arrived, so a daemon that keeps stdout open cannot hold the lookup until the timeout.
pub fn read_login_path(shell: &Path, timeout: Duration) -> Option<String> {
    unix::read_login_path(shell, timeout, LOGIN_PATH_DELIMITER)
}

/// Once per process. Unix: shell = $SHELL (else "/bin/sh"); merge with the process PATH.
/// Windows: process PATH. Safe to call twice (second call is a no-op).
pub fn init() {
    LOGIN_ENV.get_or_init(compute);
}

fn compute() -> LoginEnv {
    let current = std::env::var_os("PATH");
    #[cfg(unix)]
    {
        let shell = std::env::var_os("SHELL")
            .filter(|s| !s.is_empty())
            .map_or_else(|| PathBuf::from("/bin/sh"), PathBuf::from);
        let started = std::time::Instant::now();
        let login = read_login_path(&shell, LOGIN_SHELL_TIMEOUT);
        let source = if login.is_some() {
            SOURCE_LOGIN_SHELL
        } else {
            SOURCE_PROCESS
        };
        let merged = merge_paths(login.as_deref(), current.as_deref());
        let entries = std::env::split_paths(&merged).count();
        log::info!(
            "login shell PATH: {source} ({entries} entries, {} ms, shell {})",
            started.elapsed().as_millis(),
            shell.display()
        );
        LoginEnv {
            path: (!merged.is_empty()).then_some(merged),
            source,
        }
    }
    #[cfg(windows)]
    {
        LoginEnv {
            path: current,
            source: SOURCE_PROCESS,
        }
    }
}

/// The PATH to use for lookups and children. Before `init`: the process PATH.
pub fn path() -> Option<OsString> {
    match LOGIN_ENV.get() {
        Some(env) => env.path.clone(),
        None => std::env::var_os("PATH"),
    }
}

/// "login-shell" when the shell answered, else "process".
pub fn source() -> &'static str {
    LOGIN_ENV.get().map_or(SOURCE_PROCESS, |env| env.source)
}

#[cfg(unix)]
mod unix {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::parse_delimited;

    const POLL: Duration = Duration::from_millis(50);
    /// After the shell exited without both delimiters: how long to wait for output still in
    /// the pipe (a daemon started by the profile may keep stdout open forever). Also how long a
    /// shell that has answered gets to exit on its own before its session is killed.
    const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(200);

    enum Chunk {
        Data(Vec<u8>),
        Eof,
    }

    pub(super) fn read_login_path(
        shell: &Path,
        timeout: Duration,
        delimiter: &str,
    ) -> Option<String> {
        let script = format!(
            "printf \"%s\" \"{delimiter}\"; printf \"%s\" \"$PATH\"; printf \"%s\" \"{delimiter}\""
        );
        let mut cmd = Command::new(shell);
        cmd.arg("-ilc")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("DISABLE_AUTO_UPDATE", "1");
        if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            if Path::new(&home).is_dir() {
                cmd.current_dir(home);
            }
        }
        // SAFETY: setsid is async-signal-safe and touches no memory of the parent.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("login shell {}: could not start: {e}", shell.display());
                return None;
            }
        };
        let Some(mut stdout) = child.stdout.take() else {
            finish(&mut child, Duration::ZERO);
            return None;
        };
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name("login-shell-path".into())
            .spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match stdout.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(Chunk::Eof);
                            return;
                        }
                        Ok(n) => {
                            if tx.send(Chunk::Data(buf[..n].to_vec())).is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        if let Err(e) = reader {
            log::warn!("login shell: could not start the reader thread: {e}");
            finish(&mut child, Duration::ZERO);
            return None;
        }

        let deadline = Instant::now() + timeout;
        let mut out: Vec<u8> = Vec::new();
        let mut exited_at: Option<Instant> = None;
        loop {
            let now = Instant::now();
            if now >= deadline {
                finish(&mut child, Duration::ZERO);
                log::warn!(
                    "login shell {}: no PATH within {} ms; using the process PATH",
                    shell.display(),
                    timeout.as_millis()
                );
                return None;
            }
            match rx.recv_timeout(POLL.min(deadline - now)) {
                Ok(Chunk::Data(bytes)) => {
                    out.extend_from_slice(&bytes);
                    if let Some(path) = parse_delimited(&String::from_utf8_lossy(&out), delimiter) {
                        finish(&mut child, DRAIN_AFTER_EXIT);
                        return Some(path);
                    }
                }
                Ok(Chunk::Eof) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    finish(&mut child, DRAIN_AFTER_EXIT);
                    return answer(shell, &out, delimiter);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if exited_at.is_none() && matches!(child.try_wait(), Ok(Some(_))) {
                exited_at = Some(Instant::now());
            }
            if exited_at.is_some_and(|t| t.elapsed() >= DRAIN_AFTER_EXIT) {
                finish(&mut child, Duration::ZERO);
                return answer(shell, &out, delimiter);
            }
        }
    }

    fn answer(shell: &Path, out: &[u8], delimiter: &str) -> Option<String> {
        let parsed = parse_delimited(&String::from_utf8_lossy(out), delimiter);
        if parsed.is_none() {
            log::warn!(
                "login shell {}: no delimited PATH in its output ({} bytes); using the process PATH",
                shell.display(),
                out.len()
            );
        }
        parsed
    }

    /// Gives the shell up to `grace` to exit on its own, then kills its session (the shell is
    /// the group leader thanks to setsid) and reaps it. Once the shell has exited its group is
    /// never signalled: background jobs a profile started are left alone, and the group id
    /// could otherwise be reused.
    fn finish(child: &mut Child, grace: Duration) {
        let until = Instant::now() + grace;
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) if Instant::now() >= until => break,
                Ok(None) => std::thread::sleep(POLL.min(grace)),
            }
        }
        if let Ok(pgid) = i32::try_from(child.id()) {
            // SAFETY: killpg only takes integers; the shell is unreaped, so `pgid` is still its
            // own session/group and cannot have been reused.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "_D_";

    #[test]
    fn parse_takes_the_text_between_the_first_two_delimiters() {
        assert_eq!(
            parse_delimited("banner\nmotd _D_/a/bin:/b/bin_D_ trailing _D_x_D_", D),
            Some("/a/bin:/b/bin".to_string())
        );
        assert_eq!(
            parse_delimited("_D_  /a:/b \n_D_", D),
            Some("/a:/b".to_string())
        );
    }

    #[test]
    fn parse_strips_ansi_escapes() {
        let noisy = "\u{1b}[1;32mwelcome\u{1b}[0m\u{1b}]0;title\u{07}_D_\u{1b}[31m/a/bin\u{1b}[0m:/b_D_\u{1b}(B";
        assert_eq!(parse_delimited(noisy, D), Some("/a/bin:/b".to_string()));
        let osc_st = "\u{1b}]7;file://host/x\u{1b}\\_D_/c_D_";
        assert_eq!(parse_delimited(osc_st, D), Some("/c".to_string()));
    }

    #[test]
    fn parse_needs_two_delimiters() {
        assert_eq!(parse_delimited("_D_/a/bin", D), None);
        assert_eq!(parse_delimited("no delimiters at all", D), None);
        assert_eq!(parse_delimited("", D), None);
        assert_eq!(parse_delimited("_D_/a_D_", ""), None);
    }

    #[test]
    fn parse_of_an_empty_path_is_some_empty() {
        assert_eq!(parse_delimited("x_D__D_y", D), Some(String::new()));
        assert_eq!(parse_delimited("_D_ \n _D_", D), Some(String::new()));
    }

    fn joined(parts: &[&str]) -> OsString {
        std::env::join_paths(parts).unwrap()
    }

    fn split(p: &OsStr) -> Vec<PathBuf> {
        std::env::split_paths(p).collect()
    }

    #[test]
    fn merge_puts_login_first_and_drops_duplicates_and_empties() {
        let login = joined(&["/login/bin", "/usr/bin", "/login/bin"]);
        let current = joined(&["/usr/bin", "", "/bin", "/login/bin"]);
        let merged = merge_paths(login.to_str(), Some(&current));
        assert_eq!(
            split(&merged),
            vec![
                PathBuf::from("/login/bin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin")
            ]
        );
    }

    #[test]
    fn merge_without_login_keeps_the_process_path_deduplicated() {
        let current = joined(&["/usr/bin", "/bin", "/usr/bin"]);
        let merged = merge_paths(None, Some(&current));
        assert_eq!(
            split(&merged),
            vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
        );
        assert!(merge_paths(None, None).is_empty());
        let only_login = merge_paths(joined(&["/x"]).to_str(), None);
        assert_eq!(split(&only_login), vec![PathBuf::from("/x")]);
    }

    #[test]
    fn merge_with_an_empty_login_path_is_the_process_path() {
        let current = joined(&["/usr/bin", "/bin"]);
        assert_eq!(merge_paths(Some(""), Some(&current)), current);
    }

    #[test]
    fn source_and_path_before_or_after_init_are_consistent() {
        // Tests never call init() (it would run the developer's real login shell), so the
        // process PATH is used and the source is "process".
        if LOGIN_ENV.get().is_none() {
            assert_eq!(source(), SOURCE_PROCESS);
            assert_eq!(path(), std::env::var_os("PATH"));
        } else {
            assert!([SOURCE_PROCESS, SOURCE_LOGIN_SHELL].contains(&source()));
        }
    }

    #[cfg(unix)]
    mod fake_shell {
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        use super::super::unix;
        use crate::config::LOGIN_PATH_DELIMITER;

        const D: &str = LOGIN_PATH_DELIMITER;

        /// Writes an executable shell script. Every script answers `--probe` with exit 0 so the
        /// helper can wait out ETXTBSY (another test thread may fork while the file is open).
        fn script(body: &str) -> (PathBuf, PathBuf) {
            let dir = std::env::temp_dir().join(format!("mira-login-env-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("fake-shell");
            std::fs::write(
                &path,
                format!("#!/bin/sh\n[ \"$1\" = \"--probe\" ] && exit 0\n{body}\n"),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            for _ in 0..40 {
                match Command::new(&path)
                    .arg("--probe")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                {
                    Ok(_) => break,
                    Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => panic!("probe {}: {e}", path.display()),
                }
            }
            (dir, path)
        }

        fn read(shell: &Path, timeout: Duration) -> Option<String> {
            unix::read_login_path(shell, timeout, D)
        }

        #[test]
        fn banner_and_noise_around_the_delimiters_are_ignored() {
            // `read` proves stdin is null (it returns at once instead of waiting).
            let (dir, shell) = script(&format!(
                "read line\nprintf 'banner\\n\\033[1mMOTD\\033[0m\\n'\nprintf '%s' '{D}/fake/bin:/usr/bin{D}'\necho trailing noise"
            ));
            assert_eq!(
                read(&shell, Duration::from_secs(5)),
                Some("/fake/bin:/usr/bin".to_string())
            );
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn shell_gets_ilc_and_runs_the_command_string() {
            // A real sh runs our command string, so the quoting is tested too; any other first
            // argument than -ilc exits 3 without output.
            let (dir, shell) = script("[ \"$1\" = \"-ilc\" ] || exit 3\necho hello from the profile\nexec /bin/sh -c \"$2\"");
            let expected = std::env::var("PATH").unwrap_or_default();
            assert_eq!(
                read(&shell, Duration::from_secs(5)),
                Some(expected.trim().to_string())
            );
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn shell_runs_with_disable_auto_update() {
            let (dir, shell) = script(&format!("printf '%s' \"{D}${{DISABLE_AUTO_UPDATE}}{D}\""));
            assert_eq!(read(&shell, Duration::from_secs(5)), Some("1".to_string()));
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn a_hanging_shell_times_out() {
            let (dir, shell) = script("sleep 10\nprintf '%s' 'never'");
            let timeout = Duration::from_millis(500);
            let started = Instant::now();
            assert_eq!(read(&shell, timeout), None);
            assert!(
                started.elapsed() < timeout + Duration::from_secs(1),
                "{:?}",
                started.elapsed()
            );
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn output_without_delimiters_is_none() {
            let (dir, shell) = script("echo /usr/bin:/bin");
            let started = Instant::now();
            assert_eq!(read(&shell, Duration::from_secs(5)), None);
            assert!(started.elapsed() < Duration::from_secs(2));
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn one_delimiter_then_exit_is_none() {
            let (dir, shell) = script(&format!("printf '%s' '{D}/a/bin'"));
            assert_eq!(read(&shell, Duration::from_secs(5)), None);
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn a_daemon_holding_stdout_does_not_delay_the_answer() {
            // The profile leaves a background child with stdout open; the answer is taken as
            // soon as both delimiters arrived.
            let (dir, shell) = script(&format!("sleep 5 &\nprintf '%s' '{D}/d/bin{D}'"));
            let started = Instant::now();
            assert_eq!(
                read(&shell, Duration::from_secs(5)),
                Some("/d/bin".to_string())
            );
            assert!(started.elapsed() < Duration::from_secs(2));
            let _ = std::fs::remove_dir_all(dir);
        }

        #[test]
        fn real_interactive_login_shells_answer() {
            // bash/zsh as installed (Linux CI has bash, macOS has both): -ilc without a
            // controlling terminal must still print the PATH between the delimiters.
            for shell in ["/bin/bash", "/bin/zsh"] {
                if !Path::new(shell).is_file() {
                    continue;
                }
                let path = read(Path::new(shell), Duration::from_secs(10));
                assert!(
                    path.as_deref().is_some_and(|p| !p.is_empty()),
                    "{shell}: {path:?}"
                );
            }
        }

        #[test]
        fn a_missing_shell_is_none() {
            assert_eq!(
                read(Path::new("/no/such/shell-for-mira"), Duration::from_secs(1)),
                None
            );
        }
    }
}
