//! The Unix domain socket the hooks connect to (Linux for tests, macOS in production):
//! `$TMPDIR/mira-bots-<uid>/mira-bots-<pid>.sock` in a private directory (0700, file 0600).
//! When `$TMPDIR` makes that longer than `SOCKET_PATH_MAX` (`sun_path` is 104 bytes on macOS),
//! the short fallback `/tmp/mira-bots-<uid>/<pid>.sock` is used; only when that is too long as
//! well does the server refuse to start. Removed when the server task ends, on `RunEvent::Exit`
//! and from the panic hook.
// TODO(macos-verify): Diagnostik shows `/var/folders/…/T/mira-bots-<uid>/mira-bots-<pid>.sock`,
// the directory is `drwx------`, the file `srw-------`, hooks work, and both are gone after quit
// and after a crash (plan7 M.9). A 110-character `$TMPDIR` gives `/tmp/mira-bots-<uid>/<pid>.sock`
// and a Pipe-note saying the fallback is in use (plan7 M.15).

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::config::{SOCKET_DIR_PREFIX, SOCKET_PATH_MAX};

/// Mode of the socket directory.
pub const DIR_MODE: u32 = 0o700;
/// Mode of the socket file (set right after bind).
pub const FILE_MODE: u32 = 0o600;

/// `<tmp>/mira-bots-<uid>` (SOCKET_DIR_PREFIX + uid).
pub fn socket_dir(tmp: &Path, uid: u32) -> PathBuf {
    tmp.join(format!("{SOCKET_DIR_PREFIX}{uid}"))
}

/// `<dir>/mira-bots-<pid>.sock`.
pub fn socket_file(dir: &Path, pid: u32) -> PathBuf {
    dir.join(format!("mira-bots-{pid}.sock"))
}

/// Base directory of the fallback socket path.
pub const FALLBACK_BASE: &str = "/tmp";

/// `<base>/mira-bots-<uid>/<pid>.sock`: the short fallback name (same private directory
/// scheme, shorter file name).
pub fn fallback_file(base: &Path, uid: u32, pid: u32) -> PathBuf {
    socket_dir(base, uid).join(format!("{pid}.sock"))
}

/// `primary` when it fits in SOCKET_PATH_MAX bytes, else `fallback` when that fits, else the
/// shorter of the two (the caller's `check_length` then reports it).
pub fn first_that_fits(primary: PathBuf, fallback: PathBuf) -> PathBuf {
    if check_length(&primary).is_ok() {
        return primary;
    }
    if check_length(&fallback).is_ok() || fallback.as_os_str().len() < primary.as_os_str().len() {
        fallback
    } else {
        primary
    }
}

/// [`choose_path_in`] with the fallback under [`FALLBACK_BASE`]. Never fails (the caller runs
/// `check_length`).
pub fn choose_path(tmp: &Path, uid: u32, pid: u32) -> PathBuf {
    choose_path_in(tmp, Path::new(FALLBACK_BASE), uid, pid)
}

/// `socket_file(socket_dir(tmp, uid), pid)`; longer than SOCKET_PATH_MAX bytes →
/// `fallback_file(fallback_base, uid, pid)` (see [`first_that_fits`]).
pub fn choose_path_in(tmp: &Path, fallback_base: &Path, uid: u32, pid: u32) -> PathBuf {
    first_that_fits(
        socket_file(&socket_dir(tmp, uid), pid),
        fallback_file(fallback_base, uid, pid),
    )
}

/// Whether `path` has the fallback's file name (`<digits>.sock`).
pub fn is_fallback(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".sock"))
        .is_some_and(|stem| !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()))
}

/// Diagnostik text when `path` is the fallback: "Socket-stien under TMPDIR ville være over
/// {SOCKET_PATH_MAX} tegn; bruger {dir} i stedet". `None` otherwise.
pub fn fallback_note(path: &Path) -> Option<String> {
    is_fallback(path).then(|| {
        let dir = path.parent().unwrap_or(Path::new(FALLBACK_BASE)).display();
        format!("Socket-stien under TMPDIR ville være over {SOCKET_PATH_MAX} tegn; bruger {dir} i stedet")
    })
}

/// Err when `path.as_os_str().len() > SOCKET_PATH_MAX`:
/// "Socket-stien er for lang ({len} tegn, højst {SOCKET_PATH_MAX}): sæt TMPDIR til en kortere mappe".
pub fn check_length(path: &Path) -> Result<(), String> {
    let len = path.as_os_str().len();
    if len > SOCKET_PATH_MAX {
        Err(format!(
            "Socket-stien er for lang ({len} tegn, højst {SOCKET_PATH_MAX}): sæt TMPDIR til en kortere mappe"
        ))
    } else {
        Ok(())
    }
}

/// Creates the parent dir with 0700 or verifies it: owner != uid →
/// "Socket-mappen {dir} tilhører en anden bruger (uid {owner})"; mode & 0o077 != 0 →
/// "Socket-mappen {dir} er ikke privat (rettigheder {mode:o}); slet den og start appen igen";
/// create failure → "Kunne ikke oprette socket-mappen {dir}: {e}". Removes a stale socket
/// file (errors ignored). Texts travel as `io::Error::other(text)`.
/// An existing directory is never chmod'ed, and a symlink is not followed (it is not private).
pub fn prepare(path: &Path, uid: u32) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("/"));
    let shown = dir.display();
    match fs::DirBuilder::new().mode(DIR_MODE).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(dir).map_err(|e| {
                io::Error::other(format!("Kunne ikke oprette socket-mappen {shown}: {e}"))
            })?;
            if !meta.file_type().is_dir() {
                return Err(io::Error::other(format!(
                    "Kunne ikke oprette socket-mappen {shown}: der ligger allerede noget andet end en mappe"
                )));
            }
            if meta.uid() != uid {
                return Err(io::Error::other(format!(
                    "Socket-mappen {shown} tilhører en anden bruger (uid {})",
                    meta.uid()
                )));
            }
            let mode = meta.mode() & 0o7777;
            if mode & 0o077 != 0 {
                return Err(io::Error::other(format!(
                    "Socket-mappen {shown} er ikke privat (rettigheder {mode:o}); slet den og start appen igen"
                )));
            }
        }
        Err(e) => {
            return Err(io::Error::other(format!(
                "Kunne ikke oprette socket-mappen {shown}: {e}"
            )))
        }
    }
    // A socket left behind by a crashed run (or a reused pid).
    let _ = fs::remove_file(path);
    Ok(())
}

/// `set_permissions(path, 0o600)` right after bind.
pub fn restrict(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(FILE_MODE))
}

/// Removes the file and, if empty, its dir (only a `mira-bots-…` dir). Idempotent; errors
/// ignored.
pub fn cleanup(path: &Path) {
    let _ = fs::remove_file(path);
    if let Some(dir) = path.parent() {
        let ours = dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(SOCKET_DIR_PREFIX));
        if ours {
            // Fails while other instances still have their socket in it: fine.
            let _ = fs::remove_dir(dir);
        }
    }
}

static REGISTERED: OnceLock<PathBuf> = OnceLock::new();

/// Remembers `path` (OnceLock) for `cleanup_registered()` (RunEvent::Exit, panic hook).
pub fn register(path: PathBuf) {
    let _ = REGISTERED.set(path);
}

/// `cleanup` of the registered socket, if any. Safe to call more than once.
pub fn cleanup_registered() {
    if let Some(path) = REGISTERED.get() {
        cleanup(path);
    }
}

/// RAII: `cleanup` on drop (server task end or abort).
pub struct SocketGuard(PathBuf);

impl SocketGuard {
    pub fn new(path: PathBuf) -> Self {
        Self(path)
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        cleanup(&self.0);
    }
}

/// The real user id of this process.
pub fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, not yet existing `mira-bots-test-<uuid>` dir under the temp dir.
    fn fresh_dir() -> PathBuf {
        std::env::temp_dir().join(format!("{SOCKET_DIR_PREFIX}test-{}", uuid::Uuid::new_v4()))
    }

    fn mode(p: &Path) -> u32 {
        fs::symlink_metadata(p).unwrap().mode() & 0o777
    }

    #[test]
    fn socket_dir_and_file_names() {
        let dir = socket_dir(Path::new("/var/folders/ab/T"), 501);
        assert_eq!(dir, PathBuf::from("/var/folders/ab/T/mira-bots-501"));
        assert_eq!(
            socket_file(&dir, 4242),
            PathBuf::from("/var/folders/ab/T/mira-bots-501/mira-bots-4242.sock")
        );
        assert_eq!(
            choose_path(Path::new("/var/folders/ab/T"), 501, 4242),
            PathBuf::from("/var/folders/ab/T/mira-bots-501/mira-bots-4242.sock")
        );
        // A typical macOS $TMPDIR (49 characters) with a 7-digit pid stays well inside.
        let mac_tmp = Path::new("/var/folders/zz/zyxvpxvq6csfxvn_n0000000000000/T/");
        assert_eq!(mac_tmp.as_os_str().len(), 49);
        let p = choose_path(mac_tmp, 501, 9_999_999);
        assert!(p.starts_with(mac_tmp));
        assert!(check_length(&p).is_ok());
    }

    #[test]
    fn choose_path_falls_back_to_tmp_when_tmpdir_is_long() {
        let long = PathBuf::from(format!("/{}", "t".repeat(119)));
        assert_eq!(long.as_os_str().len(), 120);
        let p = choose_path(&long, 501, 4242);
        assert_eq!(p, PathBuf::from("/tmp/mira-bots-501/4242.sock"));
        assert!(check_length(&p).is_ok());
        assert!(is_fallback(&p));
        // Exactly at the limit: no fallback.
        let base = socket_file(&socket_dir(Path::new("/x"), 501), 4242);
        let pad = SOCKET_PATH_MAX - base.as_os_str().len();
        let tmp = PathBuf::from(format!("/{}", "x".repeat(pad + 1)));
        let p = choose_path(&tmp, 501, 4242);
        assert_eq!(p.as_os_str().len(), SOCKET_PATH_MAX);
        assert!(p.starts_with(&tmp));
        assert!(!is_fallback(&p));
        // One more character: the fallback.
        let tmp = PathBuf::from(format!("/{}", "x".repeat(pad + 2)));
        assert_eq!(
            choose_path(&tmp, 501, 4242),
            PathBuf::from("/tmp/mira-bots-501/4242.sock")
        );
    }

    #[test]
    fn a_mac_runner_tmpdir_with_a_long_test_dir_still_fits_via_the_fallback() {
        // GitHub's macOS runner: $TMPDIR is /var/folders/xx/<30>/T/ (49 characters).
        let mac_tmp = Path::new("/var/folders/zz/zyxvpxvq6csfxvn_n0000000000000/T/");
        let deep = mac_tmp.join("x".repeat(60));
        let p = choose_path(&deep, 501, 9_999_999);
        assert_eq!(p, PathBuf::from("/tmp/mira-bots-501/9999999.sock"));
        assert!(check_length(&p).is_ok());
    }

    #[test]
    fn too_long_is_reported_only_when_the_fallback_is_too_long_too() {
        // Injected bases instead of a real long $TMPDIR.
        let long_tmp = PathBuf::from(format!("/{}", "t".repeat(119)));
        let long_base = PathBuf::from(format!("/{}", "f".repeat(110)));
        let p = choose_path_in(&long_tmp, &long_base, 501, 4242);
        assert_eq!(p, fallback_file(&long_base, 501, 4242), "the shorter one");
        let err = check_length(&p).unwrap_err();
        assert!(err.starts_with("Socket-stien er for lang ("), "{err}");
        assert!(err.ends_with("sæt TMPDIR til en kortere mappe"), "{err}");
        // A short fallback base fits.
        let p = choose_path_in(&long_tmp, Path::new("/fb"), 501, 4242);
        assert_eq!(p, PathBuf::from("/fb/mira-bots-501/4242.sock"));
        assert!(check_length(&p).is_ok());
        // A fallback that is longer than the (too long) primary is not taken.
        let longer_base = PathBuf::from(format!("/{}", "f".repeat(200)));
        let p = choose_path_in(&long_tmp, &longer_base, 501, 4242);
        assert!(p.starts_with(&long_tmp));
        assert!(check_length(&p).is_err());
    }

    #[test]
    fn fallback_is_recognised_by_its_file_name() {
        assert!(is_fallback(Path::new("/tmp/mira-bots-501/4242.sock")));
        assert!(!is_fallback(Path::new(
            "/t/mira-bots-501/mira-bots-4242.sock"
        )));
        assert!(!is_fallback(Path::new("pipe")));
        assert!(!is_fallback(Path::new("/x/.sock")));
        assert!(!is_fallback(Path::new("/x/e2e.sock")));
        assert_eq!(
            fallback_note(Path::new("/tmp/mira-bots-501/4242.sock")).as_deref(),
            Some("Socket-stien under TMPDIR ville være over 100 tegn; bruger /tmp/mira-bots-501 i stedet")
        );
        assert_eq!(
            fallback_note(Path::new("/t/mira-bots-501/mira-bots-1.sock")),
            None
        );
    }

    #[test]
    fn check_length_rejects_over_100_with_danish_text() {
        let ok = PathBuf::from(format!("/{}", "a".repeat(99)));
        assert!(check_length(&ok).is_ok());
        let long = PathBuf::from(format!("/{}", "a".repeat(100)));
        assert_eq!(
            check_length(&long).unwrap_err(),
            "Socket-stien er for lang (101 tegn, højst 100): sæt TMPDIR til en kortere mappe"
        );
    }

    #[test]
    fn prepare_creates_private_dir_and_removes_stale_file() {
        let dir = fresh_dir();
        let path = socket_file(&dir, 7);
        prepare(&path, uid()).unwrap();
        assert!(dir.is_dir());
        assert_eq!(mode(&dir), DIR_MODE);
        // A stale file from a crashed run is removed; the existing private dir is accepted.
        fs::write(&path, b"stale").unwrap();
        prepare(&path, uid()).unwrap();
        assert!(!path.exists());
        assert_eq!(mode(&dir), DIR_MODE);
        cleanup(&path);
        assert!(!dir.exists());
    }

    #[test]
    fn prepare_rejects_world_readable_dir() {
        let dir = fresh_dir();
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let err = prepare(&socket_file(&dir, 7), uid())
            .unwrap_err()
            .to_string();
        assert!(err.contains("ikke privat"), "{err}");
        assert!(err.contains("rettigheder 755"), "{err}");
        assert!(err.contains("slet den og start appen igen"), "{err}");
        // Never chmod'ed.
        assert_eq!(mode(&dir), 0o755);
        fs::remove_dir(&dir).unwrap();
    }

    #[test]
    fn prepare_rejects_a_dir_of_another_user() {
        let dir = fresh_dir();
        let path = socket_file(&dir, 7);
        prepare(&path, uid()).unwrap();
        let other = uid().wrapping_add(1);
        let err = prepare(&path, other).unwrap_err().to_string();
        assert!(
            err.contains(&format!("tilhører en anden bruger (uid {})", uid())),
            "{err}"
        );
        cleanup(&path);
    }

    #[test]
    fn prepare_failure_on_a_file_says_so() {
        let dir = fresh_dir();
        fs::write(&dir, b"not a dir").unwrap();
        let err = prepare(&socket_file(&dir, 7), uid())
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("Kunne ikke oprette socket-mappen"), "{err}");
        fs::remove_file(&dir).unwrap();
    }

    #[test]
    fn cleanup_removes_empty_dir() {
        let dir = fresh_dir();
        let a = socket_file(&dir, 1);
        let b = socket_file(&dir, 2);
        prepare(&a, uid()).unwrap();
        fs::write(&a, b"").unwrap();
        fs::write(&b, b"").unwrap();
        cleanup(&a);
        assert!(!a.exists());
        assert!(dir.exists(), "another instance's socket is still there");
        cleanup(&b);
        assert!(!dir.exists());
        cleanup(&b); // idempotent
    }

    #[test]
    fn cleanup_never_removes_a_foreign_dir() {
        let parent = fresh_dir();
        let foreign = parent.join("other");
        fs::create_dir_all(&foreign).unwrap();
        cleanup(&foreign.join("x.sock"));
        assert!(foreign.exists());
        fs::remove_dir_all(&parent).unwrap();
    }

    #[test]
    fn guard_cleans_up_on_drop() {
        let dir = fresh_dir();
        let path = socket_file(&dir, 3);
        prepare(&path, uid()).unwrap();
        fs::write(&path, b"").unwrap();
        restrict(&path).unwrap();
        assert_eq!(mode(&path), FILE_MODE);
        drop(SocketGuard::new(path.clone()));
        assert!(!path.exists());
        assert!(!dir.exists());
    }
}
