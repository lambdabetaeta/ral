//! The path half of completing a typed word: where it splits into the
//! directory to list and the needle to match, which of that directory's
//! entries it offers, and how a chosen entry is spelled back.
//!
//! The directory listed is handed to the OS, so its `~` expands natively
//! ([`TildePath::expand`]).  The spelling is a path ral shows
//! ([`super::shown`]): the typed directory `/`-separated, its `~` kept as
//! typed, and expanded only on request — by [`TildePath::shown`], the inverse
//! of [`abbreviate_home`](super::abbreviate_home) — for a spelling that must
//! be quoted, where `~` would not expand.

use std::path::PathBuf;

use super::PathRules;
use super::tilde::TildePath;

/// A typed word as a path being completed: its directory part, as typed, and
/// the needle after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartialPath<'a> {
    dir: &'a str,
    needle: &'a str,
    tilde: Tilde<'a>,
}

/// What a leading `~` of the word means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tilde<'a> {
    /// The home directory, if one is known.
    Home(Option<&'a str>),
    /// A plain character, as inside quotes.
    Literal,
}

impl<'a> PartialPath<'a> {
    /// Split `word` after its last separator.  Under [`Tilde::Home`] a lone
    /// `~` is home as a directory, `~/`.
    pub fn of(word: &'a str, tilde: Tilde<'a>) -> Self {
        let word = match tilde {
            Tilde::Home(_) if word == "~" => "~/",
            _ => word,
        };
        let cut = word
            .rfind(|c| PathRules::HOST.is_separator(c))
            .map_or(0, |at| at + 1);
        let (dir, needle) = word.split_at(cut);
        Self { dir, needle, tilde }
    }

    /// What the directory's entries are matched against.
    pub fn needle(&self) -> &'a str {
        self.needle
    }

    /// Whether the word has no directory part: a needle alone.
    pub fn is_bare(&self) -> bool {
        self.dir.is_empty()
    }

    /// The directory whose entries complete the needle: `./` for a bare word,
    /// else the typed directory with its `~` expanded natively.  Relative, it
    /// is relative to whatever cwd the lister anchors against.  `None` when its
    /// `~` names an unknown home: no candidates, rather than a fabricated path.
    pub fn listing(&self) -> Option<PathBuf> {
        let dir = if self.is_bare() { "./" } else { self.dir };
        self.expand_by(dir, PathBuf::from, TildePath::expand)
    }

    /// Whether the entry `name` is offered: a dotfile only to a needle that
    /// itself starts with `.`.  Matching it against the needle is the
    /// caller's.
    pub fn offers(&self, name: &str) -> bool {
        self.needle.starts_with('.') || !name.starts_with('.')
    }

    /// The word completing to `entry` (a [`spell_entry`] spelling): the typed
    /// directory as ral shows it, then the entry.
    pub fn completed(&self, entry: &str) -> String {
        super::shown(self.dir) + entry
    }

    /// `word`, a spelling this partial path [`completed`](Self::completed),
    /// with its `~` expanded as ral shows the expansion.  Itself when it has
    /// no `~` to expand; `None` when its `~` names an unknown home.
    pub fn expand(&self, word: &str) -> Option<String> {
        self.expand_by(word, str::to_owned, TildePath::shown)
    }

    /// `word` with its `~` expanded by `tilde` against the home, or taken
    /// `as_typed` when it has no `~` to expand; `None` when its `~` names an
    /// unknown home.
    fn expand_by<'w, T>(
        &self,
        word: &'w str,
        as_typed: impl FnOnce(&'w str) -> T,
        tilde: impl FnOnce(&TildePath, &str) -> T,
    ) -> Option<T> {
        match (self.tilde, TildePath::parse(word)) {
            (Tilde::Home(home), Some(t)) => home.map(|h| tilde(&t, h)),
            _ => Some(as_typed(word)),
        }
    }
}

/// A directory entry `name` as completion spells it: a directory with `/`
/// after it.
pub fn spell_entry(name: &str, is_dir: bool) -> String {
    if is_dir {
        format!("{name}/")
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: Tilde<'static> = Tilde::Home(Some("/home/al"));

    #[test]
    fn the_word_splits_after_its_last_separator() {
        let p = PartialPath::of("src/path/wa", HOME);
        assert_eq!((p.needle(), p.is_bare()), ("wa", false));
        assert_eq!(p.listing(), Some(PathBuf::from("src/path/")));
        assert_eq!(
            p.completed(&spell_entry("walk.rs", false)),
            "src/path/walk.rs"
        );
        let bare = PartialPath::of("wa", HOME);
        assert_eq!((bare.needle(), bare.is_bare()), ("wa", true));
        assert_eq!(bare.listing(), Some(PathBuf::from("./")));
    }

    #[test]
    fn a_lone_tilde_is_home_as_a_directory_unless_literal() {
        let p = PartialPath::of("~", HOME);
        assert_eq!(
            (p.needle(), p.listing()),
            ("", Some(PathBuf::from("/home/al/")))
        );
        assert_eq!(p.completed(&spell_entry("src", true)), "~/src/");
        let literal = PartialPath::of("~", Tilde::Literal);
        assert_eq!((literal.needle(), literal.is_bare()), ("~", true));
    }

    #[test]
    fn a_tilde_expands_only_against_a_known_home() {
        let p = PartialPath::of("~/a b", HOME);
        assert_eq!(p.expand("~/a b.txt").as_deref(), Some("/home/al/a b.txt"));
        let homeless = PartialPath::of("~/x", Tilde::Home(None));
        assert_eq!(homeless.listing(), None);
        let quoted = PartialPath::of("~/x", Tilde::Literal);
        assert_eq!(quoted.listing(), Some(PathBuf::from("~/")));
    }

    #[test]
    fn a_dotfile_is_offered_only_to_a_dotted_needle() {
        let offers = |needle, name| PartialPath::of(needle, HOME).offers(name);
        assert!(!offers("", ".git"));
        assert!(!offers("g", ".git"));
        assert!(offers(".", ".git"));
        assert!(offers("", "src"));
    }
}
