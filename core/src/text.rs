//! Text primitives the whole tree shares.
//!
//! Byte↔char conversion, plurals, near-name suggestions, fuzzy ranking, and
//! [`Str`], the shared immutable string.

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

/// Byte offset to character offset — ariadne and the REPL frontends index by char.
pub fn byte_to_char(source: &str, byte_offset: usize) -> usize {
    source[..source.floor_char_boundary(byte_offset)]
        .chars()
        .count()
}

/// Inverse of [`byte_to_char`]; a `cursor` at or past the character count yields
/// `text.len()`, so the result is always a valid slice boundary.
pub fn char_to_byte(text: &str, cursor: usize) -> usize {
    text.char_indices()
        .nth(cursor)
        .map_or(text.len(), |(i, _)| i)
}

/// Up to `limit` of `candidates` within edit distance 2 of `name`, nearest
/// first, each once: the likely intents of a misspelling.
pub fn near_names<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    limit: usize,
) -> Vec<&'a str> {
    let mut near: Vec<_> = candidates
        .into_iter()
        .map(|c| (strsim::damerau_levenshtein(name, c), c))
        .filter(|(d, _)| *d <= 2)
        .collect();
    near.sort_unstable();
    near.dedup();
    near.into_iter().take(limit).map(|(_, c)| c).collect()
}

/// `n` of `noun`, agreeing in number: the one spelling of a count in prose.
pub fn plural<N: std::fmt::Display + PartialEq + From<u8> + Copy>(n: N, noun: &str) -> String {
    if n == N::from(1) {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Fuzzy-rank `items` against `needle`, best first, dropping non-matches.
///
/// The matcher is `nucleo`, the Helix team's, and this is its single home: every
/// surface that offers a user a filtered list — completion menus, pickers —
/// matches the same way, through this or through [`rank_by`] where an item is
/// not its own haystack.  An empty needle matches everything, so an empty prefix
/// lists the whole pool.  `paths` tunes the matcher for path-like haystacks (a
/// `/`-aware boundary bonus).  Ties break alphabetically so the order is
/// deterministic.
pub fn rank<T: AsRef<str>>(needle: &str, items: Vec<T>, paths: bool) -> Vec<T> {
    rank_by(needle, items, |item: &T| item.as_ref(), paths)
}

/// As [`rank`], for items that are not their own haystack.
///
/// `haystack` names the text each item matches by, borrowed from the item, so a
/// row and the string it is ranked by cannot come apart into parallel vectors.
pub fn rank_by<T>(
    needle: &str,
    items: Vec<T>,
    haystack: impl Fn(&T) -> &str,
    paths: bool,
) -> Vec<T> {
    let config = if paths {
        Config::DEFAULT.match_paths()
    } else {
        Config::DEFAULT
    };
    let mut matcher = Matcher::new(config);
    // `Pattern::new`, not `Pattern::parse`: no operator syntax, so a needle like
    // `'notes` or `^tmp` stays literal rather than being reinterpreted — the one
    // variant fit for both a path name and a free-text query.  Whitespace still
    // splits into atoms, so `claude 4` narrows by both words.
    let pattern = Pattern::new(
        needle,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut buf = Vec::new();
    let mut scored: Vec<(T, u32)> = items
        .into_iter()
        .filter_map(|item| {
            let score = pattern.score(Utf32Str::new(haystack(&item), &mut buf), &mut matcher)?;
            Some((item, score))
        })
        .collect();
    // A stable sort, so items sharing a haystack keep the order they came in.
    scored.sort_by(|(a, sa), (b, sb)| sb.cmp(sa).then_with(|| haystack(a).cmp(haystack(b))));
    scored.into_iter().map(|(item, _)| item).collect()
}

/// An immutable string, shared on clone.
///
/// `Arc<String>`, not `Arc<str>`: an owned `String` moves in without a copy,
/// and, while the count is one, back out.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Str(Arc<String>);

impl Str {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Free while this is the only holder.
    pub fn into_string(self) -> String {
        Arc::unwrap_or_clone(self.0)
    }
}

impl Deref for Str {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for Str {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq<str> for Str {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl fmt::Display for Str {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

impl fmt::Debug for Str {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl From<String> for Str {
    fn from(s: String) -> Self {
        Self(Arc::new(s))
    }
}

impl From<&str> for Str {
    fn from(s: &str) -> Self {
        Self::from(s.to_owned())
    }
}

impl From<Cow<'_, str>> for Str {
    fn from(s: Cow<'_, str>) -> Self {
        Self::from(s.into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_byte_round_trip_unicode() {
        let s = "héllo🦀world";
        let nchars = s.chars().count();
        for n in 0..=nchars {
            assert_eq!(byte_to_char(s, char_to_byte(s, n)), n);
        }
    }

    #[test]
    fn char_to_byte_past_end_clamps() {
        let s = "héllo";
        assert_eq!(char_to_byte(s, 9999), s.len());
    }

    #[test]
    fn char_to_byte_at_text_len() {
        let s = "héllo";
        let nchars = s.chars().count();
        assert_eq!(char_to_byte(s, nchars), s.len());
    }

    /// Two items may share a haystack — two providers listing one model name.
    /// Both survive, in the order they came in: the sort is stable, so the
    /// alphabetical tie-break cannot collapse them onto one another.
    #[test]
    fn identical_haystacks_both_survive_in_input_order() {
        let ranked = rank_by(
            "x",
            vec![(1, "x"), (2, "x")],
            |item: &(i32, &str)| item.1,
            false,
        );
        assert_eq!(ranked, vec![(1, "x"), (2, "x")]);
    }

    #[test]
    fn near_names_ranks_dedups_and_caps() {
        let pool = ["lenght", "length", "length", "lengths", "xyz", "len"];
        assert_eq!(
            near_names("lenght", pool, 3),
            ["lenght", "length", "lengths"]
        );
        assert_eq!(near_names("lenght", pool, 1), ["lenght"]);
        assert_eq!(near_names("qqqqq", pool, 3), Vec::<&str>::new());
    }
}
