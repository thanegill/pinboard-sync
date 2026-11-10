//! Writing a snapshot file safely: every file lands atomically (a fsync'd private temp
//! renamed over the target), created mode 0600 in a directory made mode 0700, since a
//! snapshot holds every private bookmark.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Candidate names [`create_new_temp`] tries before giving up.
const TEMP_FILE_ATTEMPTS: usize = 100;

/// Create `dir` and its parents mode 0700, no more readable than the files in it.
pub(super) fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

/// Atomically replace `path` with `body`, through a private, fsync'd temp file renamed over
/// it, so a crashed write leaves the previous snapshot intact.
pub(super) fn write_atomically(path: &Path, body: &str) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("pinboard-backup");
    let (tmp, mut file) = create_backup_tmp(&dir, file_name)?;
    write_backup_tmp(&tmp, &mut file, body)
        .and_then(|()| {
            std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
            sync_dir(&dir);
            Ok(())
        })
        .inspect_err(|_| {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a temp left behind is harmless; the write's error is the one to report"
            )]
            let _ = std::fs::remove_file(&tmp);
        })
}

/// Open `tmp` as a new mode-0600 file. `create_new` (`O_EXCL`) never follows a symlink or
/// reuses an existing file, so the mode holds; an existing path is `AlreadyExists`.
fn open_new_private(tmp: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)
}

/// A new private temp file in `dir`, with its path. Each name draws fresh entropy, so a
/// crashed run's leftover, even with this pid, is never reused.
pub(super) fn create_backup_tmp(dir: &Path, file_name: &str) -> Result<(PathBuf, std::fs::File)> {
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let pid = std::process::id();

    create_new_temp(dir, || {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let nonce = nanos ^ u128::from(COUNTER.fetch_add(1, Ordering::Relaxed));
        format!(".{file_name}.tmp.{pid}.{nonce:x}")
    })
}

/// A new private temp file in `dir`, trying names from `next_name` while the path exists,
/// at most [`TEMP_FILE_ATTEMPTS`] times.
fn create_new_temp(
    dir: &Path,
    mut next_name: impl FnMut() -> String,
) -> Result<(PathBuf, std::fs::File)> {
    let mut last_err = None;
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let tmp = dir.join(next_name());
        match open_new_private(&tmp) {
            Ok(file) => return Ok((tmp, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => last_err = Some(e),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("creating backup temp file in {}", dir.display()));
            }
        }
    }
    #[expect(clippy::expect_used, reason = "`TEMP_FILE_ATTEMPTS` is not zero")]
    let last_err = last_err.expect("loop ran at least once");
    Err(last_err)
        .with_context(|| format!("creating a unique backup temp file in {}", dir.display()))
}

/// Write `body` to `file` and fsync it, so the bytes are on disk before the rename.
fn write_backup_tmp(tmp: &Path, file: &mut std::fs::File, body: &str) -> Result<()> {
    use std::io::Write;

    file.write_all(body.as_bytes())
        .with_context(|| format!("writing backup to {}", tmp.display()))?;
    file.sync_all()
        .with_context(|| format!("flushing backup to {}", tmp.display()))
}

/// Best-effort fsync of the directory so the rename itself survives a crash.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best effort: the rename has already landed"
        )]
        let _ = handle.sync_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_dir;

    #[test]
    fn backup_temp_refuses_to_reuse_a_preexisting_file() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = scratch_dir("backup-write-squat");
        let dir = scratch.path();
        let squatted = dir.join(".pinboard-backup.json.tmp.squatter");
        std::fs::write(&squatted, "SECRET-LEAK").unwrap();
        std::fs::set_permissions(&squatted, PermissionsExt::from_mode(0o644)).unwrap();

        // create_new refuses an existing path rather than truncating it and keeping its
        // world-readable mode, so a squatted temp can't leak into a snapshot.
        let err = open_new_private(&squatted).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&squatted).unwrap(), "SECRET-LEAK");
        let mode = std::fs::metadata(&squatted).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644, "existing file left untouched");
    }

    #[test]
    fn backup_temp_does_not_follow_a_symlink() {
        let scratch = scratch_dir("backup-write-symlink");
        let dir = scratch.path();
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "PRECIOUS").unwrap();
        let link = dir.join(".pinboard-backup.json.tmp.link");
        std::os::unix::fs::symlink(&victim, &link).unwrap();

        // O_EXCL refuses the pre-existing symlink instead of following it and truncating
        // the target.
        let err = open_new_private(&link).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "PRECIOUS");
    }

    #[test]
    fn create_new_temp_retries_past_a_colliding_name() {
        let scratch = scratch_dir("backup-temp-retry");
        let dir = scratch.path();
        std::fs::write(dir.join("taken"), "SQUAT").unwrap();

        // First candidate collides with the pre-existing file; the loop must retry the
        // fresh one rather than reuse or truncate "taken".
        let mut names = ["taken", "fresh"].into_iter().map(str::to_string);
        let (tmp, _file) = create_new_temp(dir, || names.next().unwrap()).unwrap();

        assert_eq!(tmp, dir.join("fresh"));
        assert_eq!(std::fs::read_to_string(dir.join("taken")).unwrap(), "SQUAT");
    }

    #[test]
    fn create_new_temp_gives_up_after_exhausting_attempts() {
        let scratch = scratch_dir("backup-temp-exhaust");
        let dir = scratch.path();
        std::fs::write(dir.join("always"), "SQUAT").unwrap();

        // A generator that never yields a free name exhausts the bounded loop and surfaces
        // the last AlreadyExists rather than spinning forever.
        let err = create_new_temp(dir, || "always".to_string()).unwrap_err();
        assert_eq!(
            err.downcast_ref::<std::io::Error>()
                .map(std::io::Error::kind),
            Some(std::io::ErrorKind::AlreadyExists)
        );
    }
}
