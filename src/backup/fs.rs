//! Writing a snapshot file safely: every file lands atomically (a fsync'd private temp
//! renamed over the target), created mode 0600 in a directory made mode 0700, since a
//! snapshot holds every private bookmark.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Candidate names [`create_new_temp`] tries before giving up.
const TEMP_FILE_ATTEMPTS: usize = 100;

/// Create `dir` (and its parents) mode 0700. A snapshot holds every private bookmark, so
/// the directory is no more readable than the files in it.
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

/// Atomically replace `path` with `body`: write a private, fsync'd temp file next to it
/// and rename it over the target. A partial or crashed write leaves the previous snapshot
/// intact rather than truncating it. Snapshots hold every private bookmark, so the file
/// is created mode 0600.
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
            let _ = std::fs::remove_file(&tmp);
        })
}

/// Open `tmp` as a brand-new mode-0600 regular file. `create_new` (O_EXCL) never follows a
/// pre-existing symlink and never reuses an existing file's contents or permissions, so the
/// 0600 promise holds; a path that already exists errors with `AlreadyExists` instead.
fn open_new_private(tmp: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)
}

/// Create a fresh, private temp file next to the backup target and return it with its path.
/// Each attempt mixes fresh entropy into the name, so a leftover temp from a crashed run
/// (even one with this pid) can't be reused; `open_new_private` guarantees the file is new.
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

/// Open a fresh private temp file in `dir`, drawing candidate names from `next_name` and
/// retrying whenever the chosen path already exists (a leftover temp, a squatter). Bounded
/// so a name generator that keeps yielding a colliding name can't spin forever.
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
    Err(last_err.expect("loop ran at least once"))
        .with_context(|| format!("creating a unique backup temp file in {}", dir.display()))
}

/// Write `body` to the already-opened temp `file` and fsync it, so the bytes are on disk
/// before the caller renames it over the real target.
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
