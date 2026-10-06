//! `~`, the current user's home directory.
//!
//! It stands alone or before a `/` (or a `\` under Windows); any other `~` is
//! an ordinary character, so `~bob` and `a~b` are plain text.
//! [`TildePath::expand`] and [`abbreviate_home`] are inverses.

use serde::{Deserialize, Serialize};

use super::PathRules;

/// `~` or `~/sub`, the suffix keeping its separator.
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
    fn parse_for(input: &str, rules: PathRules) -> Option<Self> {
        let rest = input.strip_prefix('~')?;
        if rest.is_empty() {
            return Some(Self { suffix: None });
        }
        let separated =
            rest.starts_with('/') || (rules == PathRules::Windows && rest.starts_with('\\'));
        separated.then(|| Self {
            suffix: Some(rest.to_string()),
        })
    }

    /// `home` followed by the suffix.
    pub fn expand(&self, home: &str) -> String {
        format!("{home}{}", self.suffix.as_deref().unwrap_or_default())
    }

    /// The spelling this parsed from.
    pub(crate) fn to_literal(&self) -> String {
        format!("~{}", self.suffix.as_deref().unwrap_or_default())
    }
}

/// Fold a leading `home` prefix to `~` for display, inverting
/// [`TildePath::expand`].
///
/// The match is on component boundaries, so home `/home/al` leaves
/// `/home/alex` alone where a `starts_with` on the raw string would clip it.
pub fn abbreviate_home(path: &str, home: Option<&str>) -> String {
    abbreviate_home_for(path, home, PathRules::HOST)
}

/// [`abbreviate_home`] on strings, under given rules.
///
/// Under Windows the result is folded to `/` separators: the `~/` head
/// already commits the string to them, so a native-separator rest would print
/// mixed (`~/projects\ral`), and folding the fallback arm too keeps the two
/// shapes consistent.  Display only — never fed back into resolution or
/// matching, which accept either spelling anyway.  Off Windows `\` is an
/// ordinary filename byte, hence the gate.
#[allow(
    clippy::disallowed_methods,
    reason = "lexical Path::new for the component-boundary strip: no I/O behind it; this module is part of crate::path, where the path-construction rule lives"
)]
fn abbreviate_home_for(path: &str, home: Option<&str>, rules: PathRules) -> String {
    let shown = match home {
        None => path.to_string(),
        Some(home) if rules == PathRules::Windows => windows_strip_home(path, home),
        Some(home) => match std::path::Path::new(path).strip_prefix(home) {
            Ok(rest) => {
                if rest.as_os_str().is_empty() {
                    return "~".to_string();
                }
                format!("~/{}", rest.display())
            }
            Err(_) => path.to_string(),
        },
    };
    if rules == PathRules::Windows {
        shown.replace('\\', "/")
    } else {
        shown
    }
}

/// The Windows half of [`abbreviate_home_for`]'s strip: containment under
/// [`super::identity::starts_with_identity`]'s identity rather than
/// `Path::strip_prefix`, which is separator-insensitive but
/// case-*sensitive* — so `USERPROFILE`/`cwd` disagreeing on casing
/// (`C:\Users\al` vs `c:\users\al`) would otherwise leave the prompt showing
/// the whole path instead of folding it to `~`.
///
/// The identity check only decides *whether* home is a prefix; the displayed
/// tail is sliced from `path`'s own components, in `path`'s own casing, so
/// the user's typed spelling survives — only the fold to `~`, never a fold to
/// `home`'s case, happens here.
fn windows_strip_home(path: &str, home: &str) -> String {
    if !super::identity::starts_with_identity(path, home, PathRules::Windows) {
        return path.to_string();
    }
    let home_depth = super::identity::windows_identity_components(home).len();
    // The head fold without the case fold: the displayed tail must keep
    // `path`'s own casing, not `home`'s.
    let head = super::identity::windows_head(path);
    let tail: Vec<&str> = head
        .split(['/', '\\'])
        .filter(|c| !c.is_empty())
        .skip(home_depth)
        .collect();
    if tail.is_empty() {
        "~".to_string()
    } else {
        format!("~/{}", tail.join("/"))
    }
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
        assert_eq!(tilde(None).expand("/h"), "/h");
        assert_eq!(tilde(Some("/sub")).expand("/h"), "/h/sub");
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
    // and the fixtures are shaped so both hosts' `Path` parses agree (a
    // drive-letter *home* would strip only under a Windows-parsed `Path`,
    // so none appears here).

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
