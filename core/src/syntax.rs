//! Surface syntax: the AST shape, the lexer, the parser, and the
//! small source-level helpers (quoting, bare-word classification)
//! that operate on raw source text.

pub mod ast;
mod free_refs;
pub(crate) mod group;
pub mod lexer;
pub mod parser;
pub mod quote;
pub mod tag;

pub use quote::{is_bare_word, quote_word, quote_word_if_needed};

/// True when `word` is a ral keyword:
///
/// a control-flow keyword (`if`, `elsif`,
/// `else`, `let`, `return`, `case`) or a control-operator keyword
/// ([`ast::ScopeAst::KEYWORDS`] — `try`, `guard`, `within`, `grant`,
/// `audit`).
///
/// The single source of truth for the keyword vocabulary: the
/// parser's reserved-name check (`parser::is_reserved`) and the exarch
/// syntax highlighter both consult it, so the two cannot drift.  Value
/// literals (`true`, `false`, `unit`) are *not* keywords — they classify
/// through [`ast::WordLiteral`].
pub fn is_keyword(word: &str) -> bool {
    ast::ScopeAst::lookup_keyword(word).is_some()
        || matches!(word, "if" | "elsif" | "else" | "let" | "return" | "case")
}

/// Maximum recursive nesting accepted by the front end, enforced
/// independently by the lexer (over its delimiter stack) and the parser
/// (over recursive-descent depth).  Adversarial input like `{{{{…}}}}`
/// hits this long before any human-written program; the cap turns a
/// would-be host-stack overflow into a clean rejection.
pub(crate) const NESTING_DEPTH_LIMIT: usize = 64;

/// The shared "nesting too deep" diagnostic, so the lexer and parser
/// reject over-deep input with one wording naming the same limit.
pub(crate) fn nesting_too_deep_message() -> String {
    format!(
        "nesting is too deep (more than {NESTING_DEPTH_LIMIT} levels of \
         brackets, braces, or expression blocks) — simplify the input"
    )
}
