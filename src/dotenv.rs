//! Loading a `.env`: the first one found in the working directory or above it, parsed by
//! `dotenvy`, sets each variable the environment doesn't. A `.env` that can't be read or
//! parsed is an error, which names the line but never quotes it: a `.env` holds secrets.

use std::cell::Cell;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::{env, fs};

use anyhow::{anyhow, Context, Result};

const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";

const PARSE_HINT: &str = "look for a missing `=`, a key that doesn't start with a letter or \
                          `_` or has anything but letters, digits, `_` and `.`, an unquoted \
                          space, a quote or `${` left open, or an unknown `\\` escape";

/// Load the first `.env` in the working directory or an ancestor, setting each variable
/// the environment doesn't already set, so the environment outranks the file and the file's
/// first entry for a key outranks a later one. No `.env` anywhere is not an error. The
/// whole file is parsed before anything is set, so a `$KEY` naming a key the file gives
/// twice reads the last value given before it, where `dotenvy::dotenv` reads the first.
pub fn load() -> Result<()> {
    for (key, value) in entries_above(env::current_dir())? {
        if crate::secrets::env_var(&key).is_none() {
            #[expect(
                clippy::disallowed_methods,
                reason = "loading a `.env` is setting the process environment, which clap's env \
                          fallbacks and `secrets::env_var` then read"
            )]
            env::set_var(key, value);
        }
    }
    Ok(())
}

/// The entries of the first `.env` in `working_dir` or an ancestor, or none when there is
/// none or the working directory no longer exists.
fn entries_above(working_dir: io::Result<PathBuf>) -> Result<Vec<(String, String)>> {
    match working_dir {
        Ok(dir) => read_first(dir.ancestors()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).context("reading the working directory to find a .env"),
    }
}

/// The entries of the first of `dirs` holding a `.env` file, in file order, if any.
fn read_first<'a>(dirs: impl IntoIterator<Item = &'a Path>) -> Result<Vec<(String, String)>> {
    find(dirs)?.map_or_else(|| Ok(Vec::new()), |path| read(&path))
}

fn find<'a>(dirs: impl IntoIterator<Item = &'a Path>) -> Result<Option<PathBuf>> {
    for dir in dirs {
        let candidate = dir.join(".env");
        match fs::metadata(&candidate) {
            Ok(metadata) if metadata.is_file() => return Ok(Some(candidate)),
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("checking for {}", candidate.display()));
            }
            _ => {}
        }
    }
    Ok(None)
}

/// The entries of the `.env` at `path`. A last line without a newline gets one, since
/// dotenvy refuses one that ends in an odd number of spaces or a `\`.
fn read(path: &Path) -> Result<Vec<(String, String)>> {
    let mut bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.last().is_some_and(|&last| last != b'\n') {
        bytes.push(b'\n');
    }
    let lines_read = Cell::new(0);
    let reader = LineAtATime {
        rest: bytes.strip_prefix(UTF8_BOM).unwrap_or(&bytes),
        lines_read: &lines_read,
    };
    dotenvy::from_read_iter(reader)
        .map(|entry| entry.map_err(|e| describe(e, lines_read.get())))
        .collect::<Result<_>>()
        .with_context(|| format!("loading {}", path.display()))
}

/// What is wrong with a `.env`, `lines_read` lines into it, never quoting the line as
/// [`dotenvy::Error`]'s `Display` does. A parse error names the line its entry starts on.
fn describe(error: dotenvy::Error, lines_read: usize) -> anyhow::Error {
    match error {
        dotenvy::Error::LineParse(entry, _) => {
            let lines_after_first = entry
                .strip_suffix('\n')
                .unwrap_or(&entry)
                .matches('\n')
                .count();
            let first_line = lines_read.saturating_sub(lines_after_first);
            anyhow!("line {first_line} can't be parsed: {PARSE_HINT}")
        }
        dotenvy::Error::Io(e) => anyhow::Error::new(e).context(format!("line {lines_read}")),
        dotenvy::Error::EnvVar(_) | _ => anyhow!("line {lines_read} can't be parsed"),
    }
}

/// A reader handing out at most one line per `read`, counting each line it finishes. The
/// parser reads through a `BufReader`, which reads again only once it has used up what it
/// holds, so when the parser fails it has read exactly `lines_read` lines.
struct LineAtATime<'a> {
    rest: &'a [u8],
    lines_read: &'a Cell<usize>,
}

impl Read for LineAtATime<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let line_len = self
            .rest
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(self.rest.len(), |newline| newline + 1);
        let len = line_len.min(buf.len());
        let (chunk, rest) = self.rest.split_at(len);
        for (byte, &chunk_byte) in buf.iter_mut().zip(chunk) {
            *byte = chunk_byte;
        }
        self.rest = rest;
        if len > 0 && len == line_len {
            self.lines_read.set(self.lines_read.get() + 1);
        }
        Ok(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_dir;

    const SECRET: &str = "s3cret-value";

    fn read_text(text: &str) -> Result<Vec<(String, String)>> {
        let dir = scratch_dir("dotenv");
        fs::write(dir.path().join(".env"), text)?;
        read_first([dir.path()])
    }

    fn entries(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|&(key, value)| (key.to_owned(), value.to_owned()))
            .collect()
    }

    #[test]
    fn a_malformed_env_names_the_line_but_not_its_text() {
        for (text, line) in [
            (format!("KEY {SECRET}\n"), 1),
            (format!("A=1\n\n# a comment\nB = {SECRET} \"\n"), 4),
            (format!("A=1\nB=\"{SECRET}\nC=2\n"), 2),
            (format!("A=1\nB=\"{SECRET}\nC=2"), 2),
            (format!("A=\"x\ny\"\nB'{SECRET}'\n"), 3),
            (format!("A=1\nB=\"{SECRET}\nC=\"2\"\n"), 2),
            (format!("Z=0\nA=\"{SECRET}\ny\"${{B\n"), 2),
            (format!("A=1\nB=\"{SECRET}\n\n"), 2),
            (format!("A=1\nB=\"{SECRET}\n   \n"), 2),
            (format!("A=1\nB=\"{SECRET}\nC=2\n\n\n"), 2),
            (format!("A=1\r\nB=\"{SECRET}\r\n\r\n"), 2),
            (format!("A=1\nB={SECRET} two\n"), 2),
            (format!("A=1\nMY-KEY={SECRET}\n"), 2),
            (format!("A=1\n2FA={SECRET}\n"), 2),
        ] {
            let error = format!("{:#}", read_text(&text).unwrap_err());
            assert!(error.contains("/.env: "), "{error}");
            assert!(
                error.ends_with(&format!(": line {line} can't be parsed: {PARSE_HINT}")),
                "{text:?}: {error}"
            );
            assert!(!error.contains(SECRET), "{error}");
        }
    }

    #[test]
    fn a_line_that_is_not_utf8_is_named_by_number() {
        let dir = scratch_dir("dotenv");
        fs::write(dir.path().join(".env"), b"A=1\nB=\xFF\n").unwrap();
        let error = format!("{:#}", read_first([dir.path()]).unwrap_err());
        assert!(
            error.ends_with(": line 2: stream did not contain valid UTF-8"),
            "{error}"
        );
    }

    #[test]
    fn no_env_is_no_entries() {
        let dir = scratch_dir("dotenv");
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::create_dir(dir.path().join("sub/.env")).unwrap();
        let sub = dir.path().join("sub");
        assert_eq!(read_first([sub.as_path(), dir.path()]).unwrap(), []);
        let gone = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(entries_above(Err(gone)).unwrap(), []);
    }

    #[test]
    fn a_place_the_env_cannot_be_looked_for_in_is_an_error() {
        let unreadable = io::Error::from(io::ErrorKind::PermissionDenied);
        let error = format!("{:#}", entries_above(Err(unreadable)).unwrap_err());
        assert!(
            error.starts_with("reading the working directory"),
            "{error}"
        );

        let dir = scratch_dir("dotenv");
        let file = dir.path().join("file");
        fs::write(&file, "").unwrap();
        let error = format!("{:#}", read_first([file.as_path()]).unwrap_err());
        assert!(error.starts_with("checking for "), "{error}");
    }

    #[test]
    fn a_valid_env_is_read_from_the_nearest_directory_holding_one() {
        let dir = scratch_dir("dotenv");
        let nested = dir.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        fs::write(dir.path().join(".env"), "FAR=1\n").unwrap();
        fs::write(
            dir.path().join("a/.env"),
            "\u{feff}# credentials\nA=1\nexport B = \"two words\" # note\n\nC='x\ny'\nA=2 ",
        )
        .unwrap();
        let dirs = nested.ancestors().take_while(|d| d.starts_with(dir.path()));
        assert_eq!(
            read_first(dirs).unwrap(),
            entries(&[("A", "1"), ("B", "two words"), ("C", "x\ny"), ("A", "2")])
        );
    }
}
