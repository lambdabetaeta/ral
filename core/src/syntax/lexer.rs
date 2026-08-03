//! Lexer: source text → a flat `Vec<(Token, Span)>`.  Spans carry byte
//! offsets and a [`FileId`]; line and column are recovered at render time.
//!
//! Newlines separate statements except inside `[…]`, where they are
//! whitespace.  The *innermost* open delimiter decides, so `{ [ ] }` and
//! `[ { } ]` both behave.  A `;` is a hard separator: the parser continues
//! pipelines and chains across newlines, never across a `;`.
//!
//! Nested forms — `!{…}` and `$[…]` inside `"…"`, `$name[k]` keys, and the
//! top-level `$[…]` — are lexed in place and stored as token streams inside
//! the enclosing [`StringPart`] or [`Token::Expr`].  The parser sub-parses
//! those streams instead of re-lexing the bytes, and their spans already
//! point into the outer file, so diagnostics underline the right columns.

use crate::path::tilde::TildePath;
use crate::source::{FileId, Span, Spanned};
use crate::syntax::ast::{RedirectMode, Word};
use std::fmt;

/// The identifier alphabet, `[a-zA-Z_][a-zA-Z0-9_-]*`, as the two
/// predicates the char-by-char scan needs.
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

/// True when `ch` may appear in a bare word; the rest are metacharacters
/// that always need quoting.
///
/// The per-character source of truth, mirrored by the tree-sitter grammar.
/// It cannot see position, though: `scan_bare_fragment` still splits a `:`
/// before space, newline, or `]`, and punctuates a `,` inside `[…]`.  The
/// whole-string question is answered by [`crate::syntax::quote::is_bare_word`],
/// which lexes rather than scanning chars.
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
#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    Literal(String),
    Variable(String),
    /// `!{…}` or `!$name`, carrying its already-lexed token stream.
    Force(Vec<(Token, Span)>),
    /// `$[…]`, carrying its token stream with `&&`/`||` already fused.
    Expr(Vec<(Token, Span)>),
    /// `$name[k1][k2]`.  Each key's span runs from its opening bracket to
    /// its closing one, so a diagnostic can narrow to the key alone.
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
    /// Variant tag `` `ident ``, stored without its backtick.
    Tag(String),
    /// Deref resolved by the lexer: `$name`, `$(name)`, `$name[key]`.
    Deref(StringPart),
    /// Expression block `$[…]` outside strings, with `&&`/`||` fused.
    Expr(Vec<(Self, Span)>),
    Bang,
    Newline,
    /// Separator run containing a `;` — never crossed by continuation.
    Semi,
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
            Self::Semi => write!(f, "';'"),
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

/// Which lexical form a string came from, so an unterminated-string
/// diagnostic can name the shape that wasn't closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StringForm {
    SingleQuoted,
    DoubleQuoted,
    /// `n` extra `#`s on each side: `#'…'#` is 1, `##'…'##` is 2.
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
/// The named arms let `needs_continuation` in `core/src/syntax/parser.rs`
/// and the ariadne renderer say *what* was left open and *where* it opened
/// — each carries its opener's [`Span`], with line and column recovered at
/// render time.
#[derive(Debug, Clone)]
pub enum LexErrorKind {
    /// A string hit EOF before its close.  `inner` carries a nested failure
    /// — an unclosed `!{…}` within — so the diagnostic can anchor at the
    /// outer string and still name the inner culprit.
    UnterminatedString {
        form: StringForm,
        opened: Span,
        inner: Option<Box<Self>>,
    },
    /// A `{}` or `[]` pair that never closed.
    UnterminatedBalanced {
        open: char,
        close: char,
        opened: Span,
    },
    /// A `$(…)` that never closed.
    UnclosedDeref { opened: Span },
    /// Everything unstructured: bad escapes, unexpected characters,
    /// expected-X-found-Y, redirect faults.
    Other(String),
}

impl LexErrorKind {
    /// True for the arms that mean "the user is still typing" — the REPL
    /// prompts for more input, and an inner one is re-anchored into its
    /// enclosing string.
    pub fn is_incomplete(&self) -> bool {
        matches!(
            self,
            Self::UnterminatedString { .. }
                | Self::UnterminatedBalanced { .. }
                | Self::UnclosedDeref { .. }
        )
    }

    /// One user-facing line.  The opening position is deliberately absent:
    /// the renderer draws a secondary label at `opened`, so a `(line, col)`
    /// here would only repeat the underline.
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
    /// The opening delimiter for the "unterminated" kinds, the offending
    /// position for free-form ones.
    pub span: Span,
}

impl LexError {
    /// Synthesised from `kind`; no stored message that could drift from it.
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
/// An unterminated string, delimiter, or `$(…)`, or a lexical fault such as
/// an invalid escape or an unexpected character.
pub fn lex(source: &str) -> Result<Vec<(Token, Span)>, LexError> {
    lex_with(source, FileId::DUMMY)
}

/// Tokenise `source`, attributing every token's byte range to `file`.
///
/// # Errors
/// An unterminated string, delimiter, or `$(…)`, or a lexical fault such as
/// an invalid escape or an unexpected character.
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
    /// (`byte_offset`, char) per char: the offsets stamp byte-range spans,
    /// the vector keeps peek-by-char-index at O(1).
    chars: Vec<(usize, char)>,
    source_len: u32,
    pos: usize,
    file: FileId,
    /// Open delimiters, innermost last.  The innermost decides newline
    /// suppression, and every entry keeps its opener's span so a delimiter
    /// still open at EOF can be reported where it began.
    delim_stack: Vec<OpenDelim>,
}

/// Which paired delimiter is open: a `{…}` block, whose newlines separate
/// statements, or a `[…]` list/map, whose newlines are whitespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DelimKind {
    Brace,
    Bracket,
}

impl DelimKind {
    fn chars(self) -> (char, char) {
        match self {
            Self::Brace => ('{', '}'),
            Self::Bracket => ('[', ']'),
        }
    }
}

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

    /// Byte offset of the next char, i.e. one past the last consumed.
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

    /// Zero-width span at the cursor; [`Self::finish`] stretches it over the
    /// token once that has been consumed.
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

    /// `span` must be the opening delimiter — it becomes the anchor.
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

    /// A still-open inner form becomes the outer string's failure: both are
    /// open, and the string is the mistake.  Definite faults — a bad escape,
    /// a missing identifier — pass through, keeping their precise spot.
    fn rewrap_inner_into_string(outer_span: Span, form: StringForm, inner: LexError) -> LexError {
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
        // The *innermost* delimiter decides, not whether a bracket is open
        // anywhere: `{ [ … ] }` suppresses newlines inside the list, while
        // `[ { … } ]` keeps them as separators inside the block.
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

    /// End of input — unless a `{` or `[` is still open, which is an
    /// unterminated delimiter anchored at the innermost opener rather than a
    /// clean EOF, and is what lets the REPL prompt for the rest.  `span` is
    /// where a clean `Eof` token sits.
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
                            // The comment ran to end of input: the `Eof`
                            // token sits there, not at the opening `#`.
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
                    self.bump();
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

    /// Two-char operators lex as plain words, not as tokens of their own.
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

    /// Emit a closing token, popping only when it matches the innermost
    /// opener.  A wrong-kind closer (`}` while a `[` is open) or one at
    /// depth 0 leaves the stack alone, so newline suppression stays in step
    /// with the genuinely open delimiters and the parser reports the
    /// mismatch.
    fn close_delim(&mut self, token: Token, kind: DelimKind) -> (Token, Span) {
        let span = self.span();
        self.bump();
        if self.delim_stack.last().map(|d| d.kind) == Some(kind) {
            self.delim_stack.pop();
        }
        (token, self.finish(span))
    }

    /// Merge a maximal run of newlines, semicolons, whitespace, and comments
    /// into one separator token: [`Token::Semi`] if the run contains a `;`,
    /// soft [`Token::Newline`] otherwise.
    fn scan_separator(&mut self, span: Span) -> (Token, Span) {
        let mut hard = self.peek() == Some(';');
        self.bump();
        loop {
            match self.peek() {
                Some(';') => {
                    hard = true;
                    self.bump();
                }
                Some('\n' | '\r' | ' ' | '\t') => {
                    self.bump();
                }
                Some('#') if !self.hash_opens_quoted() => self.skip_comment(),
                _ => break,
            }
        }
        let token = if hard { Token::Semi } else { Token::Newline };
        (token, self.finish(span))
    }

    /// Does the `:` under `peek()` break the bare word?  Only when space,
    /// newline, or `]` follows: `host: val` splits, `host:5432` does not.
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

            // `suppress_newline` reads as "we are inside `[…]`", where a
            // comma punctuates instead of joining the word.
            if ch == ',' && self.suppress_newline() {
                break;
            }

            if ch == ':' && self.colon_splits_here() {
                break;
            }

            word.push(ch);
            self.bump();
        }
        word
    }

    fn scan_tilde(&mut self, span: Span) -> (Token, Span) {
        self.bump();
        let suffix = match self.peek() {
            Some(ch) if is_bare_char(ch) => self.scan_bare_fragment(),
            _ => String::new(),
        };
        let raw = format!("~{suffix}");
        let path = TildePath::parse(&raw).expect("tilde token should always parse");
        (Token::Word(Word::Tilde(path)), self.finish(span))
    }

    /// Length of the `#` run at the cursor, consuming nothing.
    fn count_hash_run(&self) -> usize {
        let mut n = 0;
        while self.peek_n(n) == Some('#') {
            n += 1;
        }
        n
    }

    /// A `#` run followed by `'` opens `#'…'#`; anything else is a comment.
    /// Shared by `next_token` and `scan_separator` so they cannot disagree.
    fn hash_opens_quoted(&self) -> bool {
        self.peek_n(self.count_hash_run()) == Some('\'')
    }

    /// Push each body char and consume the closing `'` with its `level`
    /// `#`s.  A `'` trailed by fewer than `level` `#`s is body text.
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
                        self.bump();
                        for _ in 0..level {
                            self.bump();
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

    /// The leading `#`s are already consumed; the opening `'` is not.  Body
    /// bytes are verbatim — no escapes, no interpolation.
    fn scan_quoted(&mut self, span: Span, level: usize) -> Result<(Token, Span), LexError> {
        self.bump();
        let mut body = String::new();
        self.scan_quoted_body(span, level, |c| body.push(c))?;
        Ok((Token::SingleQuoted(body), self.finish(span)))
    }

    fn scan_double_quoted(&mut self, span: Span) -> Result<(Token, Span), LexError> {
        self.bump();
        let file = span.file;
        let mut parts: Vec<Spanned<StringPart>> = Vec::new();
        let mut literal = String::new();
        // Offset of the first char buffered since the last flush; `None`
        // while the buffer is empty.
        let mut literal_start: Option<u32> = None;
        let form = StringForm::DoubleQuoted;

        loop {
            // Where whatever this iteration produces begins — a part, or a
            // literal run — read before any bumping.
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
                    // A `\<newline>` continuation emits nothing, so anchor
                    // the run only when a char actually appeared — else the
                    // following literal's span stretches back over it.
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
                            // `!$name` is `!{$name}`: synthesise the
                            // one-token group instead of re-lexing.
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

    /// Consume one escape after the `\`, which the caller has bumped.
    /// `escape_start` is that `\`'s offset, so a malformed escape underlines
    /// `\q` itself rather than the string's opening quote.
    fn scan_double_quoted_escape(
        &mut self,
        escape_start: u32,
        literal: &mut String,
    ) -> Result<(), LexError> {
        // The escape so far, from the `\` to the cursor; built at each error
        // site once the offending char is consumed.
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
                    reason = "the `n >= 0x80` guard above has already returned; fits u8"
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
                self.bump();
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
                return Err(Self::error(
                    span!(),
                    "unterminated double-quoted string after `\\`",
                ));
            }
        }
        Ok(())
    }

    /// Push the buffered literal spanned `start..end`, or nothing if the
    /// buffer is empty.  `start` is cleared either way: a no-op flush must
    /// not leave a stale offset for the next literal run to inherit.
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

    /// A deref after the `$` the caller consumed: `$name`, `$(name)`,
    /// `$name[key]`, `$[expr]`, or `None` for a bare `$`.  The bare form
    /// stops before a trailing `-` (see [`Self::scan_deref_ident`]); the
    /// explicit `$(name)` keeps one.
    fn scan_deref(&mut self) -> Result<Option<StringPart>, LexError> {
        match self.peek() {
            Some(ch) if is_ident_start(ch) => {
                // The `$` is gone, so the cursor is the name's first byte.
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
                    // `scan_token_group` has consumed the matching `]`, so
                    // the cursor sits exactly one past it.
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
                // `$(123)` is a mistake; `$(` at EOF is merely unfinished,
                // and only that one may be re-anchored as still-open by an
                // enclosing double-quoted string.
                if self.peek().is_none() {
                    return Err(Self::typed_error(
                        span,
                        LexErrorKind::UnclosedDeref { opened: span },
                    ));
                }
                if name.is_empty() {
                    return Err(Self::error(span, "expected identifier after '$('"));
                }
                if self.peek() != Some(')') {
                    return Err(Self::error(
                        span,
                        "expected ')' to close '$(...)' dereference",
                    ));
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

    /// The name of a bare `$name` or `!$name`.  A `-` is an interior name
    /// char, but a trailing one goes back to the stream as literal text, so
    /// `$os-$arch` is two derefs around a `-`.
    fn scan_deref_ident(&mut self) -> String {
        let mut name = self.scan_ident();
        while name.ends_with('-') {
            name.pop();
            self.pos -= 1;
        }
        name
    }

    /// Lex a balanced `open`/`close` body: the caller has already consumed
    /// the opener (`opener` is its span, anchoring an `UnterminatedBalanced`
    /// if EOF comes first), and this consumes the closer without emitting it.
    ///
    /// That bypass of [`Self::open_delim`] is why we push onto `delim_stack`
    /// here — it gives the body the right newline rule and makes the stack
    /// falling below our entry depth the signal that our own closer arrived.
    /// [`Self::close_delim`] does that pop on success; each error path pops
    /// explicitly.
    fn scan_token_group(
        &mut self,
        opener: Span,
        open: char,
        close: char,
    ) -> Result<Vec<(Token, Span)>, LexError> {
        debug_assert!(matches!((open, close), ('{', '}') | ('[', ']')));
        // Every lexer recursion runs through here, so `delim_stack.len()`
        // bounds the recursion depth.  Cap it, or `$[$[$[$[…` overflows the
        // call stack instead of failing cleanly.
        if self.delim_stack.len() >= crate::syntax::NESTING_DEPTH_LIMIT {
            return Err(Self::error(
                opener,
                crate::syntax::nesting_too_deep_message(),
            ));
        }
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
                // Our closer: `close_delim` already popped us, so the stack
                // sits below the entry depth.  A mismatched one (`]` while
                // scanning `{…}`) falls through and the parser reports it.
                (Token::RBrace, '{') | (Token::RBracket, '[')
                    if self.delim_stack.len() < entry_depth =>
                {
                    return Ok(tokens);
                }
                // With our delim open, `eof_or_unterminated` turns end of
                // input into the `Err` caught above.
                (Token::Eof, _) => {
                    unreachable!("next_token cannot yield Eof while a delim is open")
                }
                _ => tokens.push((tok, span)),
            }
        }
    }

    /// Lex the body of `$[…]`, whose `[` the caller consumed.  Inside an
    /// expression block adjacent `&`/`|` pairs are the logical `&&`/`||`,
    /// and fusing them belongs to the lexer because the cue — being inside
    /// `$[…]` — is lexical.
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
            // `is_fd_redirect_start` already saw a `>`/`<` past the digits,
            // and `take_while` consumed exactly those digits.
            _ => unreachable!("scan_fd_redirect entered without a trailing '>' or '<'"),
        }
    }

    /// Parse the digit prefix of an fd redirect.  Overflow is a hard error:
    /// silently coercing `99999999999>` to fd 1 is the bash sloppiness we
    /// refuse.
    fn parse_fd(digits: &str, span: Span) -> Result<u32, LexError> {
        debug_assert!(!digits.is_empty(), "scan_fd_redirect called without digits");
        digits.parse::<u32>().map_err(|_| {
            Self::error(
                span,
                format!("file descriptor '{digits}' does not fit in u32"),
            )
        })
    }

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
        // `>~` is the stream-write operator only when the `~` stands alone.
        // `>~/path` is a plain write to a tilde path, so a bare char after
        // the `~` leaves it to lex as its own `Tilde` word.
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
            // A payload glued to `<<` is the bash heredoc reflex; a genuine
            // here-string takes a space.  Rejecting it stops the quoted
            // delimiter becoming stdin while the body lines run as commands.
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

/// Fuse byte-adjacent `&`/`|` pairs into the logical `&&`/`||`, taking the
/// first member's span.  A space between them (`& &`) leaves two pipeline
/// backgrounders, and outside `$[…]` the single-char meaning stands — hence
/// only [`Lexer::scan_expr_block`] calls this.
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

    /// A bad escape underlines the escape — `\q` at bytes 4..6 — not the
    /// string's opening quote.
    #[test]
    fn bad_escape_spans_the_escape_not_the_quote() {
        let span = lex_err_span(r#""abc\q""#);
        assert_eq!((span.start, span.end), (4, 6));
    }

    /// A comment running to end of input leaves `Eof` spanned at the end of
    /// the source, not at the `#` that opened it.
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

    /// A `#'…'#` opener must survive the run of separators rather than be
    /// swallowed as a comment.
    #[test]
    fn hash_quoted_string_after_separator() {
        let expect = |sep| {
            vec![
                plain("echo"),
                plain("a"),
                sep,
                Token::SingleQuoted("hi".into()),
                Token::Eof,
            ]
        };
        assert_eq!(tok_types("echo a\n#'hi'#"), expect(Token::Newline));
        assert_eq!(tok_types("echo a;#'hi'#"), expect(Token::Semi));
    }

    /// A `\`-continuation appends nothing, so the no-op flush at `$x` must
    /// not leak the backslash's offset into the trailing literal's span.
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

    /// The same rule with no flush in between: the literal run starts at the
    /// first real char, byte 3, not at the continuation.
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

    /// A wrong-kind closer must not pop the stack, or newline suppression
    /// desyncs for the rest of the group: the stray `}` leaves the `[` open,
    /// so no `Newline` follows it.
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

    /// If `>~` swallowed the `~` in `>~/path`, the redirect would target
    /// `/path` instead of `$HOME/path`.
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

    /// With no tilde-path suffix after it, `>~` stays stream-write.
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
        assert_eq!(toks.len(), 3);
        match &toks[1] {
            Token::DoubleQuoted(parts) => {
                assert_eq!(parts.len(), 2);
                assert_eq!(parts[0].item, StringPart::Literal("hello ".into()));
                assert_eq!(parts[1].item, StringPart::Variable("name".into()));
            }
            _ => panic!("expected DoubleQuoted"),
        }
    }

    /// A bare `$name` never eats a trailing `-`, so a kebab-adjacent
    /// interpolation cannot fold the dash into the first name.
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

    /// A `-` with more name after it is interior, so a genuine kebab
    /// identifier stays one deref.
    #[test]
    fn interpolation_keeps_interior_dash() {
        let toks = tok_types("\"$os-arch\"");
        let Token::DoubleQuoted(parts) = &toks[0] else {
            panic!("expected DoubleQuoted");
        };
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].item, StringPart::Variable("os-arch".into()));
    }

    /// A `-` at end of string is literal text, not part of the name.
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

    /// `$(name)` fixes where the name ends, so it keeps the trailing `-`
    /// that the bare form drops.
    #[test]
    fn explicit_boundary_keeps_trailing_dash() {
        let toks = tok_types("\"$(foo-)\"");
        let Token::DoubleQuoted(parts) = &toks[0] else {
            panic!("expected DoubleQuoted");
        };
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].item, StringPart::Variable("foo-".into()));
    }

    /// A bare deref outside strings obeys the same rule.
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
                // The inner `echo hello` is lexed in place, not sliced back
                // out as raw text.
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
        assert!(lex_err(r#""\x80""#).contains("\\xNN"));
        assert!(lex_err(r#""\xZZ""#).contains("two hex digits"));
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
        assert_eq!(t(r#""\u{41}""#), "A");
        assert_eq!(t(r#""\u{0}""#), "\x00");
        assert_eq!(t(r#""\u{1F600}""#), "😀");
        // Surrogate, out of range, too many digits, no braces.
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
        // Outside `$[…]` these stay single-char pipeline punctuation.
        let toks = tok_types("$[1 && 0]");
        let Token::Expr(inner) = &toks[0] else {
            panic!("expected Expr token");
        };
        let kinds: Vec<&Token> = inner.iter().map(|(t, _)| t).collect();
        assert_eq!(kinds, vec![&plain("1"), &plain("&&"), &plain("0")]);
    }

    /// Fusion is by byte adjacency: `& &` is two backgrounders, not `&&`.
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
        // Inner spans index the outer source, so a diagnostic raised inside
        // `$[…]` underlines the right column.
        let toks = tok_types("$[42]");
        let Token::Expr(inner) = &toks[0] else {
            panic!("expected Expr token");
        };
        assert_eq!(inner.len(), 1);
        let (_, span) = &inner[0];
        // The `$` and `[` take one byte each, so `42` is at bytes 2..4.
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
                Token::Semi,
                plain("echo"),
                plain("b"),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn mixed_separator_run_is_hard() {
        for src in ["echo a ;\n echo b", "echo a\n; echo b", "echo a;;echo b"] {
            assert_eq!(
                tok_types(src),
                vec![
                    plain("echo"),
                    plain("a"),
                    Token::Semi,
                    plain("echo"),
                    plain("b"),
                    Token::Eof,
                ],
                "source: {src:?}"
            );
        }
    }

    #[test]
    fn colon_context_sensitive() {
        let toks = tok_types("host: val");
        assert_eq!(
            toks,
            vec![plain("host"), Token::Colon, plain("val"), Token::Eof,]
        );
        let toks = tok_types("localhost:5432");
        assert_eq!(toks, vec![plain("localhost:5432"), Token::Eof]);
    }

    #[test]
    fn equals_not_special() {
        // `=` is an ordinary bare char — no splitting rule.
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
    fn backslash_standalone_not_special() {
        let toks = tok_types("foo \\ bar");
        assert_eq!(
            toks,
            vec![plain("foo"), plain("\\"), plain("bar"), Token::Eof,]
        );
    }

    #[test]
    fn windows_path_unchanged() {
        // One bare word, backslashes and drive colon included.
        let toks = tok_types("C:\\Users\\foo");
        assert_eq!(toks, vec![plain("C:\\Users\\foo"), Token::Eof]);
    }

    #[test]
    fn deref_paren_requires_ident() {
        // EOF straight after `$(` is unclosed, not "expected identifier" —
        // there is nothing to expect yet.
        let err = lex("$(").expect_err("expected lex error");
        assert!(
            matches!(err.kind, LexErrorKind::UnclosedDeref { .. }),
            "got {:?}",
            err.kind
        );
        assert!(err.message().contains("unclosed"));

        // A real mistake: the body is not an identifier.
        let err = lex("$(1)").expect_err("expected lex error");
        assert!(err.message().contains("expected identifier after '$('"));
    }

    #[test]
    fn deref_paren_requires_closing_paren() {
        // Out of input before the `)`: unclosed, anchored at the `(`.
        let err = lex("$(name").expect_err("expected lex error");
        assert!(
            matches!(err.kind, LexErrorKind::UnclosedDeref { .. }),
            "got {:?}",
            err.kind
        );
        assert!(err.message().contains("unclosed"));

        // A non-`)` char before EOF still reaches the explicit-paren error.
        let err = lex("$(name ]").expect_err("expected lex error");
        assert!(
            err.message()
                .contains("expected ')' to close '$(...)' dereference")
        );
    }

    /// `<<` is the here-string redirect, fd-prefixable like the others.
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

    /// A payload glued to `<<` is the bash heredoc reflex; a here-string
    /// takes a space, so the glued form gets a targeted error.
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

    /// `<<` already does the here-string job, so a third `<` is a targeted
    /// error rather than a stray `Read` token the parser would choke on.
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

    /// A raw string is verbatim, so a CRLF-authored source keeps the `\r` in
    /// the value — the contract, not a bug.  What must hold either way is
    /// that the closing `'#` is found past the embedded `\r\n`.
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
        // `\n` in a raw literal is two bytes, not a newline.
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
        // A `#` run followed by anything but `'` is a comment.
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
        // A bare `'` with no `#` never closes a level-1 literal.
        let err = lex("#'body with ' but no hash").expect_err("should fail");
        assert!(err.message().contains("unterminated"));
    }

    #[test]
    fn bumped_string_close_followed_by_comment() {
        // The `#` after the close starts a comment.
        let toks = tok_types("#'foo'# # comment");
        assert_eq!(toks, vec![Token::SingleQuoted("foo".into()), Token::Eof]);
    }

    #[test]
    fn bumped_string_byte_span() {
        let src = "#'hi'#";
        let toks = lex(src).unwrap();
        assert_eq!(&src[toks[0].1.range()], "#'hi'#");
    }

    // ── byte-range span sanity checks ─────────────────────────────────────

    #[test]
    fn byte_spans_cover_full_tokens() {
        let toks = lex("echo hi").unwrap();
        assert_eq!(toks[0].1.start, 0);
        assert_eq!(toks[0].1.end, 4);
        assert_eq!(toks[1].1.start, 5);
        assert_eq!(toks[1].1.end, 7);
        assert!(matches!(toks[2].0, Token::Eof));
    }

    #[test]
    fn byte_spans_multibyte() {
        // Spans must land on byte boundaries, not char indices: `日本` is
        // 6 bytes, so slicing by them would panic if the two disagreed.
        let src = "日本 = hi";
        let toks = lex(src).unwrap();
        assert_eq!(&src[toks[0].1.range()], "日本");
        assert_eq!(&src[toks[1].1.range()], "=");
        assert_eq!(&src[toks[2].1.range()], "hi");
    }

    #[test]
    fn byte_spans_quoted_string() {
        let src = "'héllo'";
        let toks = lex(src).unwrap();
        assert_eq!(&src[toks[0].1.range()], "'héllo'");
    }
}
