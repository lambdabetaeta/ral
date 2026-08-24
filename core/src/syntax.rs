//! Surface syntax: the AST, the lexer, the parser, and the helpers that work
//! on raw source text.

pub mod ast;
mod free_refs;
pub(crate) mod group;
pub mod lexer;
pub mod parser;
pub mod quote;
pub mod tag;

pub use quote::{is_bare_word, quote_word, quote_word_if_needed};

/// True when `word` is a ral keyword: control flow, or a control operator from
/// [`ast::ScopeAst::KEYWORDS`].
///
/// Sole source of the vocabulary — the parser's `is_reserved` and exarch's
/// syntax highlighter both consult it, so the two cannot drift.  Value
/// literals (`true`, `false`) are not keywords; they classify through
/// [`ast::WordLiteral`].
pub fn is_keyword(word: &str) -> bool {
    ast::ScopeAst::lookup_keyword(word).is_some()
        || matches!(word, "if" | "elsif" | "else" | "let" | "return" | "case")
}

/// Recursion cap for the front end, enforced independently by the lexer over
/// its delimiter stack and the parser over descent depth: it turns a would-be
/// host-stack overflow on input like `{{{{…}}}}` into a clean rejection.
pub(crate) const NESTING_DEPTH_LIMIT: usize = 64;

/// One wording for both enforcement sites, naming the same limit.
pub(crate) fn nesting_too_deep_message() -> String {
    format!(
        "nesting is too deep (more than {NESTING_DEPTH_LIMIT} levels of \
         brackets, braces, or expression blocks) — simplify the input"
    )
}
