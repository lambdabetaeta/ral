use crate::wire::RalToken;
use ral_core::syntax::highlight::{Class, classify};
use std::iter::Peekable;
use std::ops::Range;
use std::str::CharIndices;

/// Async keeps long classifications off the window event loop.
#[tauri::command(async)]
pub(crate) fn highlight_ral(src: String) -> Vec<RalToken> {
    to_utf16(&src, classify(&src))
}

/// One pass requires source-ordered, disjoint ranges on character boundaries.
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

fn advance_utf16(chars: &mut Peekable<CharIndices<'_>>, utf16_at: &mut usize, target: usize) {
    while chars.peek().is_some_and(|&(i, _)| i < target) {
        let (_, ch) = chars.next().expect("just peeked");
        *utf16_at += ch.len_utf16();
    }
}

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
