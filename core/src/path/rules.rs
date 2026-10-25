//! Platform-specific lexical path rules.

/// Which platform's path rules a question is asked under.
///
/// A parameter rather than a `cfg!` read, so both tables are pinned by tests
/// on every host; the gate is [`Self::HOST`], read by the public entry points.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PathRules {
    Posix,
    Windows,
}

impl PathRules {
    pub(crate) const HOST: Self = if cfg!(windows) {
        Self::Windows
    } else {
        Self::Posix
    };

    /// Whether `c` separates path components: `/` under both rules, `\`
    /// under Windows alone; under Posix a backslash is a filename byte.
    pub(crate) fn is_separator(self, c: char) -> bool {
        c == '/' || (self == Self::Windows && c == '\\')
    }
}
