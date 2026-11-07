//! The `#!` line of an executable: the interpreter the kernel forwards to.

use super::is_absolute;
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// The absolute interpreter `file`'s first line names, read as the host's
/// kernel reads it.
pub(crate) fn shebang(file: File) -> Option<PathBuf> {
    const LIMIT: u64 = if cfg!(target_os = "macos") { 512 } else { 256 };
    let mut head = Vec::new();
    file.take(LIMIT).read_to_end(&mut head).ok()?;
    let line = head.strip_prefix(b"#!")?;
    let end = line
        .iter()
        .position(|&b| b == b'\n' || (cfg!(target_os = "macos") && b == b'#'))?;
    let blank = |b: &u8| *b == b' ' || *b == b'\t';
    let line = &line[..end];
    let start = line.iter().position(|b| !blank(b)).unwrap_or(line.len());
    let interpreter = line[start..].split(blank).next()?;
    let interpreter = PathBuf::from(OsStr::from_bytes(interpreter));
    is_absolute(&interpreter).then_some(interpreter)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    fn shebang_of(body: &str) -> Option<PathBuf> {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("script");
        std::fs::write(&path, body).expect("write");
        shebang(File::open(path).expect("open"))
    }

    #[test]
    fn a_relative_interpreter_yields_nothing() {
        assert_eq!(shebang_of("#!sh\n"), None);
        assert_eq!(shebang_of("#!\n"), None);
    }

    #[test]
    fn a_line_with_no_end_in_the_buffer_yields_nothing() {
        assert_eq!(shebang_of("#!/bin/sh"), None);
    }

    /// macOS ends the `#!` line at a `#`; Linux reads it as part of the name.
    #[test]
    fn a_hash_ends_the_interpreter_only_on_macos() {
        let expected = if cfg!(target_os = "macos") {
            "/bin/sh"
        } else {
            "/bin/sh#x"
        };
        assert_eq!(shebang_of("#!/bin/sh#x\n"), Some(PathBuf::from(expected)));
    }
}
