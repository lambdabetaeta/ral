//! Parser: token stream → AST.
//!
//! Recursive-descent over the [`crate::lexer`] output.  The grammar is
//! statement-oriented: a *program* is a sequence of statements separated
//! by newlines or `;`.  A *statement* is a `?`-chained list of pipelines;
//! a *pipeline* is `|`-connected stages; a *stage* is a let-binding,
//! `return`, `if`, or a *command* (head plus arguments).  `|`, `?`, `,`,
//! and `=` are continuation tokens — newlines around them are absorbed.
//!
//! Source positions are interleaved into statement lists as [`Ast::Pos`]
//! markers so the elaborator can re-thread them onto IR nodes without
//! the parser having to thread a span through every constructor.
//!
//! Arithmetic inside `$[...]` is parsed by a small Pratt sub-parser
//! ([`Parser::parse_expr_prec`]).  The outer lexer re-tokenises the raw
//! body, so `&&` / `||` arrive as adjacent single-char tokens; they are
//! fused back into bare-word operators before Pratt sees them.

use crate::ast::*;
use crate::lexer::{self, LexError, RedirectType, Span, StringPart, Token};
use crate::types;
use std::fmt;

// ── Parse Error ──────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub line: usize,
    pub col: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "parse error at {}:{}: {}",
            self.line, self.col, self.message
        )
    }
}

impl std::error::Error for ParseError {}

impl From<ParseError> for types::Error {
    fn from(e: ParseError) -> Self {
        types::Error::new(e.to_string(), 2)
    }
}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        ParseError {
            message: e.message,
            line: e.line,
            col: e.col,
        }
    }
}

// ── Public API ───────────────────────────────────────────────────────────

pub fn parse(source: &str) -> Result<Vec<Ast>, ParseError> {
    parse_with(source, crate::source::FileId::DUMMY)
}

/// Returns `true` when `input` is incomplete and the user's next line should
/// be joined to it before parsing.  Two cases:
///
/// 1. The input ends with a continuation token (`|`, `?`, `=`, `if`,
///    `elsif`, `else`, `,`) — the parser would ignore a newline here anyway.
/// 2. Lexing fails with an "unterminated" error — an open `'...'` or `"..."`
///    string spans a line boundary.
pub fn needs_continuation(input: &str) -> bool {
    match lexer::lex(input) {
        Err(e) => e.message.contains("unterminated"),
        Ok(tokens) => {
            let last = tokens
                .iter()
                .rev()
                .find(|(t, _)| !matches!(t, Token::Newline | Token::Eof));
            match last.map(|(t, _)| t) {
                Some(Token::Pipe) | Some(Token::Question) | Some(Token::Comma) => true,
                Some(tok) => matches!(tok.as_plain_word(), Some("=" | "if" | "elsif" | "else")),
                _ => false,
            }
        }
    }
}

pub fn parse_with(source: &str, file: crate::source::FileId) -> Result<Vec<Ast>, ParseError> {
    let tokens = lexer::lex_with(source, file)?;
    let mut parser = Parser::new(tokens);
    parser.parse_program()
}

// ── Parser ───────────────────────────────────────────────────────────────

/// Loop-body verdict for [`Parser::parse_separated_until`]: keep going
/// after this item, or treat it as the last (caller may consume a
/// trailing comma, then the closing token).
enum SepFlow {
    Cont,
    Stop,
}

struct Parser {
    tokens: Vec<(Token, Span)>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<(Token, Span)>) -> Self {
        Parser { tokens, pos: 0 }
    }

    fn peek(&self) -> &Token {
        self.tokens
            .get(self.pos)
            .map(|(t, _)| t)
            .unwrap_or(&Token::Eof)
    }

    fn span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .map(|(_, s)| *s)
            .unwrap_or(Span::zero())
    }

    fn advance(&mut self) -> &Token {
        let tok = self
            .tokens
            .get(self.pos)
            .map(|(t, _)| t)
            .unwrap_or(&Token::Eof);
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn expect(&mut self, expected: &Token) -> Result<(), ParseError> {
        let span = self.span();
        let tok = self.advance().clone();
        if std::mem::discriminant(&tok) == std::mem::discriminant(expected) {
            Ok(())
        } else {
            Err(ParseError {
                message: format!("expected {expected}, found {tok}"),
                line: span.line,
                col: span.col,
            })
        }
    }

    fn at_stmt_end(&self) -> bool {
        matches!(
            self.peek(),
            Token::Newline | Token::Eof | Token::RBrace | Token::Ampersand
        )
    }

    fn skip_newlines(&mut self) {
        while self.peek() == &Token::Newline {
            self.advance();
        }
    }

    fn error(&self, message: impl Into<String>) -> ParseError {
        let span = self.span();
        ParseError {
            message: message.into(),
            line: span.line,
            col: span.col,
        }
    }

    /// Drive a comma-separated list terminated by `end`.  The closure
    /// parses one item and signals whether the run continues (`Cont`)
    /// or that item closes it (`Stop`).  A trailing comma before `end`
    /// is allowed in either case.  `label` names the construct ("list",
    /// "map pattern", …) for the error message on a missing separator.
    fn parse_separated_until(
        &mut self,
        end: Token,
        label: &str,
        mut item: impl FnMut(&mut Self) -> Result<SepFlow, ParseError>,
    ) -> Result<(), ParseError> {
        loop {
            if self.peek() == &end {
                self.advance();
                return Ok(());
            }
            match item(self)? {
                SepFlow::Cont => {
                    if self.peek() == &Token::Comma {
                        self.advance();
                    } else if self.peek() != &end {
                        return Err(self.error(format!("expected ',' or ']' in {label}")));
                    }
                }
                SepFlow::Stop => {
                    if self.peek() == &Token::Comma {
                        self.advance();
                    }
                    self.expect(&end)?;
                    return Ok(());
                }
            }
        }
    }

    // ── Grammar productions ──────────────────────────────────────────

    /// program = stmt*
    fn parse_program(&mut self) -> Result<Vec<Ast>, ParseError> {
        let mut stmts = Vec::new();
        self.skip_newlines();
        while self.peek() != &Token::Eof && self.peek() != &Token::RBrace {
            let span = self.span();
            stmts.push(Ast::Pos(span.byte));
            stmts.push(self.parse_stmt()?);
            self.skip_newlines();
        }
        Ok(stmts)
    }

    /// stmt = binding | pipeline '&'? (NL? '?' pipeline '&'?)* NL?
    /// The `?` can appear after a newline (continuation).
    ///
    /// A `let` binding is a *statement*, never a pipeline stage or chain
    /// branch — its RHS already absorbs an entire pipeline-and-chain, and
    /// embedding it deeper would produce an `Ast::Let` in expression
    /// position (which the elaborator cannot lower).  Catching it here is
    /// what keeps `parse_stage`'s leading-`let` rejection truly defensive.
    fn parse_stmt(&mut self) -> Result<Ast, ParseError> {
        if let Some(binding) = self.try_parse_binding()? {
            // Consume the optional statement terminator (one newline).
            if self.peek() == &Token::Newline {
                self.advance();
            }
            return Ok(binding);
        }
        let first = self.parse_maybe_background_pipeline()?;
        let mut chains = vec![first];

        loop {
            if self.peek() == &Token::Question {
                self.advance();
                chains.push(self.parse_maybe_background_pipeline()?);
            } else if self.peek() == &Token::Newline {
                // Peek past newline to check for ? continuation
                let save = self.pos;
                self.advance(); // consume newline
                if self.peek() == &Token::Question {
                    self.advance(); // consume ?
                    chains.push(self.parse_maybe_background_pipeline()?);
                } else {
                    // Not a continuation — put the newline back
                    self.pos = save;
                    break;
                }
            } else {
                break;
            }
        }

        // Consume the statement terminator
        if self.peek() == &Token::Newline {
            self.advance();
        }

        if chains.len() == 1 {
            Ok(chains.remove(0))
        } else {
            Ok(Ast::Chain(chains))
        }
    }

    /// pipeline = stage ('|' stage)*
    ///
    /// `|` is a continuation token: a newline before or after `|` is ignored.
    fn parse_pipeline(&mut self) -> Result<Ast, ParseError> {
        let span = self.span();
        let first = self.parse_stage()?;
        let mut stages = vec![Ast::Pos(span.byte), first];

        // `|` is a continuation token: newlines are ignored on either side.
        while self.eat_pipe_with_newlines() {
            let span = self.span();
            stages.push(Ast::Pos(span.byte));
            stages.push(self.parse_stage()?);
        }

        if stages.len() == 2 {
            // Single stage — drop the Pos and return the stage directly.
            stages.remove(0);
            Ok(stages.remove(0))
        } else {
            Ok(Ast::Pipeline(stages))
        }
    }

    /// Consume `|` surrounded by optional newlines. Returns true if eaten;
    /// rewinds otherwise.
    fn eat_pipe_with_newlines(&mut self) -> bool {
        let save = self.pos;
        self.skip_newlines();
        if self.peek() == &Token::Pipe {
            self.advance();
            self.skip_newlines();
            true
        } else {
            self.pos = save;
            false
        }
    }

    fn parse_maybe_background_pipeline(&mut self) -> Result<Ast, ParseError> {
        let mut node = self.parse_pipeline()?;
        if self.peek() == &Token::Ampersand {
            self.advance();
            node = Ast::Background(Box::new(node));
        }
        Ok(node)
    }

    /// stage = return | if | command
    ///
    /// `let` is a statement, not a stage — `parse_stmt` peels it off
    /// before reaching here.  Seeing it now means the caller embedded
    /// a binding in pipeline or chain position (`cmd | let x = …`,
    /// `cmd ? let x = …`); reject it with a clear error rather than
    /// mis-parse `let` as a command head.
    fn parse_stage(&mut self) -> Result<Ast, ParseError> {
        if self.peek().as_plain_word() == Some("let") {
            return Err(self.error(
                "`let` is a statement, not a pipeline stage or chain branch — \
                 move the binding to its own line, or wrap the consumer in a \
                 block: `{ let x = …; … }`",
            ));
        }

        // `return` is a dedicated stage form that lifts a value into a
        // computation; it is not an implicit control-flow escape.
        if self.peek().as_plain_word() == Some("return") {
            return self.parse_return_stage();
        }

        // `if` is a syntactic stage form; it is not a regular command application.
        if self.peek().as_plain_word() == Some("if") {
            return self.parse_if();
        }

        // `case` is a syntactic stage form: `case <scrutinee> [<handlers>]`.
        // It is not a regular command application — the table argument has
        // restricted shape (tag-keyed record of thunks) and the typing rule
        // is bespoke, so the parser captures it as a dedicated AST node.
        if self.peek().as_plain_word() == Some("case") {
            return self.parse_case();
        }

        self.parse_command()
    }

    /// case = 'case' atom atom
    ///
    /// The first atom is the scrutinee (a variant value); the second is a
    /// tag-keyed record literal of handler thunks.  Both restrictions are
    /// enforced by the typechecker rather than the parser — any atom is
    /// accepted here so that error messages downstream can refer to the
    /// resolved type.
    fn parse_case(&mut self) -> Result<Ast, ParseError> {
        self.advance(); // consume `case`
        self.skip_newlines();
        let scrutinee = self.parse_atom()?;
        self.skip_newlines();
        let table = self.parse_atom()?;
        Ok(Ast::Case {
            scrutinee: Box::new(scrutinee),
            table: Box::new(table),
        })
    }

    /// if = 'if' atom atom [elsif atom atom]* [else atom]
    ///
    /// Both branches are atoms: blocks, force expressions, variables — any value.
    /// The typechecker ensures they are thunks.  When no else branch is given
    /// the condition is evaluated for side effects only (type Unit).
    fn parse_if(&mut self) -> Result<Ast, ParseError> {
        self.advance(); // consume 'if'
        self.skip_newlines();
        let cond = self.parse_atom()?;
        self.skip_newlines();
        let then = self.parse_atom()?;

        let mut elsif = Vec::new();
        let mut else_ = None;

        loop {
            // Allow the elsif/else keywords on the next line.
            let save = self.pos;
            self.skip_newlines();
            match self.peek() {
                tok if tok.as_plain_word() == Some("elsif") => {
                    self.advance();
                    self.skip_newlines();
                    let ec = self.parse_atom()?;
                    self.skip_newlines();
                    let et = self.parse_atom()?;
                    elsif.push((ec, et));
                }
                tok if tok.as_plain_word() == Some("else") => {
                    self.advance();
                    self.skip_newlines();
                    else_ = Some(Box::new(self.parse_atom()?));
                    break;
                }
                _ => {
                    // Detect old three-block syntax: `if cond then { else }`.
                    // self.pos == save means skip_newlines() consumed nothing —
                    // we are on the same line.  A bare `{` here is the missing
                    // `else` keyword.
                    if self.pos == save && matches!(self.peek(), Token::LBrace) {
                        return Err(
                            self.error("unexpected `{` after `if` — did you mean `else { … }`?")
                        );
                    }
                    self.pos = save;
                    break;
                }
            }
        }

        Ok(Ast::If {
            cond: Box::new(cond),
            then: Box::new(then),
            elsif,
            else_,
        })
    }

    fn parse_return_stage(&mut self) -> Result<Ast, ParseError> {
        self.advance(); // consume `return`

        if self.at_cmd_end() {
            return Ok(Ast::Return(None));
        }

        let val = self.parse_atom()?;
        if !self.at_cmd_end() {
            return Err(self.error("return expects at most one value argument"));
        }

        Ok(Ast::Return(Some(Box::new(val))))
    }

    /// Try to parse: let pattern = pipeline
    /// Returns None if this isn't a binding.
    fn try_parse_binding(&mut self) -> Result<Option<Ast>, ParseError> {
        if self.peek().as_plain_word() != Some("let") {
            return Ok(None);
        }
        self.advance(); // consume 'let'
        let pattern = self.parse_pattern()?;
        // Expect '='
        match self.peek() {
            tok if tok.as_plain_word() == Some("=") => {
                self.advance();
            }
            _ => return Err(self.error("expected '=' after let pattern")),
        }
        // Allow the RHS to start on the next line: `let x =\n  expr`.
        self.skip_newlines();
        // The right-hand side of let is a computation context, so it parses
        // as a pipeline (with head-form dispatch) rather than a value atom.
        // Consume any `?` chain so `let x = a ? b` binds `Chain([a, b])` to x
        // rather than producing `Chain([Let{x=a}, b])`.
        let first = self.parse_pipeline()?;
        let mut chains = vec![first];
        loop {
            if self.peek() == &Token::Question {
                self.advance();
                chains.push(self.parse_pipeline()?);
            } else if self.peek() == &Token::Newline {
                let save = self.pos;
                self.advance();
                if self.peek() == &Token::Question {
                    self.advance();
                    chains.push(self.parse_pipeline()?);
                } else {
                    self.pos = save;
                    break;
                }
            } else {
                break;
            }
        }
        let mut value = if chains.len() == 1 {
            chains.remove(0)
        } else {
            Ast::Chain(chains)
        };
        // `let x = expr &` backgrounds the RHS so the variable binds a handle.
        if self.peek() == &Token::Ampersand {
            self.advance();
            value = Ast::Background(Box::new(value));
        }
        Ok(Some(Ast::Let {
            pattern,
            value: Box::new(value),
        }))
    }

    /// Parse a pattern (for binding LHS or lambda params)
    fn parse_pattern(&mut self) -> Result<Pattern, ParseError> {
        match self.peek() {
            Token::LBracket => self.parse_pattern_inner(),
            tok if tok.as_plain_word() == Some("_") => {
                self.advance();
                Ok(Pattern::Wildcard)
            }
            Token::Word(Word::Plain(name)) if is_reserved(name) => Err(self.error(format!(
                "'{name}' is a reserved keyword and cannot be used as a binding name"
            ))),
            Token::Word(Word::Plain(name)) if is_ident(name) => {
                let name = name.clone();
                self.advance();
                Ok(Pattern::Name(name))
            }
            _ => Err(self.error("expected pattern (IDENT or [destructuring])")),
        }
    }

    fn parse_pattern_inner(&mut self) -> Result<Pattern, ParseError> {
        self.expect(&Token::LBracket)?;

        // Empty list []
        if self.peek() == &Token::RBracket {
            self.advance();
            return Ok(Pattern::List {
                elems: vec![],
                rest: None,
            });
        }

        // Peek to determine if this is a map pattern: first_word ':' ...
        let is_map = {
            if matches!(self.peek(), Token::Word(Word::Plain(_))) {
                self.tokens
                    .get(self.pos + 1)
                    .map(|(t, _)| t == &Token::Colon)
                    .unwrap_or(false)
            } else {
                false
            }
        };

        if is_map {
            self.parse_map_pattern()
        } else {
            self.parse_list_pattern()
        }
    }

    fn parse_list_pattern(&mut self) -> Result<Pattern, ParseError> {
        let mut elems = Vec::new();
        let mut rest = None;

        self.parse_separated_until(Token::RBracket, "list pattern", |p| {
            // Rest pattern: ...name — terminal, must be the last element.
            if p.peek() == &Token::Spread {
                p.advance();
                let Token::Word(Word::Plain(name)) = p.peek().clone() else {
                    return Err(p.error("expected name after '...'"));
                };
                if !is_ident(&name) {
                    return Err(p.error("rest capture name must be an IDENT"));
                }
                p.advance();
                rest = Some(name);
                return Ok(SepFlow::Stop);
            }
            elems.push(p.parse_pattern()?);
            Ok(SepFlow::Cont)
        })?;

        Ok(Pattern::List { elems, rest })
    }

    fn parse_map_pattern(&mut self) -> Result<Pattern, ParseError> {
        let mut entries = Vec::new();

        self.parse_separated_until(Token::RBracket, "map pattern", |p| {
            let key = match p.peek().clone() {
                Token::Word(Word::Plain(k)) if is_ident(&k) => {
                    p.advance();
                    k
                }
                _ => return Err(p.error("expected IDENT key in map pattern")),
            };
            p.expect(&Token::Colon)?;
            let pat = p.parse_pattern()?;
            // Optional default: = atom
            let default = if p.peek().as_plain_word() == Some("=") {
                p.advance();
                Some(p.parse_atom()?)
            } else {
                None
            };
            entries.push((key, pat, default));
            Ok(SepFlow::Cont)
        })?;

        Ok(Pattern::Map(entries))
    }

    /// primary = word | block | list | map
    fn parse_primary(&mut self) -> Result<Ast, ParseError> {
        match self.peek() {
            Token::LBrace => self.parse_block(),
            Token::LBracket => self.parse_collection(),
            _ => self.parse_word(),
        }
    }

    /// atom = primary ('[' word ']')*
    fn parse_atom(&mut self) -> Result<Ast, ParseError> {
        let mut node = self.parse_primary()?;
        // Variable indexing ($name[key]) is resolved by the lexer via adjacency.
        // Postfix indexing here is uniform over any parsed atom.
        while self.peek() == &Token::LBracket && self.next_token_is_adjacent() {
            self.advance();
            let key = self.parse_word()?;
            self.expect(&Token::RBracket)?;
            node = Ast::Index {
                target: Box::new(node),
                keys: vec![key],
            };
        }
        Ok(flatten_index(node))
    }

    fn next_token_is_adjacent(&self) -> bool {
        let Some((_, prev_span)) = self.tokens.get(self.pos.saturating_sub(1)) else {
            return false;
        };
        let Some((_, next_span)) = self.tokens.get(self.pos) else {
            return false;
        };
        prev_span.byte.end == next_span.byte.start
    }

    fn parse_redirect(&mut self) -> Result<Ast, ParseError> {
        match self.peek().clone() {
            Token::Redirect {
                fd,
                kind,
                target_fd,
            } => {
                self.advance();
                let mode = match kind {
                    RedirectType::Write => RedirectMode::Write,
                    RedirectType::StreamWrite => RedirectMode::StreamWrite,
                    RedirectType::Append => RedirectMode::Append,
                    RedirectType::Read => RedirectMode::Read,
                };
                let default_fd = if mode == RedirectMode::Read { 0 } else { 1 };
                if let Some(tfd) = target_fd {
                    Ok(Ast::Redirect {
                        fd: fd.unwrap_or(default_fd),
                        mode,
                        target: RedirectTarget::Fd(tfd),
                    })
                } else {
                    let target_ast = self.parse_word()?;
                    Ok(Ast::Redirect {
                        fd: fd.unwrap_or(default_fd),
                        mode,
                        target: RedirectTarget::File(Box::new(target_ast)),
                    })
                }
            }
            _ => Err(self.error("expected redirect")),
        }
    }

    /// arg = atom | '...' atom
    fn parse_arg(&mut self) -> Result<Ast, ParseError> {
        if self.peek() == &Token::Spread {
            self.advance();
            let val = self.parse_atom()?;
            Ok(Ast::List(vec![ListElem::Spread(val)]))
        } else {
            self.parse_atom()
        }
    }

    fn parse_head(&mut self) -> Result<Head, ParseError> {
        if self.peek() == &Token::Caret {
            self.advance();
            return match self.peek().clone() {
                Token::Word(Word::Plain(name)) => {
                    self.advance();
                    Ok(Head::ExternalName(name))
                }
                Token::Word(Word::Slash(_)) | Token::Word(Word::Tilde(_)) => {
                    Err(self.error("'^' expects a bare command name, not a path"))
                }
                _ => Err(self.error("expected bare command name after '^'")),
            };
        }
        Ok(match self.parse_atom()? {
            Ast::Word(Word::Slash(s)) => Head::Path(s),
            Ast::Word(Word::Plain(s)) => Head::Bare(s),
            Ast::Word(Word::Tilde(path)) => Head::TildePath(path),
            other => Head::Value(Box::new(other)),
        })
    }

    /// command = head (arg | redir)*
    ///
    /// The returned `Ast::App` carries a `span` covering the head and all
    /// arguments — start is the byte where `parse_head` began, end is the
    /// byte just past the last consumed token.  Diagnostics on the resulting
    /// command (e.g. T0011 for a non-callable head) underline this whole
    /// range.
    fn parse_command(&mut self) -> Result<Ast, ParseError> {
        let start = self.span().byte;
        let head = self.parse_head()?;
        let mut args = Vec::new();

        while !self.at_cmd_end() {
            if matches!(self.peek(), Token::Redirect { .. }) {
                args.push(self.parse_redirect()?);
            } else {
                args.push(self.parse_arg()?);
            }
        }
        let span = start.join(self.prev_byte_span());

        if args.is_empty() {
            match head {
                Head::Value(value) => Ok(*value),
                Head::Bare(s) if is_value_literal_name(&s) || s == "return" => {
                    Ok(Ast::Word(Word::Plain(s)))
                }
                head => Ok(Ast::App { head, args: vec![], span }),
            }
        } else {
            Ok(Ast::App { head, args, span })
        }
    }

    /// Byte span of the most recently consumed token.  Used to compute the
    /// end of an `Ast::App` once all args have been parsed.  Falls back to
    /// the current-position span at the start of input.
    fn prev_byte_span(&self) -> crate::span::Span {
        self.tokens
            .get(self.pos.saturating_sub(1))
            .map(|(_, s)| s.byte)
            .unwrap_or_else(|| self.span().byte)
    }

    /// Check if we've reached the end of a command's argument list.
    fn at_cmd_end(&self) -> bool {
        self.at_stmt_end() || self.peek() == &Token::Question || self.peek() == &Token::Pipe
    }

    /// word = WORD | QUOTED | INTERP | deref | force | expr-block
    fn parse_word(&mut self) -> Result<Ast, ParseError> {
        let span = self.span();
        match self.peek().clone() {
            Token::Word(Word::Plain(s)) => {
                self.advance();
                Ok(Ast::Word(Word::Plain(s)))
            }
            Token::Word(Word::Slash(s)) => {
                self.advance();
                Ok(Ast::Word(Word::Slash(s)))
            }
            Token::Word(Word::Tilde(path)) => {
                self.advance();
                Ok(Ast::Word(Word::Tilde(path)))
            }
            Token::SingleQuoted(s) => {
                self.advance();
                Ok(Ast::Literal(s))
            }
            Token::DoubleQuoted(parts) => {
                self.advance();
                self.parse_interpolation_parts(&parts)
            }
            Token::Deref(part) => {
                self.advance();
                match part {
                    StringPart::Variable(name) => Ok(Ast::Variable(name)),
                    StringPart::Index(name, keys) => Ok(Ast::Index {
                        target: Box::new(Ast::Variable(name)),
                        keys: parse_raw_keys(&keys)?,
                    }),
                    _ => Err(self.error("unexpected deref form")),
                }
            }
            Token::Expr(source) => {
                self.advance();
                Ok(Ast::Expr(Box::new(parse_raw_expr(&source)?)))
            }
            Token::Dollar => {
                self.advance();
                Err(self.error("expected dereference after '$' (e.g. $name, $(name), or $[...])"))
            }
            Token::Caret => Err(self.error("'^name' is only valid in command-head position")),
            Token::Bang => {
                self.advance();
                self.parse_bang()
            }
            Token::Tag(label) => {
                self.advance();
                let payload = if self.at_tag_payload_end() {
                    None
                } else {
                    Some(Box::new(self.parse_atom()?))
                };
                Ok(Ast::Tag { label, payload })
            }
            _ => Err(ParseError {
                message: format!("unexpected token: {}", self.peek()),
                line: span.line,
                col: span.col,
            }),
        }
    }

    /// True at boundaries where a `.tag` should remain nullary instead of
    /// greedily absorbing the next atom as a payload — separator and closer
    /// tokens, basically.  In atom contexts (list elements, argument lists,
    /// command heads) anything that looks like a value following a tag is
    /// taken as the payload; the writer picks separators (`,` in lists,
    /// newline in stages) to terminate.
    fn at_tag_payload_end(&self) -> bool {
        matches!(
            self.peek(),
            Token::Newline
                | Token::Eof
                | Token::RBrace
                | Token::RBracket
                | Token::RParen
                | Token::Pipe
                | Token::Question
                | Token::Ampersand
                | Token::Comma
                | Token::Colon
                | Token::Spread
                | Token::Redirect { .. }
        )
    }

    /// force = '!' primary
    ///
    /// Postfix `[k]` indexing is intentionally left to the outer `parse_atom`
    /// so that `!{cmd}[k]` parses as `Index(Force(Block), k)` — force first,
    /// then index — rather than the incorrect `Force(Index(Block, k))`.
    fn parse_bang(&mut self) -> Result<Ast, ParseError> {
        Ok(Ast::Force(Box::new(self.parse_primary()?)))
    }

    /// Parse a block: { program } or lambda: { |params| body }
    fn parse_block(&mut self) -> Result<Ast, ParseError> {
        self.expect(&Token::LBrace)?;
        if self.peek() == &Token::Pipe {
            self.advance(); // consume opening |
            let mut params = Vec::new();
            while self.peek() != &Token::Pipe {
                params.push(self.parse_pattern()?);
            }
            self.expect(&Token::Pipe)?;
            // §4.6 Currying: { |x y z| body } → { |x| { |y| { |z| body } } }
            if params.is_empty() {
                return Err(
                    self.error("lambda requires at least one parameter — use { } for thunks")
                );
            }
            let body = self.parse_program()?;
            self.expect(&Token::RBrace)?;
            if params.len() == 1 {
                Ok(Ast::Lambda {
                    param: params.remove(0),
                    body,
                })
            } else {
                let mut result = Ast::Lambda {
                    param: params.pop().unwrap(),
                    body,
                };
                while let Some(p) = params.pop() {
                    result = Ast::Lambda {
                        param: p,
                        body: vec![result],
                    };
                }
                Ok(result)
            }
        } else {
            let body = self.parse_program()?;
            self.expect(&Token::RBrace)?;
            Ok(Ast::Block(body))
        }
    }

    /// Parse a collection: list or map.
    fn parse_collection(&mut self) -> Result<Ast, ParseError> {
        self.expect(&Token::LBracket)?;

        // Empty list: []
        if self.peek() == &Token::RBracket {
            self.advance();
            return Ok(Ast::List(vec![]));
        }

        // Empty map: [:]
        if self.peek() == &Token::Colon
            && self.tokens.get(self.pos + 1).map(|(t, _)| t) == Some(&Token::RBracket)
        {
            self.advance(); // :
            self.advance(); // ]
            return Ok(Ast::Map(vec![]));
        }

        // Determine if this is a map or list by looking for bare_word ':' pattern
        let is_map = self.is_map_ahead();

        if is_map {
            self.parse_map_entries()
        } else {
            self.parse_list_elems()
        }
    }

    fn is_map_ahead(&self) -> bool {
        // A map starts with either `bare_word :` or `...`
        // Check first non-spread element
        let mut i = self.pos;
        // Skip spread entries
        while matches!(self.tokens.get(i).map(|(t, _)| t), Some(Token::Spread)) {
            // ...$expr, skip to after comma
            i += 1;
            while !matches!(
                self.tokens.get(i).map(|(t, _)| t),
                Some(Token::Comma | Token::RBracket) | None
            ) {
                i += 1;
            }
            if matches!(self.tokens.get(i).map(|(t, _)| t), Some(Token::Comma)) {
                i += 1;
            }
        }

        if let Some(
            Token::Word(Word::Plain(_)) | Token::SingleQuoted(_) | Token::Deref(_) | Token::Tag(_),
        ) = self.tokens.get(i).map(|(t, _)| t)
        {
            matches!(self.tokens.get(i + 1).map(|(t, _)| t), Some(Token::Colon))
        } else {
            false
        }
    }

    /// elem = atom | '...' atom
    fn parse_list_elems(&mut self) -> Result<Ast, ParseError> {
        let mut elems = Vec::new();

        self.parse_separated_until(Token::RBracket, "list", |p| {
            if p.peek() == &Token::Spread {
                p.advance();
                elems.push(ListElem::Spread(p.parse_atom()?));
            } else {
                elems.push(ListElem::Single(p.parse_atom()?));
            }
            Ok(SepFlow::Cont)
        })?;

        Ok(Ast::List(elems))
    }

    fn parse_map_entries(&mut self) -> Result<Ast, ParseError> {
        let mut entries = Vec::new();
        // Track the alphabet of static keys (literal `name` vs tag `.name`)
        // so that mixing them in one literal is rejected at parse time.
        // Dynamic `$var` keys do not contribute to the alphabet decision.
        let mut alphabet: Option<KeyAlphabet> = None;

        self.parse_separated_until(Token::RBracket, "map", |p| {
            if p.peek() == &Token::Spread {
                p.advance();
                entries.push(MapEntry::Spread(p.parse_atom()?));
                return Ok(SepFlow::Cont);
            }
            // mapkey = IDENT | QUOTED | deref | tag
            let (key_ast, this_alphabet) = match p.peek().clone() {
                Token::Word(Word::Plain(k)) if is_ident(&k) => {
                    p.advance();
                    (Ast::Literal(k), Some(KeyAlphabet::Bare))
                }
                Token::SingleQuoted(k) => {
                    p.advance();
                    (Ast::Literal(k), Some(KeyAlphabet::Bare))
                }
                Token::Deref(StringPart::Variable(k)) => {
                    p.advance();
                    (Ast::Variable(k), None)
                }
                Token::Tag(label) => {
                    p.advance();
                    (Ast::Literal(format!(".{label}")), Some(KeyAlphabet::Tag))
                }
                Token::Word(Word::Plain(k)) if k.parse::<f64>().is_ok() => {
                    return Err(p.error(
                        "map keys must be identifiers or quoted strings, not numbers; use '0': val",
                    ));
                }
                _ => return Err(p.error("expected map key: name, 'quoted', .tag, or $var")),
            };
            if let Some(a) = this_alphabet {
                match alphabet {
                    None => alphabet = Some(a),
                    Some(prev) if prev != a => {
                        return Err(p.error(
                            "record literal mixes bare and tag keys — pick one alphabet",
                        ));
                    }
                    Some(_) => {}
                }
            }
            p.expect(&Token::Colon)?;
            entries.push(MapEntry::Entry(key_ast, p.parse_atom()?));
            Ok(SepFlow::Cont)
        })?;

        Ok(Ast::Map(entries))
    }

    /// Parse interpolation parts from a double-quoted string.
    fn parse_interpolation_parts(&mut self, parts: &[StringPart]) -> Result<Ast, ParseError> {
        if parts.len() == 1
            && let StringPart::Literal(s) = &parts[0]
        {
            return Ok(Ast::Literal(s.clone()));
        }

        let mut ast_parts = Vec::new();
        for part in parts {
            match part {
                StringPart::Literal(s) => {
                    ast_parts.push(Ast::Literal(s.clone()));
                }
                StringPart::Variable(name) => {
                    ast_parts.push(Ast::Variable(name.clone()));
                }
                StringPart::Force(source) => {
                    let stmts = parse(source)?;
                    let inner = if stmts.len() == 1 {
                        stmts.into_iter().next().unwrap()
                    } else {
                        Ast::Block(stmts)
                    };
                    ast_parts.push(Ast::Force(Box::new(inner)));
                }
                StringPart::Expr(source) => {
                    ast_parts.push(Ast::Expr(Box::new(parse_raw_expr(source)?)));
                }
                StringPart::Index(name, keys) => {
                    ast_parts.push(Ast::Index {
                        target: Box::new(Ast::Variable(name.clone())),
                        keys: parse_raw_keys(keys)?,
                    });
                }
            }
        }

        Ok(Ast::Interpolation(ast_parts))
    }

    // ── Arithmetic (Pratt parser) ────────────────────────────────────

    fn parse_expr_prec(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut left = self.parse_expr_atom()?;

        while let Some((op, prec)) = self.peek_expr_op() {
            if prec < min_prec {
                break;
            }
            self.advance(); // consume operator token
            let right = self.parse_expr_prec(prec + 1)?;
            left = match op {
                InfixOp::And => Expr::And(Box::new(left), Box::new(right)),
                InfixOp::Or => Expr::Or(Box::new(left), Box::new(right)),
                InfixOp::Op(o) => Expr::BinOp(Box::new(left), o, Box::new(right)),
            };
        }

        Ok(left)
    }

    fn parse_expr_atom(&mut self) -> Result<Expr, ParseError> {
        match self.peek().clone() {
            Token::LParen => {
                self.advance();
                let expr = self.parse_expr_prec(0)?;
                self.expect(&Token::RParen)?;
                Ok(expr)
            }
            Token::Deref(part) => {
                let result = match part {
                    StringPart::Variable(name) => Ok(Expr::Var(name)),
                    StringPart::Index(name, keys) => Ok(Expr::Index(name, parse_raw_keys(&keys)?)),
                    _ => Err(self.error("unexpected deref in expression")),
                };
                self.advance();
                result
            }
            Token::Bang => {
                self.advance();
                let inner = self.parse_bang()?;
                match inner {
                    Ast::Force(body) => Ok(Expr::Force(body)),
                    _ => Err(self.error("expected block or variable after ! in expression")),
                }
            }
            Token::Word(Word::Plain(s)) if s == "-" => {
                self.advance();
                let inner = self.parse_expr_atom()?;
                // Fold the negation into literal atoms so the unary-minus
                // zero does not force a spurious `Float` side into the
                // binary operator's type check (e.g. `-1.5` stays `Float`,
                // `-$x` stays the operand's numeric type via `Int` zero).
                Ok(match inner {
                    Expr::Integer(n) => Expr::Integer(-n),
                    Expr::Number(n) => Expr::Number(-n),
                    other => Expr::BinOp(Box::new(Expr::Integer(0)), ExprOp::Sub, Box::new(other)),
                })
            }
            Token::Word(Word::Plain(s)) if s == "not" => {
                self.advance();
                // `not` is a prefix operator binding tighter than any binary
                // op; recursing into `parse_expr_atom` would bind too loose
                // (disallowing e.g. `not $x == 0`), so delegate to the Pratt
                // loop at the highest precedence available.
                let inner = self.parse_expr_atom()?;
                Ok(Expr::Not(Box::new(inner)))
            }
            Token::Word(Word::Plain(s)) if s == "true" => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            Token::Word(Word::Plain(s)) if s == "false" => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            Token::Word(Word::Plain(s)) => {
                if let Ok(n) = s.parse::<i64>() {
                    self.advance();
                    Ok(Expr::Integer(n))
                } else if let Ok(n) = s.parse::<f64>() {
                    self.advance();
                    Ok(Expr::Number(n))
                } else {
                    Err(self.error(format!("expected expression atom, found '{s}'")))
                }
            }
            _ => Err(self.error(format!("unexpected token in expression: {}", self.peek()))),
        }
    }

    fn peek_expr_op(&self) -> Option<(InfixOp, u8)> {
        // Precedence (low → high): ||=1, &&=2, comparison=3, add/sub=4,
        // mul/div/mod=5.  Unary `-` / `not` bind tighter than any binary
        // and are handled in `parse_expr_atom`.
        match self.peek() {
            Token::Word(Word::Plain(s)) => match s.as_str() {
                "||" => Some((InfixOp::Or, 1)),
                "&&" => Some((InfixOp::And, 2)),
                "+" => Some((InfixOp::Op(ExprOp::Add), 4)),
                "-" => Some((InfixOp::Op(ExprOp::Sub), 4)),
                "*" => Some((InfixOp::Op(ExprOp::Mul), 5)),
                "/" => Some((InfixOp::Op(ExprOp::Div), 5)),
                "%" => Some((InfixOp::Op(ExprOp::Mod), 5)),
                "==" => Some((InfixOp::Op(ExprOp::Eq), 3)),
                "!=" => Some((InfixOp::Op(ExprOp::Ne), 3)),
                "<" => Some((InfixOp::Op(ExprOp::Lt), 3)),
                ">" => Some((InfixOp::Op(ExprOp::Gt), 3)),
                "<=" => Some((InfixOp::Op(ExprOp::Le), 3)),
                ">=" => Some((InfixOp::Op(ExprOp::Ge), 3)),
                _ => None,
            },
            Token::Word(Word::Slash(s)) if s == "/" => Some((InfixOp::Op(ExprOp::Div), 5)),
            // < and > are lexed as Redirect tokens, handle them as expr operators
            Token::Redirect {
                fd: None,
                kind: lexer::RedirectType::Read,
                target_fd: None,
            } => Some((InfixOp::Op(ExprOp::Lt), 3)),
            Token::Redirect {
                fd: None,
                kind: lexer::RedirectType::Write,
                target_fd: None,
            } => Some((InfixOp::Op(ExprOp::Gt), 3)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum InfixOp {
    Op(ExprOp),
    And,
    Or,
}

/// Parse raw key strings into Ast nodes (used for $name[key] index keys).
fn parse_raw_keys(keys: &[String]) -> Result<Vec<Ast>, ParseError> {
    keys.iter()
        .map(|k| {
            let toks = lexer::lex(k)?;
            let mut sub = Parser::new(toks);
            sub.parse_word()
        })
        .collect()
}

/// Parse raw source as an expression-block body.  The inner re-lex
/// produces single-character `Token::Ampersand` / `Token::Pipe` even for
/// `&&` / `||`; fuse adjacent pairs into `Token::Word(Word::Plain("&&" | "||"))` so
/// the Pratt parser can treat them like the other multi-char operators.
fn parse_raw_expr(source: &str) -> Result<Expr, ParseError> {
    let tokens = lexer::lex(source)?;
    let tokens = fuse_logical_pairs(tokens);
    let mut sub = Parser::new(tokens);
    sub.parse_expr_prec(0)
}

fn fuse_logical_pairs(tokens: Vec<(Token, Span)>) -> Vec<(Token, Span)> {
    let mut out: Vec<(Token, Span)> = Vec::with_capacity(tokens.len());
    let mut iter = tokens.into_iter().peekable();
    while let Some((tok, span)) = iter.next() {
        let fused = match (&tok, iter.peek().map(|(t, _)| t)) {
            (Token::Ampersand, Some(Token::Ampersand)) => Some("&&"),
            (Token::Pipe, Some(Token::Pipe)) => Some("||"),
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

/// Keywords and value literals that may not be used as binding names.
fn is_reserved(s: &str) -> bool {
    matches!(
        s,
        "if" | "elsif" | "else" | "let" | "return" | "true" | "false" | "unit" | "case"
    )
}

/// Distinguishes the two record-key alphabets so mixed-alphabet literals
/// (`[host: ..., .dev: ...]`) are rejected at parse time.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyAlphabet {
    Bare,
    Tag,
}

/// IDENT = [a-zA-Z_][a-zA-Z0-9_-]*
fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Flatten nested single-key `Index { Index { x, [k1] }, [k2] }` into `Index { x, [k1, k2] }`.
fn flatten_index(node: Ast) -> Ast {
    match node {
        Ast::Index { target, keys } => match *target {
            Ast::Index {
                target: inner,
                keys: mut inner_keys,
            } => {
                inner_keys.extend(keys);
                flatten_index(Ast::Index {
                    target: inner,
                    keys: inner_keys,
                })
            }
            other => Ast::Index {
                target: Box::new(other),
                keys,
            },
        },
        other => other,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::tilde::TildePath;

    fn plain(s: &str) -> Ast {
        Ast::Word(Word::Plain(s.into()))
    }

    fn tilde_word(path: TildePath) -> Ast {
        Ast::Word(Word::Tilde(path))
    }

    fn bare_head(s: &str) -> Head {
        Head::Bare(s.into())
    }

    fn path_head(s: &str) -> Head {
        Head::Path(s.into())
    }

    fn external_head(s: &str) -> Head {
        Head::ExternalName(s.into())
    }

    fn value_head(ast: Ast) -> Head {
        Head::Value(Box::new(ast))
    }

    /// Construct `Ast::App` with a zero-width span — the parser tests do not
    /// inspect spans, and `strip_one` already drops them, so this keeps the
    /// expected-AST literals readable.
    fn app(head: Head, args: Vec<Ast>) -> Ast {
        Ast::App {
            head,
            args,
            span: crate::span::Span::point(crate::source::FileId::DUMMY, 0),
        }
    }

    /// Strip Pos nodes and unwrap lone-atom Commands for test assertions.
    fn strip_pos(ast: Vec<Ast>) -> Vec<Ast> {
        ast.into_iter()
            .filter(|n| !matches!(n, Ast::Pos(_)))
            .map(strip_one)
            .collect()
    }

    fn strip_head(head: Head) -> Head {
        match head {
            Head::Value(ast) => Head::Value(Box::new(strip_one(*ast))),
            other => other,
        }
    }

    fn strip_one(n: Ast) -> Ast {
        match n {
            // Unwrap Command { name: X, args: [] } → X (lone atom in command position)
            Ast::App { head, args, .. } if args.is_empty() => match head {
                Head::Bare(s) => plain(&s),
                Head::Path(s) => Ast::Word(Word::Slash(s)),
                Head::TildePath(path) => tilde_word(path),
                Head::Value(ast) => strip_one(*ast),
                Head::ExternalName(s) => app(Head::ExternalName(s), vec![]),
            },
            Ast::Return(None) => Ast::Return(None),
            Ast::Return(Some(value)) => Ast::Return(Some(Box::new(strip_one(*value)))),
            Ast::App { head, args, .. } => app(strip_head(head), strip_pos(args)),
            Ast::Block(body) => Ast::Block(strip_pos(body)),
            Ast::Lambda { param, body } => Ast::Lambda {
                param,
                body: strip_pos(body),
            },
            Ast::Pipeline(stages) => Ast::Pipeline(strip_pos(stages)),
            Ast::Chain(parts) => Ast::Chain(strip_pos(parts)),
            Ast::Background(inner) => Ast::Background(Box::new(strip_one(*inner))),
            Ast::Force(inner) => Ast::Force(Box::new(strip_one(*inner))),
            Ast::Let { pattern, value } => Ast::Let {
                pattern,
                value: Box::new(strip_one(*value)),
            },
            other => other,
        }
    }

    #[test]
    fn parse_simple_command() {
        let ast = strip_pos(parse("echo hello").unwrap());
        assert_eq!(ast, vec![app(bare_head("echo"), vec![plain("hello")])]);
    }

    #[test]
    fn parse_variable() {
        let ast = strip_pos(parse("echo $x").unwrap());
        assert_eq!(
            ast,
            vec![app(bare_head("echo"), vec![Ast::Variable("x".into())])]
        );
    }

    #[test]
    fn parse_explicit_value_head_application() {
        let ast = strip_pos(parse("$map $upper ['a']").unwrap());
        assert_eq!(
            ast,
            vec![app(
                value_head(Ast::Variable("map".into())),
                vec![
                    Ast::Variable("upper".into()),
                    Ast::List(vec![ListElem::Single(Ast::Literal("a".into()))]),
                ],
            )]
        );
    }

    #[test]
    fn parse_explicit_value_head_without_args_remains_value() {
        let ast = strip_pos(parse("$map").unwrap());
        assert_eq!(ast, vec![Ast::Variable("map".into())]);
    }

    #[test]
    fn parse_external_name_head_application() {
        let ast = strip_pos(parse("^git status").unwrap());
        assert_eq!(
            ast,
            vec![app(external_head("git"), vec![plain("status")])]
        );
    }

    #[test]
    fn parse_external_name_head_without_args() {
        let ast = parse("^git").unwrap();
        match ast.as_slice() {
            [Ast::Pos(_), Ast::App { head, args, .. }] => {
                assert!(args.is_empty());
                assert_eq!(head, &external_head("git"));
            }
            _ => panic!("expected zero-arg external-name app, got {ast:?}"),
        }
    }

    #[test]
    fn parse_external_name_rejected_in_arg_position() {
        assert!(parse("echo ^git").is_err());
    }

    #[test]
    fn parse_binding() {
        let ast = strip_pos(parse("let x = hello").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Pattern::Name("x".into()),
                value: Box::new(plain("hello")),
            }]
        );
    }

    #[test]
    fn parse_pipeline() {
        let ast = strip_pos(parse("echo hello | upper").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Pipeline(vec![
                app(bare_head("echo"), vec![plain("hello")]),
                plain("upper"),
            ])]
        );
    }

    #[test]
    fn parse_pipeline_quoted_literal_stage() {
        let ast = strip_pos(parse("'abc' | blah").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Pipeline(vec![
                Ast::Literal("abc".into()),
                plain("blah"),
            ])]
        );
    }

    #[test]
    fn parse_chain() {
        let ast = strip_pos(parse("return true ? echo yes").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Chain(vec![
                Ast::Return(Some(Box::new(plain("true")))),
                app(bare_head("echo"), vec![plain("yes")]),
            ])]
        );
    }

    #[test]
    fn parse_block_stmt() {
        let ast = strip_pos(parse("{ echo hello }").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Block(vec![app(
                bare_head("echo"),
                vec![plain("hello")]
            )])]
        );
    }

    #[test]
    fn parse_lambda_arg() {
        let ast = strip_pos(parse("echo { |x| echo $x }").unwrap());
        assert_eq!(
            ast,
            vec![app(
                bare_head("echo"),
                vec![Ast::Lambda {
                    param: Pattern::Name("x".into()),
                    body: vec![app(
                        bare_head("echo"),
                        vec![Ast::Variable("x".into())]
                    )],
                }],
            )]
        );
    }

    #[test]
    fn parse_return_stage() {
        let ast = strip_pos(parse("return $x").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Return(Some(Box::new(Ast::Variable("x".into()))))]
        );
    }

    #[test]
    fn parse_return_unit_stage() {
        let ast = strip_pos(parse("return").unwrap());
        assert_eq!(ast, vec![Ast::Return(None)]);
    }

    #[test]
    fn parse_return_force_argument() {
        let ast = strip_pos(parse("return !{hostname}").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Return(Some(Box::new(Ast::Force(Box::new(
                Ast::Block(vec![plain("hostname")])
            )))))]
        );
    }

    #[test]
    fn parse_list() {
        let ast = strip_pos(parse("return [a, b, c]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Return(Some(Box::new(Ast::List(vec![
                ListElem::Single(plain("a")),
                ListElem::Single(plain("b")),
                ListElem::Single(plain("c")),
            ]))))]
        );
    }

    #[test]
    fn parse_map() {
        let ast = strip_pos(parse("return [host: localhost, port: 8080]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Return(Some(Box::new(Ast::Map(vec![
                MapEntry::Entry(Ast::Literal("host".into()), plain("localhost")),
                MapEntry::Entry(Ast::Literal("port".into()), plain("8080")),
            ]))))]
        );
    }

    #[test]
    fn parse_command_substitution() {
        let ast = strip_pos(parse("let name = !{hostname}").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Pattern::Name("name".into()),
                value: Box::new(Ast::Force(Box::new(Ast::Block(vec![plain("hostname")])))),
            }]
        );
    }

    #[test]
    fn parse_arithmetic() {
        let ast = strip_pos(parse("$[2 + 3]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Expr(Box::new(Expr::BinOp(
                Box::new(Expr::Integer(2)),
                ExprOp::Add,
                Box::new(Expr::Integer(3)),
            )))]
        );
    }

    #[test]
    fn parse_arithmetic_precedence() {
        let ast = strip_pos(parse("$[2 + 3 * 4]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Expr(Box::new(Expr::BinOp(
                Box::new(Expr::Integer(2)),
                ExprOp::Add,
                Box::new(Expr::BinOp(
                    Box::new(Expr::Integer(3)),
                    ExprOp::Mul,
                    Box::new(Expr::Integer(4)),
                )),
            )))]
        );
    }

    #[test]
    fn parse_index() {
        let ast = strip_pos(parse("echo $items[0]").unwrap());
        assert_eq!(
            ast,
            vec![app(
                bare_head("echo"),
                vec![Ast::Index {
                    target: Box::new(Ast::Variable("items".into())),
                    keys: vec![plain("0")],
                }],
            )]
        );
    }

    #[test]
    fn parse_postfix_index_on_list_literal() {
        let ast = strip_pos(parse("return ['a'][0]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Return(Some(Box::new(Ast::Index {
                target: Box::new(Ast::List(vec![ListElem::Single(Ast::Literal("a".into()))])),
                keys: vec![plain("0")],
            })))]
        );
    }

    #[test]
    fn parse_interpolation() {
        let ast = strip_pos(parse("echo \"hello $name\"").unwrap());
        assert_eq!(
            ast,
            vec![app(
                bare_head("echo"),
                vec![Ast::Interpolation(vec![
                    Ast::Literal("hello ".into()),
                    Ast::Variable("name".into()),
                ])],
            )]
        );
    }

    #[test]
    fn parse_multiple_stmts() {
        let ast = strip_pos(parse("x = 5\necho $x").unwrap());
        assert_eq!(ast.len(), 2);
    }

    #[test]
    fn parse_destructuring() {
        let ast = strip_pos(parse("let [first, second] = [a, b]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Pattern::List {
                    elems: vec![
                        Pattern::Name("first".into()),
                        Pattern::Name("second".into()),
                    ],
                    rest: None,
                },
                value: Box::new(Ast::List(vec![
                    ListElem::Single(plain("a")),
                    ListElem::Single(plain("b")),
                ])),
            }]
        );
    }

    #[test]
    fn parse_rest_pattern() {
        let ast = strip_pos(parse("let [head, ...rest] = $list").unwrap());
        match &ast[0] {
            Ast::Let { pattern, .. } => {
                assert_eq!(
                    *pattern,
                    Pattern::List {
                        elems: vec![Pattern::Name("head".into())],
                        rest: Some("rest".into()),
                    }
                );
            }
            _ => panic!("expected binding"),
        }
    }

    #[test]
    fn parse_wildcard_pattern() {
        let ast = strip_pos(parse("let _ = hello").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Pattern::Wildcard,
                value: Box::new(plain("hello")),
            }]
        );
    }

    #[test]
    fn parse_wildcard_in_destructuring() {
        let ast = strip_pos(parse("let [_, x] = [a, b]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Pattern::List {
                    elems: vec![Pattern::Wildcard, Pattern::Name("x".into())],
                    rest: None,
                },
                value: Box::new(Ast::List(vec![
                    ListElem::Single(plain("a")),
                    ListElem::Single(plain("b")),
                ])),
            }]
        );
    }

    #[test]
    fn parse_command_with_lambda_arg() {
        let ast = strip_pos(parse("for $items { |x| echo $x }").unwrap());
        match &ast[0] {
            Ast::App { head, args, .. } => {
                assert_eq!(head, &bare_head("for"));
                assert_eq!(args.len(), 2); // $items and the lambda
                assert!(matches!(args[0], Ast::Variable(_)));
                assert!(matches!(args[1], Ast::Lambda { .. }));
            }
            _ => panic!("expected command"),
        }
    }

    #[test]
    fn parse_spread_in_list() {
        let ast = strip_pos(parse("return [...$a, b]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Return(Some(Box::new(Ast::List(vec![
                ListElem::Spread(Ast::Variable("a".into())),
                ListElem::Single(plain("b")),
            ]))))]
        );
    }

    #[test]
    fn parse_empty_map() {
        let ast = strip_pos(parse("[:]").unwrap());
        assert_eq!(ast, vec![Ast::Map(vec![])]);
    }

    #[test]
    fn parse_empty_list() {
        let ast = strip_pos(parse("[]").unwrap());
        assert_eq!(ast, vec![Ast::List(vec![])]);
    }

    #[test]
    fn parse_map_with_blocks() {
        // First test: multiline map standalone parses as a value form.
        let src1 = "[\n    quit: { echo q },\n    help: { echo h },\n]";
        let ast1 = strip_pos(parse(src1).unwrap());
        assert_eq!(ast1.len(), 1);
        assert!(matches!(&ast1[0], Ast::Map(_)));

        // Second test: multiline map as command argument
        let src = "dispatch $action [\n    quit: { echo quitting },\n    help: { echo help },\n    _: { echo unknown },\n]";
        let ast = strip_pos(parse(src).unwrap());
        match &ast[0] {
            Ast::App { head, args, .. } => {
                assert_eq!(head, &bare_head("dispatch"));
                assert_eq!(args.len(), 2);
                assert!(matches!(&args[1], Ast::Map(_)));
            }
            _ => panic!("expected command, got {:?}", ast[0]),
        }
    }

    #[test]
    fn parse_newline_separates_statements_in_block_inside_map() {
        let src = "return [prompt: { let x = hi\nreturn \"$x> \" }]";
        let ast = strip_pos(parse(src).unwrap());
        match &ast[0] {
            Ast::Return(Some(val)) => match val.as_ref() {
                Ast::Map(entries) => {
                    assert_eq!(entries.len(), 1);
                    let MapEntry::Entry(_, value) = &entries[0] else {
                        panic!("expected map entry");
                    };
                    let Ast::Block(stmts) = value else {
                        panic!("expected block in prompt entry");
                    };
                    let stmts: Vec<&Ast> =
                        stmts.iter().filter(|n| !matches!(n, Ast::Pos(_))).collect();
                    assert!(matches!(stmts[0], Ast::Let { .. }));
                    assert!(matches!(stmts[1], Ast::Return(Some(_))));
                }
                _ => panic!("expected map"),
            },
            _ => panic!("expected return map"),
        }
    }

    #[test]
    fn parse_if_else_blocks_across_newline_with_explicit_else() {
        let src = "return [aliases: [ls: { |args| if $is-mac { echo a }\nelse { echo b } }]]";
        let ast = strip_pos(parse(src).unwrap());
        let Ast::Return(Some(val)) = &ast[0] else {
            panic!("expected return");
        };
        let Ast::Map(entries) = val.as_ref() else {
            panic!("expected map");
        };
        let MapEntry::Entry(_, aliases_val) = &entries[0] else {
            panic!("expected aliases entry");
        };
        let Ast::Map(alias_entries) = aliases_val else {
            panic!("expected aliases map");
        };
        let MapEntry::Entry(_, ls_val) = &alias_entries[0] else {
            panic!("expected ls entry");
        };
        let Ast::Lambda { body, .. } = ls_val else {
            panic!("expected lambda");
        };
        let body: Vec<&Ast> = body.iter().filter(|n| !matches!(n, Ast::Pos(_))).collect();
        assert_eq!(body.len(), 1);
        let Ast::If {
            cond,
            then,
            elsif,
            else_,
        } = body[0]
        else {
            panic!("expected Ast::If, got {:?}", body[0]);
        };
        assert!(matches!(cond.as_ref(), Ast::Variable(s) if s == "is-mac"));
        assert!(matches!(then.as_ref(), Ast::Block(_)));
        assert!(elsif.is_empty());
        assert!(matches!(
            else_.as_ref().map(|b| b.as_ref()),
            Some(Ast::Block(_))
        ));
    }

    #[test]
    fn parse_redirect() {
        let ast = strip_pos(parse("echo hello > out.txt").unwrap());
        match &ast[0] {
            Ast::App { args, .. } => {
                assert!(args.len() >= 2);
                assert!(matches!(args.last(), Some(Ast::Redirect { .. })));
            }
            _ => panic!("expected command"),
        }
    }

    #[test]
    fn parse_tilde() {
        let ast = strip_pos(parse("~").unwrap());
        assert_eq!(
            ast,
            vec![tilde_word(TildePath {
                user: None,
                suffix: None,
            })]
        );
    }

    #[test]
    fn parse_tilde_as_command_arg() {
        // ~ as an argument to a command should parse as Tilde, not be wrapped in Command
        let ast = strip_pos(parse("cd ~").unwrap());
        assert_eq!(
            ast,
            vec![app(
                bare_head("cd"),
                vec![tilde_word(TildePath {
                    user: None,
                    suffix: None,
                })],
            )]
        );
    }

    #[test]
    fn parse_tilde_user() {
        let ast = strip_pos(parse("~root").unwrap());
        assert_eq!(
            ast,
            vec![tilde_word(TildePath {
                user: Some("root".into()),
                suffix: None,
            })]
        );
    }

    #[test]
    fn parse_tilde_path_suffix() {
        let ast = strip_pos(parse("~/foo/bar").unwrap());
        assert_eq!(
            ast,
            vec![tilde_word(TildePath {
                user: None,
                suffix: Some("/foo/bar".into()),
            })]
        );
    }

    #[test]
    fn parse_tilde_path_command_head_without_args() {
        let ast = parse("~/.local/bin/claude").unwrap();
        match ast.as_slice() {
            [Ast::Pos(_), Ast::App { head, args, .. }] => {
                assert!(args.is_empty());
                assert_eq!(
                    head,
                    &Head::TildePath(TildePath {
                        user: None,
                        suffix: Some("/.local/bin/claude".into()),
                    })
                );
            }
            _ => panic!("expected zero-arg command app, got {ast:?}"),
        }
    }

    #[test]
    fn parse_list_in_command_position_remains_value() {
        let ast = strip_pos(parse("[1,2]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::List(vec![
                ListElem::Single(plain("1")),
                ListElem::Single(plain("2")),
            ])]
        );
    }

    #[test]
    fn parse_external_name_rejected_for_path_head() {
        assert!(parse("^./script").is_err());
    }

    #[test]
    fn parse_literal_path_head_without_args() {
        let ast = parse("./script").unwrap();
        match ast.as_slice() {
            [Ast::Pos(_), Ast::App { head, args, .. }] => {
                assert!(args.is_empty());
                assert_eq!(head, &path_head("./script"));
            }
            _ => panic!("expected zero-arg path app, got {ast:?}"),
        }
    }

    #[test]
    fn parse_tilde_with_space_is_bare() {
        // "~ foo" — space between ~ and word means ~ is standalone, foo is a separate arg
        let ast = strip_pos(parse("echo ~ foo").unwrap());
        assert_eq!(
            ast,
            vec![app(
                bare_head("echo"),
                vec![
                    tilde_word(TildePath {
                        user: None,
                        suffix: None,
                    }),
                    plain("foo"),
                ],
            )]
        );
    }

    #[test]
    fn parse_nested_blocks() {
        let ast = strip_pos(parse("{ { echo inner } }").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Block(vec![Ast::Block(vec![app(
                bare_head("echo"),
                vec![plain("inner")]
            )])])]
        );
    }

    #[test]
    fn parse_force_stmt_still_allowed() {
        let ast = strip_pos(parse("!{echo hello}").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Force(Box::new(Ast::Block(vec![app(
                bare_head("echo"),
                vec![plain("hello")]
            )])))]
        );
    }

    #[test]
    fn bare_bang_is_not_a_literal_word() {
        assert!(parse("echo !").is_err());
    }

    #[test]
    fn let_rhs_on_next_line() {
        // `let x =\n expr` — newline between = and RHS is allowed.
        let ast = strip_pos(parse("let x =\necho hi").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Pattern::Name("x".into()),
                value: Box::new(app(bare_head("echo"), vec![plain("hi")])),
            }]
        );
    }

    #[test]
    fn let_rhs_on_next_line_multiple_newlines() {
        // Multiple blank lines between = and RHS are also fine.
        let ast = strip_pos(parse("let x =\n\necho hi").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Pattern::Name("x".into()),
                value: Box::new(app(bare_head("echo"), vec![plain("hi")])),
            }]
        );
    }

    #[test]
    fn let_destructure_rhs_on_next_line() {
        // Destructuring pattern with newline before RHS.
        let ast = strip_pos(parse("let [a, b] =\n[1, 2]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Pattern::List {
                    elems: vec![Pattern::Name("a".into()), Pattern::Name("b".into())],
                    rest: None,
                },
                value: Box::new(Ast::List(vec![
                    ListElem::Single(plain("1")),
                    ListElem::Single(plain("2")),
                ])),
            }]
        );
    }

    #[test]
    fn let_rhs_chain_continues_before_question() {
        let ast = strip_pos(parse("let x = echo hi\n? echo bye").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Pattern::Name("x".into()),
                value: Box::new(Ast::Chain(vec![
                    app(bare_head("echo"), vec![plain("hi")]),
                    app(bare_head("echo"), vec![plain("bye")]),
                ])),
            }]
        );
    }

    #[test]
    fn pipeline_continuation_after_pipe() {
        // cmd1 |\ncmd2 — pipe at end of line continues.
        let ast = strip_pos(parse("echo hello |\nupper").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Pipeline(vec![
                app(bare_head("echo"), vec![plain("hello")]),
                plain("upper"),
            ])]
        );
    }

    #[test]
    fn pipeline_continuation_before_pipe() {
        // cmd1\n| cmd2 — pipe at start of next line continues.
        let ast = strip_pos(parse("echo hello\n| upper").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Pipeline(vec![
                app(bare_head("echo"), vec![plain("hello")]),
                plain("upper"),
            ])]
        );
    }

    #[test]
    fn newline_terminates_command_args() {
        // echo hello\nworld — two separate statements, not one command.
        let ast = strip_pos(parse("echo hello\nworld").unwrap());
        assert_eq!(ast.len(), 2);
    }

    #[test]
    fn caret_is_not_a_continuation_token() {
        assert!(!needs_continuation("^"));
    }

    #[test]
    fn if_same_line_bare_block_is_error() {
        // Old three-block syntax: if cond then else (no `else` keyword).
        let err = parse("if $c { a } { b }").unwrap_err();
        assert!(
            err.message.contains("else"),
            "error should hint at `else`: {err:?}"
        );
    }

    #[test]
    fn if_newline_block_is_valid() {
        // Bare block on the next line is a separate statement — valid.
        assert!(parse("if $c { a }\n{ b }").is_ok());
    }

    #[test]
    fn if_with_else_keyword_is_valid() {
        assert!(parse("if $c { a } else { b }").is_ok());
    }
}
