//! Lexer: source text → a flat `Vec<Lexeme>`.  Spans carry byte
//! offsets and a [`FileId`]; line and column are recovered at render time.
//!
//! The innermost open delimiter sets the lexical mode.  Newlines separate
//! statements except inside `[…]` and `$[…]`, where they are whitespace, so
//! `{ [ ] }` and `[ { } ]` both behave.  Inside `$[…]` the operators are
//! tokens of their own, [`Token::Op`] — `<` is a comparison, not a redirect, and
//! `1+1` is three tokens — while everywhere else they keep their shell meaning.  A `;`
//! is a hard separator: the parser continues pipelines and chains across
//! newlines, never across a `;`.
//!
//! Nested forms — the body of `$[…]`, and every `$…` / `!…` splice inside
//! `"…"`: are lexed in place and stored as token streams inside
//! [`Token::Expr`] or [`StringPart::Splice`].  The parser sub-parses those
//! streams instead of re-lexing the bytes, and their spans already point
//! into the outer file, so diagnostics underline the right columns.

use crate::ir::{ArithOp, BinaryOp, CompareOp, EqOp, WriteMode};
use crate::path::tilde::TildePath;
use crate::source::{FileId, Span, Spanned};
use crate::syntax::ast::Word;
use crate::syntax::numeral;
use std::fmt;

/// The identifier alphabet, `[a-zA-Z_][a-zA-Z0-9_-]*`, as the two
/// predicates the char-by-char scan needs.
fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_ident_cont(ch: char) -> bool {
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

/// The word-*continuation* predicate: true when `ch` may appear after the
/// first char of a bare word; the rest are metacharacters that always need
/// quoting.
///
/// Word *start* is narrower: `?`, `&`, `#`, `...`, `~`-forms, a digit run
/// before `>`/`<`, `:` and `,` all begin something else when they open a
/// token and are ordinary mid-word.  Position matters beyond that too:
/// `scan_bare_fragment` splits a `:` before whitespace, `,`, or a closer (`]`, `}`, `)`), and
/// punctuates a `,` inside `[…]`.  The whole-string question is answered by
/// [`crate::syntax::quote::is_bare_word`], which lexes rather than scanning
/// chars.
fn continues_bare_word(ch: char) -> bool {
    !(ch.is_ascii_control()
        || matches!(
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
                | '<'
                | '>'
                | '"'
                | '\''
                | '`'
                | '('
                | ')'
                | ';'
        ))
}

/// The Unicode bidirectional controls, refused everywhere in the source
/// (Trojan Source: they reorder how code displays relative to how it runs).
fn bidi_control_name(ch: char) -> Option<&'static str> {
    Some(match ch {
        '\u{202A}' => "left-to-right embedding",
        '\u{202B}' => "right-to-left embedding",
        '\u{202C}' => "pop directional formatting",
        '\u{202D}' => "left-to-right override",
        '\u{202E}' => "right-to-left override",
        '\u{2066}' => "left-to-right isolate",
        '\u{2067}' => "right-to-left isolate",
        '\u{2068}' => "first strong isolate",
        '\u{2069}' => "pop directional isolate",
        _ => return None,
    })
}

/// The bare-word characters that are operators inside `$[…]`, where they end
/// a word instead: `1+1` is a sum there and a word everywhere else.  `=` is
/// `Assign` until a second `=` makes it `==`; `&` takes its own arms, for
/// `&&`; `<` `>` `!` `|` are not bare anywhere.
fn operator_char(ch: char) -> Option<Operator> {
    let arith = |op| Operator::Binary(BinaryOp::Arith(op));
    Some(match ch {
        '+' => arith(ArithOp::Add),
        '-' => arith(ArithOp::Sub),
        '*' => arith(ArithOp::Mul),
        '/' => arith(ArithOp::Div),
        '%' => arith(ArithOp::Mod),
        '=' => Operator::Assign,
        _ => return None,
    })
}

/// Parts of an interpolated (double-quoted) string.
#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    Literal(String),
    /// A `$…` or `!…` splice, as the very tokens it lexes to outside a
    /// string — `$name`, `$(name)`, `$[…]`, `!{…}`, `!$name`, the two
    /// undelimited forms with their `[key]` groups (see [`Lexer::scan_splice`])
    /// — so the parser reads it as one atom.  Also a leading `~` before `/`
    /// or the closing quote, as a lone tilde word.
    Splice(Vec<Lexeme>),
}

/// A token with its location.  Always located, unlike an AST node's
/// `Spanned<T>`, whose span may be synthetic.
#[derive(Debug, Clone, PartialEq)]
pub struct Lexeme {
    pub token: Token,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Word(Word),
    SingleQuoted(String),
    DoubleQuoted(Vec<Spanned<StringPart>>),
    Caret,
    Pipe,
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
    /// `$name` or `$(name)`, stored without its sigil; `delimited` records
    /// which.
    Variable {
        name: String,
        delimited: bool,
    },
    /// Expression block `$[…]`, carrying its body's token stream.
    Expr(Vec<Lexeme>),
    Bang,
    /// An operator of `$[…]`, emitted nowhere else.
    Op(Operator),
    Newline,
    /// Separator run containing a `;` — never crossed by continuation.
    Semi,
    /// A redirect operator that takes a word; `stderr` is set only for
    /// `2>`, `2>>`, `2>~`, so it implies `op` is a write.
    Redirect {
        stderr: bool,
        op: RedirectOp,
    },
    /// `2>&1`.
    StderrToStdout,
    Eof,
}

/// An operator inside `$[…]`: the binary ones the IR already names, the two
/// connectives, and the one spelling that is an error wherever it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Binary(BinaryOp),
    And,
    Or,
    /// A lone `=`.
    Assign,
}

impl fmt::Display for Operator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binary(op) => fmt::Display::fmt(op, f),
            Self::And => f.write_str("&&"),
            Self::Or => f.write_str("||"),
            Self::Assign => f.write_str("="),
        }
    }
}

/// The operator of a word-taking redirect, as spelled; the parser's
/// `parser::redirect_word` assigns its stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectOp {
    Read,
    HereString,
    Write(WriteMode),
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Word(Word::Tilde(path)) => write!(f, "{}", path.to_literal()),
            Self::Word(Word::Plain(s) | Word::Slash(s)) | Self::SingleQuoted(s) => {
                write!(f, "'{s}'")
            }
            Self::DoubleQuoted(_) => write!(f, "\"...\""),
            Self::Caret => write!(f, "^"),
            Self::Pipe => write!(f, "|"),
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
            Self::Variable { name, .. } => write!(f, "${name}"),
            Self::Expr(_) => write!(f, "$[...]"),
            Self::Bang => write!(f, "!"),
            Self::Op(op) => write!(f, "{op}"),
            Self::Newline => write!(f, "newline"),
            Self::Semi => write!(f, "';'"),
            Self::Redirect { .. } | Self::StderrToStdout => write!(f, "redirect"),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringForm {
    SingleQuoted,
    DoubleQuoted,
    /// `n` extra `#`s on each side: `#'…'#` is 1, `##'…'##` is 2.
    BumpedSingle(usize),
}

impl StringForm {
    /// The exact delimiter that closes this form — what the user must type.
    /// A bumped string wants its `#` run back after the `'`, so the reflex
    /// of a bare `'` leaves it open however often it is tried.
    pub(crate) fn closing(&self) -> String {
        match self {
            Self::SingleQuoted => "'".into(),
            Self::DoubleQuoted => "\"".into(),
            Self::BumpedSingle(n) => format!("'{}", "#".repeat(*n)),
        }
    }
}

impl fmt::Display for StringForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SingleQuoted => f.write_str("single-quoted string"),
            Self::DoubleQuoted => f.write_str("double-quoted string"),
            // The `#` level is not spelled out here: every diagnostic that
            // names a form also quotes its `closing()`, which shows the run.
            Self::BumpedSingle(_) => f.write_str("bumped single-quoted string"),
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
    /// A closer that is not the innermost opener's, as `}` in `[a }`.  Not
    /// incomplete: no further input can make it right.
    Mismatched {
        open: char,
        opened: Span,
        close: char,
    },
    /// Everything unstructured: bad escapes, unexpected characters,
    /// expected-X-found-Y, redirect faults.
    Other(String),
}

impl LexErrorKind {
    /// True for the arms that mean "the user is still typing": the REPL
    /// prompts for more input, and an inner one is re-anchored into its
    /// enclosing string.
    pub(crate) fn is_incomplete(&self) -> bool {
        matches!(
            self,
            Self::UnterminatedString { .. }
                | Self::UnterminatedBalanced { .. }
                | Self::UnclosedDeref { .. }
        )
    }

    /// This kind's own line, with no nested culprit.  The opening position is
    /// deliberately absent: the renderer draws a secondary label at `opened`,
    /// so a `(line, col)` here would only repeat the underline.
    pub(crate) fn headline(&self) -> String {
        match self {
            Self::UnterminatedString { form, .. } => {
                format!("unterminated {form}: expected closing `{}`", form.closing())
            }
            Self::UnterminatedBalanced { open, close, .. } => {
                format!("unterminated `{open}…{close}`")
            }
            Self::UnclosedDeref { .. } => "unclosed `$(…)` dereference".into(),
            Self::Mismatched { open, close, .. } => {
                format!("mismatched `{close}`: the innermost open delimiter is `{open}`")
            }
            Self::Other(s) => s.clone(),
        }
    }

    /// One user-facing line: the headline, then the nested one it hides.
    pub fn message(&self) -> String {
        match self {
            Self::UnterminatedString {
                inner: Some(inner), ..
            } => format!("{}; nested {}", self.headline(), inner.message()),
            _ => self.headline(),
        }
    }
}

#[derive(Debug)]
pub struct LexError {
    pub kind: LexErrorKind,
    /// The opening delimiter for the "unterminated" kinds, the offending
    /// position for free-form ones and a mismatched closer.
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
pub fn lex(source: &str) -> Result<Vec<Lexeme>, LexError> {
    lex_with(source, FileId::DUMMY)
}

/// Tokenise `source`, attributing every token's byte range to `file`.
///
/// # Errors
/// An unterminated string, delimiter, or `$(…)`, or a lexical fault such as
/// an invalid escape or an unexpected character.
pub(crate) fn lex_with(source: &str, file: FileId) -> Result<Vec<Lexeme>, LexError> {
    if let Some((at, ch, name)) = source
        .char_indices()
        .find_map(|(i, ch)| bidi_control_name(ch).map(|n| (i, ch, n)))
    {
        let byte = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
        let (at, end) = (byte(at), byte(at + ch.len_utf8()));
        return Err(Lexer::error(
            Span::new(file, at, end),
            format!(
                "bidirectional control character U+{:04X} ({name}) is not allowed in ral source: \
                 it can make the text read differently from how it runs; \
                 to put it in a string, write `\\u{{{:X}}}`",
                ch as u32, ch as u32
            ),
        ));
    }
    let mut lexer = Lexer::new(source, file);
    let mut tokens = Vec::new();
    loop {
        let lexeme = lexer.next_token()?;
        let is_eof = lexeme.token == Token::Eof;
        tokens.push(lexeme);
        if is_eof {
            break;
        }
    }
    Ok(tokens)
}

/// The literal text buffered inside a double-quoted string, anchored at the
/// offset of its first char; `start` is `Some` exactly when `text` is
/// non-empty, so a `\<newline>` continuation, which pushes nothing, anchors
/// nothing.
#[derive(Default)]
struct LiteralRun {
    start: Option<u32>,
    text: String,
}

impl LiteralRun {
    fn push(&mut self, at: u32, ch: char) {
        self.start.get_or_insert(at);
        self.text.push(ch);
    }

    /// Emit the run as a `Literal` spanning `start..end`, if non-empty.
    fn flush(&mut self, parts: &mut Vec<Spanned<StringPart>>, end: u32, file: FileId) {
        let text = std::mem::take(&mut self.text);
        if let Some(start) = self.start.take() {
            parts.push(Spanned::new(
                Span::new(file, start, end),
                StringPart::Literal(text),
            ));
        }
    }
}

struct Lexer<'a> {
    /// (`byte_offset`, char) per char: the offsets stamp byte-range spans,
    /// the vector keeps peek-by-char-index at O(1).
    chars: Vec<(usize, char)>,
    source: &'a str,
    pos: usize,
    file: FileId,
    /// Open delimiters, innermost last.  The innermost decides newline
    /// suppression, and every entry keeps its opener's span so a delimiter
    /// still open at EOF can be reported where it began.
    delim_stack: Vec<OpenDelim>,
}

/// Which paired delimiter is open, and so which lexical mode holds: a `{…}`
/// block, whose newlines separate statements; a `[…]` list/map, whose
/// newlines are whitespace and whose commas punctuate; or a `$[…]`
/// expression, which is a bracket whose operators are words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DelimKind {
    Brace,
    Bracket,
    Expr,
}

impl DelimKind {
    fn chars(self) -> (char, char) {
        match self {
            Self::Brace => ('{', '}'),
            Self::Bracket | Self::Expr => ('[', ']'),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct OpenDelim {
    kind: DelimKind,
    opened: Span,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str, file: FileId) -> Self {
        Self {
            chars: source.char_indices().collect(),
            source,
            pos: 0,
            file,
            delim_stack: Vec::new(),
        }
    }

    /// Byte offset of the next char, i.e. one past the last consumed.
    fn byte_pos(&self) -> u32 {
        let byte = self.chars.get(self.pos).map_or(self.source.len(), |(b, _)| *b);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "byte offset in a source in the u32 span system (< 4 GiB)"
        )]
        {
            byte as u32
        }
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

    /// Consume one char; the span covers exactly it.
    fn bump_spanned(&mut self) -> Span {
        let start = self.span();
        self.bump();
        self.finish(start)
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

    fn innermost(&self) -> Option<DelimKind> {
        self.delim_stack.last().map(|d| d.kind)
    }

    /// Inside `[…]` or `$[…]`, newlines are whitespace and commas punctuate.
    fn suppress_newline(&self) -> bool {
        // The *innermost* delimiter decides, not whether a bracket is open
        // anywhere: `{ [ … ] }` suppresses newlines inside the list, while
        // `[ { … } ]` keeps them as separators inside the block.
        matches!(self.innermost(), Some(DelimKind::Bracket | DelimKind::Expr))
    }

    /// Inside `$[…]`, where the operators are tokens of their own.
    fn in_expr(&self) -> bool {
        self.innermost() == Some(DelimKind::Expr)
    }

    /// Whitespace and `#` comments, up to but not including a significant
    /// newline.
    fn skip_inline_whitespace(&mut self) {
        while let Some(ch) = self.peek() {
            match ch {
                ' ' | '\t' | '\r' => {
                    self.bump();
                }
                '\n' if self.suppress_newline() => {
                    self.bump();
                }
                '#' if !self.hash_opens_quoted() => {
                    while self.peek().is_some_and(|ch| ch != '\n') {
                        self.bump();
                    }
                }
                _ => break,
            }
        }
    }

    /// End of input — unless a `{` or `[` is still open, which is an
    /// unterminated delimiter anchored at the innermost opener rather than a
    /// clean EOF, and is what lets the REPL prompt for the rest.  `span` is
    /// where a clean `Eof` token sits.
    fn eof_or_unterminated(&self, span: Span) -> Result<Lexeme, LexError> {
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
        Ok(Lexeme {
            token: Token::Eof,
            span,
        })
    }

    fn next_token(&mut self) -> Result<Lexeme, LexError> {
        self.skip_inline_whitespace();

        let span = self.span();
        let Some(ch) = self.peek() else {
            return self.eof_or_unterminated(span);
        };

        match ch {
            '#' if self.hash_opens_quoted() => {
                let level = self.count_hash_run();
                for _ in 0..level {
                    self.bump();
                }
                self.scan_quoted(span, level)
            }
            '\n' | ';' => Ok(self.scan_separator(span)),
            '{' => Ok(self.open_delim(Token::LBrace, DelimKind::Brace)),
            '}' => self.close_delim(Token::RBrace),
            '[' => Ok(self.open_delim(Token::LBracket, DelimKind::Bracket)),
            ']' => self.close_delim(Token::RBracket),
            '(' => Ok(self.bump_simple(Token::LParen, span)),
            ')' => Ok(self.bump_simple(Token::RParen, span)),
            '|' if self.in_expr() && self.peek_n(1) == Some('|') => {
                Ok(self.two_char_op(Operator::Or, span))
            }
            '|' => Ok(self.bump_simple(Token::Pipe, span)),
            '&' if self.in_expr() && self.peek_n(1) == Some('&') => {
                Ok(self.two_char_op(Operator::And, span))
            }
            '&' if self.peek_n(1) == Some('&') => Err(Self::error(
                Span::new(span.file, span.start, span.start + 2),
                "ral has no `&&`: a newline or `;` sequences commands, and an \
                     uncaught failure already stops the script. Inside `$[…]`, \
                     `&&` is the Boolean connective",
            )),
            '&' if self.in_expr() => Err(Self::error(
                Span::new(span.file, span.start, span.start + 1),
                "`&` is not an operator: the Boolean connective is `&&`",
            )),
            '&' => Err(Self::error(
                Span::new(span.file, span.start, span.start + 1),
                "`&` does not background a command in ral: wrap it in \
                     `spawn { … }`, which returns a handle you `await`",
            )),
            ',' if self.suppress_newline() => Ok(self.bump_simple(Token::Comma, span)),
            ',' => Ok(self.scan_bare_word(span)),
            '$' => {
                self.bump();
                match self.scan_dollar()? {
                    Some(token) => Ok(Lexeme {
                        token,
                        span: self.finish(span),
                    }),
                    None => Err(Self::error(
                        self.finish(span),
                        "expected a name after `$`: write `$name`, `$(name)`, or `$[…]`",
                    )),
                }
            }
            '^' => Ok(self.bump_simple(Token::Caret, span)),
            '!' if self.in_expr() && self.peek_n(1) == Some('=') => {
                Ok(self.two_char_op(Operator::Binary(BinaryOp::Eq(EqOp::Ne)), span))
            }
            '!' => Ok(self.bump_simple(Token::Bang, span)),
            '?' => Ok(self.bump_simple(Token::Question, span)),
            '\'' => self.scan_quoted(span, 0),
            '"' => self.scan_double_quoted(span),
            '<' | '>' if self.in_expr() => Ok(self.scan_comparison(span)),
            '>' => self.scan_redirect_gt(None, span),
            '<' => self.scan_redirect_lt(None, span),
            _ if !self.in_expr() && ch.is_ascii_digit() && self.is_fd_redirect_start() => {
                self.scan_fd_redirect(span)
            }
            '.' if self.peek_n(1) == Some('.') && self.peek_n(2) == Some('.') => {
                self.bump();
                self.bump();
                self.bump();
                Ok(Lexeme {
                    token: Token::Spread,
                    span: self.finish(span),
                })
            }
            '`' => {
                self.bump();
                let label = self.scan_ident();
                if label.is_empty() {
                    Err(Self::error(
                        self.finish(span),
                        "expected a tag label after the backtick, as in `` `ok ``: \
                             a backtick never runs a command here; write `!{cmd}` for that",
                    ))
                } else {
                    Ok(Lexeme {
                        token: Token::Tag(label),
                        span: self.finish(span),
                    })
                }
            }
            _ if let Some(op) = operator_char(ch).filter(|_| self.in_expr()) => {
                Ok(self.scan_operator(op, span))
            }
            _ if self.in_expr() && self.at_numeral() => Ok(self.scan_numeral_word(span)),
            // Every metacharacter `continues_bare_word` rejects is matched above;
            // what is left over is exactly the ASCII controls.
            _ if continues_bare_word(ch) => Ok(self.scan_bare_word(span)),
            _ => {
                let span = self.bump_spanned();
                Err(Self::error(
                    span,
                    format!(
                        "control character U+{:04X} cannot appear bare in ral source: \
                         inside a double-quoted string it is written `\\u{{{:X}}}`",
                        ch as u32, ch as u32
                    ),
                ))
            }
        }
    }

    fn bump_simple(&mut self, token: Token, span: Span) -> Lexeme {
        self.bump();
        Lexeme {
            token,
            span: self.finish(span),
        }
    }

    fn two_char_op(&mut self, op: Operator, span: Span) -> Lexeme {
        self.bump();
        self.bump();
        Lexeme {
            token: Token::Op(op),
            span: self.finish(span),
        }
    }

    /// `<`, `>`, `<=`, `>=` inside `$[…]`.
    fn scan_comparison(&mut self, span: Span) -> Lexeme {
        let lt = self.bump() == Some('<');
        let or_eq = self.peek() == Some('=');
        if or_eq {
            self.bump();
        }
        let op = match (lt, or_eq) {
            (true, false) => CompareOp::Lt,
            (true, true) => CompareOp::Le,
            (false, false) => CompareOp::Gt,
            (false, true) => CompareOp::Ge,
        };
        Lexeme {
            token: Token::Op(Operator::Binary(BinaryOp::Compare(op))),
            span: self.finish(span),
        }
    }

    /// `op` is the operator under the cursor; a second `=` after `Assign`
    /// makes it `==`.
    fn scan_operator(&mut self, op: Operator, span: Span) -> Lexeme {
        self.bump();
        let op = if op == Operator::Assign && self.peek() == Some('=') {
            self.bump();
            Operator::Binary(BinaryOp::Eq(EqOp::Eq))
        } else {
            op
        };
        Lexeme {
            token: Token::Op(op),
            span: self.finish(span),
        }
    }

    /// The unscanned source.
    fn rest(&self) -> &str {
        &self.source[self.byte_pos() as usize..]
    }

    fn at_numeral(&self) -> bool {
        numeral::prefix(self.rest()).is_some()
    }

    /// In `$[…]` `+` and `-` are operators, so the word is the numeral prefix
    /// plus any bare text glued to it.
    fn scan_numeral_word(&mut self, span: Span) -> Lexeme {
        let (len, _) = numeral::prefix(self.rest()).expect("caller saw a numeral");
        let mut word: String = (0..len)
            .map(|_| self.bump().expect("prefix is in range"))
            .collect();
        word.push_str(&self.scan_bare_fragment());
        Lexeme {
            token: Token::Word(Word::Plain(word)),
            span: self.finish(span),
        }
    }

    fn open_delim(&mut self, token: Token, kind: DelimKind) -> Lexeme {
        let span = self.span();
        self.bump();
        let opened = self.finish(span);
        self.delim_stack.push(OpenDelim { kind, opened });
        Lexeme {
            token,
            span: opened,
        }
    }

    /// Emit the closing token under the cursor, popping the innermost opener
    /// it closes.  A closer that closes something else is an error; one at
    /// depth 0 is a token, which the parser reports as unmatched.
    fn close_delim(&mut self, token: Token) -> Result<Lexeme, LexError> {
        let start = self.span();
        let close = self.bump().expect("caller peeked a closer");
        let span = self.finish(start);
        match self.delim_stack.last().copied() {
            Some(open) if open.kind.chars().1 == close => {
                self.delim_stack.pop();
            }
            Some(open) => {
                let kind = LexErrorKind::Mismatched {
                    open: open.kind.chars().0,
                    opened: open.opened,
                    close,
                };
                return Err(Self::typed_error(span, kind));
            }
            None => {}
        }
        Ok(Lexeme { token, span })
    }

    /// Merge a maximal run of newlines, semicolons, whitespace, and comments
    /// into one separator token: [`Token::Semi`] if the run contains a `;`,
    /// soft [`Token::Newline`] otherwise.
    fn scan_separator(&mut self, span: Span) -> Lexeme {
        let mut hard = self.peek() == Some(';');
        self.bump();
        loop {
            self.skip_inline_whitespace();
            match self.peek() {
                Some(';') => {
                    hard = true;
                    self.bump();
                }
                Some('\n') => {
                    self.bump();
                }
                _ => break,
            }
        }
        let token = if hard { Token::Semi } else { Token::Newline };
        Lexeme {
            token,
            span: self.finish(span),
        }
    }

    /// Does the `:` under `peek()` break the bare word?  Only when whitespace,
    /// `,`, a closer (`]`, `}`, `)`), or end of input follows: `host: val`,
    /// `[a:]` and `{a:}` split, `host:5432` does not.
    fn colon_splits_here(&self) -> bool {
        self.peek_n(1)
            .is_none_or(|next| matches!(next, ' ' | '\t' | '\r' | '\n' | ']' | '}' | ')' | ','))
    }

    fn scan_bare_word(&mut self, span: Span) -> Lexeme {
        if self.peek() == Some(':') && self.colon_splits_here() {
            self.bump();
            return Lexeme {
                token: Token::Colon,
                span: self.finish(span),
            };
        }

        let word = self.scan_bare_fragment();
        let word = if let Some(path) = TildePath::parse(&word) {
            Word::Tilde(path)
        } else if word.contains('/') {
            Word::Slash(word)
        } else {
            Word::Plain(word)
        };
        Lexeme {
            token: Token::Word(word),
            span: self.finish(span),
        }
    }

    fn scan_bare_fragment(&mut self) -> String {
        let mut word = String::new();
        while let Some(ch) = self.peek() {
            if !continues_bare_word(ch) {
                break;
            }

            // `suppress_newline` reads as "we are inside `[…]`", where a
            // comma punctuates instead of joining the word.
            if ch == ',' && self.suppress_newline() {
                break;
            }

            // `&` ends a word here because `&&` is a connective.
            if self.in_expr() && (operator_char(ch).is_some() || ch == '&') {
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

    /// Length of the `#` run at the cursor, consuming nothing.
    fn count_hash_run(&self) -> usize {
        let mut n = 0;
        while self.peek_n(n) == Some('#') {
            n += 1;
        }
        n
    }

    /// A `#` run followed by `'` opens `#'…'#`; anything else is a comment.
    /// Shared by `next_token` and `skip_inline_whitespace` so they cannot
    /// disagree.
    fn hash_opens_quoted(&self) -> bool {
        self.peek_n(self.count_hash_run()) == Some('\'')
    }

    /// Push each body char and consume the closing `'` with its `level`
    /// `#`s.  A `'` trailed by fewer than `level` `#`s is body text.
    fn scan_quoted_body(
        &mut self,
        span: Span,
        level: usize,
        body: &mut String,
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
                    body.push('\'');
                    self.bump();
                }
                Some(ch) => {
                    body.push(ch);
                    self.bump();
                }
            }
        }
    }

    /// The leading `#`s are already consumed; the opening `'` is not.  Body
    /// bytes are verbatim — no escapes, no interpolation.
    fn scan_quoted(&mut self, span: Span, level: usize) -> Result<Lexeme, LexError> {
        self.bump();
        let mut body = String::new();
        self.scan_quoted_body(span, level, &mut body)?;
        Ok(Lexeme {
            token: Token::SingleQuoted(body),
            span: self.finish(span),
        })
    }

    fn scan_double_quoted(&mut self, span: Span) -> Result<Lexeme, LexError> {
        self.bump();
        let file = span.file;
        let mut parts: Vec<Spanned<StringPart>> = Vec::new();
        let mut run = LiteralRun::default();
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
                    run.flush(&mut parts, cursor, file);
                    self.bump();
                    break;
                }
                Some('\\') => {
                    self.bump();
                    self.scan_double_quoted_escape(cursor, &mut run)?;
                }
                Some(sigil @ ('$' | '!')) => {
                    let splice = self
                        .scan_splice()
                        .map_err(|inner| Self::rewrap_inner_into_string(span, form, inner))?;
                    if let Some(tokens) = splice {
                        run.flush(&mut parts, cursor, file);
                        let part_end = self.byte_pos();
                        parts.push(Spanned::new(
                            Span::new(file, cursor, part_end),
                            StringPart::Splice(tokens),
                        ));
                    } else {
                        // A sigil that opens nothing is text: `"$ 5"`, `"hi!"`.
                        run.push(cursor, sigil);
                    }
                }
                Some('~')
                    if cursor == span.start + 1 && matches!(self.peek_n(1), Some('/' | '"')) =>
                {
                    self.bump();
                    let tilde = Span::new(file, cursor, self.byte_pos());
                    let home = Token::Word(Word::Tilde(TildePath { suffix: None }));
                    parts.push(Spanned::new(
                        tilde,
                        StringPart::Splice(vec![Lexeme {
                            token: home,
                            span: tilde,
                        }]),
                    ));
                }
                Some(ch) => {
                    run.push(cursor, ch);
                    self.bump();
                }
            }
        }

        Ok(Lexeme {
            token: Token::DoubleQuoted(parts),
            span: self.finish(span),
        })
    }

    /// Consume one escape after the `\`, which the caller has bumped.
    /// `escape_start` is that `\`'s offset, so a malformed escape underlines
    /// `\q` itself rather than the string's opening quote.
    fn scan_double_quoted_escape(
        &mut self,
        escape_start: u32,
        run: &mut LiteralRun,
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
                run.push(escape_start, '\n');
            }
            Some('r') => {
                self.bump();
                run.push(escape_start, '\r');
            }
            Some('t') => {
                self.bump();
                run.push(escape_start, '\t');
            }
            Some('\\') => {
                self.bump();
                run.push(escape_start, '\\');
            }
            Some('0') => {
                self.bump();
                run.push(escape_start, '\0');
            }
            Some('e') => {
                self.bump();
                run.push(escape_start, '\x1b');
            }
            Some('"') => {
                self.bump();
                run.push(escape_start, '"');
            }
            Some('$') => {
                self.bump();
                run.push(escape_start, '$');
            }
            Some('!') => {
                self.bump();
                run.push(escape_start, '!');
            }
            Some('~') => {
                self.bump();
                run.push(escape_start, '~');
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
                        "\\xNN is limited to \\x00 through \\x7F; write non-ASCII characters \
                         directly or as \\u{…}",
                    ));
                }
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "the `n >= 0x80` guard above has already returned; fits u8"
                )]
                run.push(escape_start, n as u8 as char);
            }
            Some('u') => {
                self.bump();
                if self.peek() != Some('{') {
                    return Err(Self::error(span!(), "\\u escape is written \\u{…}"));
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
                            return Err(Self::error(span!(), "\\u{…} takes one to six hex digits"));
                        }
                    }
                }
                if digits.is_empty() {
                    return Err(Self::error(span!(), "\\u{…} takes one to six hex digits"));
                }
                self.bump();
                let cp = u32::from_str_radix(&digits, 16).unwrap();
                let ch = char::from_u32(cp).ok_or_else(|| {
                    Self::error(span!(), format!("\\u{{{digits}}} is not a Unicode scalar"))
                })?;
                run.push(escape_start, ch);
            }
            Some('\'') => {
                self.bump();
                return Err(Self::error(
                    span!(),
                    "`'` needs no escape inside a double-quoted string: write it directly",
                ));
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

    /// A splice inside `"…"`, at its `$` or `!`: the tokens of `$name`,
    /// `$(name)`, `$[…]`, `!{…}` or `!$name`.  `None` when the sigil opens
    /// nothing and is text.
    ///
    /// `$(name)` marks the end of a name, so the `[` after it is string
    /// text; every other splice continues into adjacent `[key]` groups.
    fn scan_splice(&mut self) -> Result<Option<Vec<Lexeme>>, LexError> {
        let start = self.span();
        let mut tokens = Vec::new();
        if self.peek() == Some('!') {
            self.bump();
            match self.peek() {
                Some('{') => {
                    tokens.push(Lexeme {
                        token: Token::Bang,
                        span: self.finish(start),
                    });
                    let open = self.bump_spanned();
                    let (body, close) = self.scan_token_group(open, DelimKind::Brace)?;
                    tokens.push(Lexeme {
                        token: Token::LBrace,
                        span: open,
                    });
                    tokens.extend(body);
                    tokens.push(Lexeme {
                        token: Token::RBrace,
                        span: close,
                    });
                }
                // `{` is kept so the `${…}` diagnostic still fires.
                Some('$')
                    if self
                        .peek_n(1)
                        .is_some_and(|c| is_ident_start(c) || matches!(c, '(' | '[' | '{')) =>
                {
                    tokens.push(Lexeme {
                        token: Token::Bang,
                        span: self.finish(start),
                    });
                    let dollar = self.span();
                    self.bump();
                    let token = self.scan_dollar()?.expect("guard saw a `$` form");
                    tokens.push(Lexeme {
                        token,
                        span: self.finish(dollar),
                    });
                }
                _ => return Ok(None),
            }
        } else {
            self.bump();
            match self.scan_dollar()? {
                Some(token) => tokens.push(Lexeme {
                    token,
                    span: self.finish(start),
                }),
                None => return Ok(None),
            }
        }
        let delimited = matches!(
            tokens.last(),
            Some(Lexeme {
                token: Token::Variable {
                    delimited: true,
                    ..
                },
                ..
            })
        );
        while !delimited && self.peek() == Some('[') {
            let open = self.bump_spanned();
            let (body, close) = self.scan_token_group(open, DelimKind::Bracket)?;
            tokens.push(Lexeme {
                token: Token::LBracket,
                span: open,
            });
            tokens.extend(body);
            tokens.push(Lexeme {
                token: Token::RBracket,
                span: close,
            });
        }
        Ok(Some(tokens))
    }

    /// The token after a `$` the caller consumed: `$name`, `$(name)`, or
    /// `$[…]`; `None` for a bare `$`.  The bare name stops before a trailing
    /// `-` (see [`Self::scan_deref_ident`]); the explicit `$(name)` keeps one.
    /// A following `[key]` is not this token's business: outside a string it
    /// lexes as an ordinary bracket group and the parser reads the adjacency.
    fn scan_dollar(&mut self) -> Result<Option<Token>, LexError> {
        match self.peek() {
            Some(ch) if is_ident_start(ch) => Ok(Some(Token::Variable {
                name: self.scan_deref_ident(),
                delimited: false,
            })),
            Some('{') => {
                let dollar = Span::new(self.file, self.byte_pos() - 1, self.byte_pos());
                Err(Self::error(
                    dollar,
                    "`${…}` is bash: write `$name`, `$(name)` to mark where a name ends, \
                     or `!{…}` to run a command",
                ))
            }
            Some('(') => {
                let dollar = Span::new(self.file, self.byte_pos() - 1, self.byte_pos());
                self.bump();
                let opened = self.finish(dollar);
                let body = self.pos;
                let name = self.scan_ident();
                // `$(123)` is a mistake; `$(` at EOF is merely unfinished,
                // and only that one may be re-anchored as still-open by an
                // enclosing double-quoted string.
                if self.peek().is_none() {
                    return Err(Self::typed_error(
                        opened,
                        LexErrorKind::UnclosedDeref { opened },
                    ));
                }
                if name.is_empty() {
                    return Err(Self::error(
                        self.finish(dollar),
                        "expected identifier after `$(`",
                    ));
                }
                match self.peek() {
                    Some(')') => {}
                    Some(' ' | '\t') => {
                        return Err(Self::error(
                            self.finish(dollar),
                            format!(
                                "`$(…)` holds one name, `$(name)`; to run a command and use \
                                 its output, write `!{{{}}}`",
                                self.command_text(body)
                            ),
                        ));
                    }
                    _ => {
                        return Err(Self::error(
                            self.finish(dollar),
                            "expected `)` to close `$(...)` dereference",
                        ));
                    }
                }
                self.bump();
                Ok(Some(Token::Variable {
                    name,
                    delimited: true,
                }))
            }
            Some('[') => {
                let open = self.bump_spanned();
                let (body, _) = self.scan_token_group(open, DelimKind::Expr)?;
                Ok(Some(Token::Expr(body)))
            }
            _ => Ok(None),
        }
    }

    /// The text from char index `from` to the next `)` or the end of the
    /// line, trimmed and capped: what a bash `$(cmd args)` would have run.
    fn command_text(&self, from: usize) -> String {
        const CAP: usize = 40;
        let line: String = self.chars[from..]
            .iter()
            .map(|&(_, c)| c)
            .take_while(|&c| !matches!(c, ')' | '\n'))
            .collect();
        let line = line.trim_end();
        match line.char_indices().nth(CAP) {
            Some((end, _)) => format!("{}…", &line[..end]),
            None => line.to_owned(),
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

    /// Lex a balanced body under `kind`: the caller has already consumed the
    /// opener (`opener` is its span, anchoring an `UnterminatedBalanced` if
    /// EOF comes first), and this consumes the closer without emitting it,
    /// returning the body and the closer's span.
    ///
    /// That bypass of [`Self::open_delim`] is why we push onto `delim_stack`
    /// here — it gives the body its lexical mode and makes the stack falling
    /// below our entry depth the signal that our own closer arrived.
    /// [`Self::close_delim`] does that pop on success; each error path pops
    /// explicitly.
    fn scan_token_group(
        &mut self,
        opener: Span,
        kind: DelimKind,
    ) -> Result<(Vec<Lexeme>, Span), LexError> {
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
            kind,
            opened: opener,
        });
        let entry_depth = self.delim_stack.len();
        let mut tokens = Vec::new();

        loop {
            let lexeme = match self.next_token() {
                Ok(lexeme) => lexeme,
                Err(e) => {
                    self.delim_stack.pop();
                    return Err(e);
                }
            };
            match (&lexeme.token, kind) {
                // Our closer: `close_delim` already popped us, so the stack
                // sits below the entry depth.
                (Token::RBrace, DelimKind::Brace)
                | (Token::RBracket, DelimKind::Bracket | DelimKind::Expr)
                    if self.delim_stack.len() < entry_depth =>
                {
                    return Ok((tokens, lexeme.span));
                }
                // With our delim open, `eof_or_unterminated` turns end of
                // input into the `Err` caught above.
                (Token::Eof, _) => {
                    unreachable!("next_token cannot yield Eof while a delim is open")
                }
                _ => tokens.push(lexeme),
            }
        }
    }

    fn is_fd_redirect_start(&self) -> bool {
        let mut offset = 0;
        while self.peek_n(offset).is_some_and(|ch| ch.is_ascii_digit()) {
            offset += 1;
        }
        matches!(self.peek_n(offset), Some('>' | '<'))
    }

    /// A digit run glued to `>`/`<`: read whole, then judged against the
    /// nine-spelling vocabulary by `redirect` and `dup`.
    fn scan_fd_redirect(&mut self, span: Span) -> Result<Lexeme, LexError> {
        let digits = self.take_while(|ch| ch.is_ascii_digit());
        match self.peek() {
            Some('>') => self.scan_redirect_gt(Some(&digits), span),
            Some('<') => self.scan_redirect_lt(Some(&digits), span),
            // `is_fd_redirect_start` already saw a `>`/`<` past the digits,
            // and `take_while` consumed exactly those digits.
            _ => unreachable!("scan_fd_redirect entered without a trailing '>' or '<'"),
        }
    }

    /// ral has no numbered descriptors, so anything but `2>` is refused here
    /// rather than left to mean whatever that fd happens to be in the
    /// process: a pipe or a pinned binary of the runtime's own.
    fn fd_refusal(digits: &str) -> String {
        format!(
            "file descriptor {digits}: ral has only standard input (0), \
             standard output (1) and standard error (2), so write a file \
             with `> file` or `2> file`"
        )
    }

    /// Judge a redirect spelled with `fd`, which is gone from the token.
    /// `stderr` is set only for `2` with a write, by construction.
    fn redirect(&self, fd: Option<&str>, op: RedirectOp, span: Span) -> Result<Lexeme, LexError> {
        let token = |stderr| {
            Ok(Lexeme {
                token: Token::Redirect { stderr, op },
                span: self.finish(span),
            })
        };
        let Some(digits) = fd else {
            return token(false);
        };
        let refuse = |m: String| Err(Self::error(self.finish(span), m));
        match (digits.parse::<u32>(), op) {
            (Ok(2), RedirectOp::Write(_)) => token(true),
            (Ok(0), RedirectOp::Write(_)) => refuse(STDIN_UNWRITABLE.into()),
            (Ok(0), _) => refuse("`<` already reads standard input: drop the `0`".into()),
            (Ok(1), RedirectOp::Write(_)) => refuse(
                "`>` already writes standard output: drop the `1`; \
                 to pass `1` as an argument, put a space before `>`"
                    .into(),
            ),
            (Ok(n @ (1 | 2)), RedirectOp::Read) => refuse(format!(
                "`<` always feeds standard input, so `{n}<` reads nothing in ral: \
                 drop the `{n}`, or did you mean `{n}> file` to write there?"
            )),
            (Ok(1 | 2), RedirectOp::HereString) => {
                refuse("`<<` always feeds stdin: drop the file-descriptor prefix".into())
            }
            _ => refuse(Self::fd_refusal(digits)),
        }
    }

    /// `fd>&to`: `2>&1` is the one dup ral models.
    fn dup(&self, fd: Option<&str>, to: &str, span: Span) -> Result<Lexeme, LexError> {
        let refuse = |m: String| Err(Self::error(self.finish(span), m));
        let fd = fd.unwrap_or("1");
        match (fd.parse::<u32>(), to.parse::<u32>()) {
            (Ok(0), Ok(0..=2)) => refuse(STDIN_UNWRITABLE.into()),
            (Ok(2), Ok(1)) => Ok(Lexeme {
                token: Token::StderrToStdout,
                span: self.finish(span),
            }),
            // A bare `>` writes fd 1, so `>&2` is `1>&2` spelled short.  Both
            // are the bash idiom for a diagnostic, and a diagnostic is a
            // builtin here rather than a second name for the byte channel.
            (Ok(1), Ok(2)) => refuse(
                "ral has no `1>&2`: to write a diagnostic, use \
                 `warn \"…\"`, which puts one line on standard error. \
                 Did you mean `2>&1`, folding a command's standard error \
                 into its standard output?"
                    .into(),
            ),
            (Ok(a @ (1 | 2)), Ok(b @ (1 | 2))) if a == b => refuse(format!(
                "`{a}>&{a}` names the stream it already is: drop it"
            )),
            (Ok(a @ 1..=2), Ok(b @ 0..=2)) => refuse(format!(
                "ral has no fd plumbing beyond `2>&1`, so `{a}>&{b}` has nothing to mean"
            )),
            (Ok(0..=2), _) => refuse(Self::fd_refusal(to)),
            _ => refuse(Self::fd_refusal(fd)),
        }
    }

    fn scan_redirect_gt(&mut self, fd: Option<&str>, span: Span) -> Result<Lexeme, LexError> {
        self.bump();
        if self.peek() == Some('>') {
            self.bump();
            return self.redirect(fd, RedirectOp::Write(WriteMode::Append), span);
        }
        // `>~` is the stream-write operator only when the `~` stands alone:
        // a bare char after it makes a word, so `>~/path` writes to `~/path`.
        if self.peek() == Some('~') && !self.peek_n(1).is_some_and(continues_bare_word) {
            self.bump();
            return self.redirect(fd, RedirectOp::Write(WriteMode::Stream), span);
        }
        if self.peek() == Some('&') {
            self.bump();
            let to = self.take_while(|ch| ch.is_ascii_digit());
            if to.is_empty() {
                return Err(Self::error(
                    self.finish(span),
                    "expected file descriptor after `>&`",
                ));
            }
            return self.dup(fd, &to, span);
        }
        self.redirect(fd, RedirectOp::Write(WriteMode::Write), span)
    }

    fn scan_redirect_lt(&mut self, fd: Option<&str>, span: Span) -> Result<Lexeme, LexError> {
        self.bump();
        if self.peek() == Some('<') {
            self.bump();
            if self.peek() == Some('<') {
                self.bump();
                return Err(Self::error(
                    self.finish(span),
                    "`<<<` is bash's here-string operator: ral's `<<` already \
                     feeds a string to stdin, so drop one `<`",
                ));
            }
            // A payload glued to `<<` is the bash heredoc reflex; a genuine
            // here-string takes a space.  Rejecting it stops the quoted
            // delimiter becoming stdin while the body lines run as commands.
            if self.peek().is_some_and(|ch| !ch.is_whitespace()) {
                return Err(Self::error(
                    self.finish(span),
                    format!(
                        "`<<` takes a space before its payload: {}",
                        crate::syntax::NO_HEREDOCS
                    ),
                ));
            }
            return self.redirect(fd, RedirectOp::HereString, span);
        }
        self.redirect(fd, RedirectOp::Read, span)
    }
}

const STDIN_UNWRITABLE: &str =
    "standard input cannot be written to: did you mean `< file`, which reads one into it?";

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> Token {
        Token::Word(Word::Plain(s.into()))
    }

    fn op(s: &str) -> Token {
        let arith = |op| Operator::Binary(BinaryOp::Arith(op));
        let compare = |op| Operator::Binary(BinaryOp::Compare(op));
        let eq = |op| Operator::Binary(BinaryOp::Eq(op));
        Token::Op(match s {
            "+" => arith(ArithOp::Add),
            "-" => arith(ArithOp::Sub),
            "*" => arith(ArithOp::Mul),
            "/" => arith(ArithOp::Div),
            "%" => arith(ArithOp::Mod),
            "<" => compare(CompareOp::Lt),
            ">" => compare(CompareOp::Gt),
            "<=" => compare(CompareOp::Le),
            ">=" => compare(CompareOp::Ge),
            "==" => eq(EqOp::Eq),
            "!=" => eq(EqOp::Ne),
            "&&" => Operator::And,
            "||" => Operator::Or,
            "=" => Operator::Assign,
            _ => panic!("no operator spelled {s:?}"),
        })
    }

    fn slash(s: &str) -> Token {
        Token::Word(Word::Slash(s.into()))
    }

    fn tilde_tok(suffix: Option<&str>) -> Token {
        Token::Word(Word::Tilde(TildePath {
            suffix: suffix.map(str::to_owned),
        }))
    }

    #[test]
    fn bidi_controls_refused_everywhere() {
        let m = lex_err("echo a\u{202E}b");
        assert!(m.contains("U+202E") && m.contains("right-to-left override"));
        let sp = lex_err_span("echo a\u{202E}b");
        assert_eq!((sp.start, sp.end), (6, 9));
        lex_err("'\u{202E}'");
        lex_err("\"\u{202E}\"");
        lex_ok("\"\\u{202E}\"");
    }

    #[test]
    fn c0_controls_refused_bare_only() {
        assert!(lex_err("echo a\u{1}b").contains("U+0001"));
        let sp = lex_err_span("echo a\u{1}b");
        assert_eq!((sp.start, sp.end), (6, 7));
        assert!(lex_err("echo \u{7F}").contains("U+007F"));
        lex_ok("echo '\u{1}'");
        lex_ok("\"\u{1}\"");
    }

    fn tok_types(source: &str) -> Vec<Token> {
        lex(source).unwrap().into_iter().map(|l| l.token).collect()
    }

    fn lex_ok(source: &str) -> Vec<Token> {
        lex(source)
            .unwrap_or_else(|e| panic!("expected Ok: {source:?}\n  error: {}", e.message()))
            .into_iter()
            .map(|l| l.token)
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

    /// The parts of the one double-quoted token `source` lexes to.
    fn string_parts(source: &str) -> Vec<Spanned<StringPart>> {
        match lex_ok(source).into_iter().next() {
            Some(Token::DoubleQuoted(parts)) => parts,
            other => panic!("expected DoubleQuoted, got {other:?}"),
        }
    }

    fn splice_kinds(part: &StringPart) -> Vec<&Token> {
        let StringPart::Splice(tokens) = part else {
            panic!("expected a splice, got {part:?}");
        };
        tokens.iter().map(|l| &l.token).collect()
    }

    fn variable(name: &str) -> Token {
        Token::Variable {
            name: name.into(),
            delimited: false,
        }
    }

    fn delimited(name: &str) -> Token {
        Token::Variable {
            name: name.into(),
            delimited: true,
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
        let Lexeme { token: tok, span } = lex(src).unwrap().pop().unwrap();
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
        let Token::DoubleQuoted(parts) = &toks[0].token else {
            panic!("expected DoubleQuoted");
        };
        assert_eq!(parts.len(), 2);
        let var = parts[0].span.unwrap();
        let lit = parts[1].span.unwrap();
        assert_eq!(splice_kinds(&parts[0].item), vec![&variable("x")]);
        assert_eq!((var.start, var.end), (3, 5));
        assert_eq!(parts[1].item, StringPart::Literal(" y".into()));
        assert_eq!((lit.start, lit.end), (5, 7));
    }

    /// The same rule with no flush in between: the literal run starts at the
    /// first real char, byte 3, not at the continuation.
    #[test]
    fn line_continuation_does_not_stretch_following_literal() {
        let toks = lex("\"\\\nabc\"").unwrap();
        let Token::DoubleQuoted(parts) = &toks[0].token else {
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
    fn variable_token() {
        let toks = tok_types("echo $x");
        assert_eq!(toks, vec![plain("echo"), variable("x"), Token::Eof]);
    }

    /// Outside a string `$x[0]` is two tokens and a bracket group; the
    /// parser reads the adjacency, as it does for `!{f}[0]`.
    #[test]
    fn indexed_variable_is_not_fused() {
        let toks = tok_types("$xs[0]");
        assert_eq!(
            toks,
            vec![
                variable("xs"),
                Token::LBracket,
                plain("0"),
                Token::RBracket,
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

    /// A closer that is not the innermost opener's is a definite error at the
    /// closer, naming the opener; no more input can repair it.
    #[test]
    fn a_mismatched_closer_is_a_definite_error() {
        let err = lex("[a }\nb]").expect_err("`}` does not close `[`");
        assert!(
            matches!(
                err.kind,
                LexErrorKind::Mismatched {
                    open: '[',
                    close: '}',
                    ..
                }
            ),
            "got {:?}",
            err.kind
        );
        assert!(!err.kind.is_incomplete());
        assert_eq!(err.span.range(), 3..4);
        assert_eq!(
            lex_err("{ ]"),
            "mismatched `]`: the innermost open delimiter is `{`"
        );
    }

    /// The same law inside a nested form, and for every kind of opener.
    #[test]
    fn a_mismatched_closer_is_refused_inside_every_nested_form() {
        for src in ["$[1 }", "{ [a }", "echo \"!{ a ]\"", "echo \"$h[k }\""] {
            assert!(
                matches!(
                    lex(src).map_err(|e| e.kind),
                    Err(LexErrorKind::Mismatched { .. })
                ),
                "{src:?}"
            );
        }
    }

    /// At depth 0 there is no opener to mismatch: the closer is a token, and
    /// the parser calls it unmatched.
    #[test]
    fn a_closer_at_depth_zero_is_a_token() {
        assert_eq!(tok_types("}"), vec![Token::RBrace, Token::Eof]);
        assert_eq!(
            tok_types("{ } ]"),
            vec![Token::LBrace, Token::RBrace, Token::RBracket, Token::Eof]
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
        assert!(err.contains("expected a tag label after the backtick, as in `` `ok ``"));
        assert!(err.contains("a backtick never runs a command here; write `!{cmd}` for that"));
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
                stderr: false,
                op: RedirectOp::Write(WriteMode::Write),
            }
        ));
    }

    #[test]
    fn redirect_stderr() {
        let toks = tok_types("cmd 2> err.log");
        assert!(matches!(
            toks[1],
            Token::Redirect {
                stderr: true,
                op: RedirectOp::Write(WriteMode::Write),
            }
        ));
    }

    #[test]
    fn redirect_stderr_to_stdout() {
        let toks = tok_types("cmd 2>&1");
        assert!(matches!(toks[1], Token::StderrToStdout));
        assert_eq!(
            lex("cmd 2>&1").unwrap()[1].span,
            Span::new(FileId::DUMMY, 4, 8)
        );
    }

    /// If `>~` swallowed the `~` in `>~/path`, the redirect would target
    /// `/path` instead of `~/path`.
    #[test]
    fn redirect_gt_then_tilde_path() {
        let toks = tok_types("echo hi >~/dir");
        assert!(
            matches!(
                toks[2],
                Token::Redirect {
                    stderr: false,
                    op: RedirectOp::Write(WriteMode::Write),
                }
            ),
            "expected a plain Write redirect, got {:?}",
            toks[2]
        );
        assert_eq!(toks[3], tilde_tok(Some("/dir")));
    }

    /// With no tilde-path suffix after it, `>~` stays stream-write.
    #[test]
    fn redirect_gt_tilde_standalone_is_stream_write() {
        let toks = tok_types("echo hi >~ sock");
        assert!(
            matches!(
                toks[2],
                Token::Redirect {
                    stderr: false,
                    op: RedirectOp::Write(WriteMode::Stream),
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
        assert_eq!(toks[2], variable("a"));
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
                variable("x"),
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
        let parts = string_parts("\"hello $name\"");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].item, StringPart::Literal("hello ".into()));
        assert_eq!(splice_kinds(&parts[1].item), vec![&variable("name")]);
    }

    /// A bare `$name` never eats a trailing `-`, so a kebab-adjacent
    /// interpolation cannot fold the dash into the first name.
    #[test]
    fn interpolation_stops_before_trailing_dash() {
        let parts = string_parts("\"$os-$arch\"");
        assert_eq!(parts.len(), 3);
        assert_eq!(splice_kinds(&parts[0].item), vec![&variable("os")]);
        assert_eq!(parts[1].item, StringPart::Literal("-".into()));
        assert_eq!(splice_kinds(&parts[2].item), vec![&variable("arch")]);
    }

    /// A `-` with more name after it is interior, so a genuine kebab
    /// identifier stays one deref.
    #[test]
    fn interpolation_keeps_interior_dash() {
        let parts = string_parts("\"$os-arch\"");
        assert_eq!(parts.len(), 1);
        assert_eq!(splice_kinds(&parts[0].item), vec![&variable("os-arch")]);
    }

    /// A `-` at end of string is literal text, not part of the name.
    #[test]
    fn interpolation_trailing_dash_at_end() {
        let parts = string_parts("\"$foo-\"");
        assert_eq!(parts.len(), 2);
        assert_eq!(splice_kinds(&parts[0].item), vec![&variable("foo")]);
        assert_eq!(parts[1].item, StringPart::Literal("-".into()));
    }

    /// `$(name)` fixes where the name ends, so it keeps the trailing `-`
    /// that the bare form drops.
    #[test]
    fn explicit_boundary_keeps_trailing_dash() {
        let parts = string_parts("\"$(foo-)\"");
        assert_eq!(parts.len(), 1);
        assert_eq!(splice_kinds(&parts[0].item), vec![&delimited("foo-")]);
    }

    /// A bare deref outside strings obeys the same rule.
    #[test]
    fn bare_deref_stops_before_trailing_dash() {
        let toks = tok_types("$os-$arch");
        assert_eq!(
            toks,
            vec![variable("os"), plain("-"), variable("arch"), Token::Eof]
        );
    }

    /// A splice is the tokens the same text lexes to outside the string: the
    /// inner `echo hello` is lexed in place, not sliced back out as raw text,
    /// and `!$d` is `!` then `$d` — not a synthesised `!{$d}`.
    #[test]
    fn double_quoted_splices_are_outer_tokens() {
        let parts = string_parts("\"!{echo hello}\"");
        assert_eq!(parts.len(), 1);
        assert_eq!(
            splice_kinds(&parts[0].item),
            vec![
                &Token::Bang,
                &Token::LBrace,
                &plain("echo"),
                &plain("hello"),
                &Token::RBrace,
            ]
        );

        let parts = string_parts("\"!$d\"");
        assert_eq!(
            splice_kinds(&parts[0].item),
            vec![&Token::Bang, &variable("d")]
        );
    }

    /// Adjacent `[key]` groups extend every splice but `$(name)`, whose `[`
    /// is text, and each closer keeps its own span so the parser sees the
    /// same stream the outer lexer would emit.
    #[test]
    fn double_quoted_splice_takes_postfix_keys() {
        let src = "\"$h[file][0]\"";
        let parts = string_parts(src);
        assert_eq!(parts.len(), 1);
        assert_eq!(
            splice_kinds(&parts[0].item),
            vec![
                &variable("h"),
                &Token::LBracket,
                &plain("file"),
                &Token::RBracket,
                &Token::LBracket,
                &plain("0"),
                &Token::RBracket,
            ]
        );
        let StringPart::Splice(tokens) = &parts[0].item else {
            unreachable!()
        };
        assert_eq!(&src[tokens[3].span.range()], "]");
        assert_eq!(&src[tokens[6].span.range()], "]");

        for splice in ["$x[k]", "!$x[k]", "!{f}[k]", "$[xs][0]"] {
            let parts = string_parts(&format!("\"{splice}\""));
            assert_eq!(parts.len(), 1, "{splice}");
            // The space stands in for the `"`, so nested spans line up.
            let outside = tok_types(&format!(" {splice}"));
            let outside: Vec<&Token> = outside[..outside.len() - 1].iter().collect();
            assert_eq!(splice_kinds(&parts[0].item), outside, "{splice}");
        }

        for (src, head) in [
            ("\"$(x)[k]\"", vec![&delimited("x")]),
            ("\"!$(x)[k]\"", vec![&Token::Bang, &delimited("x")]),
        ] {
            let parts = string_parts(src);
            assert_eq!(parts.len(), 2, "{src}");
            assert_eq!(splice_kinds(&parts[0].item), head, "{src}");
            assert_eq!(parts[1].item, StringPart::Literal("[k]".into()), "{src}");
        }
    }

    /// A sigil that opens nothing is text, `!$` included.
    #[test]
    fn double_quoted_bare_sigils_are_literal() {
        let parts = string_parts("\"cost: $ 5!$ ok\"");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].item, StringPart::Literal("cost: $ 5!$ ok".into()));
    }

    #[test]
    fn double_quoted_x_escape() {
        // \x41 = 'A'; \x7F = DEL (upper ASCII boundary).
        let t = |src: &str| match &lex_ok(src)[0] {
            Token::DoubleQuoted(p) => match &p[0].item {
                StringPart::Literal(s) => s.clone(),
                StringPart::Splice(_) => panic!("expected Literal"),
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
                StringPart::Splice(_) => panic!("expected Literal"),
            },
            _ => panic!("expected DoubleQuoted"),
        };
        assert_eq!(t(r#""\u{41}""#), "A");
        assert_eq!(t(r#""\u{0}""#), "\x00");
        assert_eq!(t(r#""\u{1F600}""#), "😀");
        // Surrogate, out of range, too many digits, no braces.
        assert!(lex_err(r#""\u{D800}""#).contains("Unicode scalar"));
        assert!(lex_err(r#""\u{110000}""#).contains("Unicode scalar"));
        assert!(lex_err(r#""\u{1234567}""#).contains("one to six hex digits"));
        assert!(lex_err(r#""\u{}""#).contains("one to six hex digits"));
        assert!(lex_err(r#""\u41""#).contains("\\u escape is written"));
    }

    #[test]
    fn dollar_bracket_arithmetic() {
        let toks = tok_types("$[2 + 3]");
        let Token::Expr(inner) = &toks[0] else {
            panic!("expected Expr token");
        };
        let kinds: Vec<&Token> = inner.iter().map(|l| &l.token).collect();
        assert_eq!(kinds, vec![&plain("2"), &op("+"), &plain("3")]);
    }

    /// Inside `$[…]` the operator characters end a word, so no spacing is
    /// needed, and a numeral is read whole through its exponent sign.
    #[test]
    fn dollar_bracket_operators_split_words_and_numerals_stay_whole() {
        let toks = tok_types("$[1+1*2==-3/4%5 && 1.5e+3-.5 && a-b]");
        let Token::Expr(inner) = &toks[0] else {
            panic!("expected Expr token");
        };
        let kinds: Vec<&Token> = inner.iter().map(|l| &l.token).collect();
        assert_eq!(
            kinds,
            vec![
                &plain("1"),
                &op("+"),
                &plain("1"),
                &op("*"),
                &plain("2"),
                &op("=="),
                &op("-"),
                &plain("3"),
                &op("/"),
                &plain("4"),
                &op("%"),
                &plain("5"),
                &op("&&"),
                &plain("1.5e+3"),
                &op("-"),
                &plain(".5"),
                &op("&&"),
                &plain("a"),
                &op("-"),
                &plain("b"),
            ]
        );
        // Outside, the same characters are word characters.
        assert_eq!(
            tok_types("echo 1+1"),
            vec![plain("echo"), plain("1+1"), Token::Eof]
        );
    }

    /// Inside `$[…]` the operators are `Token::Op`, redirect spellings included.
    /// An exponent is part of a float only: `1e+5` is `1e`, `+`, `5`.
    #[test]
    fn dollar_bracket_exponent_without_a_point_splits() {
        let toks = tok_types("$[1e+5]");
        let Token::Expr(inner) = &toks[0] else {
            panic!("expected Expr token");
        };
        let kinds: Vec<&Token> = inner.iter().map(|l| &l.token).collect();
        assert_eq!(kinds, vec![&plain("1e"), &op("+"), &plain("5")]);
    }

    #[test]
    fn dollar_bracket_operators_are_ops() {
        let toks = tok_types("$[1 && 0 || 2>3 && 2>=3 && 1<2 && 1<=2 && 1!=2]");
        let Token::Expr(inner) = &toks[0] else {
            panic!("expected Expr token");
        };
        let kinds: Vec<&Token> = inner.iter().map(|l| &l.token).collect();
        assert_eq!(
            kinds,
            vec![
                &plain("1"),
                &op("&&"),
                &plain("0"),
                &op("||"),
                &plain("2"),
                &op(">"),
                &plain("3"),
                &op("&&"),
                &plain("2"),
                &op(">="),
                &plain("3"),
                &op("&&"),
                &plain("1"),
                &op("<"),
                &plain("2"),
                &op("&&"),
                &plain("1"),
                &op("<="),
                &plain("2"),
                &op("&&"),
                &plain("1"),
                &op("!="),
                &plain("2"),
            ]
        );
    }

    /// A lone `=` is an `Op` as well; that it is no operator is the parser's
    /// to say.
    #[test]
    fn dollar_bracket_lone_equals_is_an_op() {
        let toks = tok_types("$[1 = 2 == 3]");
        let Token::Expr(inner) = &toks[0] else {
            panic!("expected Expr token");
        };
        let kinds: Vec<&Token> = inner.iter().map(|l| &l.token).collect();
        assert_eq!(
            kinds,
            vec![&plain("1"), &op("="), &plain("2"), &op("=="), &plain("3")]
        );
    }

    /// The mode is the innermost delimiter's: a `!{…}` inside `$[…]` is back
    /// in the shell, where `<` reads a file, and a lone `&` in the
    /// expression names the connective it is not.
    #[test]
    fn dollar_bracket_mode_is_innermost() {
        let toks = tok_types("$[!{wc -l < f} > 1]");
        let Token::Expr(inner) = &toks[0] else {
            panic!("expected Expr token");
        };
        assert!(inner.iter().any(|l| matches!(
            l.token,
            Token::Redirect {
                op: RedirectOp::Read,
                ..
            }
        )));
        assert!(lex_err("$[1 & 0]").contains("`&&`"));
        // Glued: `&` ends a word here, so `true&&false` is a conjunction
        // rather than one long command name.
        let Token::Expr(glued) = &tok_types("$[true&&false]")[0] else {
            panic!("expected Expr token");
        };
        assert_eq!(glued.len(), 3, "expected `true`, `&&`, `false`: {glued:?}");
    }

    /// Outside `$[…]` the shell meaning stands: a word may not start with `&`
    /// or `&&`, but carries either inside it, and `>=` is a redirect to a
    /// word starting with `=`.
    #[test]
    fn shell_mode_keeps_shell_meanings() {
        assert!(lex_err("sleep 1 &").contains("spawn"));
        assert!(lex_err("a && b").contains("no `&&`"));
        assert_eq!(
            tok_types("curl h/?a=1&b=2 a&&b"),
            vec![
                plain("curl"),
                slash("h/?a=1&b=2"),
                plain("a&&b"),
                Token::Eof
            ]
        );
        assert_eq!(
            tok_types("echo a >= b"),
            vec![
                plain("echo"),
                plain("a"),
                Token::Redirect {
                    stderr: false,
                    op: RedirectOp::Write(WriteMode::Write),
                },
                plain("="),
                plain("b"),
                Token::Eof,
            ]
        );
    }

    #[test]
    fn bare_dollar_is_refused() {
        assert!(lex_err("echo $").contains("$name"));
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
        let span = &inner[0].span;
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
    fn colon_splits_before_every_closer() {
        for (open, close, o, c) in [
            (Token::LBracket, Token::RBracket, '[', ']'),
            (Token::LBrace, Token::RBrace, '{', '}'),
            (Token::LParen, Token::RParen, '(', ')'),
        ] {
            let src = format!("{o}a:{c}");
            assert_eq!(
                tok_types(&src),
                vec![open, plain("a"), Token::Colon, close, Token::Eof],
                "source: {src:?}"
            );
        }
        assert_eq!(tok_types("host:8080"), vec![plain("host:8080"), Token::Eof]);
        assert_eq!(tok_types("a:b"), vec![plain("a:b"), Token::Eof]);
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
        assert_eq!(toks, vec![tilde_tok(None), Token::Eof,]);
    }

    #[test]
    fn tilde_inside_a_word_is_ordinary() {
        let toks = tok_types("foo~bar");
        assert_eq!(toks, vec![plain("foo~bar"), Token::Eof]);
    }

    /// A tilde not standing alone or before `/` is plain text.
    #[test]
    fn tilde_before_a_name_is_ordinary() {
        assert_eq!(tok_types("~bob"), vec![plain("~bob"), Token::Eof]);
        assert_eq!(tok_types("~bob/x"), vec![slash("~bob/x"), Token::Eof]);
    }

    #[test]
    fn tilde_path_token_is_structured() {
        let toks = tok_types("~/bin/claude");
        assert_eq!(toks, vec![tilde_tok(Some("/bin/claude")), Token::Eof,]);
    }

    #[test]
    fn tildes_in_a_list_are_tilde_words() {
        assert_eq!(
            tok_types("[~, ~/x]"),
            vec![
                Token::LBracket,
                tilde_tok(None),
                Token::Comma,
                tilde_tok(Some("/x")),
                Token::RBracket,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn leading_tilde_in_a_string_is_a_splice() {
        let parts = string_parts(r#""~/a b""#);
        assert_eq!(parts.len(), 2);
        assert_eq!(splice_kinds(&parts[0].item), vec![&tilde_tok(None)]);
        let tilde = parts[0].span.unwrap();
        assert_eq!((tilde.start, tilde.end), (1, 2));
        assert_eq!(parts[1].item, StringPart::Literal("/a b".into()));
    }

    #[test]
    fn lone_tilde_string_is_one_splice() {
        let parts = string_parts(r#""~""#);
        assert_eq!(parts.len(), 1);
        assert_eq!(splice_kinds(&parts[0].item), vec![&tilde_tok(None)]);
    }

    /// Only a leading `~`, and only before `/` or the closing quote.
    #[test]
    fn other_tildes_in_a_string_are_text() {
        for (src, text) in [
            (r#""~5""#, "~5"),
            (r#""a~/b""#, "a~/b"),
            (r#""\~/x""#, "~/x"),
        ] {
            let parts = string_parts(src);
            assert_eq!(
                parts.iter().map(|p| &p.item).collect::<Vec<_>>(),
                vec![&StringPart::Literal(text.into())],
                "{src}"
            );
        }
    }

    #[test]
    fn slash_bearing_bare_word_is_path_token() {
        let toks = tok_types("./script");
        assert_eq!(toks, vec![slash("./script"), Token::Eof]);
    }

    #[test]
    fn tilde_with_space_stays_two_tokens() {
        let toks = tok_types("~ foo");
        assert_eq!(toks, vec![tilde_tok(None), plain("foo"), Token::Eof,]);
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
        assert!(err.message().contains("expected identifier after `$(`"));
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

        // Any other non-`)` char still reaches the explicit-paren error.
        let err = lex("$(name]").expect_err("expected lex error");
        assert!(
            err.message()
                .contains("expected `)` to close `$(...)` dereference")
        );
    }

    /// Bash's `${…}` is refused at the `$`, in a string as out of one.
    #[test]
    fn dollar_brace_is_bash() {
        let said = "`${…}` is bash: write `$name`, `$(name)` to mark where a name ends, \
                    or `!{…}` to run a command";
        for (src, at) in [
            ("echo ${x}", 5),
            ("echo \"a ${x} b\"", 8),
            ("echo !${x}", 6),
        ] {
            assert_eq!(lex_err(src), said, "{src:?}");
            assert_eq!(lex_err_span(src).range(), at..at + 1, "{src:?}");
        }
    }

    /// Bash's `$(cmd args)` is told the form that runs a command, with the
    /// command as written, capped.
    #[test]
    fn dollar_paren_command_is_bash() {
        for (src, shown) in [
            ("echo $(ls -la)", "ls -la"),
            ("echo $(ls -la", "ls -la"),
            ("echo $(ls   )", "ls"),
            ("echo \"$(date +%s)\"", "date +%s"),
            ("echo $(ls\t-la)\necho a", "ls\t-la"),
        ] {
            assert_eq!(
                lex_err(src),
                format!(
                    "`$(…)` holds one name, `$(name)`; to run a command and use its \
                     output, write `!{{{shown}}}`"
                ),
                "{src:?}"
            );
        }
        let long = format!("echo $(ls {})", "x".repeat(60));
        let shown = format!("ls {}…", "x".repeat(37));
        assert!(
            lex_err(&long).ends_with(&format!("write `!{{{shown}}}`")),
            "{long:?}"
        );
    }

    /// `<<` is the here-string redirect.
    #[test]
    fn herestring_redirect() {
        let tokens = lex("cat << x").unwrap();
        assert!(
            tokens.iter().any(|l| matches!(
                l.token,
                Token::Redirect {
                    stderr: false,
                    op: RedirectOp::HereString,
                }
            )),
            "got {tokens:?}"
        );
        assert!(lex_err("cat 0<< x").contains("drop the `0`"));
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

    /// No numbered descriptors: an fd past 2 on either side of a redirect is
    /// refused at the lexer, so nothing downstream ever names one.
    #[test]
    fn redirect_fd_above_two_is_refused() {
        for src in [
            "cmd 3> f",
            "cmd 3>> f",
            "cmd 4< f",
            "cmd 2>&3",
            "cmd 99999999999> f",
        ] {
            let err = lex(src).expect_err("numbered descriptors must not lex");
            assert!(
                err.message().contains("standard error (2)"),
                "for {src:?} got: {}",
                err.message()
            );
        }
    }

    /// `1>` is refused with advice, so `echo 1>file` cannot write an empty
    /// file; the other fd-prefixed spellings are refused alike.
    #[test]
    fn fd_prefixed_spellings_are_refused() {
        assert!(lex_err("echo 1>file").contains("put a space before `>`"));
        assert!(lex_err("cat 0< f").contains("drop the `0`"));
        assert!(lex_err("cat 1< f").contains("did you mean `1> file`"));
        assert!(lex_err("cmd 0> f").contains("standard input cannot be written to"));
        assert!(lex_err("cmd 1>&1").contains("names the stream it already is"));
        assert!(lex_err("cmd 2>&2").contains("names the stream it already is"));
        assert!(lex_err("cmd 1>&0").contains("nothing to mean"));
    }

    #[test]
    fn redirect_dup_requires_target_fd() {
        let err = lex("cmd 2>&").expect_err("expected lex error");
        assert!(
            err.message()
                .contains("expected file descriptor after `>&`")
        );
    }

    /// Diagnostics are `warn`'s job, so fd 1 onto fd 2 is refused — under both
    /// spellings, since a bare `>` already means fd 1.  The refusal must point
    /// at the verb, and at `2>&1` for a program holding it backwards.
    #[test]
    fn stdout_onto_stderr_is_refused_for_warn() {
        for src in ["cmd 1>&2", "cmd >&2"] {
            let err = lex(src).expect_err("`1>&2` must not lex");
            let msg = err.message();
            assert!(msg.contains("warn"), "for {src:?} got: {msg}");
            assert!(msg.contains("2>&1"), "for {src:?} got: {msg}");
        }
        lex("cmd 2>&1").expect("`2>&1` is the direction that stays");
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
        // A bare `'` with no `#` never closes a level-1 literal, so the
        // message must spell out the `#` run the closer still wants —
        // otherwise it says what is wrong without saying what to type.
        let err = lex("#'body with ' but no hash").expect_err("should fail");
        assert!(err.message().contains("expected closing `'#`"), "{err}");
        let deep = lex("###'body").expect_err("should fail");
        assert!(deep.message().contains("expected closing `'###`"), "{deep}");
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
        assert_eq!(&src[toks[0].span.range()], "#'hi'#");
    }

    // ── byte-range span sanity checks ─────────────────────────────────────

    #[test]
    fn byte_spans_cover_full_tokens() {
        let toks = lex("echo hi").unwrap();
        assert_eq!(toks[0].span.start, 0);
        assert_eq!(toks[0].span.end, 4);
        assert_eq!(toks[1].span.start, 5);
        assert_eq!(toks[1].span.end, 7);
        assert!(matches!(toks[2].token, Token::Eof));
    }

    #[test]
    fn byte_spans_multibyte() {
        // Spans must land on byte boundaries, not char indices: `日本` is
        // 6 bytes, so slicing by them would panic if the two disagreed.
        let src = "日本 = hi";
        let toks = lex(src).unwrap();
        assert_eq!(&src[toks[0].span.range()], "日本");
        assert_eq!(&src[toks[1].span.range()], "=");
        assert_eq!(&src[toks[2].span.range()], "hi");
    }

    #[test]
    fn byte_spans_quoted_string() {
        let src = "'héllo'";
        let toks = lex(src).unwrap();
        assert_eq!(&src[toks[0].span.range()], "'héllo'");
    }

    #[test]
    fn splice_openers_are_one_char_wide() {
        let parts = string_parts("\"$x[0]\"");
        let StringPart::Splice(tokens) = &parts[0].item else {
            panic!("expected a splice");
        };
        let Lexeme { span: lbracket, .. } =
            tokens.iter().find(|l| l.token == Token::LBracket).unwrap();
        assert_eq!((lbracket.start, lbracket.end), (3, 4));
        let span = lex_err_span("$[1 +");
        assert_eq!((span.start, span.end), (1, 2));
    }

    #[test]
    fn unterminated_bumped_string_wants_quote_and_hash() {
        let msg = lex_err("echo a #'tis");
        assert!(msg.contains("bumped single-quoted string"), "{msg}");
        assert!(msg.contains("`'#`"), "{msg}");
    }

    #[test]
    fn bang_dollar_without_a_name_is_text() {
        assert_eq!(
            string_parts("\"!$ !$-\""),
            vec![Spanned::new(
                Span::new(FileId::DUMMY, 1, 7),
                StringPart::Literal("!$ !$-".into())
            )]
        );
    }
}
