//! `~`, the current user's home directory.
//!
//! It stands alone or before a `/` (or a `\` under Windows); any other `~` is
//! an ordinary character, so `~bob` and `a~b` are plain text.
//! The suffix is stored as written, separator byte and all, for
//! [`TildePath::to_literal`] and the native expansion handed to the OS,
//! [`TildePath::expand`]; what ral shows is `/`-separated whatever was
//! written, and on shown strings [`TildePath::shown`] and [`abbreviate_home`]
//! are inverses.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::PathRules;
use super::show::fold_for;

/// `~` or `~/sub`, the suffix stored with the separator it was written with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TildePath {
    pub suffix: Option<String>,
}

impl TildePath {
    /// `~` or `~/rest`; `None` for any other spelling.
    pub fn parse(input: &str) -> Option<Self> {
        Self::parse_for(input, PathRules::HOST)
    }

    /// [`Self::parse`] under given rules.  The suffix keeps the separator byte
    /// it was written with: [`Self::to_literal`] returns the spelling.
    pub(crate) fn parse_for(input: &str, rules: PathRules) -> Option<Self> {
        let rest = input.strip_prefix('~')?;
        if rest.is_empty() {
            return Some(Self { suffix: None });
        }
        rest.starts_with(|c| rules.is_separator(c)).then(|| Self {
            suffix: Some(rest.to_string()),
        })
    }

    /// The native expansion, as the OS takes it: `home` followed by the
    /// suffix, in `home`'s own spelling, lifted to a path and never read for
    /// a sigil again.
    pub(crate) fn expand(&self, home: &str) -> PathBuf {
        PathBuf::from(format!(
            "{home}{}",
            self.suffix.as_deref().unwrap_or_default()
        ))
    }

    /// The expansion as ral shows it: [`Self::expand`] shown
    /// ([`super::shown`]), so a suffix written `~\sub` under Windows reads
    /// `/`-separated like the home before it.  The one spelling `~`, `home`
    /// and a `~/…` command head share.
    pub(crate) fn shown(&self, home: &str) -> String {
        super::shown(self.expand(home))
    }

    /// The spelling this parsed from.
    pub(crate) fn to_literal(&self) -> String {
        format!("~{}", self.suffix.as_deref().unwrap_or_default())
    }
}

/// Fold a leading `home` prefix to `~` for display, inverting
/// [`TildePath::shown`].
///
/// The match is on component boundaries, so home `/home/al` leaves
/// `/home/alex` alone where a `starts_with` on the raw string would clip it.
pub fn abbreviate_home(path: &str, home: Option<&str>) -> String {
    abbreviate_home_for(path, home, PathRules::HOST)
}

/// [`abbreviate_home`] on strings, under given rules, read alike from any
/// host.
///
/// Where `home` is a prefix of `path`, the names past it follow `~/`,
/// `/`-separated, in `path`'s own spelling; elsewhere `path` is folded as
/// [`super::show::fold_for`] folds it.  Display only.
fn abbreviate_home_for(path: &str, home: Option<&str>, rules: PathRules) -> String {
    let tail = home.and_then(|home| match rules {
        PathRules::Posix => posix_home_tail(path, home),
        PathRules::Windows => windows_home_tail(path, home),
    });
    match tail {
        None => fold_for(path, rules).into_owned(),
        Some(tail) if tail.is_empty() => "~".to_string(),
        Some(tail) => format!("~/{}", tail.join("/")),
    }
}

/// The names of `path` past `home` under Posix rules, if `home` is a prefix
/// of it on component boundaries, so home `/h/al` is no prefix of
/// `/h/alex`.  Names compare byte for byte, as POSIX names them: `\` is a
/// byte of one, and an empty or `.` name, of a doubled or trailing `/`,
/// none.
fn posix_home_tail<'a>(path: &'a str, home: &str) -> Option<Vec<&'a str>> {
    fn names(s: &str) -> impl Iterator<Item = &str> {
        s.split('/').filter(|name| !matches!(*name, "" | "."))
    }
    if path.starts_with('/') != home.starts_with('/') {
        return None;
    }
    let mut tail = names(path);
    names(home)
        .all(|name| tail.next() == Some(name))
        .then(|| tail.collect())
}

/// The Windows half of [`abbreviate_home_for`]'s strip: containment under
/// [`super::identity::starts_with_identity`]'s identity rather than a
/// byte-exact one, so `USERPROFILE`/`cwd` disagreeing on casing
/// (`C:\Users\al` vs `c:\users\al`) or on separators still fold to `~`
/// instead of leaving the prompt showing the whole path.
///
/// The identity check only decides *whether* home is a prefix; the displayed
/// tail is sliced from `path`'s own components, in `path`'s own casing, so
/// the user's typed spelling survives — only the fold to `~`, never a fold to
/// `home`'s case, happens here.
fn windows_home_tail<'a>(path: &'a str, home: &str) -> Option<Vec<&'a str>> {
    if !super::identity::starts_with_identity(path, home, PathRules::Windows) {
        return None;
    }
    let home_depth = super::identity::windows_identity_components(home).len();
    // The head fold without the case fold: the displayed tail must keep
    // `path`'s own casing, not `home`'s.
    let head = super::identity::windows_head(path);
    Some(
        head.split(|c| PathRules::Windows.is_separator(c))
            .filter(|c| !c.is_empty())
            .skip(home_depth)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tilde(suffix: Option<&str>) -> TildePath {
        TildePath {
            suffix: suffix.map(str::to_owned),
        }
    }

    #[test]
    fn parse_admits_a_lone_tilde_and_a_separated_one() {
        assert_eq!(TildePath::parse("~"), Some(tilde(None)));
        assert_eq!(TildePath::parse("~/x"), Some(tilde(Some("/x"))));
    }

    #[test]
    fn parse_refuses_every_other_tilde() {
        for input in ["~bob", "~bob/x", "a~b", "~5", "x"] {
            assert_eq!(TildePath::parse(input), None, "{input}");
        }
    }

    #[test]
    fn expansion_appends_the_suffix_to_home() {
        assert_eq!(tilde(None).expand("/h"), PathBuf::from("/h"));
        assert_eq!(tilde(Some("/sub")).expand("/h"), PathBuf::from("/h/sub"));
    }

    #[test]
    fn to_literal_reconstructs_the_parsed_spelling() {
        for input in ["~", "~/sub"] {
            assert_eq!(TildePath::parse(input).unwrap().to_literal(), input);
        }
    }

    // `parse_for` rather than `parse` below: the rules are pinned, so the
    // Windows separator rule is exercised on every host.

    #[test]
    fn backslash_separates_on_windows_and_round_trips() {
        let parsed = TildePath::parse_for(r"~\sub", PathRules::Windows);
        assert_eq!(parsed, Some(tilde(Some(r"\sub"))));
        assert_eq!(parsed.unwrap().to_literal(), r"~\sub");
    }

    /// `shown` reads [`PathRules::HOST`], so this one is pinned on Windows
    /// alone: the suffix folds with the home, and the fold inverts it.
    #[cfg(windows)]
    #[test]
    fn shown_folds_a_backslash_suffix_and_abbreviation_inverts_it() {
        let home = r"C:\Users\u";
        let shown = tilde(Some(r"\sub")).shown(home);
        assert_eq!(shown, "C:/Users/u/sub");
        assert_eq!(abbreviate_home(&shown, Some(home)), "~/sub");
    }

    #[test]
    fn backslash_is_an_ordinary_byte_off_windows() {
        assert_eq!(TildePath::parse_for(r"~\sub", PathRules::Posix), None);
    }

    /// An unknown home folds nothing — the prompt shows the path whole rather
    /// than an accidental `~`-relative reading of it.
    #[test]
    fn abbreviation_without_home_leaves_the_path_alone() {
        assert_eq!(abbreviate_home_for("/a/b", None, PathRules::Posix), "/a/b");
    }

    // No `cfg(windows)` on the fold tests below: the rules are a parameter,
    // and nothing the host parses is read, so each runs alike on every host.

    #[test]
    fn abbreviation_renders_forward_slashes_on_windows() {
        assert_eq!(
            abbreviate_home_for(r"/h/projects\ral", Some("/h"), PathRules::Windows),
            "~/projects/ral"
        );
    }

    #[test]
    fn abbreviation_folds_separators_in_the_unabbreviated_fallback_on_windows() {
        assert_eq!(
            abbreviate_home_for(r"D:\work\thing", Some(r"C:\Users\al"), PathRules::Windows),
            "D:/work/thing"
        );
    }

    /// Off Windows `\` is an ordinary filename byte, so display folding must
    /// leave it alone.
    #[test]
    fn abbreviation_keeps_backslash_bytes_off_windows() {
        assert_eq!(
            abbreviate_home_for(r"/h/we\ird", Some("/h"), PathRules::Posix),
            r"~/we\ird"
        );
    }

    /// Under Posix rules a `\` home is one name, never a prefix of a longer
    /// one, as a host `Path` on Windows would read it; a trailing `/` on home
    /// is no name.
    #[test]
    fn abbreviation_under_posix_rules_splits_on_slash_alone() {
        assert_eq!(
            abbreviate_home_for(r"C:\Users\al\x", Some(r"C:\Users\al"), PathRules::Posix),
            r"C:\Users\al\x"
        );
        assert_eq!(
            abbreviate_home_for("/h/x", Some("/h/"), PathRules::Posix),
            "~/x"
        );
        assert_eq!(abbreviate_home_for("/h", Some("/h"), PathRules::Posix), "~");
        assert_eq!(
            abbreviate_home_for("/home/alex", Some("/home/al"), PathRules::Posix),
            "/home/alex"
        );
    }

    /// `USERPROFILE` and the path under test disagree on casing — exactly the
    /// drift `Path::strip_prefix` cannot see past, since it folds separators
    /// but not case.  The displayed tail keeps `path`'s own casing (`MyProject`,
    /// not `myproject`): only the prefix comparison is case-insensitive, not
    /// the fold that renders `MyProject` on top of it.
    #[test]
    fn abbreviation_is_case_insensitive_to_home_on_windows() {
        assert_eq!(
            abbreviate_home_for("/h/users/MyProject", Some("/H/Users"), PathRules::Windows),
            "~/MyProject"
        );
    }

    /// Same identity rule as `identity::starts_with_identity`'s own tests: a
    /// verbatim `\\?\` prefix and a case difference both fall away, and the
    /// tail keeps its own case regardless.
    #[test]
    fn abbreviation_strips_verbatim_prefix_and_case_on_windows() {
        assert_eq!(
            abbreviate_home_for(
                r"\\?\C:\Users\Al\Work",
                Some(r"c:\users\al"),
                PathRules::Windows
            ),
            "~/Work"
        );
    }

    /// A `Path::strip_prefix`-only rule would leave this printed in full: the
    /// case mismatch defeats a byte-exact strip even though the two spellings
    /// name the same directory under Windows identity.
    #[test]
    fn abbreviation_off_windows_stays_case_sensitive() {
        assert_eq!(
            abbreviate_home_for("/h/users/x", Some("/H/Users"), PathRules::Posix),
            "/h/users/x"
        );
    }
}
