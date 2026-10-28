//! The external-editor contract the ral and exarch prompts share.
//!
//! Which command runs, split without a shell, and what text may cross its
//! draft file.  Where the draft lives, and under whose authority, is each
//! host's own.

use std::fmt;
use std::io::{self, Read};
use std::string::FromUtf8Error;

/// The most a draft may hold, written out or read back.
pub const MAX_DRAFT_BYTES: usize = 16 * 1024 * 1024;

/// A program and its arguments, none of them containing NUL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditorCommand {
    program: String,
    args: Vec<String>,
}

impl EditorCommand {
    /// The first nonblank of `visual` and `editor`, else `vi` (`notepad` on
    /// Windows).  Words split as a POSIX shell would on Unix, by the native
    /// command-line rules on Windows; nothing is expanded or run.
    ///
    /// # Errors
    /// The chosen variable's: a NUL byte, unbalanced quoting, an empty first word.
    pub fn select(visual: Option<&str>, editor: Option<&str>) -> Result<Self, CommandError> {
        [("VISUAL", visual), ("EDITOR", editor)]
            .into_iter()
            .find_map(|(var, spec)| Some((var, spec.filter(|s| !s.trim().is_empty())?)))
            .map_or_else(
                || Ok(Self::platform_default()),
                |(var, spec)| Self::parse(spec).map_err(|fault| CommandError { var, fault }),
            )
    }

    fn platform_default() -> Self {
        Self {
            program: if cfg!(windows) { "notepad" } else { "vi" }.into(),
            args: Vec::new(),
        }
    }

    fn parse(spec: &str) -> Result<Self, Fault> {
        // Splitting only drops and regroups characters: this covers every word.
        if spec.contains('\0') {
            return Err(Fault::Nul);
        }
        #[cfg(not(windows))]
        let words = shlex::split(spec).ok_or(Fault::Unbalanced)?;
        #[cfg(windows)]
        let words = winsplit::split(spec);
        let mut words = words.into_iter();
        match words.next() {
            Some(program) if !program.is_empty() => Ok(Self {
                program,
                args: words.collect(),
            }),
            _ => Err(Fault::NoProgram),
        }
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    /// The arguments before the draft's path, which the caller appends.
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// Why the variable [`EditorCommand::select`] chose names no runnable command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandError {
    var: &'static str,
    fault: Fault,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    Nul,
    #[cfg(not(windows))]
    Unbalanced,
    NoProgram,
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let var = self.var;
        match self.fault {
            Fault::Nul => write!(f, "{var} contains a NUL byte; remove it"),
            #[cfg(not(windows))]
            Fault::Unbalanced => write!(
                f,
                "{var} has an unmatched quote or a trailing backslash; \
                 it is split into words as a POSIX shell would, with nothing expanded"
            ),
            Fault::NoProgram => write!(f, "{var} names no program: its first word is empty"),
        }
    }
}

impl std::error::Error for CommandError {}

/// Why a draft cannot cross the editor's file.
#[derive(Debug)]
pub enum DraftError {
    /// Over [`MAX_DRAFT_BYTES`]: refused whole, never truncated.
    TooLarge,
    Read(io::Error),
    NotUtf8(FromUtf8Error),
}

impl fmt::Display for DraftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(
                f,
                "the draft exceeds the {} MiB limit",
                MAX_DRAFT_BYTES >> 20
            ),
            Self::Read(e) => write!(f, "{e}"),
            Self::NotUtf8(e) => write!(f, "the draft is not UTF-8: {e}"),
        }
    }
}

impl std::error::Error for DraftError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::TooLarge => None,
            Self::Read(e) => Some(e),
            Self::NotUtf8(e) => Some(e),
        }
    }
}

/// The bytes to write for `text`.
///
/// # Errors
/// [`DraftError::TooLarge`] alone.
pub fn admit_draft(text: &str) -> Result<&[u8], DraftError> {
    if text.len() > MAX_DRAFT_BYTES {
        return Err(DraftError::TooLarge);
    }
    Ok(text.as_bytes())
}

/// The edited draft, byte for byte: trailing newlines and CRs included.
///
/// # Errors
/// The read's, [`DraftError::TooLarge`], [`DraftError::NotUtf8`].
pub fn read_draft(reader: impl Read) -> Result<String, DraftError> {
    let bytes = crate::io::read_bounded(reader, MAX_DRAFT_BYTES)
        .map_err(DraftError::Read)?
        .ok_or(DraftError::TooLarge)?;
    String::from_utf8(bytes).map_err(DraftError::NotUtf8)
}

/// The edit's outcome once its draft is removed: a removal failure is never
/// dropped, and where both fail, `both` reports the two together.
///
/// # Errors
/// `edited`'s, `removed`'s, or `both` of them.
pub fn after_cleanup<T, E>(
    edited: Result<T, E>,
    removed: Result<(), E>,
    both: impl FnOnce(E, E) -> E,
) -> Result<T, E> {
    match (edited, removed) {
        (edited, Ok(())) => edited,
        (Ok(_), Err(removal)) => Err(removal),
        (Err(edit), Err(removal)) => Err(both(edit, removal)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(program: &str, args: &[&str]) -> EditorCommand {
        EditorCommand {
            program: program.into(),
            args: args.iter().map(|&a| a.into()).collect(),
        }
    }

    #[test]
    fn visual_then_editor_then_the_platform_default() {
        let default = if cfg!(windows) { "notepad" } else { "vi" };
        assert_eq!(
            EditorCommand::select(Some("ed -s"), Some("nano")),
            Ok(command("ed", &["-s"]))
        );
        assert_eq!(
            EditorCommand::select(Some(" \t"), Some("nano")),
            Ok(command("nano", &[]))
        );
        assert_eq!(
            EditorCommand::select(None, Some("nano")),
            Ok(command("nano", &[]))
        );
        assert_eq!(EditorCommand::select(None, None), Ok(command(default, &[])));
        assert_eq!(
            EditorCommand::select(Some(""), Some("  ")),
            Ok(command(default, &[]))
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_words_keep_quotes_and_expand_nothing() {
        assert_eq!(
            EditorCommand::select(
                Some(r#"'/path with spaces/editor' --flag "two words" '' '$HOME' ';'"#),
                None,
            ),
            Ok(command(
                "/path with spaces/editor",
                &["--flag", "two words", "", "$HOME", ";"]
            )),
        );
        for unbalanced in ["editor '", "editor \"x", "editor \\"] {
            let error = EditorCommand::select(Some(unbalanced), None).unwrap_err();
            assert_eq!(error.fault, Fault::Unbalanced, "{unbalanced:?}");
            assert!(
                error
                    .to_string()
                    .starts_with("VISUAL has an unmatched quote")
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_words_keep_paths_and_quoted_arguments() {
        assert_eq!(
            EditorCommand::select(
                Some(
                    r#""C:\Program Files\Editor\editor.exe" --wait "two words" "" C:\drafts\f.md "%HOME%" ";""#
                ),
                None,
            ),
            Ok(command(
                r"C:\Program Files\Editor\editor.exe",
                &["--wait", "two words", "", r"C:\drafts\f.md", "%HOME%", ";"]
            )),
        );
    }

    #[test]
    fn an_empty_first_word_names_no_program() {
        for spec in ["\"\" --flag", "\"\""] {
            let error = EditorCommand::select(None, Some(spec)).unwrap_err();
            assert_eq!(
                error,
                CommandError {
                    var: "EDITOR",
                    fault: Fault::NoProgram
                }
            );
            assert_eq!(
                error.to_string(),
                "EDITOR names no program: its first word is empty"
            );
        }
    }

    #[test]
    fn a_nul_anywhere_is_refused_naming_its_variable() {
        for spec in [
            "edi\0tor",
            "editor arg\0ument",
            "editor '\0'",
            "editor \\\0",
        ] {
            let error = EditorCommand::select(Some(spec), Some("nano")).unwrap_err();
            assert_eq!(error.fault, Fault::Nul, "{spec:?}");
            assert_eq!(error.to_string(), "VISUAL contains a NUL byte; remove it");
        }
    }

    #[test]
    fn a_draft_crosses_whole_or_not_at_all() {
        let limit = "x".repeat(MAX_DRAFT_BYTES);
        assert_eq!(admit_draft(&limit).unwrap().len(), MAX_DRAFT_BYTES);
        let over = admit_draft(&"λ".repeat(MAX_DRAFT_BYTES / 2 + 1)).unwrap_err();
        assert!(matches!(over, DraftError::TooLarge));
        assert_eq!(over.to_string(), "the draft exceeds the 16 MiB limit");

        assert_eq!(read_draft(limit.as_bytes()).unwrap(), limit);
        let mut stream = io::Cursor::new(vec![b'x'; MAX_DRAFT_BYTES + 100]);
        assert!(matches!(read_draft(&mut stream), Err(DraftError::TooLarge)));
        assert_eq!(stream.position(), MAX_DRAFT_BYTES as u64 + 1);
    }

    #[test]
    fn readback_is_byte_exact_utf8() {
        assert_eq!(
            read_draft("λ\r\ntwo\n\n".as_bytes()).unwrap(),
            "λ\r\ntwo\n\n"
        );
        let error = read_draft(&[0xff][..]).unwrap_err();
        assert!(matches!(error, DraftError::NotUtf8(_)));
        assert!(error.to_string().starts_with("the draft is not UTF-8"));
    }

    #[test]
    fn a_removal_failure_is_never_dropped() {
        let both = |edit: String, removal: String| format!("{edit}; {removal}");
        let ok = || Ok::<_, String>(());
        let fail = |what: &str| Err::<(), _>(what.to_owned());
        assert_eq!(after_cleanup(Ok(1), ok(), both), Ok(1));
        assert_eq!(
            after_cleanup(Err::<i32, _>("edit".into()), ok(), both),
            Err("edit".into())
        );
        assert_eq!(
            after_cleanup(Ok(1), fail("removal"), both),
            Err("removal".into())
        );
        assert_eq!(
            after_cleanup(Err::<i32, _>("edit".into()), fail("removal"), both),
            Err("edit; removal".into())
        );
    }
}
