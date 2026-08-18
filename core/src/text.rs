//! Text primitives the whole tree shares.
//!
//! Snapping byte offsets to UTF-8 char boundaries, byte↔char conversion, and
//! fuzzy ranking.  std's `str::floor_char_boundary` / `ceil_char_boundary` are
//! still unstable, hence the first of those.

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher};

/// Snap `offset` to the nearest char boundary at or before it, clamped into `s`.
pub fn floor_char_boundary(s: &str, offset: usize) -> usize {
    let clamped = offset.min(s.len());
    (0..=clamped)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0)
}

/// Snap `offset` to the nearest char boundary at or after it, clamped to `s.len()`.
pub fn ceil_char_boundary(s: &str, offset: usize) -> usize {
    let mut i = offset.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Byte offset to character offset — ariadne and the REPL frontends index by char.
pub fn byte_to_char(source: &str, byte_offset: usize) -> usize {
    source[..floor_char_boundary(source, byte_offset)]
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

/// Fuzzy-rank `items` against `needle`, best first, dropping non-matches.
///
/// The matcher is `nucleo`, the Helix team's, and this is its single home: every
/// surface that offers a user a filtered list — completion menus, pickers —
/// matches the same way.  An empty needle matches everything, so an empty prefix
/// lists the whole pool.  `paths` tunes the matcher for path-like haystacks (a
/// `/`-aware boundary bonus).  Ties break alphabetically so the order is
/// deterministic.
pub fn rank<T: AsRef<str>>(needle: &str, items: Vec<T>, paths: bool) -> Vec<T> {
    let config = if paths {
        Config::DEFAULT.match_paths()
    } else {
        Config::DEFAULT
    };
    let mut matcher = Matcher::new(config);
    let atom = Atom::new(
        needle,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );
    let mut scored = atom.match_list(items, &mut matcher);
    // `match_list` already orders by score descending (stable in input order on
    // ties); re-break ties alphabetically for a deterministic order.
    scored.sort_by(|(a, sa), (b, sb)| sb.cmp(sa).then_with(|| a.as_ref().cmp(b.as_ref())));
    scored.into_iter().map(|(item, _)| item).collect()
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

    #[test]
    fn floor_snaps_back_into_a_codepoint() {
        // "λ" is two bytes (CE BB); offset 1 is mid-codepoint.
        assert_eq!(floor_char_boundary("λx", 1), 0);
        assert_eq!(floor_char_boundary("λx", 2), 2);
    }

    #[test]
    fn ceil_snaps_forward_into_a_codepoint() {
        assert_eq!(ceil_char_boundary("λx", 1), 2);
        assert_eq!(ceil_char_boundary("λx", 0), 0);
    }

    #[test]
    fn both_clamp_past_the_end() {
        assert_eq!(floor_char_boundary("ab", 9), 2);
        assert_eq!(ceil_char_boundary("ab", 9), 2);
    }
}
