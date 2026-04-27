//! Lexer: source text → token stream.
//!
//! Produces a flat `Vec<(Token, Span)>` from raw source.  Spans carry
//! line/column for legacy error paths and a byte-range [`ByteSpan`] for
//! ariadne diagnostics.  Newlines are statement separators except inside
//! `[...]` (lists/maps), where they are whitespace; this is decided by the
//! innermost open delimiter so nested `{ [ ] }` and `[ { } ]` both behave.
//!
//! Bare-word recognition is broad: anything not in the metacharacter set
//! is part of a word, including `:` and `=`.  `:` only splits when followed
//! by space, newline, or `]` — so `host:5432` stays one token but `host:`
//! splits.  `$`, `^`, `!`, `~` introduce structured forms (deref, expr
//! block, force, tilde path) and never appear mid-word.

use crate::ast::Word;
use crate::source::FileId;
use crate::span::Span as ByteSpan;
use crate::util::TildePath;
use std::fmt;

/// Source location attached to every token.
///
/// Carries both line/column (consumed by legacy parser error paths) and a
/// byte-range `ByteSpan` that will drive structured diagnostics. Line/column
/// are scheduled for removal once the parser/AST migrate to `ByteSpan` (S3).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub line: usize,
    pub col: usize,
    pub byte: ByteSpan,
}

impl Span {
    pub fn zero() -> Span {
        Span {
            line: 0,
            col: 0,
            byte: ByteSpan::point(FileId::DUMMY, 0),
        }
    }
}

/// Parts of an interpolated (double-quoted) string.
#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    Literal(String),
    Variable(String),
    /// Raw source text inside !{...} to be parsed later.
    Force(String),
    /// Raw source text inside $[...] to be parsed later.
    Expr(String),
    /// Variable with adjacent index keys: $name[k1][k2] — keys are raw text.
    Index(String, Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum RedirectType {
    Write,        // > — atomic (tmp + fsync + rename) for regular files
    StreamWrite,  // >~ — streaming truncate, POSIX `>` semantics
    Append,       // >>
    Read,         // <
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Word(Word),
    SingleQuoted(String),
    DoubleQuoted(Vec<StringPart>),
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
    /// Deref resolved by lexer: $name, $(name), $name[key].
    Deref(StringPart),
    /// Expression block: $[expr].
    Expr(String),
    Bang,
    Newline,
    Redirect {
        fd: Option<u32>,
        kind: RedirectType,
        target_fd: Option<u32>,
    },
    Eof,
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Word(Word::Plain(s)) => write!(f, "'{s}'"),
            Token::Word(Word::Slash(s)) => write!(f, "'{s}'"),
            Token::Word(Word::Tilde(path)) => {
                let mut rendered = "~".to_string();
                if let Some(user) = &path.user {
                    rendered.push_str(user);
                }
                if let Some(suffix) = &path.suffix {
                    rendered.push_str(suffix);
                }
                write!(f, "{rendered}")
            }
            Token::SingleQuoted(s) => write!(f, "'{s}'"),
            Token::DoubleQuoted(_) => write!(f, "\"...\""),
            Token::Dollar => write!(f, "$"),
            Token::Caret => write!(f, "^"),
            Token::Pipe => write!(f, "|"),
            Token::Ampersand => write!(f, "&"),
            Token::Question => write!(f, "?"),
            Token::Colon => write!(f, ":"),
            Token::LBrace => write!(f, "{{"),
            Token::RBrace => write!(f, "}}"),
            Token::LBracket => write!(f, "["),
            Token::RBracket => write!(f, "]"),
            Token::LParen => write!(f, "("),
            Token::RParen => write!(f, ")"),
            Token::Comma => write!(f, ","),
            Token::Spread => write!(f, "..."),
            Token::Deref(part) => match part {
                StringPart::Variable(n) => write!(f, "${n}"),
                StringPart::Index(n, _) => write!(f, "${n}[...]"),
                _ => write!(f, "$..."),
            },
            Token::Expr(_) => write!(f, "$[...]"),
            Token::Bang => write!(f, "!"),
            Token::Newline => write!(f, "newline"),
            Token::Redirect { .. } => write!(f, "redirect"),
            Token::Eof => write!(f, "end of input"),
        }
    }
}

impl Token {
    pub fn as_plain_word(&self) -> Option<&str> {
        match self {
            Token::Word(word) => word.as_plain(),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct LexError {
    pub message: String,
    pub line: usize,
    pub col: usize,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "lex error at {}:{}: {}",
            self.line, self.col, self.message
        )
    }
}

/// Tokenise `source` with a placeholder file id.
pub fn lex(source: &str) -> Result<Vec<(Token, Span)>, LexError> {
    lex_with(source, FileId::DUMMY)
}

/// Tokenise `source` attributing every token's byte-range to `file`.
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
    /// (byte_offset, char) for each char. Byte offsets let us stamp byte-range
    /// spans while keeping O(1) peek-by-char-index semantics.
    chars: Vec<(usize, char)>,
    source_len: u32,
    pos: usize,
    line: usize,
    col: usize,
    file: FileId,
    /// Stack of currently-open delimiters, innermost last. `true` means `[`,
    /// `false` means `{`. Newlines are suppressed when the innermost open
    /// delimiter is `[` — this makes multiline list/map literals work
    /// regardless of whether they appear inside a block body.
    delim_stack: Vec<bool>,
}

impl Lexer {
    fn new(source: &str, file: FileId) -> Self {
        Self {
            chars: source.char_indices().collect(),
            source_len: source.len() as u32,
            pos: 0,
            line: 1,
            col: 1,
            file,
            delim_stack: Vec::new(),
        }
    }

    /// Current byte offset (one past the last-consumed char).
    fn byte_pos(&self) -> u32 {
        self.chars
            .get(self.pos)
            .map(|(b, _)| *b as u32)
            .unwrap_or(self.source_len)
    }

    /// Span marker for the current position; byte range is zero-width until
    /// the token is finalised via `finish`.
    fn span(&self) -> Span {
        let p = self.byte_pos();
        Span {
            line: self.line,
            col: self.col,
            byte: ByteSpan::new(self.file, p, p),
        }
    }

    /// Extend `start` so its byte range covers up to the current position.
    fn finish(&self, mut start: Span) -> Span {
        start.byte.end = self.byte_pos();
        start
    }

    fn error(&self, span: Span, message: impl Into<String>) -> LexError {
        LexError {
            message: message.into(),
            line: span.line,
            col: span.col,
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
        if ch == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
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
        matches!(self.delim_stack.last(), Some(true))
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

    fn skip_layout(&mut self) {
        self.skip_inline_whitespace();
    }

    fn skip_comment(&mut self) {
        while self.peek().is_some_and(|ch| ch != '\n') {
            self.bump();
        }
    }

    fn is_bare_char(ch: char) -> bool {
        !matches!(
            ch,
            ' ' | '\t'
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
                | ','
                | '('
                | ')'
                | ';'
        )
        // Note: : and = ARE bare chars. Context-sensitive splitting happens in
        // scan_bare_word. . is also a bare char (for paths, URLs, etc.)
    }

    fn next_token(&mut self) -> Result<(Token, Span), LexError> {
        loop {
            self.skip_layout();

            let span = self.span();
            let Some(ch) = self.peek() else {
                return Ok((Token::Eof, span));
            };

            return match ch {
                '#' => {
                    self.skip_comment();
                    match self.peek() {
                        Some('\n') if self.suppress_newline() => {
                            self.bump();
                            continue;
                        }
                        Some('\n') => Ok(self.scan_separator(span)),
                        _ => Ok((Token::Eof, span)),
                    }
                }
                '\n' | ';' => Ok(self.scan_separator(span)),
                '{' => Ok(self.bump_delimited(Token::LBrace, Delimiter::Brace)),
                '}' => Ok(self.bump_delimited(Token::RBrace, Delimiter::CloseBrace)),
                '[' => Ok(self.bump_delimited(Token::LBracket, Delimiter::Bracket)),
                ']' => Ok(self.bump_delimited(Token::RBracket, Delimiter::CloseBracket)),
                '(' => Ok(self.bump_simple(Token::LParen, span)),
                ')' => Ok(self.bump_simple(Token::RParen, span)),
                '|' => Ok(self.bump_simple(Token::Pipe, span)),
                '&' => Ok(self.bump_simple(Token::Ampersand, span)),
                ',' => Ok(self.bump_simple(Token::Comma, span)),
                '$' => {
                    self.bump();
                    match self.scan_deref()? {
                        Some(StringPart::Expr(raw)) => Ok((Token::Expr(raw), self.finish(span))),
                        Some(part) => Ok((Token::Deref(part), self.finish(span))),
                        None => Ok((Token::Dollar, self.finish(span))),
                    }
                }
                '^' => Ok(self.bump_simple(Token::Caret, span)),
                '!' if self.peek_n(1) == Some('=') => {
                    self.bump();
                    self.bump();
                    Ok((Token::Word(Word::Plain("!=".into())), self.finish(span)))
                }
                '!' => Ok(self.bump_simple(Token::Bang, span)),
                '~' => Ok(self.scan_tilde(span)),
                '?' => Ok(self.bump_simple(Token::Question, span)),
                '\'' => self.scan_single_quoted(span),
                '"' => self.scan_double_quoted(span),
                '>' if self.peek_n(1) == Some('=') => {
                    self.bump();
                    self.bump();
                    Ok((Token::Word(Word::Plain(">=".into())), self.finish(span)))
                }
                '>' => self.scan_redirect_gt(None, span),
                '<' if self.peek_n(1) == Some('=') => {
                    self.bump();
                    self.bump();
                    Ok((Token::Word(Word::Plain("<=".into())), self.finish(span)))
                }
                '<' => Ok(self.scan_redirect_lt(None, span)),
                _ if ch.is_ascii_digit() && self.is_fd_redirect_start() => {
                    self.scan_fd_redirect(span)
                }
                '.' if self.peek_n(1) == Some('.') && self.peek_n(2) == Some('.') => {
                    self.bump();
                    self.bump();
                    self.bump();
                    Ok((Token::Spread, self.finish(span)))
                }
                _ if Self::is_bare_char(ch) => Ok(self.scan_bare_word(span)),
                _ => {
                    self.bump();
                    Err(self.error(span, format!("unexpected character: '{ch}'")))
                }
            };
        }
    }

    fn bump_simple(&mut self, token: Token, span: Span) -> (Token, Span) {
        self.bump();
        (token, self.finish(span))
    }

    fn bump_delimited(&mut self, token: Token, delimiter: Delimiter) -> (Token, Span) {
        let span = self.span();
        self.bump();
        match delimiter {
            Delimiter::Brace => self.delim_stack.push(false),
            Delimiter::Bracket => self.delim_stack.push(true),
            Delimiter::CloseBrace | Delimiter::CloseBracket => {
                self.delim_stack.pop();
            }
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
                Some('#') => self.skip_comment(),
                _ => break,
            }
        }
        (Token::Newline, self.finish(span))
    }

    fn scan_bare_word(&mut self, span: Span) -> (Token, Span) {
        if self.peek() == Some(':')
            && self
                .peek_n(1)
                .is_none_or(|next| matches!(next, ' ' | '\t' | '\n' | ']'))
        {
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
            if !Self::is_bare_char(ch) {
                break;
            }

            // `host: val`  → Bare("host"), Colon, Bare("val")
            // `host:5432`  → Bare("host:5432")
            if ch == ':' {
                let splits = self
                    .peek_n(1)
                    .is_none_or(|next| matches!(next, ' ' | '\t' | '\n' | ']'));
                if splits {
                    break;
                }
            }

            word.push(ch);
            self.bump();
        }
        word
    }

    fn scan_tilde(&mut self, span: Span) -> (Token, Span) {
        self.bump(); // consume '~'
        let suffix = match self.peek() {
            Some(ch) if Self::is_bare_char(ch) => self.scan_bare_fragment(),
            _ => String::new(),
        };
        let raw = format!("~{suffix}");
        let path = TildePath::parse(&raw).expect("tilde token should always parse");
        (Token::Word(Word::Tilde(path)), self.finish(span))
    }

    fn scan_single_quoted(&mut self, span: Span) -> Result<(Token, Span), LexError> {
        self.bump();
        let mut value = String::new();

        loop {
            match self.peek() {
                None => return Err(self.error(span, "unterminated single-quoted string")),
                Some('\'') => {
                    self.bump();
                    if self.peek() == Some('\'') {
                        self.bump();
                        value.push('\'');
                    } else {
                        break;
                    }
                }
                Some(ch) => {
                    value.push(ch);
                    self.bump();
                }
            }
        }

        Ok((Token::SingleQuoted(value), self.finish(span)))
    }

    fn scan_double_quoted(&mut self, span: Span) -> Result<(Token, Span), LexError> {
        self.bump();
        let mut parts = Vec::new();
        let mut literal = String::new();

        loop {
            match self.peek() {
                None => return Err(self.error(span, "unterminated double-quoted string")),
                Some('"') => {
                    self.bump();
                    if self.peek() == Some('"') {
                        self.bump();
                        literal.push('"');
                    } else {
                        break;
                    }
                }
                Some('\\') => {
                    self.bump();
                    self.scan_double_quoted_escape(&mut literal);
                }
                Some('$') => {
                    self.bump();
                    match self.scan_deref()? {
                        Some(part) => {
                            Self::flush_literal(&mut parts, &mut literal);
                            parts.push(part);
                        }
                        None => literal.push('$'),
                    }
                }
                Some('!') => {
                    self.bump();
                    match self.peek() {
                        Some('{') => {
                            Self::flush_literal(&mut parts, &mut literal);
                            self.bump();
                            parts.push(StringPart::Force(self.scan_balanced('{', '}')?));
                        }
                        Some('$') => {
                            Self::flush_literal(&mut parts, &mut literal);
                            self.bump();
                            let name = self.scan_ident();
                            parts.push(StringPart::Force(format!("${name}")));
                        }
                        _ => literal.push('!'),
                    }
                }
                Some(ch) => {
                    literal.push(ch);
                    self.bump();
                }
            }
        }

        Self::flush_literal(&mut parts, &mut literal);
        Ok((Token::DoubleQuoted(parts), self.finish(span)))
    }

    fn scan_double_quoted_escape(&mut self, literal: &mut String) {
        match self.peek() {
            Some('n') => {
                self.bump();
                literal.push('\n');
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
            Some(ch) => {
                literal.push('\\');
                literal.push(ch);
                self.bump();
            }
            None => literal.push('\\'),
        }
    }

    fn flush_literal(parts: &mut Vec<StringPart>, literal: &mut String) {
        if !literal.is_empty() {
            parts.push(StringPart::Literal(std::mem::take(literal)));
        }
    }

    /// IDENT = [^ \t\n|{}[\]$<>"'#,();=:!]+
    /// Scan a deref after $: $name, $(name), $name[key], $[arith].
    /// Returns None for bare $ (not followed by ident/paren/bracket).
    fn scan_deref(&mut self) -> Result<Option<StringPart>, LexError> {
        match self.peek() {
            Some(ch) if Self::is_ident_start(ch) => {
                let name = self.scan_ident();
                let mut keys = Vec::new();
                while self.peek() == Some('[') {
                    self.bump();
                    keys.push(self.scan_balanced('[', ']')?);
                }
                Ok(Some(if keys.is_empty() {
                    StringPart::Variable(name)
                } else {
                    StringPart::Index(name, keys)
                }))
            }
            Some('(') => {
                let span = self.span();
                self.bump();
                let name = self.scan_ident();
                if name.is_empty() {
                    return Err(self.error(span, "expected identifier after '$('"));
                }
                if self.peek() != Some(')') {
                    return Err(self.error(span, "expected ')' to close '$(...)' dereference"));
                }
                self.bump();
                Ok(Some(StringPart::Variable(name)))
            }
            Some('[') => {
                self.bump();
                Ok(Some(StringPart::Expr(self.scan_balanced('[', ']')?)))
            }
            _ => Ok(None),
        }
    }

    fn scan_ident(&mut self) -> String {
        let Some(ch) = self.peek() else {
            return String::new();
        };
        if !Self::is_ident_start(ch) {
            return String::new();
        }

        let mut name = String::new();
        name.push(ch);
        self.bump();
        name.push_str(&self.take_while(Self::is_ident_cont));
        name
    }

    /// IDENT = [a-zA-Z_][a-zA-Z0-9_-]*
    fn is_ident_start(ch: char) -> bool {
        ch.is_ascii_alphabetic() || ch == '_'
    }

    fn is_ident_cont(ch: char) -> bool {
        ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
    }

    /// Scan balanced delimiters, returning the content between them.
    /// The opening delimiter has already been consumed.
    fn scan_balanced(&mut self, open: char, close: char) -> Result<String, LexError> {
        let start = self.span();
        let mut depth = 1usize;
        let mut content = String::new();

        while let Some(ch) = self.peek() {
            match ch {
                c if c == open => {
                    depth += 1;
                    content.push(c);
                    self.bump();
                }
                c if c == close => {
                    depth -= 1;
                    self.bump();
                    if depth == 0 {
                        return Ok(content);
                    }
                    content.push(c);
                }
                '\'' => self.copy_single_quoted(&mut content),
                '"' => self.copy_double_quoted(&mut content),
                _ => {
                    content.push(ch);
                    self.bump();
                }
            }
        }

        Err(self.error(start, format!("unterminated '{open}...{close}'")))
    }

    fn copy_single_quoted(&mut self, out: &mut String) {
        out.push('\'');
        self.bump();

        while let Some(ch) = self.peek() {
            out.push(ch);
            self.bump();
            if ch == '\'' {
                if self.peek() == Some('\'') {
                    out.push('\'');
                    self.bump();
                } else {
                    break;
                }
            }
        }
    }

    fn copy_double_quoted(&mut self, out: &mut String) {
        out.push('"');
        self.bump();

        while let Some(ch) = self.peek() {
            out.push(ch);
            self.bump();
            match ch {
                '"' if self.peek() == Some('"') => {
                    out.push('"');
                    self.bump();
                }
                '"' => break,
                '\\' => {
                    if let Some(escaped) = self.peek() {
                        out.push(escaped);
                        self.bump();
                    }
                }
                _ => {}
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

    fn scan_fd_redirect(&mut self, span: Span) -> Result<(Token, Span), LexError> {
        let fd_digits = self.take_while(|ch| ch.is_ascii_digit());

        match self.peek() {
            Some('>') => {
                let fd = Some(fd_digits.parse::<u32>().unwrap_or(1));
                self.scan_redirect_gt(fd, span)
            }
            Some('<') => {
                let fd = Some(fd_digits.parse::<u32>().unwrap_or(0));
                Ok(self.scan_redirect_lt(fd, span))
            }
            _ => Ok((Token::Word(Word::Plain(fd_digits)), self.finish(span))),
        }
    }

    fn scan_redirect_gt(&mut self, fd: Option<u32>, span: Span) -> Result<(Token, Span), LexError> {
        self.bump();
        if self.peek() == Some('>') {
            self.bump();
            return Ok((
                Token::Redirect {
                    fd,
                    kind: RedirectType::Append,
                    target_fd: None,
                },
                self.finish(span),
            ));
        }

        if self.peek() == Some('~') {
            self.bump();
            return Ok((
                Token::Redirect {
                    fd,
                    kind: RedirectType::StreamWrite,
                    target_fd: None,
                },
                self.finish(span),
            ));
        }

        if self.peek() == Some('&') {
            self.bump();
            let target_fd = self.take_while(|ch| ch.is_ascii_digit());
            if target_fd.is_empty() {
                return Err(self.error(span, "expected file descriptor after '>&'"));
            }

            return Ok((
                Token::Redirect {
                    fd,
                    kind: RedirectType::Write,
                    target_fd: Some(target_fd.parse::<u32>().unwrap_or(1)),
                },
                self.finish(span),
            ));
        }

        Ok((
            Token::Redirect {
                fd,
                kind: RedirectType::Write,
                target_fd: None,
            },
            self.finish(span),
        ))
    }

    fn scan_redirect_lt(&mut self, fd: Option<u32>, span: Span) -> (Token, Span) {
        self.bump();
        (
            Token::Redirect {
                fd,
                kind: RedirectType::Read,
                target_fd: None,
            },
            self.finish(span),
        )
    }
}

enum Delimiter {
    Brace,
    CloseBrace,
    Bracket,
    CloseBracket,
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
    fn single_quoted_embedded() {
        let toks = tok_types("echo 'it''s'");
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
                kind: RedirectType::Write,
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
                kind: RedirectType::Write,
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
                kind: RedirectType::Write,
                target_fd: Some(1)
            }
        ));
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
                assert_eq!(parts[0], StringPart::Literal("hello ".into()));
                assert_eq!(parts[1], StringPart::Variable("name".into()));
            }
            _ => panic!("expected DoubleQuoted"),
        }
    }

    #[test]
    fn double_quoted_substitution() {
        let toks = tok_types("\"!{echo hello}\"");
        match &toks[0] {
            Token::DoubleQuoted(parts) => {
                assert_eq!(parts.len(), 1);
                assert_eq!(parts[0], StringPart::Force("echo hello".into()));
            }
            _ => panic!("expected DoubleQuoted"),
        }
    }

    #[test]
    fn dollar_bracket_arithmetic() {
        let toks = tok_types("$[2 + 3]");
        assert_eq!(toks[0], Token::Expr("2 + 3".into()));
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
        let err = lex("$(").expect_err("expected lex error");
        assert!(err.message.contains("expected identifier after '$('"));

        let err = lex("$(1)").expect_err("expected lex error");
        assert!(err.message.contains("expected identifier after '$('"));
    }

    #[test]
    fn deref_paren_requires_closing_paren() {
        let err = lex("$(name").expect_err("expected lex error");
        assert!(
            err.message
                .contains("expected ')' to close '$(...)' dereference")
        );
    }

    #[test]
    fn redirect_dup_requires_target_fd() {
        let err = lex("cmd 2>&").expect_err("expected lex error");
        assert!(err.message.contains("expected file descriptor after '>&'"));
    }

    // ── byte-range span sanity checks ─────────────────────────────────────

    #[test]
    fn byte_spans_cover_full_tokens() {
        // ASCII: spans should be [start, start + len).
        let toks = lex("echo hi").unwrap();
        // echo
        assert_eq!(toks[0].1.byte.start, 0);
        assert_eq!(toks[0].1.byte.end, 4);
        // hi
        assert_eq!(toks[1].1.byte.start, 5);
        assert_eq!(toks[1].1.byte.end, 7);
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
        assert_eq!(&src[toks[0].1.byte.range()], "日本");
        // =
        assert_eq!(&src[toks[1].1.byte.range()], "=");
        // hi
        assert_eq!(&src[toks[2].1.byte.range()], "hi");
    }

    #[test]
    fn byte_spans_quoted_string() {
        let src = "'héllo'";
        let toks = lex(src).unwrap();
        // Whole quoted token including the surrounding quotes.
        assert_eq!(&src[toks[0].1.byte.range()], "'héllo'");
    }
}
