//! Ending an agent's child and giving it a terminal environment. Unix: the child is a
//! session/process-group leader (portable-pty calls setsid), so SIGTERM goes to the whole
//! group and SIGKILL follows after a grace period. Windows: portable-pty's killer
//! (TerminateProcess) as before; a Job Object for the tree is backlog.
//!
//! PID reuse: `killpg` is only sent while `exited` is false, i.e. before the waiter thread has
//! reaped the leader, so its pid (= the group id) cannot have been reused yet. The window
//! between reading the flag and the SIGKILL is microseconds; accepted (plan7 G.1). Children that
//! call `setsid` themselves leave the group and survive (plan7 G.2, descendant walk is backlog).
// TODO(macos-verify): stopping an agent ends claude, node, mira-mcp and Bash-tool children
// (`ps -o pid,pgid,comm`), also during `sleep 60`; SIGKILL after 2 s for a child that ignores
// SIGTERM (plan7 M.11). Quit (island button or Cmd+Q) leaves no processes and no zombies
// within about 2 s (plan7 M.12).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use portable_pty::ChildKiller;

use super::AgentError;
use crate::config::{COLORTERM_DEFAULT, TERM_DEFAULT};

/// Env added to every child before `SpawnSpec.env` (which wins). Unix: `TERM`/`COLORTERM`
/// when the app's own env lacks them (Finder/Dock start) and `PATH` from the login shell
/// when `login_env::source() == "login-shell"`. Windows: empty.
pub fn spawn_env_extra() -> Vec<(String, String)> {
    #[cfg(unix)]
    {
        let has = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        terminal_env_defaults_for(has("TERM"), has("COLORTERM"))
    }
    #[cfg(windows)]
    {
        Vec::new()
    }
}

/// Pure part of [`spawn_env_extra`] (tested on both platforms): the defaults for whichever of
/// `TERM`/`COLORTERM` is missing.
pub fn terminal_env_defaults_for(has_term: bool, has_colorterm: bool) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if !has_term {
        out.push(("TERM".to_string(), TERM_DEFAULT.to_string()));
    }
    if !has_colorterm {
        out.push(("COLORTERM".to_string(), COLORTERM_DEFAULT.to_string()));
    }
    out
}

/// Starts ending the child. Never blocks.
/// Unix: `killpg(pid, SIGTERM)`; ESRCH or `pid == None` → `killer.kill()`; then a detached
/// thread `pty-reaper-<pid>` sleeps `grace` and sends `killpg(pid, SIGKILL)` unless `exited`
/// is set (set by the waiter thread BEFORE on_exit, i.e. never under the manager lock).
/// Nothing is sent once `exited` is set (the pid may be reused).
/// Windows: `killer.kill()`.
pub fn terminate(
    pid: Option<u32>,
    exited: &Arc<AtomicBool>,
    killer: &mut (dyn ChildKiller + Send + Sync),
    grace: Duration,
) -> Result<(), AgentError> {
    #[cfg(unix)]
    {
        unix::terminate(pid, exited, killer, grace)
    }
    #[cfg(windows)]
    {
        let _ = (pid, exited, grace);
        killer.kill()?;
        Ok(())
    }
}

/// Quit path, after `terminate` was called for every child: unix polls every 50 ms up to
/// `budget` until each `exited` is set or the group is gone, then SIGKILLs the rest.
/// Windows: returns at once. May be called under the manager lock (see `exited` above).
pub fn finish_all(children: &[(u32, Arc<AtomicBool>)], budget: Duration) {
    #[cfg(unix)]
    {
        unix::finish_all(children, budget);
    }
    #[cfg(windows)]
    {
        let _ = (children, budget);
    }
}

#[cfg(unix)]
/// `kill(-pid, 0)`: false on ESRCH, true otherwise (EPERM counts as alive).
pub(crate) fn group_alive(pid: u32) -> bool {
    match unix::group_id(Some(pid)) {
        Some(pgid) => match unix::signal_group(pgid, 0) {
            Ok(()) => true,
            Err(e) => e.raw_os_error() != Some(libc::ESRCH),
        },
        None => false,
    }
}

#[cfg(unix)]
mod unix {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use portable_pty::ChildKiller;

    use super::{group_alive, AgentError};

    const POLL: Duration = Duration::from_millis(50);

    /// The group id to signal: the child's pid (it is the group leader). `None` for a missing
    /// pid, one above `i32::MAX`, 0/1, or our own group (never signal the app itself).
    pub(super) fn group_id(pid: Option<u32>) -> Option<i32> {
        let pgid = i32::try_from(pid?).ok()?;
        // SAFETY: getpgrp has no preconditions.
        let own = unsafe { libc::getpgrp() };
        (pgid > 1 && pgid != own).then_some(pgid)
    }

    pub(super) fn signal_group(pgid: i32, sig: libc::c_int) -> io::Result<()> {
        // SAFETY: killpg only takes integers; `group_id` excluded 0, 1 and our own group.
        if unsafe { libc::killpg(pgid, sig) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub(super) fn terminate(
        pid: Option<u32>,
        exited: &Arc<AtomicBool>,
        killer: &mut (dyn ChildKiller + Send + Sync),
        grace: Duration,
    ) -> Result<(), AgentError> {
        if exited.load(Ordering::SeqCst) {
            return Ok(());
        }
        let Some(pgid) = group_id(pid) else {
            killer.kill()?;
            return Ok(());
        };
        match signal_group(pgid, libc::SIGTERM) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {
                killer.kill()?;
                return Ok(());
            }
            Err(e) => {
                log::debug!("killpg({pgid}, SIGTERM): {e}");
                return killer
                    .kill()
                    .map_err(|k| AgentError::Pty(format!("killpg: {e}; kill: {k}")));
            }
        }
        let flag = Arc::clone(exited);
        let spawned = std::thread::Builder::new()
            .name(format!("pty-reaper-{pgid}"))
            .spawn(move || {
                std::thread::sleep(grace);
                if !flag.load(Ordering::SeqCst) {
                    match signal_group(pgid, libc::SIGKILL) {
                        Ok(()) => log::info!("agent process group {pgid}: SIGKILL after grace"),
                        Err(e) => log::debug!("killpg({pgid}, SIGKILL): {e}"),
                    }
                }
            });
        if let Err(e) = spawned {
            log::warn!("could not start the reaper thread for group {pgid}: {e}");
        }
        Ok(())
    }

    pub(super) fn finish_all(children: &[(u32, Arc<AtomicBool>)], budget: Duration) {
        let deadline = Instant::now() + budget;
        loop {
            let pending: Vec<(i32, &Arc<AtomicBool>)> = children
                .iter()
                .filter(|(pid, flag)| !flag.load(Ordering::SeqCst) && group_alive(*pid))
                .filter_map(|(pid, flag)| group_id(Some(*pid)).map(|g| (g, flag)))
                .collect();
            if pending.is_empty() {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                for (pgid, flag) in pending {
                    if !flag.load(Ordering::SeqCst) {
                        match signal_group(pgid, libc::SIGKILL) {
                            Ok(()) => log::info!("quit: SIGKILL to agent process group {pgid}"),
                            Err(e) => log::debug!("quit: killpg({pgid}, SIGKILL): {e}"),
                        }
                    }
                }
                return;
            }
            std::thread::sleep(POLL.min(deadline - now));
        }
    }
}

/// Test helper: the process is gone, or (Linux) a zombie nobody reaps — container PID 1 may
/// not reap orphans (env.md).
#[cfg(all(test, unix))]
pub(crate) fn pid_is_dead(pid: u32) -> bool {
    let Ok(p) = i32::try_from(pid) else {
        return true;
    };
    // SAFETY: signal 0 only checks existence.
    if unsafe { libc::kill(p, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        return true;
    }
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // "<pid> (<comm>) <state> …"; comm may contain ')' so split at the last one.
        if let Some((_, rest)) = stat.rsplit_once(')') {
            return rest.trim_start().starts_with('Z');
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_env_defaults_fill_only_what_is_missing() {
        let both = terminal_env_defaults_for(false, false);
        assert_eq!(
            both,
            vec![
                ("TERM".to_string(), "xterm-256color".to_string()),
                ("COLORTERM".to_string(), "truecolor".to_string()),
            ]
        );
        assert_eq!(
            terminal_env_defaults_for(true, false),
            vec![("COLORTERM".to_string(), "truecolor".to_string())]
        );
        assert!(terminal_env_defaults_for(true, true).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn group_ids_never_include_our_own_group_or_init() {
        let own = unsafe { libc::getpgrp() } as u32;
        assert_eq!(unix::group_id(Some(own)), None);
        assert_eq!(unix::group_id(Some(0)), None);
        assert_eq!(unix::group_id(Some(1)), None);
        assert_eq!(unix::group_id(None), None);
        assert_eq!(unix::group_id(Some(u32::MAX)), None);
        assert!(!group_alive(u32::MAX));
    }

    #[cfg(windows)]
    #[test]
    fn spawn_env_extra_is_empty_on_windows() {
        assert!(spawn_env_extra().is_empty());
    }
}
