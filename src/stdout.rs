//! Command output on stdout, where a reader that has gone away is not an error.
//!
//! `println!` panics when stdout is a closed pipe (`pinboard-sync config example | head`),
//! exiting 101. Every command writes its output through here instead, which drops what
//! the reader no longer wants and carries on.

use std::fmt;
use std::io::{self, Write};

/// Write `text` to stdout, unchanged. A closed pipe is `Ok`; any other failure is the
/// caller's to report.
#[cfg(not(test))]
pub fn write(text: impl AsRef<[u8]>) -> io::Result<()> {
    write_to(&mut io::stdout().lock(), text.as_ref())
}

/// Tests write into [`crate::test_support::capturing_stdout`] when inside one, else through
/// `print!`, the only output libtest captures.
#[cfg(test)]
pub fn write(text: impl AsRef<[u8]>) -> io::Result<()> {
    let text = String::from_utf8_lossy(text.as_ref());
    if !crate::test_support::capture_stdout(&text) {
        print!("{text}");
    }
    Ok(())
}

/// `println!` for line-by-line output that carries on after a closed pipe, the dry-run
/// previews and `doctor`: a closed pipe drops the line, and any other failure panics as
/// `println!` does.
macro_rules! outln {
    ($($arg:tt)*) => {
        $crate::stdout::line(format_args!($($arg)*))
    };
}
pub(crate) use outln;

#[doc(hidden)]
pub fn line(line: fmt::Arguments<'_>) {
    if let Err(e) = write(format!("{line}\n")) {
        panic!("failed printing to stdout: {e}");
    }
}

fn write_to(out: &mut impl Write, text: &[u8]) -> io::Result<()> {
    match out.write_all(text).and_then(|()| out.flush()) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingWriter(io::ErrorKind);

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(self.0.into())
        }
    }

    #[test]
    fn a_closed_pipe_is_not_an_error() {
        write_to(&mut FailingWriter(io::ErrorKind::BrokenPipe), b"output\n").unwrap();
    }

    #[test]
    fn any_other_write_failure_is() {
        let error =
            write_to(&mut FailingWriter(io::ErrorKind::StorageFull), b"output\n").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    }

    #[test]
    fn the_text_is_written_unchanged() {
        let mut out = Vec::new();
        write_to(&mut out, b"[dry-run] a\n          b\n").unwrap();
        assert_eq!(out, b"[dry-run] a\n          b\n");
    }
}
