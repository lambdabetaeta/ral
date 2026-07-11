//! UTF-8 boundary snapping.
//!
//! A byte offset can land inside a multi-byte sequence — synthesised or
//! inherited from a different source, computed from a width budget, or
//! taken from a span anchored in another file.  Slicing there panics, so
//! every such offset is first snapped to a char boundary.  The std
//! `str::floor_char_boundary` / `ceil_char_boundary` that would do this
//! are still unstable, so these are the stable equivalents the workspace
//! shares (diagnostics, the REPL editor's byte↔char map, exarch's
//! output digest).

/// Snap `offset` to the nearest char boundary at or before it (clamped
/// into `s`).  Returns `0` when no earlier boundary exists.
pub fn floor_char_boundary(s: &str, offset: usize) -> usize {
    let clamped = offset.min(s.len());
    (0..=clamped)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0)
}

/// Snap `offset` to the nearest char boundary at or after it (clamped to
/// `s.len()`).
pub fn ceil_char_boundary(s: &str, offset: usize) -> usize {
    let mut i = offset.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Convert a byte offset to a character offset.  Ariadne uses character
/// offsets, so every byte offset must pass through this before being handed
/// to the rendering layer.
pub fn byte_to_char(source: &str, byte_offset: usize) -> usize {
    source[..floor_char_boundary(source, byte_offset)]
        .chars()
        .count()
}

/// Convert a character offset to a byte offset — the inverse of
/// [`byte_to_char`].
///
/// A `cursor` value at or past the character count returns `text.len()`, so
/// the result is always a valid slice boundary.
pub fn char_to_byte(text: &str, cursor: usize) -> usize {
    text.char_indices()
        .nth(cursor)
        .map_or(text.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_byte_round_trip_ascii() {
        let s = "hello";
        for n in 0..=s.chars().count() {
            assert_eq!(byte_to_char(s, char_to_byte(s, n)), n);
        }
    }

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
