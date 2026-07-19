//! Lexer: source text → token stream.
//!
//! Produces a flat `Vec<(Token, Span)>` from raw source.  Spans are the
//! canonical [`crate::source::Span`] — byte offsets plus a [`FileId`]; line
//! and column are recovered at render time from the source text, not carried
//! on every token.  Newlines are statement separators except inside `[...]`
//! (lists/maps), where they are whitespace; this is decided by the innermost
//! open delimiter so nested `{ [ ] }` and `[ { } ]` both behave.
//!
//! Bare-word recognition is broad: anything not in the metacharacter set
//! is part of a word, including `:` and `=`.  `:` only splits when followed
//! by space, newline, or `]` — so `host:5432` stays one token but `host:`
//! splits.  `$`, `^`, `!`, `~` introduce structured forms (deref, expr
//! block, force, tilde path) and never appear mid-word.
//! Commas are punctuation only while lexing inside `[...]`; elsewhere they
//! are ordinary bare-word characters.
//!
//! **Single-quoted strings** `'…'` are verbatim literals with no escapes
//! and no interpolation.  Hash-bumping handles `'` in the body: the opener
//! is `n` `#`s followed by `'` (n ≥ 0); the close is `'` followed by `n`
//! `#`s.  A `'` in the body followed by fewer than `n` `#`s is literal.
//! At top level, a run of `#`s not followed by `'` is a comment.
//!
//! **Nested syntactic forms** (`!{…}` and `$[…]` inside `"…"`, plus
//! `$name[k]` index keys, and the top-level `$[…]`) are lexed *in
//! place*: the lexer recurses via [`Lexer::scan_token_group`] and
//! stores the resulting `Vec<(Token, Span)>` inside the enclosing
//! [`StringPart`] or [`Token::Expr`].  The parser builds a sub-parser
//! over that stream rather than re-lexing the raw source bytes — lex
//! once, not twice — and inner-token spans already attribute to the
//! outer file so diagnostics underline the right columns.

use crate::path::tilde::TildePath;
use crate::source::{FileId, Span, Spanned};
use crate::syntax::ast::{RedirectMode, Word};
use std::fmt;

/// The identifier alphabet, `[a-zA-Z_][a-zA-Z0-9_-]*`: a name may start
/// with an ASCII letter or `_`, and continue with those plus digits and
/// `-`.  The lexer scans names char-by-char; [`is_ident`] validates a
/// whole candidate string.  Both forms live here so the alphabet is
/// defined once.
pub(crate) fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

pub(crate) fn is_ident_cont(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

/// Validate a whole candidate string against the identifier alphabet.
pub(crate) fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if is_ident_start(c) => {}
        _ => return false,
    }
    chars.all(is_ident_cont)
}

/// True when `ch` may appear in a bare word.
///
/// The complementary
/// metacharacters (whitespace, the brackets/braces, quote markers,
/// operators) terminate or refuse a bare word and so always need
/// quoting in source.
///
/// Notes that don't fit the static character set:
///
/// - `:` and `=` are bare; `scan_bare_word` decides when they split a
///   token (only when `:` is followed by space, newline, or `]`).
/// - `,` is bare outside `[...]`; inside list/map context the lexer
///   treats it as punctuation instead.  For quoting-from-strings we
///   don't know the surrounding context, so callers that need a
///   context-free decision should consider `,` non-bare too.
///
/// This is the single source of truth for the per-character bare-word
/// alphabet: the lexer's own scanning ([`Lexer::scan_bare_word`]) consults
/// it directly, and the tree-sitter grammar mirrors it.  Context-sensitive
/// bareness ([`crate::syntax::quote::is_bare_word`]) is decided by full
/// lexing via [`lex`], not by this predicate.
pub(crate) fn is_bare_char(ch: char) -> bool {
    !matches!(
        ch,
        ' ' | '\t'
            | '\r'
            | '\n'
            | '|'
            | '{'
            | '}'
            | '['
            | ']'
            | '$'
            | '^'
            | '!'
            | '~'
            | '<'
            | '>'
            | '"'
            | '\''
            | '`'
            | '('
            | ')'
            | ';'
    )
}

/// Parts of an interpolated (double-quoted) string.
///
/// Nested syntactic forms (`!{…}`, `$[…]`, and index keys `[k]`) are
/// lexed *in place* — the lexer recurses into them and stores the
/// resulting token stream alongside its outer-source spans.  The parser
/// builds a sub-parser over that stream rather than re-lexing the
/// original bytes; this keeps "lex once" honest and means diagnostic
/// spans inside an interpolation point at the right column of the
/// outer source.
#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    Literal(String),
    Variable(String),
    /// `!{…}` (or `!$name`) inside `"…"`.  Carries the inner token
    /// stream with outer-file spans.
    Force(Vec<(Token, Span)>),
    /// `$[…]` inside `"…"`.  Carries the expression-block token stream
    /// (already with `&&`/`||` fused — see [`Lexer::scan_expr_block`]).
    Expr(Vec<(Token, Span)>),
    /// Variable with adjacent index keys: `$name[k1][k2]`.  The name is
    /// a [`Spanned`] over the `$name` head; each key is a [`Spanned`]
    /// over its own token stream, with span covering opening bracket
    /// through closing bracket for diagnostic narrowing.
    Index {
        name: Spanned<String>,
        keys: Vec<Spanned<Vec<(Token, Span)>>>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Word(Word),
    SingleQuoted(String),
    DoubleQuoted(Vec<Spanned<StringPart>>),
    Dollar,
    Caret,
    Pipe,
    Ampersand,
    Question,
    Colon,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Comma,
    Spread,
    /// Variant tag `` `ident `` — the label is stored without its backtick.
    /// Construction (`` `ok 5 ``), tag-keyed record keys (`` [`ok: 5] ``), and case
    /// handler tables share this token.
    Tag(String),
    /// Deref resolved by lexer: `$name`, `$(name)`, `$name[key]`.
    Deref(StringPart),
    /// Expression block `$[…]` outside of strings.  Carries the
    /// expression-block token stream (already with `&&`/`||` fused —
    /// see [`Lexer::scan_expr_block`]).
    Expr(Vec<(Self, Span)>),
    Bang,
    Newline,
    Redirect {
        fd: Option<u32>,
        kind: RedirectMode,
        target_fd: Option<u32>,
    },
    Eof,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Word(Word::Tilde(path)) => {
                let mut rendered = "~".to_string();
                if let Some(user) = &path.user {
                    rendered.push_str(user);
                }
                if let Some(suffix) = &path.suffix {
                    rendered.push_str(suffix);
                }
                write!(f, "{rendered}")
            }
            Self::Word(Word::Plain(s) | Word::Slash(s)) | Self::SingleQuoted(s) => {
                write!(f, "'{s}'")
            }
            Self::DoubleQuoted(_) => write!(f, "\"...\""),
            Self::Dollar => write!(f, "$"),
            Self::Caret => write!(f, "^"),
            Self::Pipe => write!(f, "|"),
            Self::Ampersand => write!(f, "&"),
            Self::Question => write!(f, "?"),
            Self::Colon => write!(f, ":"),
            Self::LBrace => write!(f, "{{"),
            Self::RBrace => write!(f, "}}"),
            Self::LBracket => write!(f, "["),
            Self::RBracket => write!(f, "]"),
            Self::LParen => write!(f, "("),
            Self::RParen => write!(f, ")"),
            Self::Comma => write!(f, ","),
            Self::Spread => write!(f, "..."),
            Self::Tag(s) => write!(f, "`{s}"),
            Self::Deref(part) => match part {
                StringPart::Variable(n) => write!(f, "${n}"),
                StringPart::Index { name, .. } => write!(f, "${}[...]", name.item),
                _ => write!(f, "$..."),
            },
            Self::Expr(_) => write!(f, "$[...]"),
            Self::Bang => write!(f, "!"),
            Self::Newline => write!(f, "newline"),
            Self::Redirect { .. } => write!(f, "redirect"),
            Self::Eof => write!(f, "end of input"),
        }
    }
}

impl Token {
    pub fn as_plain_word(&self) -> Option<&str> {
        match self {
            Self::Word(word) => word.as_plain(),
            _ => None,
        }
    }
}

/// Which lexical form a string came from.  Used in
/// [`LexErrorKind::UnterminatedString`] so the diagnostic can name the
/// shape that wasn't closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StringForm {
    SingleQuoted,
    DoubleQuoted,
    /// `n` extra `#`s on each side, e.g. `#'…'#` (1) or `##'…'##` (2).
    BumpedSingle(usize),
}

impl fmt::Display for StringForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SingleQuoted => f.write_str("single-quoted string"),
            Self::DoubleQuoted => f.write_str("double-quoted string"),
            Self::BumpedSingle(n) => write!(f, "bumped single-quoted string ({n} hashes)"),
        }
    }
}

/// Structured lexer error.
///
/// The `Other` arm preserves the original
/// free-form messages; the named arms exist so consumers (REPL
/// continuation, Ariadne renderer) can reason about *what* was
/// unterminated and *where* it opened.
///
/// Each arm carries a single [`Span`] anchoring the opening delimiter.
/// The diagnostic layer recovers line/column at render time from the
/// originating source text — there is no precomputed `(line, col)` here.
#[derive(Debug, Clone)]
pub enum LexErrorKind {
    /// A string literal hit EOF before its closing delimiter.
    /// `inner` carries a nested-form failure (e.g. an unclosed `!{…}`
    /// inside a double-quoted string) so the diagnostic can both
    /// anchor at the outer string and explain the inner culprit.
    UnterminatedString {
        form: StringForm,
        opened: Span,
        inner: Option<Box<Self>>,
    },
    /// A balanced delimiter pair (`{}`, `[]`) opened inside an
    /// interpolation or expression block was not closed.  Anchored at
    /// the opening delimiter.
    UnterminatedBalanced {
        open: char,
        close: char,
        opened: Span,
    },
    /// A `$(...)` dereference was opened and never closed.
    UnclosedDeref { opened: Span },
    /// Free-form lexer errors — invalid escapes, unexpected characters,
    /// expected-X-found-Y, redirect parse errors, and so on.
    Other(String),
}

impl LexErrorKind {
    /// True for the still-open arms — a string, balanced pair, or `$(…)`
    /// that ran past end of input.  These are the kinds that mean "the
    /// user is mid-typing": the REPL prompts for more, and an inner one is
    /// re-anchored into its enclosing string.  The single source of truth
    /// for which lexer kinds signal incompleteness.
    pub fn is_incomplete(&self) -> bool {
        matches!(
            self,
            Self::UnterminatedString { .. }
                | Self::UnterminatedBalanced { .. }
                | Self::UnclosedDeref { .. }
        )
    }

    /// Render this kind as a single user-facing message line.
    ///
    /// The opening-delimiter position is *not* in this string: the
    /// ariadne renderer draws a secondary label at `opened`, so a `(line,
    /// col)` suffix here would duplicate what the underline already shows.
    pub fn message(&self) -> String {
        match self {
            Self::UnterminatedString { form, inner, .. } => {
                let mut msg = format!("unterminated {form}");
                if let Some(inner) = inner {
                    msg.push_str("; nested ");
                    msg.push_str(&inner.message());
                }
                msg
            }
            Self::UnterminatedBalanced { open, close, .. } => {
                format!("unterminated '{open}…{close}'")
            }
            Self::UnclosedDeref { .. } => "unclosed `$(…)` dereference".into(),
            Self::Other(s) => s.clone(),
        }
    }
}

#[derive(Debug)]
pub struct LexError {
    pub kind: LexErrorKind,
    /// Byte range of the *primary* anchor (the opening delimiter for
    /// "unterminated …" kinds, or the location reported by `error()`
    /// for free-form ones).
    pub span: Span,
}

impl LexError {
    /// User-facing message synthesised from `kind`.  Single source of
    /// truth — there is no separate `message` field to drift from it.
    pub fn message(&self) -> String {
        self.kind.message()
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lex error: {}", self.message())
    }
}

/// Tokenise `source` with a placeholder file id.
///
/// # Errors
/// Returns `Err` if a string, balanced delimiter, or `$(…)` runs to EOF
/// unterminated, or on a free-form lexical fault — an invalid escape or an
/// unexpected character.
pub fn lex(source: &str) -> Result<Vec<(Token, Span)>, LexError> {
    lex_with(source, FileId::DUMMY)
}

/// Tokenise `source` attributing every token's byte-range to `file`.
///
/// # Errors
/// Returns `Err` if a string, balanced delimiter, or `$(…)` runs to EOF
/// unterminated, or on a free-form lexical fault — an invalid escape or an
/// unexpected character.
pub fn lex_with(source: &str, file: FileId) -> Result<Vec<(Token, Span)>, LexError> {
    let mut lexer = Lexer::new(source, file);
    let mut tokens = Vec::new();
    loop {
        let (tok, span) = lexer.next_token()?;
        let is_eof = tok == Token::Eof;
        tokens.push((tok, span));
        if is_eof {
            break;
        }
    }
    Ok(tokens)
}

struct Lexer {
    /// (`byte_offset`, char) for each char. Byte offsets let us stamp byte-range
    /// spans while keeping O(1) peek-by-char-index semantics.
    chars: Vec<(usize, char)>,
    source_len: u32,
    pos: usize,
    file: FileId,
    /// Stack of currently-open delimiters, innermost last.  Newlines are
    /// suppressed when the innermost open delimiter is a `[`-style
    /// bracket — this makes multiline list/map literals work regardless
    /// of whether they appear inside a block body.  Each entry also keeps
    /// the opener's span so an unterminated delimiter at EOF can be
    /// reported (and the REPL can prompt for continuation).
    delim_stack: Vec<OpenDelim>,
}

/// Which kind of paired delimiter is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DelimKind {
    /// `{ … }` — a block; newlines inside are statement separators.
    Brace,
    /// `[ … ]` — a list/map literal; newlines inside are whitespace.
    Bracket,
}

impl DelimKind {
    /// The `(open, close)` characters for this delimiter kind, for
    /// building an [`LexErrorKind::UnterminatedBalanced`].
    fn chars(self) -> (char, char) {
        match self {
            Self::Brace => ('{', '}'),
            Self::Bracket => ('[', ']'),
        }
    }
}

/// An open delimiter on the lexer's stack: its kind plus the span of the
/// opener, so an unterminated delimiter can be anchored at where it opened.
#[derive(Debug, Clone, Copy)]
struct OpenDelim {
    kind: DelimKind,
    opened: Span,
}

impl Lexer {
    fn new(source: &str, file: FileId) -> Self {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "byte length of a source in the u32 span system (< 4 GiB)"
        )]
        let source_len = source.len() as u32;
        Self {
            chars: source.char_indices().collect(),
            source_len,
            pos: 0,
            file,
            delim_stack: Vec::new(),
        }
    }

    /// Current byte offset (one past the last-consumed char).
    fn byte_pos(&self) -> u32 {
        self.chars.get(self.pos).map_or(self.source_len, |(b, _)| {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "byte length of a source in the u32 span system (< 4 GiB)"
            )]
            {
                *b as u32
            }
        })
    }

    /// Zero-width span at the current cursor; the byte range is extended
    /// to cover the full token by [`Self::finish`] once it is consumed.
    fn span(&self) -> Span {
        Span::point(self.file, self.byte_pos())
    }

    /// Extend `start` so its byte range covers up to the current position.
    fn finish(&self, start: Span) -> Span {
        Span::new(start.file, start.start, self.byte_pos())
    }

    fn error(span: Span, message: impl Into<String>) -> LexError {
        Self::typed_error(span, LexErrorKind::Other(message.into()))
    }

    fn typed_error(span: Span, kind: LexErrorKind) -> LexError {
        LexError { kind, span }
    }

    /// Build an `UnterminatedString` error anchored at `span` (the
    /// opening delimiter), optionally carrying an inner-form failure.
    fn err_unterminated_string(
        span: Span,
        form: StringForm,
        inner: Option<Box<LexErrorKind>>,
    ) -> LexError {
        Self::typed_error(
            span,
            LexErrorKind::UnterminatedString {
                form,
                opened: span,
                inner,
            },
        )
    }

    /// If `inner` reports a nested unterminated form, re-anchor it as
    /// the outer string's failure (the user's mistake is "I opened a
    /// string and a `!{…}` inside it; both are still open").  Other
    /// failures (an invalid escape, "expected identifier after `$(`")
    /// pass through unchanged so the user still sees the precise spot.
    fn rewrap_inner_into_string(
        outer_span: Span,
        form: StringForm,
        inner: LexError,
    ) -> LexError {
        if inner.kind.is_incomplete() {
            Self::err_unterminated_string(outer_span, form, Some(Box::new(inner.kind)))
        } else {
            inner
        }
    }

    fn peek(&self) -> Option<char> {
        self.peek_n(0)
    }

    fn peek_n(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).map(|(_, c)| *c)
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += 1;
        Some(ch)
    }

    fn take_while(&mut self, mut pred: impl FnMut(char) -> bool) -> String {
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            if !pred(ch) {
                break;
            }
            out.push(ch);
            self.bump();
        }
        out
    }

    fn suppress_newline(&self) -> bool {
        // Newlines are whitespace inside a `[...]` but separate statements
        // inside a `{...}`.  What matters is the *innermost* currently-open
        // delimiter, not whether any bracket is open somewhere in the stack:
        // a block containing a list (`{ [ ... ] }`) suppresses newlines
        // inside the list, while a list containing a block (`[ { ... } ]`)
        // treats newlines inside the block as statement separators.
        matches!(
            self.delim_stack.last().map(|d| d.kind),
            Some(DelimKind::Bracket)
        )
    }

    fn skip_inline_whitespace(&mut self) {
        while let Some(ch) = self.peek() {
            match ch {
                ' ' | '\t' | '\r' => {
                    self.bump();
                }
                '\n' if self.suppress_newline() => {
                    self.bump();
                }
                _ => break,
            }
        }
    }

    fn skip_comment(&mut self) {
        while self.peek().is_some_and(|ch| ch != '\n') {
            self.bump();
        }
    }

    /// Resolve end of input.  A top-level `{` / `[` left open is an
    /// unterminated delimiter, not a clean EOF: report it anchored at the
    /// innermost opener.  This both gives batch scripts a real diagnostic
    /// and lets the REPL's `needs_continuation` (which keys off
    /// `UnterminatedBalanced`) prompt for the rest of the input.  `span`
    /// is the position the clean `Eof` token should carry.
    fn eof_or_unterminated(&self, span: Span) -> Result<(Token, Span), LexError> {
        if let Some(open) = self.delim_stack.last().copied() {
            let (o, c) = open.kind.chars();
            return Err(Self::typed_error(
                open.opened,
                LexErrorKind::UnterminatedBalanced {
                    open: o,
                    close: c,
                    opened: open.opened,
                },
            ));
        }
        Ok((Token::Eof, span))
    }

    fn next_token(&mut self) -> Result<(Token, Span), LexError> {
        loop {
            self.skip_inline_whitespace();

            let span = self.span();
            let Some(ch) = self.peek() else {
                return self.eof_or_unterminated(span);
            };

            return match ch {
                '#' => {
                    if self.hash_opens_quoted() {
                        let level = self.count_hash_run();
                        for _ in 0..level {
                            self.bump();
                        }
                        self.scan_quoted(span, level)
                    } else {
                        self.skip_comment();
                        match self.peek() {
                            Some('\n') if self.suppress_newline() => {
                                self.bump();
                                continue;
                            }
                            Some('\n') => Ok(self.scan_separator(span)),
                            // The comment ran to end of input — the `Eof`
                            // token spans the end of the source, not the
                            // `#` that opened the comment, but an open
                            // `{` / `[` is still unterminated.
                            _ => self.eof_or_unterminated(self.span()),
                        }
                    }
                }
                '\n' | ';' => Ok(self.scan_separator(span)),
                '{' => Ok(self.open_delim(Token::LBrace, DelimKind::Brace)),
                '}' => Ok(self.close_delim(Token::RBrace, DelimKind::Brace)),
                '[' => Ok(self.open_delim(Token::LBracket, DelimKind::Bracket)),
                ']' => Ok(self.close_delim(Token::RBracket, DelimKind::Bracket)),
                '(' => Ok(self.bump_simple(Token::LParen, span)),
                ')' => Ok(self.bump_simple(Token::RParen, span)),
                '|' => Ok(self.bump_simple(Token::Pipe, span)),
                '&' => Ok(self.bump_simple(Token::Ampersand, span)),
                ',' if self.suppress_newline() => Ok(self.bump_simple(Token::Comma, span)),
                ',' => Ok(self.scan_bare_word(span)),
                '$' => {
                    self.bump();
                    match self.scan_deref()? {
                        Some(StringPart::Expr(toks)) => Ok((Token::Expr(toks), self.finish(span))),
                        Some(part) => Ok((Token::Deref(part), self.finish(span))),
                        None => Ok((Token::Dollar, self.finish(span))),
                    }
                }
                '^' => Ok(self.bump_simple(Token::Caret, span)),
                '!' if self.peek_n(1) == Some('=') => Ok(self.two_char_word("!=", span)),
                '!' => Ok(self.bump_simple(Token::Bang, span)),
                '~' => Ok(self.scan_tilde(span)),
                '?' => Ok(self.bump_simple(Token::Question, span)),
                '\'' => self.scan_quoted(span, 0),
                '"' => self.scan_double_quoted(span),
                '>' if self.peek_n(1) == Some('=') => Ok(self.two_char_word(">=", span)),
                '>' => self.scan_redirect_gt(None, span),
                '<' if self.peek_n(1) == Some('=') => Ok(self.two_char_word("<=", span)),
                '<' => self.scan_redirect_lt(None, span),
                _ if ch.is_ascii_digit() && self.is_fd_redirect_start() => {
                    self.scan_fd_redirect(span)
                }
                '.' if self.peek_n(1) == Some('.') && self.peek_n(2) == Some('.') => {
                    self.bump();
                    self.bump();
                    self.bump();
                    Ok((Token::Spread, self.finish(span)))
                }
                '`' => {
                    self.bump(); // consume '`'
                    let label = self.scan_ident();
                    if label.is_empty() {
                        Err(Self::error(
                            span,
                            "expected tag label after backtick; quote literal backticks",
                        ))
                    } else {
                        Ok((Token::Tag(label), self.finish(span)))
                    }
                }
                _ if is_bare_char(ch) => Ok(self.scan_bare_word(span)),
                _ => {
                    self.bump();
                    Err(Self::error(span, format!("unexpected character: '{ch}'")))
                }
            };
        }
    }

    fn bump_simple(&mut self, token: Token, span: Span) -> (Token, Span) {
        self.bump();
        (token, self.finish(span))
    }

    /// Consume a two-character operator and emit it as a plain word.
    fn two_char_word(&mut self, op: &str, span: Span) -> (Token, Span) {
        self.bump();
        self.bump();
        (Token::Word(Word::Plain(op.into())), self.finish(span))
    }

    fn open_delim(&mut self, token: Token, kind: DelimKind) -> (Token, Span) {
        let span = self.span();
        self.bump();
        let opened = self.finish(span);
        self.delim_stack.push(OpenDelim { kind, opened });
        (token, opened)
    }

    /// Emit a closing-delimiter token, popping the matching opener off
    /// the delim stack.  The pop is *conditional*: it fires only when the
    /// innermost open delimiter is the `kind` this closer matches.  A
    /// wrong-kind closer (`}` while a `[` is open) or one at depth 0
    /// emits its token without touching the stack, so the
    /// newline-suppression state stays in sync with the genuinely open
    /// delimiters and the parser is left to report the mismatch.
    fn close_delim(&mut self, token: Token, kind: DelimKind) -> (Token, Span) {
        let span = self.span();
        self.bump();
        if self.delim_stack.last().map(|d| d.kind) == Some(kind) {
            self.delim_stack.pop();
        }
        (token, self.finish(span))
    }

    fn scan_separator(&mut self, span: Span) -> (Token, Span) {
        self.bump();
        loop {
            match self.peek() {
                Some('\n' | ';' | '\r' | ' ' | '\t') => {
                    self.bump();
                }
                Some('#') if !self.hash_opens_quoted() => self.skip_comment(),
                _ => break,
            }
        }
        (Token::Newline, self.finish(span))
    }

    /// At a `:` held in `peek()`, does it break the bare word rather than
    /// belong to it?  `host: val` splits into `Bare("host"), Colon`; the
    /// `:` in `host:5432` does not.  The rule is the follower after the
    /// colon — defined once and shared by `scan_bare_word` (a leading `:`)
    /// and `scan_bare_fragment` (a `:` mid-fragment).
    fn colon_splits_here(&self) -> bool {
        self.peek_n(1)
            .is_none_or(|next| matches!(next, ' ' | '\t' | '\r' | '\n' | ']'))
    }

    fn scan_bare_word(&mut self, span: Span) -> (Token, Span) {
        if self.peek() == Some(':') && self.colon_splits_here() {
            self.bump();
            return (Token::Colon, self.finish(span));
        }

        let word = self.scan_bare_fragment();
        let token = if word.contains('/') {
            Token::Word(Word::Slash(word))
        } else {
            Token::Word(Word::Plain(word))
        };
        (token, self.finish(span))
    }

    fn scan_bare_fragment(&mut self) -> String {
        let mut word = String::new();
        while let Some(ch) = self.peek() {
            if !is_bare_char(ch) {
                break;
            }

            // Inside `[...]`, comma is punctuation, not part of the word.
            if ch == ',' && self.suppress_newline() {
                break;
            }

            // `host: val`  → Bare("host"), Colon, Bare("val")
            // `host:5432`  → Bare("host:5432")
            if ch == ':' && self.colon_splits_here() {
                break;
            }

            word.push(ch);
            self.bump();
        }
        word
    }

    fn scan_tilde(&mut self, span: Span) -> (Token, Span) {
        self.bump(); // consume '~'
        let suffix = match self.peek() {
            Some(ch) if is_bare_char(ch) => self.scan_bare_fragment(),
            _ => String::new(),
        };
        let raw = format!("~{suffix}");
        let path = TildePath::parse(&raw).expect("tilde token should always parse");
        (Token::Word(Word::Tilde(path)), self.finish(span))
    }

    /// Count the run of `#`s starting at the current position without
    /// consuming.  Used to disambiguate a hash-bumped string opener from
    /// a comment.
    fn count_hash_run(&self) -> usize {
        let mut n = 0;
        while self.peek_n(n) == Some('#') {
            n += 1;
        }
        n
    }

    /// At a `#`, does the run of `#`s open a hash-bumped single-quoted
    /// string rather than a comment?  A run of `level` hashes followed by
    /// `'` opens `#'…'#`; any other follower is a comment.  Shared by
    /// `next_token` and `scan_separator` so the two cannot disagree.
    fn hash_opens_quoted(&self) -> bool {
        self.peek_n(self.count_hash_run()) == Some('\'')
    }

    /// Scan the body of a hash-bumped literal at the given level, calling
    /// `push` for each body char.  Consumes the closing `'` and its
    /// `level` `#`s.  Errs on EOF before the close.
    fn scan_quoted_body<F: FnMut(char)>(
        &mut self,
        span: Span,
        level: usize,
        mut push: F,
    ) -> Result<(), LexError> {
        loop {
            match self.peek() {
                None => {
                    let form = if level == 0 {
                        StringForm::SingleQuoted
                    } else {
                        StringForm::BumpedSingle(level)
                    };
                    return Err(Self::err_unterminated_string(span, form, None));
                }
                Some('\'') => {
                    let mut hashes = 0usize;
                    while self.peek_n(1 + hashes) == Some('#') {
                        hashes += 1;
                    }
                    if hashes >= level {
                        self.bump(); // '
                        for _ in 0..level {
                            self.bump(); // matching #s
                        }
                        return Ok(());
                    }
                    push('\'');
                    self.bump();
                }
                Some(ch) => {
                    push(ch);
                    self.bump();
                }
            }
        }
    }

    /// Scan a single-quoted string at the given hash level.  The leading
    /// `#`s (if any) have already been consumed; the opening `'` is still
    /// in the stream.  Body bytes are verbatim — no escapes, no
    /// interpolation.
    fn scan_quoted(&mut self, span: Span, level: usize) -> Result<(Token, Span), LexError> {
        self.bump(); // opening '
        let mut body = String::new();
        self.scan_quoted_body(span, level, |c| body.push(c))?;
        Ok((Token::SingleQuoted(body), self.finish(span)))
    }

    fn scan_double_quoted(&mut self, span: Span) -> Result<(Token, Span), LexError> {
        self.bump();
        let file = span.file;
        let mut parts: Vec<Spanned<StringPart>> = Vec::new();
        let mut literal = String::new();
        // Byte offset of the first char buffered into `literal` since the
        // last flush; `None` when the buffer is empty.
        let mut literal_start: Option<u32> = None;
        let form = StringForm::DoubleQuoted;

        loop {
            // Byte position of the next char before any bumping.  Used
            // both as the start of a non-literal part and as the start
            // of a literal run when the next char ends up appended to
            // `literal`.
            let cursor = self.byte_pos();
            match self.peek() {
                None => {
                    return Err(Self::err_unterminated_string(span, form, None));
                }
                Some('"') => {
                    Self::flush_literal(&mut parts, &mut literal, &mut literal_start, cursor, file);
                    self.bump();
                    break;
                }
                Some('\\') => {
                    let before = literal.len();
                    self.bump();
                    self.scan_double_quoted_escape(cursor, &mut literal)?;
                    // A `\<newline>` continuation emits nothing.  Anchor the
                    // literal run's start only when the escape actually
                    // produced a char, so a leading continuation does not
                    // stretch the following literal's span back over itself.
                    if literal.len() > before {
                        literal_start.get_or_insert(cursor);
                    }
                }
                Some('$') => {
                    self.bump();
                    match self.scan_deref() {
                        Ok(Some(part)) => {
                            Self::flush_literal(
                                &mut parts,
                                &mut literal,
                                &mut literal_start,
                                cursor,
                                file,
                            );
                            let part_end = self.byte_pos();
                            parts.push(Spanned::new(Span::new(file, cursor, part_end), part));
                        }
                        Ok(None) => {
                            literal_start.get_or_insert(cursor);
                            literal.push('$');
                        }
                        Err(inner) => {
                            return Err(Self::rewrap_inner_into_string(span, form, inner));
                        }
                    }
                }
                Some('!') => {
                    self.bump();
                    match self.peek() {
                        Some('{') => {
                            Self::flush_literal(
                                &mut parts,
                                &mut literal,
                                &mut literal_start,
                                cursor,
                                file,
                            );
                            let open = self.span();
                            self.bump();
                            match self.scan_token_group(open, '{', '}') {
                                Ok(body) => {
                                    let part_end = self.byte_pos();
                                    parts.push(Spanned::new(
                                        Span::new(file, cursor, part_end),
                                        StringPart::Force(body),
                                    ));
                                }
                                Err(inner) => {
                                    return Err(Self::rewrap_inner_into_string(span, form, inner));
                                }
                            }
                        }
                        Some('$') => {
                            Self::flush_literal(
                                &mut parts,
                                &mut literal,
                                &mut literal_start,
                                cursor,
                                file,
                            );
                            // `!$name` desugars to `!{<deref>}` — synthesise
                            // a single-token group rather than re-lexing
                            // `${name}`.  The deref span covers `$name`.
                            let deref_span = self.span();
                            self.bump();
                            let name = self.scan_deref_ident();
                            if name.is_empty() {
                                return Err(Self::error(
                                    deref_span,
                                    "expected identifier after `!$` in double-quoted string",
                                ));
                            }
                            let inner_span = self.finish(deref_span);
                            let part_end = self.byte_pos();
                            parts.push(Spanned::new(
                                Span::new(file, cursor, part_end),
                                StringPart::Force(vec![(
                                    Token::Deref(StringPart::Variable(name)),
                                    inner_span,
                                )]),
                            ));
                        }
                        _ => {
                            literal_start.get_or_insert(cursor);
                            literal.push('!');
                        }
                    }
                }
                Some(ch) => {
                    literal_start.get_or_insert(cursor);
                    literal.push(ch);
                    self.bump();
                }
            }
        }

        Ok((Token::DoubleQuoted(parts), self.finish(span)))
    }

    /// Consume one escape sequence after the `\` (already bumped by the
    /// caller).  `escape_start` is the byte offset of the `\`, so a
    /// malformed escape underlines `\q` itself rather than the string's
    /// opening quote.
    fn scan_double_quoted_escape(
        &mut self,
        escape_start: u32,
        literal: &mut String,
    ) -> Result<(), LexError> {
        // Span of the escape so far: from the `\` to the cursor.  Built
        // at each error site after the offending char is consumed.
        macro_rules! span {
            () => {
                Span::new(self.file, escape_start, self.byte_pos())
            };
        }
        match self.peek() {
            Some('n') => {
                self.bump();
                literal.push('\n');
            }
            Some('r') => {
                self.bump();
                literal.push('\r');
            }
            Some('t') => {
                self.bump();
                literal.push('\t');
            }
            Some('\\') => {
                self.bump();
                literal.push('\\');
            }
            Some('0') => {
                self.bump();
                literal.push('\0');
            }
            Some('e') => {
                self.bump();
                literal.push('\x1b');
            }
            Some('"') => {
                self.bump();
                literal.push('"');
            }
            Some('$') => {
                self.bump();
                literal.push('$');
            }
            Some('!') => {
                self.bump();
                literal.push('!');
            }
            Some('\n') => {
                self.bump();
            }
            Some('\r') => {
                self.bump();
                if self.peek() == Some('\n') {
                    self.bump();
                }
            }
            Some('x') => {
                self.bump();
                let h1 = self
                    .peek()
                    .and_then(|c| c.to_digit(16))
                    .ok_or_else(|| Self::error(span!(), "\\x escape needs two hex digits"))?;
                self.bump();
                let h2 = self
                    .peek()
                    .and_then(|c| c.to_digit(16))
                    .ok_or_else(|| Self::error(span!(), "\\x escape needs two hex digits"))?;
                self.bump();
                let n = (h1 << 4) | h2;
                if n >= 0x80 {
                    return Err(Self::error(
                        span!(),
                        "\\xNN must be \\x00..\\x7F (use Bytes for non-ASCII)",
                    ));
                }
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "n is guarded < 0x80 on line 1048; fits u8"
                )]
                literal.push(n as u8 as char);
            }
            Some('u') => {
                self.bump();
                if self.peek() != Some('{') {
                    return Err(Self::error(span!(), "\\u escape must be \\u{X..}"));
                }
                self.bump();
                let mut digits = String::new();
                loop {
                    match self.peek() {
                        Some('}') => break,
                        Some(c) if c.is_ascii_hexdigit() && digits.len() < 6 => {
                            digits.push(c);
                            self.bump();
                        }
                        _ => {
                            return Err(Self::error(span!(), "\\u{X..} expects 1–6 hex digits"));
                        }
                    }
                }
                if digits.is_empty() {
                    return Err(Self::error(span!(), "\\u{X..} expects 1–6 hex digits"));
                }
                self.bump(); // '}'
                let cp = u32::from_str_radix(&digits, 16).unwrap();
                let ch = char::from_u32(cp).ok_or_else(|| {
                    Self::error(span!(), format!("\\u{{{digits}}} is not a Unicode scalar"))
                })?;
                literal.push(ch);
            }
            Some(ch) => {
                self.bump();
                return Err(Self::error(
                    span!(),
                    format!("unknown escape `\\{ch}` in double-quoted string"),
                ));
            }
            None => {
                return Err(Self::error(span!(), "unterminated double-quoted string after `\\`"));
            }
        }
        Ok(())
    }

    /// Push the buffered literal as a [`StringPart::Literal`] spanned over
    /// the byte range `start..end`.  Caller threads `start` as the byte
    /// position where the first literal char landed and `end` as the byte
    /// position immediately past the last literal char.  Always clears
    /// `start`, so a no-op flush at a non-literal part cannot leave a
    /// stale offset for the next literal run to inherit.  No push when the
    /// buffer is empty.
    fn flush_literal(
        parts: &mut Vec<Spanned<StringPart>>,
        literal: &mut String,
        start: &mut Option<u32>,
        end: u32,
        file: FileId,
    ) {
        let start = start.take();
        if !literal.is_empty() {
            let s = start.expect("literal_start set when buffer is non-empty");
            parts.push(Spanned::new(
                Span::new(file, s, end),
                StringPart::Literal(std::mem::take(literal)),
            ));
        }
    }

    /// Scan a deref after $: $name, $(name), $name[key], $[arith].  The
    /// bare `$name` form reads its name with [`Self::scan_deref_ident`],
    /// which stops before a trailing `-` so `$os-$arch` splits into two
    /// derefs; the explicit-boundary form `$(name)` uses [`Self::scan_ident`]
    /// directly and keeps a trailing `-`.  Both draw on the identifier
    /// alphabet `[a-zA-Z_][a-zA-Z0-9_-]*` ([`is_ident_start`]/[`is_ident_cont`]).
    /// Returns None for bare $ (not followed by ident/paren/bracket).
    fn scan_deref(&mut self) -> Result<Option<StringPart>, LexError> {
        match self.peek() {
            Some(ch) if is_ident_start(ch) => {
                // `self.span()` here is the cursor position after the
                // `$` (consumed by the caller); the ident runs from
                // there to wherever `scan_ident` leaves the cursor.
                let name_start = self.span().start;
                let name_file = self.span().file;
                let name = self.scan_deref_ident();
                let name_end = self.span().start;
                let name = Spanned::new(Span::new(name_file, name_start, name_end), name);
                let mut keys: Vec<Spanned<Vec<(Token, Span)>>> = Vec::new();
                while self.peek() == Some('[') {
                    let open = self.span();
                    self.bump();
                    let body = self.scan_token_group(open, '[', ']')?;
                    // `scan_token_group` returns once it has consumed
                    // the matching `]`; `self.span()` is now the byte
                    // span of the token *after* `]`, so its `.start`
                    // is exactly one past the closing bracket.
                    let after_close = self.span().start;
                    let key_span = Span::new(open.file, open.start, after_close);
                    keys.push(Spanned::new(key_span, body));
                }
                Ok(Some(if keys.is_empty() {
                    StringPart::Variable(name.item)
                } else {
                    StringPart::Index { name, keys }
                }))
            }
            Some('(') => {
                let span = self.span();
                self.bump();
                let name = self.scan_ident();
                // Distinguish "syntactic mistake" from "still open at EOF":
                // `$(123)` is the former, `$(` then EOF is the latter, and
                // the latter belongs to UnclosedDeref so an enclosing
                // double-quoted string can re-anchor it.
                if self.peek().is_none() {
                    return Err(
                        Self::typed_error(span, LexErrorKind::UnclosedDeref { opened: span })
                    );
                }
                if name.is_empty() {
                    return Err(Self::error(span, "expected identifier after '$('"));
                }
                if self.peek() != Some(')') {
                    return Err(Self::error(span, "expected ')' to close '$(...)' dereference"));
                }
                self.bump();
                Ok(Some(StringPart::Variable(name)))
            }
            Some('[') => {
                let open = self.span();
                self.bump();
                Ok(Some(StringPart::Expr(self.scan_expr_block(open)?)))
            }
            _ => Ok(None),
        }
    }

    fn scan_ident(&mut self) -> String {
        let Some(ch) = self.peek() else {
            return String::new();
        };
        if !is_ident_start(ch) {
            return String::new();
        }

        let mut name = String::new();
        name.push(ch);
        self.bump();
        name.push_str(&self.take_while(is_ident_cont));
        name
    }

    /// Scan the name of a bare `$name` or `!$name` dereference.  A `-` is a
    /// valid interior name char, but a *trailing* one is left in the stream
    /// as literal text rather than eaten into the name: `$os-$arch`
    /// interpolates `os` and `arch` around a literal `-`, and `$foo-` names
    /// `foo`.  The explicit-boundary form `$(name)` keeps a trailing `-`,
    /// since its parens already fix where the name ends.
    fn scan_deref_ident(&mut self) -> String {
        let mut name = self.scan_ident();
        while name.ends_with('-') {
            name.pop();
            self.pos -= 1;
        }
        name
    }

    /// Lex tokens inside a balanced `open`/`close` pair until the
    /// matching close.  The opening delimiter is *already consumed* by
    /// the caller; `opener` is its span (used to anchor an
    /// `UnterminatedBalanced` error if EOF arrives first).  The closing
    /// delimiter is consumed by this method and *not* included in the
    /// returned stream.
    ///
    /// Newline handling matches the rest of the lexer: a `[` opener
    /// makes newlines whitespace (list/map-literal style), a `{` opener
    /// makes them statement separators (block style).  Both are managed
    /// by the existing `delim_stack` — the caller already consumed the
    /// open without going through [`Self::open_delim`], so we push
    /// once on entry.  Nested groups inside the body push/pop through
    /// [`Self::open_delim`] / [`Self::close_delim`] in
    /// [`Self::next_token`], so the recursion depth is already tracked
    /// centrally — we watch `delim_stack.len()` drop back below the
    /// entry level to recognise our matching close.
    fn scan_token_group(
        &mut self,
        opener: Span,
        open: char,
        close: char,
    ) -> Result<Vec<(Token, Span)>, LexError> {
        debug_assert!(matches!((open, close), ('{', '}') | ('[', ']')));
        // Lexer recursion runs through this method (interpolation
        // `!{…}`, expression block `$[…]`, indexed deref `$name[k]`),
        // so `delim_stack.len()` doubles as the recursion depth.  Cap
        // it so adversarial input like `$[$[$[$[…` rejects cleanly
        // rather than overflowing the call stack.  Real programs sit
        // well below the cap.
        if self.delim_stack.len() >= crate::syntax::NESTING_DEPTH_LIMIT {
            return Err(Self::error(opener, crate::syntax::nesting_too_deep_message()));
        }
        // The caller already consumed the opener without going through
        // `open_delim`, so we mirror its delim_stack push here.  On a
        // successful matching close, `next_token`'s `close_delim` pops
        // it for us; on every error exit we pop explicitly to leave
        // the lexer's state coherent.
        self.delim_stack.push(OpenDelim {
            kind: match open {
                '[' => DelimKind::Bracket,
                '{' => DelimKind::Brace,
                _ => unreachable!("scan_token_group opener is restricted to '[' or '{{'"),
            },
            opened: opener,
        });
        let entry_depth = self.delim_stack.len();
        let mut tokens = Vec::new();

        loop {
            let (tok, span) = match self.next_token() {
                Ok(pair) => pair,
                Err(e) => {
                    self.delim_stack.pop();
                    return Err(e);
                }
            };
            match (&tok, open) {
                // The matching close: `next_token`'s `close_delim`
                // already popped our delim, so the stack now sits one
                // below the entry level.  Mismatched openers (e.g. a
                // `]` while scanning a `{…}` group) fall through to
                // the catch-all and are pushed as-is — the parser
                // catches those.
                (Token::RBrace, '{') | (Token::RBracket, '[')
                    if self.delim_stack.len() < entry_depth =>
                {
                    return Ok(tokens);
                }
                // EOF inside an open group is unreachable: `next_token`
                // routes end of input through `eof_or_unterminated`, which
                // returns `Err(UnterminatedBalanced …)` whenever a delim is
                // open — and our own group delim is always open here.  That
                // `Err` is caught above, so the loop never sees an `Eof`.
                (Token::Eof, _) => {
                    unreachable!("next_token cannot yield Eof while a delim is open")
                }
                _ => tokens.push((tok, span)),
            }
        }
    }

    /// Lex the body of an expression block `$[…]`.  The opening `[` is
    /// already consumed.  Inside an expression block, adjacent `&` /
    /// `|` pairs are the logical operators `&&` / `||` — the lexer
    /// fuses them here so the parser sees the same tokens it would for
    /// a bare-word `&&` outside any nesting.  Fusion lives in the
    /// lexer because the contextual cue (we're inside `$[…]`) is
    /// lexical; SPEC §1 documents the rule.
    fn scan_expr_block(&mut self, opener: Span) -> Result<Vec<(Token, Span)>, LexError> {
        let raw = self.scan_token_group(opener, '[', ']')?;
        Ok(fuse_paired_pipeline_ops(raw))
    }

    fn is_fd_redirect_start(&self) -> bool {
        let mut offset = 0;
        while self.peek_n(offset).is_some_and(|ch| ch.is_ascii_digit()) {
            offset += 1;
        }
        matches!(self.peek_n(offset), Some('>' | '<'))
    }

    fn scan_fd_redirect(&mut self, span: Span) -> Result<(Token, Span), LexError> {
        let fd_digits = self.take_while(|ch| ch.is_ascii_digit());

        match self.peek() {
            Some('>') => {
                let fd = Some(Self::parse_fd(&fd_digits, span)?);
                self.scan_redirect_gt(fd, span)
            }
            Some('<') => {
                let fd = Some(Self::parse_fd(&fd_digits, span)?);
                self.scan_redirect_lt(fd, span)
            }
            // `scan_fd_redirect` is only entered after `is_fd_redirect_start`
            // confirmed a `>`/`<` follows the digit run, and `take_while`
            // consumed exactly that run — so the next char is always one of them.
            _ => unreachable!("scan_fd_redirect entered without a trailing '>' or '<'"),
        }
    }

    /// Parse the digit-prefix of an fd redirect.  Empty input is a bug
    /// (the caller only enters this path after seeing at least one digit
    /// followed by `>` or `<`), so the digit string must be non-empty.
    /// Overflow is a hard error — silently coercing `99999999999>` to
    /// fd 1 is exactly the bash-style sloppy inheritance we want to
    /// avoid.
    fn parse_fd(digits: &str, span: Span) -> Result<u32, LexError> {
        debug_assert!(!digits.is_empty(), "scan_fd_redirect called without digits");
        digits.parse::<u32>().map_err(|_| {
            Self::error(
                span,
                format!("file descriptor '{digits}' does not fit in u32"),
            )
        })
    }

    /// Build a `Token::Redirect` and finish its span — one place to keep
    /// the field order and the span-finish call in sync.
    fn finish_redirect(
        &self,
        fd: Option<u32>,
        kind: RedirectMode,
        target_fd: Option<u32>,
        span: Span,
    ) -> (Token, Span) {
        (
            Token::Redirect {
                fd,
                kind,
                target_fd,
            },
            self.finish(span),
        )
    }

    fn scan_redirect_gt(&mut self, fd: Option<u32>, span: Span) -> Result<(Token, Span), LexError> {
        self.bump();
        if self.peek() == Some('>') {
            self.bump();
            return Ok(self.finish_redirect(fd, RedirectMode::Append, None, span));
        }
        // `>~` is the stream-write operator only when the `~` stands
        // alone; `>~/path` and `>~user/path` are a plain write whose
        // target is a tilde path, so a bare char after `~` (the start of
        // a tilde-path suffix) yields a `Write` redirect and lets the
        // following `~…` lex as its own `Tilde` word.
        if self.peek() == Some('~') && !self.peek_n(1).is_some_and(is_bare_char) {
            self.bump();
            return Ok(self.finish_redirect(fd, RedirectMode::StreamWrite, None, span));
        }
        if self.peek() == Some('&') {
            self.bump();
            let target_digits = self.take_while(|ch| ch.is_ascii_digit());
            if target_digits.is_empty() {
                return Err(Self::error(span, "expected file descriptor after '>&'"));
            }
            let n = Self::parse_fd(&target_digits, span)?;
            return Ok(self.finish_redirect(fd, RedirectMode::Write, Some(n), span));
        }
        Ok(self.finish_redirect(fd, RedirectMode::Write, None, span))
    }

    fn scan_redirect_lt(&mut self, fd: Option<u32>, span: Span) -> Result<(Token, Span), LexError> {
        self.bump();
        if self.peek() == Some('<') {
            self.bump();
            if self.peek() == Some('<') {
                self.bump();
                return Err(Self::error(
                    self.finish(span),
                    "`<<<` is bash's here-string operator — ral's `<<` already \
                     feeds a string to stdin, so drop one `<`",
                ));
            }
            // A payload glued to `<<` is near-certainly the bash heredoc
            // reflex (`<<EOF`, `<<'EOF'`); a genuine here-string is spelled
            // with a space. Rejecting the glued form here keeps the quoted
            // heredoc delimiter from silently becoming stdin while the
            // intended body lines run as stray commands.
            if self.peek().is_some_and(|ch| !ch.is_whitespace()) {
                return Err(Self::error(
                    self.finish(span),
                    "ral has no heredocs: `<<` feeds a string to stdin and \
                     takes a space before its payload — `cmd << #' ... '#` \
                     (a raw string, which may use newlines)",
                ));
            }
            return Ok(self.finish_redirect(fd, RedirectMode::HereString, None, span));
        }
        Ok(self.finish_redirect(fd, RedirectMode::Read, None, span))
    }
}

/// Fuse adjacent `&` / `|` pairs in an expression-block token stream
/// into the logical operators `&&` / `||`.  This is purely a contextual
/// rewrite: outside `$[…]` the standalone `&` and `|` keep their
/// pipeline meaning, so the lexer only invokes this from
/// [`Lexer::scan_expr_block`].
///
/// The two members must be byte-adjacent: `&&` is one operator, but
/// `& &` (a space between) is two pipeline backgrounders and is left
/// alone.  The output operator takes the span of the first member of the
/// pair — adequate for diagnostics at the operator position, and the
/// second member's span is one byte to the right so error labels stay
/// tight.
fn fuse_paired_pipeline_ops(tokens: Vec<(Token, Span)>) -> Vec<(Token, Span)> {
    let mut out: Vec<(Token, Span)> = Vec::with_capacity(tokens.len());
    let mut iter = tokens.into_iter().peekable();
    while let Some((tok, span)) = iter.next() {
        let adjacent = iter.peek().is_some_and(|(_, next)| span.end == next.start);
        let fused = match (&tok, iter.peek().map(|(t, _)| t)) {
            (Token::Ampersand, Some(Token::Ampersand)) if adjacent => Some("&&"),
            (Token::Pipe, Some(Token::Pipe)) if adjacent => Some("||"),
            _ => None,
        };
        if let Some(s) = fused {
            iter.next();
            out.push((Token::Word(Word::Plain(s.into())), span));
        } else {
            out.push((tok, span));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> Token {
        Token::Word(Word::Plain(s.into()))
    }

    fn slash(s: &str) -> Token {
        Token::Word(Word::Slash(s.into()))
    }

    fn tilde_tok(user: Option<&str>, suffix: Option<&str>) -> Token {
        Token::Word(Word::Tilde(TildePath {
            user: user.map(str::to_owned),
            suffix: suffix.map(str::to_owned),
        }))
    }

    fn tok_types(source: &str) -> Vec<Token> {
        lex(source).unwrap().into_iter().map(|(t, _)| t).collect()
    }

    fn lex_ok(source: &str) -> Vec<Token> {
        lex(source)
            .unwrap_or_else(|e| panic!("expected Ok: {source:?}\n  error: {}", e.message()))
            .into_iter()
            .map(|(t, _)| t)
            .collect()
    }

    fn lex_err(source: &str) -> String {
        match lex(source) {
            Err(e) => e.message(),
            Ok(_) => panic!("expected Err: {source:?}"),
        }
    }

    fn lex_err_span(source: &str) -> Span {
        match lex(source) {
            Err(e) => e.span,
            Ok(_) => panic!("expected Err: {source:?}"),
        }
    }

    /// F13: a bad escape underlines the escape itself, not the string's
    /// opening quote.  In `"abc\q"` the `\q` is at bytes 4..6.
    #[test]
    fn bad_escape_spans_the_escape_not_the_quote() {
        let span = lex_err_span(r#""abc\q""#);
        assert_eq!((span.start, span.end), (4, 6));
    }

    /// F13: a comment that runs to end of input yields an `Eof` token
    /// spanned at the end of the source, not at the `#` that opened it.
    #[test]
    fn trailing_comment_eof_spans_end_of_input() {
        let src = "echo a # tail";
        let (tok, span) = lex(src).unwrap().pop().unwrap();
        assert_eq!(tok, Token::Eof);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "test literal length; trivially fits u32"
        )]
        {
            assert_eq!(span.start, src.len() as u32);
        }
    }

    /// After a newline or `;` separator, a hash-bumped single-quoted
    /// string opens a literal; only a bare `#` run starts a comment.  The
    /// `#'…'#` opener must survive the separator run, not be swallowed.
    #[test]
    fn hash_quoted_string_after_separator() {
        let expect = vec![
            plain("echo"),
            plain("a"),
            Token::Newline,
            Token::SingleQuoted("hi".into()),
            Token::Eof,
        ];
        assert_eq!(tok_types("echo a\n#'hi'#"), expect);
        assert_eq!(tok_types("echo a;#'hi'#"), expect);
    }

    /// A `\`-continuation in a double-quoted string appends nothing, so
    /// the no-op flush at the following `$x` must not leak the backslash
    /// offset into the trailing literal's span.  In `"\<nl>$x y"` the
    /// trailing literal ` y` is at bytes 5..7, not 1..7.
    #[test]
    fn line_continuation_does_not_stretch_literal_span() {
        let toks = lex("\"\\\n$x y\"").unwrap();
        let Token::DoubleQuoted(parts) = &toks[0].0 else {
            panic!("expected DoubleQuoted");
        };
        assert_eq!(parts.len(), 2);
        let var = parts[0].span.unwrap();
        let lit = parts[1].span.unwrap();
        assert_eq!(parts[0].item, StringPart::Variable("x".into()));
        assert_eq!((var.start, var.end), (3, 5));
        assert_eq!(parts[1].item, StringPart::Literal(" y".into()));
        assert_eq!((lit.start, lit.end), (5, 7));
    }

    /// The same rule when the continuation is *adjacent* to literal text
    /// with no intervening flush: a leading `\`-continuation emits nothing,
    /// so the literal run must start at the first real char.  In
    /// `"\<nl>abc"` the literal `abc` is at bytes 3..6, not 1..6.
    #[test]
    fn line_continuation_does_not_stretch_following_literal() {
        let toks = lex("\"\\\nabc\"").unwrap();
        let Token::DoubleQuoted(parts) = &toks[0].0 else {
            panic!("expected DoubleQuoted");
        };
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].item, StringPart::Literal("abc".into()));
        let lit = parts[0].span.unwrap();
        assert_eq!((lit.start, lit.end), (3, 6));
    }

    #[test]
    fn bare_words() {
        let toks = tok_types("ls -la /tmp");
        assert_eq!(
            toks,
            vec![plain("ls"), plain("-la"), slash("/tmp"), Token::Eof,]
        );
    }

    #[test]
    fn assignment() {
        // With `let`, = is just a bare word. No special lexer rule.
        let toks = tok_types("let x = hello");
        assert_eq!(
            toks,
            vec![
                plain("let"),
                plain("x"),
                plain("="),
                plain("hello"),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn variable() {
        let toks = tok_types("echo $x");
        assert_eq!(
            toks,
            vec![
                plain("echo"),
                Token::Deref(StringPart::Variable("x".into())),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn single_quoted() {
        let toks = tok_types("echo 'hello world'");
        assert_eq!(
            toks,
            vec![
                plain("echo"),
                Token::SingleQuoted("hello world".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn single_quoted_embed_via_bump() {
        let toks = tok_types("echo #'it's'#");
        assert_eq!(
            toks,
            vec![
                plain("echo"),
                Token::SingleQuoted("it's".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn newlines_as_separators() {
        let toks = tok_types("echo a\necho b");
        assert_eq!(
            toks,
            vec![
                plain("echo"),
                plain("a"),
                Token::Newline,
                plain("echo"),
                plain("b"),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn crlf_terminates_bare_words() {
        let toks = tok_types("echo a\r\necho b");
        assert_eq!(
            toks,
            vec![
                plain("echo"),
                plain("a"),
                Token::Newline,
                plain("echo"),
                plain("b"),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn newlines_suppressed_in_brackets() {
        let toks = tok_types("[a,\nb,\nc]");
        assert_eq!(
            toks,
            vec![
                Token::LBracket,
                plain("a"),
                Token::Comma,
                plain("b"),
                Token::Comma,
                plain("c"),
                Token::RBracket,
                Token::Eof,
            ]
        );
    }

    /// F12: a wrong-kind closer must not pop the delim stack — otherwise
    /// the newline-suppression state desyncs for the rest of the group.
    /// Here a stray `}` sits inside a `[…]`; the bracket stays open, so
    /// the newline after it is still suppressed (no `Newline` token).
    #[test]
    fn mismatched_closer_does_not_desync_newline_suppression() {
        let toks = tok_types("[a }\nb]");
        assert_eq!(
            toks,
            vec![
                Token::LBracket,
                plain("a"),
                Token::RBrace,
                plain("b"),
                Token::RBracket,
                Token::Eof,
            ],
            "the stray `}}` must not pop the bracket and re-enable newlines"
        );
    }

    #[test]
    fn commas_are_bare_outside_brackets() {
        let toks = tok_types("echo a,b,c");
        assert_eq!(toks, vec![plain("echo"), plain("a,b,c"), Token::Eof]);
    }

    #[test]
    fn dot_is_bare_word_char() {
        let toks = tok_types("echo .env");
        assert_eq!(toks, vec![plain("echo"), plain(".env"), Token::Eof]);
    }

    #[test]
    fn backtick_tag_token() {
        let toks = tok_types("return `ok 5");
        assert_eq!(
            toks,
            vec![
                plain("return"),
                Token::Tag("ok".into()),
                plain("5"),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn bare_backtick_is_lex_error() {
        let err = lex_err("echo `");
        assert!(err.contains("expected tag label after backtick"));
    }

    #[test]
    fn block_tokens() {
        let toks = tok_types("{ echo hello }");
        assert_eq!(
            toks,
            vec![
                Token::LBrace,
                plain("echo"),
                plain("hello"),
                Token::RBrace,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn pipe_and_question() {
        let toks = tok_types("a | b ? c");
        assert_eq!(
            toks,
            vec![
                plain("a"),
                Token::Pipe,
                plain("b"),
                Token::Question,
                plain("c"),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn redirect() {
        let toks = tok_types("echo hello > out.txt");
        assert!(matches!(
            toks[2],
            Token::Redirect {
                fd: None,
                kind: RedirectMode::Write,
                target_fd: None
            }
        ));
    }

    #[test]
    fn redirect_stderr() {
        let toks = tok_types("cmd 2> err.log");
        assert!(matches!(
            toks[1],
            Token::Redirect {
                fd: Some(2),
                kind: RedirectMode::Write,
                target_fd: None
            }
        ));
    }

    #[test]
    fn redirect_stderr_to_stdout() {
        let toks = tok_types("cmd 2>&1");
        assert!(matches!(
            toks[1],
            Token::Redirect {
                fd: Some(2),
                kind: RedirectMode::Write,
                target_fd: Some(1)
            }
        ));
    }

    /// F6: `>~/path` is a plain write whose target is a tilde path —
    /// the `>~` stream-write operator must not swallow the `~`, or the
    /// redirect would target `/path` instead of `$HOME/path`.
    #[test]
    fn redirect_gt_then_tilde_path() {
        let toks = tok_types("echo hi >~/dir");
        assert!(
            matches!(
                toks[2],
                Token::Redirect {
                    fd: None,
                    kind: RedirectMode::Write,
                    target_fd: None
                }
            ),
            "expected a plain Write redirect, got {:?}",
            toks[2]
        );
        assert_eq!(toks[3], tilde_tok(None, Some("/dir")));
    }

    /// `>~` stays the stream-write operator when the `~` stands alone
    /// (no tilde-path suffix follows).
    #[test]
    fn redirect_gt_tilde_standalone_is_stream_write() {
        let toks = tok_types("echo hi >~ sock");
        assert!(
            matches!(
                toks[2],
                Token::Redirect {
                    fd: None,
                    kind: RedirectMode::StreamWrite,
                    target_fd: None
                }
            ),
            "expected a StreamWrite redirect, got {:?}",
            toks[2]
        );
    }

    #[test]
    fn spread() {
        let toks = tok_types("[...$a, b]");
        assert_eq!(toks[1], Token::Spread);
        assert_eq!(toks[2], Token::Deref(StringPart::Variable("a".into())));
    }

    #[test]
    fn lambda_tokens() {
        let toks = tok_types("{ |x| echo $x }");
        assert_eq!(
            toks,
            vec![
                Token::LBrace,
                Token::Pipe,
                plain("x"),
                Token::Pipe,
                plain("echo"),
                Token::Deref(StringPart::Variable("x".into())),
                Token::RBrace,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn hash_midword_is_bare() {
        // # is only a comment when it starts a token; mid-word it is literal.
        let toks = tok_types("curl http://host:8080/foo#anchor");
        assert_eq!(
            toks,
            vec![
                plain("curl"),
                slash("http://host:8080/foo#anchor"),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn comment() {
        let toks = tok_types("echo a # comment\necho b");
        assert_eq!(
            toks,
            vec![
                plain("echo"),
                plain("a"),
                Token::Newline,
                plain("echo"),
                plain("b"),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn double_quoted_interpolation() {
        let toks = tok_types("echo \"hello $name\"");
        assert_eq!(toks.len(), 3); // echo, doubleQuoted, eof
        match &toks[1] {
            Token::DoubleQuoted(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0].item, StringPart::Literal("hello ".into()));
                assert_eq!(parts[1].item, StringPart::Variable("name".into()));
            }
            _ => panic!("expected DoubleQuoted"),
        }
    }

    /// A bare `$name` never eats a trailing `-`: `"$os-$arch"` splits into
    /// two derefs around a literal `-`, so kebab-adjacent interpolations
    /// don't silently fold the dash into the first name.
    #[test]
    fn interpolation_stops_before_trailing_dash() {
        let toks = tok_types("\"$os-$arch\"");
        let Token::DoubleQuoted(parts) = &toks[0] else {
            panic!("expected DoubleQuoted");
        };
        let items: Vec<&StringPart> = parts.iter().map(|p| &p.item).collect();
        assert_eq!(
            items,
            vec![
                &StringPart::Variable("os".into()),
                &StringPart::Literal("-".into()),
                &StringPart::Variable("arch".into()),
            ]
        );
    }

    /// A `-` with more name after it is still an interior name char, so a
    /// genuine kebab identifier `$os-arch` stays a single deref.
    #[test]
    fn interpolation_keeps_interior_dash() {
        let toks = tok_types("\"$os-arch\"");
        let Token::DoubleQuoted(parts) = &toks[0] else {
            panic!("expected DoubleQuoted");
        };
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].item, StringPart::Variable("os-arch".into()));
    }

    /// A trailing `-` at end of string names the identifier without it and
    /// leaves the `-` as literal text.
    #[test]
    fn interpolation_trailing_dash_at_end() {
        let toks = tok_types("\"$foo-\"");
        let Token::DoubleQuoted(parts) = &toks[0] else {
            panic!("expected DoubleQuoted");
        };
        let items: Vec<&StringPart> = parts.iter().map(|p| &p.item).collect();
        assert_eq!(
            items,
            vec![
                &StringPart::Variable("foo".into()),
                &StringPart::Literal("-".into()),
            ]
        );
    }

    /// The explicit-boundary form `$(name)` fixes where the name ends, so it
    /// keeps a trailing `-` that the bare form would drop.
    #[test]
    fn explicit_boundary_keeps_trailing_dash() {
        let toks = tok_types("\"$(foo-)\"");
        let Token::DoubleQuoted(parts) = &toks[0] else {
            panic!("expected DoubleQuoted");
        };
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].item, StringPart::Variable("foo-".into()));
    }

    /// A bare deref outside strings (`Token::Deref`) obeys the same rule:
    /// `$os-$arch` is two derefs around a literal-`-` bare word.
    #[test]
    fn bare_deref_stops_before_trailing_dash() {
        let toks = tok_types("$os-$arch");
        assert_eq!(
            toks,
            vec![
                Token::Deref(StringPart::Variable("os".into())),
                plain("-"),
                Token::Deref(StringPart::Variable("arch".into())),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn double_quoted_substitution() {
        let toks = tok_types("\"!{echo hello}\"");
        match &toks[0] {
            Token::DoubleQuoted(parts) => {
                assert_eq!(parts.len(), 1);
                let StringPart::Force(inner) = &parts[0].item else {
                    panic!("expected Force");
                };
                // Two tokens lexed inline from the inner `echo hello`:
                // the lexer no longer slices a raw substring back out.
                let kinds: Vec<&Token> = inner.iter().map(|(t, _)| t).collect();
                assert_eq!(kinds, vec![&plain("echo"), &plain("hello")]);
            }
            _ => panic!("expected DoubleQuoted"),
        }
    }

    #[test]
    fn double_quoted_x_escape() {
        // \x41 = 'A'; \x7F = DEL (upper ASCII boundary).
        let t = |src: &str| match &lex_ok(src)[0] {
            Token::DoubleQuoted(p) => match &p[0].item {
                StringPart::Literal(s) => s.clone(),
                _ => panic!("expected Literal"),
            },
            _ => panic!("expected DoubleQuoted"),
        };
        assert_eq!(t(r#""\x41""#), "A");
        assert_eq!(t(r#""\x7F""#), "\x7F");
        assert_eq!(t(r#""\x00""#), "\x00");
        // \x80 is rejected.
        assert!(lex_err(r#""\x80""#).contains("\\xNN"));
        // Non-hex digits rejected.
        assert!(lex_err(r#""\xZZ""#).contains("two hex digits"));
        // Only one hex digit (bare 'r' is unknown escape after that).
        assert!(lex_err(r#""\x4""#).contains("two hex digits"));
    }

    #[test]
    fn double_quoted_u_escape() {
        let t = |src: &str| match &lex_ok(src)[0] {
            Token::DoubleQuoted(p) => match &p[0].item {
                StringPart::Literal(s) => s.clone(),
                _ => panic!("expected Literal"),
            },
            _ => panic!("expected DoubleQuoted"),
        };
        // Basic code points.
        assert_eq!(t(r#""\u{41}""#), "A");
        assert_eq!(t(r#""\u{0}""#), "\x00");
        assert_eq!(t(r#""\u{1F600}""#), "😀");
        // Surrogate, out-of-range, too many digits, no braces all rejected.
        assert!(lex_err(r#""\u{D800}""#).contains("Unicode scalar"));
        assert!(lex_err(r#""\u{110000}""#).contains("Unicode scalar"));
        assert!(lex_err(r#""\u{1234567}""#).contains("1–6 hex digits"));
        assert!(lex_err(r#""\u{}""#).contains("1–6 hex digits"));
        assert!(lex_err(r#""\u41""#).contains("\\u escape must be"));
    }

    #[test]
    fn dollar_bracket_arithmetic() {
        let toks = tok_types("$[2 + 3]");
        let Token::Expr(inner) = &toks[0] else {
            panic!("expected Expr token");
        };
        let kinds: Vec<&Token> = inner.iter().map(|(t, _)| t).collect();
        assert_eq!(kinds, vec![&plain("2"), &plain("+"), &plain("3")]);
    }

    #[test]
    fn dollar_bracket_fuses_logical_ops() {
        // `&&` and `||` are paired by the lexer inside `$[…]` —
        // outside, they remain single-char pipeline punctuation.
        let toks = tok_types("$[1 && 0]");
        let Token::Expr(inner) = &toks[0] else {
            panic!("expected Expr token");
        };
        let kinds: Vec<&Token> = inner.iter().map(|(t, _)| t).collect();
        assert_eq!(kinds, vec![&plain("1"), &plain("&&"), &plain("0")]);
    }

    /// F9: fusion is by byte adjacency — `& &` with a space between is
    /// two separate backgrounders, not the `&&` operator.
    #[test]
    fn dollar_bracket_does_not_fuse_spaced_ampersands() {
        let toks = tok_types("$[1 & & 0]");
        let Token::Expr(inner) = &toks[0] else {
            panic!("expected Expr token");
        };
        let kinds: Vec<&Token> = inner.iter().map(|(t, _)| t).collect();
        assert_eq!(
            kinds,
            vec![
                &plain("1"),
                &Token::Ampersand,
                &Token::Ampersand,
                &plain("0"),
            ]
        );
    }

    #[test]
    fn dollar_bracket_inner_spans_offset_into_outer_source() {
        // Spans on inner tokens point at the outer source bytes, so
        // diagnostics raised inside `$[…]` underline the right column.
        let toks = tok_types("$[42]");
        let Token::Expr(inner) = &toks[0] else {
            panic!("expected Expr token");
        };
        assert_eq!(inner.len(), 1);
        let (_, span) = &inner[0];
        // `42` lives at bytes 2..4 of the input — the `$` and `[`
        // each take one byte.
        assert_eq!(span.start, 2);
        assert_eq!(span.end, 4);
    }

    #[test]
    fn semicolon_separator() {
        let toks = tok_types("echo a; echo b");
        assert_eq!(
            toks,
            vec![
                plain("echo"),
                plain("a"),
                Token::Newline,
                plain("echo"),
                plain("b"),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn colon_context_sensitive() {
        // Trailing colon before whitespace → splits
        let toks = tok_types("host: val");
        assert_eq!(
            toks,
            vec![plain("host"), Token::Colon, plain("val"), Token::Eof,]
        );
        // Embedded colon → stays as one token
        let toks = tok_types("localhost:5432");
        assert_eq!(toks, vec![plain("localhost:5432"), Token::Eof]);
    }

    #[test]
    fn equals_not_special() {
        // = is a normal bare char. No context-sensitive splitting.
        let toks = tok_types("x = 5");
        assert_eq!(toks, vec![plain("x"), plain("="), plain("5"), Token::Eof,]);
        let toks = tok_types("-DFOO=bar");
        assert_eq!(toks, vec![plain("-DFOO=bar"), Token::Eof]);
    }

    #[test]
    fn map_literal() {
        let toks = tok_types("[host: localhost, port: 8080]");
        assert_eq!(
            toks,
            vec![
                Token::LBracket,
                plain("host"),
                Token::Colon,
                plain("localhost"),
                Token::Comma,
                plain("port"),
                Token::Colon,
                plain("8080"),
                Token::RBracket,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn empty_lambda() {
        let toks = tok_types("|| { echo hello }");
        assert_eq!(
            toks,
            vec![
                Token::Pipe,
                Token::Pipe,
                Token::LBrace,
                plain("echo"),
                plain("hello"),
                Token::RBrace,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn tilde() {
        let toks = tok_types("~");
        assert_eq!(toks, vec![tilde_tok(None, None), Token::Eof,]);
    }

    #[test]
    fn tilde_is_not_part_of_bare_word() {
        let toks = tok_types("foo~bar");
        assert_eq!(
            toks,
            vec![plain("foo"), tilde_tok(Some("bar"), None), Token::Eof,]
        );
    }

    #[test]
    fn tilde_path_token_is_structured() {
        let toks = tok_types("~/bin/claude");
        assert_eq!(
            toks,
            vec![tilde_tok(None, Some("/bin/claude")), Token::Eof,]
        );
    }

    #[test]
    fn slash_bearing_bare_word_is_path_token() {
        let toks = tok_types("./script");
        assert_eq!(toks, vec![slash("./script"), Token::Eof]);
    }

    #[test]
    fn tilde_with_space_stays_two_tokens() {
        let toks = tok_types("~ foo");
        assert_eq!(toks, vec![tilde_tok(None, None), plain("foo"), Token::Eof,]);
    }

    #[test]
    fn caret_is_not_part_of_bare_word() {
        let toks = tok_types("^git");
        assert_eq!(toks, vec![Token::Caret, plain("git"), Token::Eof]);
    }

    #[test]
    fn caret_splits_bare_words() {
        let toks = tok_types("foo^bar");
        assert_eq!(
            toks,
            vec![plain("foo"), Token::Caret, plain("bar"), Token::Eof,]
        );
    }

    #[test]
    fn backslash_not_special_in_middle() {
        // \ not before \n: tokenizes as part of a bare word.
        let toks = tok_types("foo\\bar");
        assert_eq!(toks, vec![plain("foo\\bar"), Token::Eof]);
    }

    #[test]
    fn backslash_standalone_not_special() {
        // Standalone \ surrounded by spaces: still a bare word.
        let toks = tok_types("foo \\ bar");
        assert_eq!(
            toks,
            vec![plain("foo"), plain("\\"), plain("bar"), Token::Eof,]
        );
    }

    #[test]
    fn windows_path_unchanged() {
        // C:\Users\foo must tokenize as a single bare word.
        let toks = tok_types("C:\\Users\\foo");
        assert_eq!(toks, vec![plain("C:\\Users\\foo"), Token::Eof]);
    }

    #[test]
    fn deref_paren_requires_ident() {
        // EOF immediately after `$(` is an unclosed deref, not an
        // "expected identifier" mistake — there's nothing to expect yet.
        let err = lex("$(").expect_err("expected lex error");
        assert!(
            matches!(err.kind, LexErrorKind::UnclosedDeref { .. }),
            "got {:?}",
            err.kind
        );
        assert!(err.message().contains("unclosed"));

        // A real syntactic error: the body is not an identifier.
        let err = lex("$(1)").expect_err("expected lex error");
        assert!(err.message().contains("expected identifier after '$('"));
    }

    #[test]
    fn deref_paren_requires_closing_paren() {
        // `$(name` runs out of input before the closing paren — that's
        // an unclosed deref, anchored at `(`.
        let err = lex("$(name").expect_err("expected lex error");
        assert!(
            matches!(err.kind, LexErrorKind::UnclosedDeref { .. }),
            "got {:?}",
            err.kind
        );
        assert!(err.message().contains("unclosed"));

        // A `(` followed by a name and a non-`)` character before EOF
        // still falls through to the explicit-paren error.
        let err = lex("$(name ]").expect_err("expected lex error");
        assert!(
            err.message()
                .contains("expected ')' to close '$(...)' dereference")
        );
    }

    /// `<<` lexes as the here-string redirect, fd-prefixable like the
    /// other redirect operators.
    #[test]
    fn herestring_redirect() {
        let tokens = lex("cat << x").unwrap();
        assert!(
            tokens.iter().any(|(t, _)| matches!(
                t,
                Token::Redirect {
                    fd: None,
                    kind: RedirectMode::HereString,
                    target_fd: None,
                }
            )),
            "got {tokens:?}"
        );
        let tokens = lex("cat 0<< x").unwrap();
        assert!(
            tokens.iter().any(|(t, _)| matches!(
                t,
                Token::Redirect {
                    fd: Some(0),
                    kind: RedirectMode::HereString,
                    target_fd: None,
                }
            )),
            "got {tokens:?}"
        );
    }

    /// A payload glued to `<<` is the bash heredoc reflex (`<<EOF`,
    /// `<<'EOF'`); a genuine here-string takes a space before its
    /// payload, so the glued form is a targeted lex error.
    #[test]
    fn glued_herestring_payload_is_rejected() {
        for src in ["cat <<EOF", "cat <<'EOF'", "cat <<\"EOF\"", "cat 0<<$x"] {
            let err = lex(src).expect_err("glued `<<` payload must not lex");
            assert!(
                err.message().contains("ral has no heredocs"),
                "for {src:?} got: {}",
                err.message()
            );
        }
    }

    /// `<<<` is the bash-herestring reflex; ral's `<<` already does that
    /// job, so a third `<` is a targeted lex error rather than a stray
    /// `Read` redirect token that would confuse the parser downstream.
    #[test]
    fn triple_lt_is_rejected() {
        for src in ["cat <<< x", "cat 0<<< x"] {
            let err = lex(src).expect_err("`<<<` must not lex");
            assert!(
                err.message().contains("here-string operator"),
                "for {src:?} got: {}",
                err.message()
            );
        }
    }

    #[test]
    fn redirect_dup_requires_target_fd() {
        let err = lex("cmd 2>&").expect_err("expected lex error");
        assert!(
            err.message()
                .contains("expected file descriptor after '>&'")
        );
    }

    // ── hash-bumped single-quoted strings ────────────────────────────────────

    #[test]
    fn bumped_string_level1_empty() {
        let toks = tok_types("#''#");
        assert_eq!(toks, vec![Token::SingleQuoted(String::new()), Token::Eof]);
    }

    #[test]
    fn bumped_string_level1_contains_single_quote() {
        let toks = tok_types("#'it's fine'#");
        assert_eq!(
            toks,
            vec![Token::SingleQuoted("it's fine".into()), Token::Eof]
        );
    }

    #[test]
    fn bumped_string_level1_contains_double_quote() {
        let toks = tok_types(r#"#'say "hi" please'#"#);
        assert_eq!(
            toks,
            vec![Token::SingleQuoted(r#"say "hi" please"#.into()), Token::Eof]
        );
    }

    #[test]
    fn bumped_string_level2() {
        let toks = tok_types("##'body with '# inside'##");
        assert_eq!(
            toks,
            vec![
                Token::SingleQuoted("body with '# inside".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn bumped_string_multiline() {
        let toks = tok_types("#'line1\nline2'#");
        assert_eq!(
            toks,
            vec![Token::SingleQuoted("line1\nline2".into()), Token::Eof]
        );
    }

    /// A raw string is verbatim: a source authored with CRLF line endings
    /// (Notepad, VS Code set to CRLF) carries the `\r` into the literal's
    /// value rather than losing it — that's the raw-string contract, not
    /// a CRLF-intolerance bug. What must still work regardless is finding
    /// the closing `'#` on the far side of the embedded `\r\n`.
    #[test]
    fn bumped_string_multiline_preserves_embedded_cr() {
        let toks = tok_types("#'line1\r\nline2'#");
        assert_eq!(
            toks,
            vec![Token::SingleQuoted("line1\r\nline2".into()), Token::Eof]
        );
    }

    #[test]
    fn bumped_string_no_escape_processing() {
        // \n inside a literal is two literal bytes, not a newline.
        let toks = tok_types(r"#'\n\t\\'#");
        assert_eq!(
            toks,
            vec![Token::SingleQuoted(r"\n\t\\".into()), Token::Eof]
        );
    }

    #[test]
    fn bumped_string_in_command() {
        let toks = tok_types("echo #'hello'#");
        assert_eq!(
            toks,
            vec![
                plain("echo"),
                Token::SingleQuoted("hello".into()),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn hash_run_without_quote_is_comment() {
        // # / ## / ### followed by anything other than ' is just a comment.
        assert_eq!(tok_types("# foo"), vec![Token::Eof]);
        assert_eq!(tok_types("## foo"), vec![Token::Eof]);
        assert_eq!(tok_types("###foo"), vec![Token::Eof]);
    }

    #[test]
    fn bumped_string_unterminated() {
        let err = lex("#'unclosed").expect_err("should fail");
        assert!(err.message().contains("unterminated"));
    }

    #[test]
    fn bumped_string_unterminated_needs_hash() {
        // Body has a bare ' but no '#, so never closes at level 1.
        let err = lex("#'body with ' but no hash").expect_err("should fail");
        assert!(err.message().contains("unterminated"));
    }

    #[test]
    fn bumped_string_close_followed_by_comment() {
        // #'foo'#  # comment — the trailing # after the close starts a comment.
        let toks = tok_types("#'foo'# # comment");
        assert_eq!(toks, vec![Token::SingleQuoted("foo".into()), Token::Eof]);
    }

    #[test]
    fn bumped_string_byte_span() {
        // Span should cover the full #'…'# token including the surrounding #s.
        let src = "#'hi'#";
        let toks = lex(src).unwrap();
        assert_eq!(&src[toks[0].1.range()], "#'hi'#");
    }

    // ── byte-range span sanity checks ─────────────────────────────────────

    #[test]
    fn byte_spans_cover_full_tokens() {
        // ASCII: spans should be [start, start + len).
        let toks = lex("echo hi").unwrap();
        // echo
        assert_eq!(toks[0].1.start, 0);
        assert_eq!(toks[0].1.end, 4);
        // hi
        assert_eq!(toks[1].1.start, 5);
        assert_eq!(toks[1].1.end, 7);
        // EOF
        assert!(matches!(toks[2].0, Token::Eof));
    }

    #[test]
    fn byte_spans_multibyte() {
        // "日本" = 6 bytes (each char 3 bytes in UTF-8). `= ` precedes, `hi`
        // trails. Underlines must align with byte boundaries, not char indices.
        let src = "日本 = hi";
        let toks = lex(src).unwrap();
        // 日本 — bare word, 6 bytes
        assert_eq!(&src[toks[0].1.range()], "日本");
        // =
        assert_eq!(&src[toks[1].1.range()], "=");
        // hi
        assert_eq!(&src[toks[2].1.range()], "hi");
    }

    #[test]
    fn byte_spans_quoted_string() {
        let src = "'héllo'";
        let toks = lex(src).unwrap();
        // Whole quoted token including the surrounding quotes.
        assert_eq!(&src[toks[0].1.range()], "'héllo'");
    }
}
