//! Ral syntax highlighting for the window — the one command
//! [`ral-highlight.js`](../../ui/js/ral-highlight.js) calls.
//!
//! Colours come from [`ral_core::syntax::highlight::classify`], the same
//! classification exarch's TUI already uses, so the window and the TUI can
//! never disagree about what a token is.

use ral_core::syntax::highlight::{Class, classify};
use std::iter::Peekable;
use std::ops::Range;
use std::str::CharIndices;

/// One classified token, as the window receives it.
///
/// `start` and `end` are UTF-16 code-unit offsets into `src` — what JS
/// `String.prototype.slice` indexes by — converted here from the lexer's own
/// byte offsets, since the window is the one place that needs the
/// conversion.
#[derive(Clone, serde::Serialize, ts_rs::TS)]
#[ts(export, export_to = "../ui/js/bindings/")]
pub struct RalToken {
    pub start: usize,
    pub end: usize,
    /// Lowercase: `"keyword"`, `"string"`, `"tag"`, `"variable"`, `"punct"`.
    pub class: String,
}

/// Classify `src` as ral source.
///
/// A token with no hue of its own ([`Class::Plain`]) is left out entirely —
/// the window renders those stretches as plain text, like the gaps between
/// tokens.
#[tauri::command]
pub fn highlight_ral(src: String) -> Vec<RalToken> {
    to_utf16(&src, classify(&src))
}

/// Convert each range's byte offsets to UTF-16 code-unit offsets, in one
/// linear pass over `src` — sound because `classify` hands back its ranges
/// in source order, non-overlapping, each falling on a char boundary.
fn to_utf16(src: &str, tokens: Vec<(Range<usize>, Class)>) -> Vec<RalToken> {
    let mut chars = src.char_indices().peekable();
    let mut utf16_at = 0usize;
    tokens
        .into_iter()
        .filter_map(|(range, class)| {
            let class = class_name(class)?;
            advance_utf16(&mut chars, &mut utf16_at, range.start);
            let start = utf16_at;
            advance_utf16(&mut chars, &mut utf16_at, range.end);
            Some(RalToken {
                start,
                end: utf16_at,
                class: class.to_string(),
            })
        })
        .collect()
}

/// Walk `chars` up to (not past) byte offset `target`, adding each
/// character's UTF-16 width to `utf16_at` as it goes.
fn advance_utf16(chars: &mut Peekable<CharIndices<'_>>, utf16_at: &mut usize, target: usize) {
    while chars.peek().is_some_and(|&(i, _)| i < target) {
        let (_, ch) = chars.next().expect("just peeked");
        *utf16_at += ch.len_utf16();
    }
}

/// `None` for [`Class::Plain`], which the window renders as plain text.
fn class_name(class: Class) -> Option<&'static str> {
    match class {
        Class::Keyword => Some("keyword"),
        Class::String => Some("string"),
        Class::Tag => Some("tag"),
        Class::Variable => Some("variable"),
        Class::Punct => Some("punct"),
        Class::Plain => None,
    }
}
