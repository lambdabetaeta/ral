//! Recursive-descent parser: [`crate::syntax::lexer`] tokens → AST.
//!
//! The grammar is statement-oriented: a program is statements separated by
//! newlines or `;`; a statement is a `let` binding or a `?`-chain of
//! `|`-pipelines, each arm optionally backgrounded with `&`; a stage is
//! `return`, `if`, `case`, a control operator, or a command.  Newlines bend
//! around continuations — freely either side of `|`, one before `?`, any
//! number after a binder's `=` — and inside `[…]` the lexer drops them.
//!
//! Each [`Stmt`] carries the span of its own tokens and the elaborator stamps
//! that span on the IR it emits, so no constructor here threads a span.
//! Arithmetic inside `$[…]` is a Pratt sub-parser over the tokens the outer
//! lexer already produced for the block: no re-lex, no substring round trip.

use crate::source::{Span, Spanned};
use crate::syntax::ast::{
    Ast, BinaryOp, Expr, Head, IfBranch, ListElem, MapEntry, MapKey, MapPatternEntry, Pattern,
    Redirect, RedirectMode, RedirectTarget, ScopeAst, ScopeKeyword, Stmt, Word, WordLiteral,
};
use crate::syntax::lexer::{self, LexError, LexErrorKind, StringPart, Token};
use crate::types;
use std::fmt;

// ── Parse Error ──────────────────────────────────────────────────────────

/// Why a parse failed because the input *stopped short* rather than being
/// malformed — the REPL reads another line instead of reporting an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incompleteness {
    /// A string, balanced `{}` / `[]`, or `$(…)` ran past end of input —
    /// exactly the kinds [`LexErrorKind::is_incomplete`] admits.
    UnclosedLexeme,
    BinderAwaitingRhs,
    /// A `|`, `?`, `if`, `elsif`, or `else` was consumed and the input ran
    /// out before the stage, branch, or body it demands.
    AwaitingContinuation,
}

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    /// The offending token, or the opening delimiter for a lexer error.
    pub span: Option<Span>,
    /// Set for a lexer-originating failure; carries the structure the
    /// diagnostic layer needs to draw more than one label.
    pub lex_kind: Option<LexErrorKind>,
    /// Set when the input merely ran short; drives REPL line continuation.
    pub incompleteness: Option<Incompleteness>,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error: {}", self.message)
    }
}

impl std::error::Error for ParseError {}

impl From<ParseError> for types::Error {
    fn from(e: ParseError) -> Self {
        Self::new(e.to_string(), 2)
    }
}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        let incompleteness = e
            .kind
            .is_incomplete()
            .then_some(Incompleteness::UnclosedLexeme);
        Self {
            message: e.kind.message(),
            span: Some(e.span),
            lex_kind: Some(e.kind),
            incompleteness,
        }
    }
}

// ── Public API ───────────────────────────────────────────────────────────

/// Parse `source` into a statement list under a placeholder file id.
///
/// # Errors
/// Returns `Err` if lexing fails or the tokens do not form a valid program.
pub fn parse(source: &str) -> Result<Vec<Stmt>, ParseError> {
    parse_with(source, crate::source::FileId::DUMMY)
}

/// Returns `true` when `input` is incomplete and the user's next line should
/// be joined to it before parsing.
///
/// Runs the real parser and reads its own [`Incompleteness`] verdict rather
/// than guessing from the raw text.
pub fn needs_continuation(input: &str) -> bool {
    matches!(
        parse(input),
        Err(ParseError {
            incompleteness: Some(_),
            ..
        })
    )
}

/// Parse `source` into a statement list, attributing spans to `file`.
///
/// # Errors
/// Returns `Err` if lexing fails or the tokens do not form a valid program.
pub fn parse_with(source: &str, file: crate::source::FileId) -> Result<Vec<Stmt>, ParseError> {
    let tokens = lexer::lex_with(source, file)?;
    Parser::run_complete(tokens, Parser::parse_program)
}

// ── Parser ───────────────────────────────────────────────────────────────

/// Loop-body verdict for [`Parser::parse_separated_until`]: keep going after
/// this item, or treat it as the last one.
enum SepFlow {
    Cont,
    Stop,
}

struct Parser {
    tokens: Vec<(Token, Span)>,
    pos: usize,
    /// Descent depth, maintained only by [`Parser::nested`].  Values,
    /// arithmetic, and patterns each pass through one guarded chokepoint per
    /// level, so this one counter bounds all three.
    depth: usize,
}

impl Parser {
    fn new(tokens: Vec<(Token, Span)>) -> Self {
        Self {
            tokens,
            pos: 0,
            depth: 0,
        }
    }

    /// Parse a token stream and require that `body` consumed all of it.  The
    /// sole constructor, so no entry point — top level or sub-stream — can let
    /// a production that stops early silently drop the remainder.
    fn run_complete<T>(
        tokens: Vec<(Token, Span)>,
        body: impl FnOnce(&mut Self) -> Result<T, ParseError>,
    ) -> Result<T, ParseError> {
        let mut parser = Self::new(tokens);
        let value = body(&mut parser)?;
        if parser.peek() != &Token::Eof {
            // `parse_program` stops at `}` without consuming it, so a leftover
            // one means an unmatched brace — which may sit mid-program, where
            // "trailing input" would be doubly false.
            if parser.peek() == &Token::RBrace {
                return Err(parser.error("unmatched `}` — no enclosing block is open"));
            }
            let found = parser.peek().clone();
            return Err(parser.error(format!(
                "trailing input: unexpected {found} after the parse completed"
            )));
        }
        Ok(value)
    }

    /// Run `body` one level deeper, rejecting past the cap so adversarial
    /// nesting fails cleanly instead of overflowing the host stack.  A closure,
    /// not an RAII guard: holding `&mut self.depth` would forbid the `&mut
    /// self` calls the body must make.
    fn nested<T>(
        &mut self,
        body: impl FnOnce(&mut Self) -> Result<T, ParseError>,
    ) -> Result<T, ParseError> {
        if self.depth >= crate::syntax::NESTING_DEPTH_LIMIT {
            return Err(self.error(crate::syntax::nesting_too_deep_message()));
        }
        self.depth += 1;
        let result = body(self);
        self.depth -= 1;
        result
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).map_or(&Token::Eof, |(t, _)| t)
    }

    /// Span of the current token, or of the last one past the end.  The lexer
    /// always emits an `Eof`, so only an empty vector reaches the fallback.
    fn span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .or_else(|| self.tokens.last())
            .map_or_else(|| Span::point(crate::source::FileId::DUMMY, 0), |(_, s)| *s)
    }

    fn advance(&mut self) -> &Token {
        let tok = self.tokens.get(self.pos).map_or(&Token::Eof, |(t, _)| t);
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn expect(&mut self, expected: &Token) -> Result<(), ParseError> {
        let tok = self.peek().clone();
        if std::mem::discriminant(&tok) == std::mem::discriminant(expected) {
            self.advance();
            Ok(())
        } else {
            Err(self.error(format!("expected {expected}, found {tok}")))
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
        ParseError {
            message: message.into(),
            span: Some(self.span()),
            lex_kind: None,
            incompleteness: None,
        }
    }

    /// Like [`Self::error`], but the REPL reads another line instead of
    /// reporting it.
    fn incomplete(&self, why: Incompleteness, message: impl Into<String>) -> ParseError {
        ParseError {
            incompleteness: Some(why),
            ..self.error(message)
        }
    }

    /// Like [`Self::error`] but points at `span` rather than the current token.
    fn error_at(span: Span, message: impl Into<String>) -> ParseError {
        ParseError {
            message: message.into(),
            span: Some(span),
            lex_kind: None,
            incompleteness: None,
        }
    }

    /// Called just after consuming a `|`, `?`, `if`, `elsif`, or `else` that
    /// now demands a stage, branch, or body: end of input here is the user
    /// mid-typing, not a dangling operator.
    fn require_continuation(&self, what: &str) -> Result<(), ParseError> {
        if self.peek() == &Token::Eof {
            return Err(self.incomplete(
                Incompleteness::AwaitingContinuation,
                format!("expected {what} after the continuation"),
            ));
        }
        Ok(())
    }

    /// Run `parse` and report the span from the current token to the last it
    /// consumed.
    fn capture_span<T>(
        &mut self,
        parse: impl FnOnce(&mut Self) -> Result<T, ParseError>,
    ) -> Result<(Span, T), ParseError> {
        let start = self.span();
        let v = parse(self)?;
        let span = start.join(self.prev_byte_span());
        Ok((span, v))
    }

    /// Drive a comma-separated list terminated by `end`, with a trailing comma
    /// allowed.  `label` names the construct in the missing-separator error.
    fn parse_separated_until(
        &mut self,
        end: &Token,
        label: &str,
        mut item: impl FnMut(&mut Self) -> Result<SepFlow, ParseError>,
    ) -> Result<(), ParseError> {
        loop {
            if self.peek() == end {
                self.advance();
                return Ok(());
            }
            match item(self)? {
                SepFlow::Cont => {
                    if self.peek() == &Token::Comma {
                        self.advance();
                    } else if self.peek() != end {
                        return Err(self.error(format!("expected ',' or '{end}' in {label}")));
                    }
                }
                SepFlow::Stop => {
                    if self.peek() == &Token::Comma {
                        self.advance();
                    }
                    self.expect(end)?;
                    return Ok(());
                }
            }
        }
    }

    // ── Grammar productions ──────────────────────────────────────────

    /// program = stmt*
    fn parse_program(&mut self) -> Result<Vec<Stmt>, ParseError> {
        let mut stmts = Vec::new();
        self.skip_newlines();
        while self.peek() != &Token::Eof && self.peek() != &Token::RBrace {
            // `parse_stmt` leaves the newline, so this span never swallows a
            // separator.  Underlining the whole statement is the only anchor a
            // diagnostic has when the guilty sub-expression carries no span.
            let (span, kind) = self.capture_span(Self::parse_stmt)?;
            stmts.push(Spanned::new(span, kind));
            self.skip_newlines();
        }
        Ok(stmts)
    }

    /// stmt = binding | chain
    ///
    /// Peeling `let` off above the chain is what keeps `Ast::Let` out of
    /// expression position, where the elaborator treats one as unreachable.
    /// The trailing newline stays for `parse_program`.
    fn parse_stmt(&mut self) -> Result<Ast, ParseError> {
        match self.parse_binding_opt()? {
            Some(binding) => Ok(binding),
            None => self.parse_chain(),
        }
    }

    /// chain = bg-pipeline (NL? '?' bg-pipeline)*
    ///
    /// The statement-level chain, whose arms may each carry a trailing `&`.
    fn parse_chain(&mut self) -> Result<Ast, ParseError> {
        self.parse_chain_of(Self::parse_bg_pipeline)
    }

    /// The `let`-RHS chain: bare pipeline arms, so a per-arm `&` is rejected —
    /// see [`Self::parse_binding_opt`].
    fn parse_chain_no_bg(&mut self) -> Result<Ast, ParseError> {
        self.parse_chain_of(Self::parse_pipeline)
    }

    /// A singleton chain collapses to its bare arm, so `Ast::Chain` always
    /// means two or more branches and downstream passes need no length guard.
    fn parse_chain_of(
        &mut self,
        parse_arm: fn(&mut Self) -> Result<Ast, ParseError>,
    ) -> Result<Ast, ParseError> {
        let (sp0, arm0) = self.capture_span(parse_arm)?;
        let mut arms = vec![Spanned::new(sp0, arm0)];
        while self.eat_chain_question() {
            self.require_continuation("a chain branch")?;
            let (sp, arm) = self.capture_span(parse_arm)?;
            arms.push(Spanned::new(sp, arm));
        }
        Ok(if arms.len() == 1 {
            arms.remove(0).item
        } else {
            Ast::Chain(arms)
        })
    }

    /// Consume `?` with at most one preceding newline and none after; rewinds
    /// and returns false if no `?` follows.
    fn eat_chain_question(&mut self) -> bool {
        let save = self.pos;
        if self.peek() == &Token::Newline {
            self.advance();
        }
        if self.peek() == &Token::Question {
            self.advance();
            true
        } else {
            self.pos = save;
            false
        }
    }

    /// pipeline = stage ('|' stage)*
    fn parse_pipeline(&mut self) -> Result<Ast, ParseError> {
        let (first_span, first) = self.capture_span(Self::parse_stage)?;
        let mut stages = vec![Spanned::new(first_span, first)];

        while self.eat_continuation(&Token::Pipe) {
            self.require_continuation("a pipeline stage")?;
            let (span, stage) = self.capture_span(Self::parse_stage)?;
            stages.push(Spanned::new(span, stage));
        }

        if stages.len() == 1 {
            Ok(stages.remove(0).item)
        } else {
            Ok(Ast::Pipeline(stages))
        }
    }

    /// Consume `tok` with any number of newlines on either side; rewinds and
    /// returns false on a miss.  The narrower `?` rule lives in
    /// [`Self::eat_chain_question`].
    fn eat_continuation(&mut self, tok: &Token) -> bool {
        let save = self.pos;
        self.skip_newlines();
        if self.peek() == tok {
            self.advance();
            self.skip_newlines();
            true
        } else {
            self.pos = save;
            false
        }
    }

    /// bg-pipeline = pipeline '&'?
    fn parse_bg_pipeline(&mut self) -> Result<Ast, ParseError> {
        // Span captured before the `&` is seen, so a `Background` underlines
        // the pipeline and not the operator.
        let (inner_span, node) = self.capture_span(Self::parse_pipeline)?;
        if self.peek() == &Token::Ampersand {
            self.advance();
            return Ok(Ast::Background(Spanned::boxed(inner_span, node)));
        }
        Ok(node)
    }

    /// stage = return-stage | if-stage | case-stage | control-op | command
    ///
    /// The dispatch below is also the list of words reserved in stage position.
    fn parse_stage(&mut self) -> Result<Ast, ParseError> {
        match self.peek().as_plain_word() {
            // `parse_stmt` peels `let` off first, so arriving here means a
            // binding embedded in pipeline or chain position.
            Some("let") => Err(self.error(
                "`let` is a statement, not a pipeline stage or chain branch — \
                 move the binding to its own line, or wrap the consumer in a \
                 block: `{ let x = …; … }`",
            )),
            Some("return") => self.parse_return_stage(),
            Some("if") => self.parse_if(),
            Some("case") => self.parse_case(),
            // `^try` and friends stay external: a `Token::Caret` head yields
            // no plain word, so it falls to `parse_command` below.
            Some(name) => match ScopeAst::lookup_keyword(name) {
                Some(kw) => self.parse_control_op(kw),
                None => self.parse_command(),
            },
            None => self.parse_command(),
        }
    }

    /// Parse the keyword `kw` names, then exactly `kw.arity` operands and any
    /// trailing redirects, into an [`Ast::Scope`].  Operands are atoms, not
    /// arguments: `parse_arg` would admit an `Ast::Spread` that these fixed
    /// positions have no lowering for.
    fn parse_control_op(&mut self, kw: &ScopeKeyword) -> Result<Ast, ParseError> {
        self.advance(); // consume the head name
        let mut operands = Vec::with_capacity(kw.arity);
        while !self.at_cmd_end() && !matches!(self.peek(), Token::Redirect { .. }) {
            if self.peek() == &Token::Spread {
                return Err(self.error(format!(
                    "`{name}` takes its operands by position ({operands_desc}); \
                     a spread `...` has no meaning here",
                    name = kw.name,
                    operands_desc = kw.operand_desc,
                )));
            }
            operands.push(self.parse_atom()?);
        }
        if operands.len() != kw.arity {
            return Err(self.error(format!(
                "{name} requires {arity} argument{plural} ({operands_desc}); got {got}",
                name = kw.name,
                arity = kw.arity,
                plural = if kw.arity == 1 { "" } else { "s" },
                operands_desc = kw.operand_desc,
                got = operands.len(),
            )));
        }
        let redirects = self.collect_trailing_redirects()?;
        let op = (kw.build)(operands);
        Ok(Ast::Scope { op, redirects })
    }

    /// Only a fixed-arity form can collect redirects at the end like this;
    /// `parse_command` interleaves them, since a command takes them anywhere.
    fn collect_trailing_redirects(&mut self) -> Result<Vec<Redirect>, ParseError> {
        let mut redirects = Vec::new();
        while !self.at_cmd_end() && matches!(self.peek(), Token::Redirect { .. }) {
            redirects.push(self.parse_redirect()?);
        }
        Ok(redirects)
    }

    /// case = 'case' atom atom  (scrutinee, then a tag-keyed table of thunks)
    ///
    /// Any atom is accepted for either: the typechecker enforces the shapes,
    /// and its errors can name the resolved type where the parser could not.
    fn parse_case(&mut self) -> Result<Ast, ParseError> {
        self.advance(); // consume `case`
        self.skip_newlines();
        let (scrut_span, scrutinee) = self.capture_span(Self::parse_atom)?;
        self.skip_newlines();
        let (table_span, table) = self.capture_span(Self::parse_atom)?;
        Ok(Ast::Case {
            scrutinee: Spanned::boxed(scrut_span, scrutinee),
            table: Spanned::boxed(table_span, table),
        })
    }

    /// if = 'if' atom atom ('elsif' atom atom)* ('else' atom)?
    ///
    /// Conditions and bodies are any atom; the typechecker demands the thunks.
    /// The leading `if` and every `elsif` collapse into one `branches` vector.
    fn parse_if(&mut self) -> Result<Ast, ParseError> {
        self.advance(); // consume 'if'
        self.skip_newlines();
        self.require_continuation("the `if` condition")?;
        let mut branches = vec![self.parse_if_branch()?];
        let mut else_ = None;

        loop {
            // `elsif` / `else` may open the next line, so newlines are skipped
            // speculatively and rewound if neither keyword follows.
            let save = self.pos;
            self.skip_newlines();
            match self.peek() {
                tok if tok.as_plain_word() == Some("elsif") => {
                    self.advance();
                    self.skip_newlines();
                    self.require_continuation("the `elsif` condition")?;
                    branches.push(self.parse_if_branch()?);
                }
                tok if tok.as_plain_word() == Some("else") => {
                    self.advance();
                    self.skip_newlines();
                    self.require_continuation("the `else` body")?;
                    let (body_span, body) = self.capture_span(Self::parse_atom)?;
                    else_ = Some(Spanned::boxed(body_span, body));
                    break;
                }
                _ => {
                    // `self.pos == save` means no newline intervened, so a `{`
                    // on this line is a third block where `else` should be —
                    // on the next line it would be a statement of its own.
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

        Ok(Ast::If { branches, else_ })
    }

    /// One `cond body` pair, shared by the leading `if` and every `elsif`.
    fn parse_if_branch(&mut self) -> Result<IfBranch, ParseError> {
        let (cond_span, cond) = self.capture_span(Self::parse_atom)?;
        self.skip_newlines();
        self.require_continuation("the `if` body")?;
        let (body_span, body) = self.capture_span(Self::parse_atom)?;
        Ok(IfBranch {
            cond: Spanned::boxed(cond_span, cond),
            body: Spanned::boxed(body_span, body),
        })
    }

    fn parse_return_stage(&mut self) -> Result<Ast, ParseError> {
        self.advance(); // consume `return`

        if self.at_cmd_end() {
            return Ok(Ast::Return(None));
        }

        let (val_span, val) = self.capture_span(Self::parse_atom)?;
        if !self.at_cmd_end() {
            return Err(self.error("return expects at most one value argument"));
        }
        Ok(Ast::Return(Some(Spanned::boxed(val_span, val))))
    }

    /// binding = 'let' pattern '=' pipeline (NL? '?' pipeline)* '&'?
    ///
    /// `None` when the next token is not `let`, so the caller falls through to
    /// the chain statement.  This RHS is narrower than [`Self::parse_chain`]:
    /// a per-arm `&` is rejected and a single trailing one backgrounds the
    /// whole RHS, so `let x = a ? b &` cannot background `b` alone.
    fn parse_binding_opt(&mut self) -> Result<Option<Ast>, ParseError> {
        if self.peek().as_plain_word() != Some("let") {
            return Ok(None);
        }
        self.advance(); // consume 'let'
        let (pattern_span, pattern) = self.capture_span(Self::parse_pattern)?;
        match self.peek() {
            tok if tok.as_plain_word() == Some("=") => {
                self.advance();
            }
            _ => return Err(self.error("expected '=' after the binding name in `let`")),
        }
        // The RHS may start on the next line: `let x =\n  expr`.
        self.skip_newlines();
        if self.peek() == &Token::Eof {
            return Err(self.incomplete(
                Incompleteness::BinderAwaitingRhs,
                "expected the right-hand side of the `let` binding",
            ));
        }
        // Two spans, deliberately: `inner_span` stops before the `&` so a
        // `Background` underlines only the chain, while `value_span` reaches
        // past it so `Let.value` covers the whole RHS.
        let (inner_span, mut value) = self.capture_span(Self::parse_chain_no_bg)?;
        if self.peek() == &Token::Ampersand {
            self.advance();
            value = Ast::Background(Spanned::boxed(inner_span, value));
        }
        let value_span = inner_span.join(self.prev_byte_span());
        Ok(Some(Ast::Let {
            pattern: Spanned::new(pattern_span, pattern),
            value: Spanned::boxed(value_span, value),
        }))
    }

    /// A binding LHS or lambda parameter, and the pattern grammar's sole entry:
    /// list and map patterns recurse back here per element, so the one
    /// `nested()` guard bounds the whole recursion.
    fn parse_pattern(&mut self) -> Result<Pattern, ParseError> {
        self.nested(|p| match p.peek() {
            Token::LBracket => p.parse_pattern_inner(),
            tok if tok.as_plain_word() == Some("_") => {
                p.advance();
                Ok(Pattern::Wildcard)
            }
            Token::Word(Word::Plain(name)) if is_reserved(name) => Err(p.error(format!(
                "'{name}' is a reserved keyword and cannot be used as a binding name"
            ))),
            Token::Word(Word::Plain(name)) if lexer::is_ident(name) => {
                let name = name.clone();
                p.advance();
                Ok(Pattern::Name(name))
            }
            _ => Err(p.error(
                "expected a pattern: a name like `x`, `_` to ignore, \
                 or a destructuring `[a, b]` / `[host: h, port: p]`",
            )),
        })
    }

    fn parse_pattern_inner(&mut self) -> Result<Pattern, ParseError> {
        self.expect(&Token::LBracket)?;

        if self.peek() == &Token::RBracket {
            self.advance();
            return Ok(Pattern::List {
                elems: vec![],
                rest: None,
            });
        }

        // Same key alphabet as a map literal minus the dynamic `$var` key,
        // which a pattern cannot bind through.
        let is_map = self.key_colon_at(self.pos, /*allow_deref=*/ false);

        if is_map {
            self.parse_map_pattern()
        } else {
            self.parse_list_pattern()
        }
    }

    fn parse_list_pattern(&mut self) -> Result<Pattern, ParseError> {
        let mut elems = Vec::new();
        let mut rest = None;

        self.parse_separated_until(&Token::RBracket, "list pattern", |p| {
            // `...name` is terminal: `SepFlow::Stop` is what forbids elements
            // after it.
            if p.peek() == &Token::Spread {
                p.advance();
                let Token::Word(Word::Plain(name)) = p.peek().clone() else {
                    return Err(p.error("expected name after '...'"));
                };
                if is_reserved(&name) {
                    return Err(p.error(format!(
                        "'{name}' is a reserved keyword and cannot be used as a binding name"
                    )));
                }
                if !lexer::is_ident(&name) {
                    return Err(p.error(
                        "rest pattern `...name` needs a plain identifier after the dots, \
                         e.g. `...rest`",
                    ));
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
        // Mirrors the literal side: bare and tag alphabets cannot mix.
        let mut alphabet: Option<bool> = None;

        self.parse_separated_until(&Token::RBracket, "map pattern", |p| {
            let key = p.parse_static_key()?;
            p.check_key_alphabet(
                &mut alphabet,
                key.is_tag(),
                "map pattern mixes bare and tag keys — pick one alphabet",
            )?;
            p.expect(&Token::Colon)?;
            let pattern = p.parse_pattern()?;
            let default = if p.peek().as_plain_word() == Some("=") {
                p.advance();
                Some(p.parse_atom()?)
            } else {
                None
            };
            entries.push(MapPatternEntry {
                key,
                pattern,
                default,
            });
            Ok(SepFlow::Cont)
        })?;

        Ok(Pattern::Map(entries))
    }

    /// pkey = IDENT | QUOTED | TAG — the map-*pattern* key alphabet.  Literals
    /// additionally admit `$deref`, which is [`Self::parse_map_key`]'s job.
    fn parse_static_key(&mut self) -> Result<MapKey, ParseError> {
        match self.peek().clone() {
            Token::Word(Word::Plain(k)) if lexer::is_ident(&k) => {
                self.advance();
                Ok(MapKey::Bare(k))
            }
            Token::SingleQuoted(k) => {
                self.advance();
                Ok(MapKey::Bare(k))
            }
            Token::Tag(label) => {
                self.advance();
                Ok(MapKey::Tag(label))
            }
            _ => Err(self.error("expected map pattern key: name, 'quoted', or backtick tag")),
        }
    }

    /// mapkey = IDENT | QUOTED | deref | TAG — the map-*literal* key alphabet.
    /// The form is returned rather than the entry, because which [`MapEntry`]
    /// to build is only settled once the `:` and value are consumed.
    fn parse_map_key(&mut self) -> Result<MapKeyForm, ParseError> {
        match self.peek().clone() {
            Token::Word(Word::Plain(k)) if lexer::is_ident(&k) => {
                Ok(MapKeyForm::Static(self.parse_static_key()?))
            }
            Token::SingleQuoted(_) | Token::Tag(_) => {
                Ok(MapKeyForm::Static(self.parse_static_key()?))
            }
            Token::Deref(StringPart::Variable(k)) => {
                self.advance();
                Ok(MapKeyForm::Deref(k))
            }
            Token::Word(Word::Plain(k)) if k.parse::<f64>().is_ok() => Err(self.error(
                "map keys must be identifiers or quoted strings, not numbers; use '0': val",
            )),
            _ => Err(self.error("expected map key: name, 'quoted', backtick tag, or $var")),
        }
    }

    /// The first key fixes the alphabet and every later one must match, so
    /// `` [host: …, `dev: …] `` is rejected in literal and pattern alike.
    fn check_key_alphabet(
        &self,
        seen_is_tag: &mut Option<bool>,
        this_is_tag: bool,
        mismatch_msg: &str,
    ) -> Result<(), ParseError> {
        match seen_is_tag {
            None => *seen_is_tag = Some(this_is_tag),
            Some(prev) if *prev != this_is_tag => return Err(self.error(mismatch_msg)),
            Some(_) => {}
        }
        Ok(())
    }

    /// primary = word | tag | block | collection
    ///
    /// The value grammar's `nested()` chokepoint.  Arithmetic and patterns
    /// route around it and guard themselves, at [`Self::parse_expr_atom`] and
    /// [`Self::parse_pattern`]; the lexer caps its delimiter nesting likewise.
    fn parse_primary(&mut self) -> Result<Ast, ParseError> {
        self.nested(|p| match p.peek() {
            Token::LBrace => p.parse_block(),
            Token::LBracket => p.parse_collection(),
            _ => p.parse_word(),
        })
    }

    /// `atom = primary ('[' word ']')*`
    ///
    /// A run of postfix keys becomes one flat `Ast::Index`, never a nest of
    /// single-key ones.
    fn parse_atom(&mut self) -> Result<Ast, ParseError> {
        let (node_span, node) = self.capture_span(Self::parse_primary)?;
        let mut new_keys: Vec<Spanned<Ast>> = Vec::new();
        while self.peek() == &Token::LBracket && self.next_token_is_adjacent() {
            // Brackets included, so the caret underlines what the user wrote.
            let (span, k) = self.capture_span(|p| {
                p.advance();
                let k = p.parse_word()?;
                p.expect(&Token::RBracket)?;
                Ok(k)
            })?;
            new_keys.push(Spanned::new(span, k));
        }
        if new_keys.is_empty() {
            return Ok(node);
        }
        // `$name[k]` is fused by the lexer and reaches us already an
        // `Ast::Index`, so extend that node rather than wrapping it.
        Ok(match node {
            Ast::Index {
                target,
                keys: mut existing,
            } => {
                existing.extend(new_keys);
                Ast::Index {
                    target,
                    keys: existing,
                }
            }
            other => Ast::Index {
                target: Spanned::boxed(node_span, other),
                keys: new_keys,
            },
        })
    }

    /// No gap since the previous token — what separates `$xs[0]` (an index)
    /// from `cmd $xs [0]` (a second argument).
    fn next_token_is_adjacent(&self) -> bool {
        let Some((_, prev_span)) = self.tokens.get(self.pos.saturating_sub(1)) else {
            return false;
        };
        let Some((_, next_span)) = self.tokens.get(self.pos) else {
            return false;
        };
        prev_span.end == next_span.start
    }

    fn parse_redirect(&mut self) -> Result<Redirect, ParseError> {
        match self.peek().clone() {
            Token::Redirect {
                fd,
                kind: mode,
                target_fd,
            } => {
                let op_span = self.span();
                self.advance();
                if mode == RedirectMode::HereString && fd.is_some_and(|n| n != 0) {
                    return Err(Self::error_at(
                        op_span,
                        "`<<` always feeds stdin — drop the file-descriptor prefix",
                    ));
                }
                // Reads and here-strings feed fd 0; everything else writes fd 1.
                let default_fd = u32::from(!matches!(
                    mode,
                    RedirectMode::Read | RedirectMode::HereString
                ));
                let target = if let Some(tfd) = target_fd {
                    RedirectTarget::Fd(tfd)
                } else {
                    let (word_span, word) = self.capture_span(Self::parse_word)?;
                    if mode == RedirectMode::HereString
                        && let Ast::Word(w) = &word
                    {
                        let message: String = match w {
                            Word::Plain(_) => "ral has no heredocs: `<<` feeds a string to \
                                 stdin. Use a raw string: `cmd << #' ... '#`, \
                                 which may use newlines"
                                .into(),
                            Word::Slash(_) | Word::Tilde(_) => {
                                "`<<` feeds a string to stdin, not a file — \
                                 to read a file into stdin, use `< path`"
                                    .into()
                            }
                        };
                        return Err(Self::error_at(word_span, message));
                    }
                    RedirectTarget::File(Box::new(word))
                };
                Ok(Redirect {
                    fd: fd.unwrap_or(default_fd),
                    mode,
                    target,
                })
            }
            _ => Err(self.error("expected redirect")),
        }
    }

    /// arg = atom | '...' atom
    ///
    /// The [`Ast::Spread`] node tells the elaborator to splice `x`'s elements
    /// into the argument list; `[...x]` is a literal and stays one argument.
    fn parse_arg(&mut self) -> Result<Ast, ParseError> {
        if self.peek() == &Token::Spread {
            self.advance();
            let (span, inner) = self.capture_span(Self::parse_atom)?;
            Ok(Ast::Spread(Spanned::boxed(span, inner)))
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
                Token::Word(Word::Slash(_) | Word::Tilde(_)) => {
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
    fn parse_command(&mut self) -> Result<Ast, ParseError> {
        if matches!(self.peek(), Token::Redirect { .. }) {
            return Err(self.error("redirect must follow a command"));
        }
        let head = self.parse_head()?;
        let mut args: Vec<Spanned<Ast>> = Vec::new();
        let mut redirects = Vec::new();
        while !self.at_cmd_end() {
            if matches!(self.peek(), Token::Redirect { .. }) {
                redirects.push(self.parse_redirect()?);
            } else {
                let (arg_span, arg) = self.capture_span(Self::parse_arg)?;
                args.push(Spanned::new(arg_span, arg));
            }
        }

        // A bare head that is really a value (`$x`, `true`, `42`) sheds the
        // `Ast::Call` wrapper so downstream passes see the value itself.
        if args.is_empty() && redirects.is_empty() {
            match head {
                Head::Value(value) => return Ok(*value),
                Head::Bare(s) if WordLiteral::classify(&s).is_some() => {
                    return Ok(Ast::Word(Word::Plain(s)));
                }
                _ => {}
            }
        }
        Ok(Ast::Call {
            head,
            args,
            redirects,
        })
    }

    /// Byte span of the last consumed token — where the production that just
    /// finished ends.  Falls back to the current one at input start.
    fn prev_byte_span(&self) -> Span {
        self.tokens
            .get(self.pos.saturating_sub(1))
            .map_or_else(|| self.span(), |(_, s)| *s)
    }

    /// End of a command's argument list: a statement end, or the `?` / `|`
    /// that hands the result to the next arm or stage.
    fn at_cmd_end(&self) -> bool {
        self.at_stmt_end() || self.peek() == &Token::Question || self.peek() == &Token::Pipe
    }

    /// word = WORD | QUOTED | INTERP | deref | force | expr-block
    fn parse_word(&mut self) -> Result<Ast, ParseError> {
        match self.peek().clone() {
            Token::Word(w) => {
                self.advance();
                Ok(Ast::Word(w))
            }
            Token::SingleQuoted(s) => {
                self.advance();
                Ok(Ast::Literal(s))
            }
            Token::DoubleQuoted(parts) => {
                self.advance();
                Self::parse_interpolation_parts(&parts)
            }
            Token::Deref(part) => {
                self.advance();
                match part {
                    StringPart::Variable(name) => Ok(Ast::Variable(name)),
                    StringPart::Index { name, keys } => deref_index_to_ast(name, keys),
                    _ => Err(self.error(
                        "unexpected form after `$` — write `$name`, `$(name)`, \
                         `$name[key]`, or `$[expr]`",
                    )),
                }
            }
            Token::Expr(tokens) => {
                self.advance();
                Ok(Ast::Expr(Box::new(parse_expr_block(tokens)?)))
            }
            Token::Dollar => {
                self.advance();
                Err(self.error("expected dereference after '$' (e.g. $name, $(name), or $[...])"))
            }
            Token::Caret => Err(self.error("'^name' is only valid in command-head position")),
            Token::Bang => self.parse_bang(),
            Token::Tag(label) => {
                self.advance();
                let payload = if self.at_tag_payload_end() {
                    None
                } else {
                    let (payload_span, p) = self.capture_span(Self::parse_atom)?;
                    Some(Spanned::boxed(payload_span, p))
                };
                Ok(Ast::Tag { label, payload })
            }
            _ => Err(self.error(format!("unexpected token: {}", self.peek()))),
        }
    }

    /// Separators and closers, at which a backtick tag stays nullary.  It is
    /// otherwise greedy: anything value-shaped after it becomes the payload.
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

    /// force = '!' primary — both callers leave the `!` for us, so the span
    /// starts there.  A `primary`, not an atom: leaving postfix `[k]` to the
    /// enclosing `parse_atom` makes `!{cmd}[k]` force first and index after.
    fn parse_bang(&mut self) -> Result<Ast, ParseError> {
        let (span, inner) = self.capture_span(|p| {
            p.advance(); // consume `!`
            p.parse_primary()
        })?;
        Ok(Ast::Force(Spanned::boxed(span, inner)))
    }

    /// block = '{' program '}' | '{' '|' pattern+ '|' program '}'
    fn parse_block(&mut self) -> Result<Ast, ParseError> {
        self.expect(&Token::LBrace)?;
        if self.peek() == &Token::Pipe {
            self.advance(); // consume opening |
            let mut params: Vec<Spanned<Pattern>> = Vec::new();
            while self.peek() != &Token::Pipe {
                let (sp, p) = self.capture_span(Self::parse_pattern)?;
                params.push(Spanned::new(sp, p));
            }
            self.expect(&Token::Pipe)?;
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
                // Curry: { |x y z| body } → { |x| { |y| { |z| body } } }.  The
                // synthetic wrapper statements borrow the real body's span, so
                // a diagnostic inside them still lands on user code.
                let synth_span: Option<Span> = body
                    .first()
                    .and_then(|s| s.span)
                    .or_else(|| Some(self.span()));
                let mut result = Ast::Lambda {
                    param: params.pop().unwrap(),
                    body,
                };
                while let Some(p) = params.pop() {
                    result = Ast::Lambda {
                        param: p,
                        body: vec![Spanned::with_span(synth_span, result)],
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

    /// collection = list | map — `[]` is the empty list, `[:]` the empty map,
    /// and anything else is settled by lookahead for a `key:`.
    fn parse_collection(&mut self) -> Result<Ast, ParseError> {
        self.expect(&Token::LBracket)?;

        if self.peek() == &Token::RBracket {
            self.advance();
            return Ok(Ast::List(vec![]));
        }

        if self.peek() == &Token::Colon
            && self.tokens.get(self.pos + 1).map(|(t, _)| t) == Some(&Token::RBracket)
        {
            self.advance(); // :
            self.advance(); // ]
            return Ok(Ast::Map(vec![]));
        }

        let is_map = self.is_map_ahead();

        if is_map {
            self.parse_map_entries()
        } else {
            self.parse_list_elems()
        }
    }

    fn is_map_ahead(&self) -> bool {
        let mut i = self.pos;
        // A leading `...` decides nothing, so skip past it.  Its operand is a
        // whole atom and may nest (`...[a: 1]`), so only a `,` or `]` at the
        // operand's own level ends it — or the end of input, without which an
        // unterminated nesting would spin here.
        while matches!(self.tokens.get(i).map(|(t, _)| t), Some(Token::Spread)) {
            i += 1;
            let mut depth = 0usize;
            loop {
                match self.tokens.get(i).map(|(t, _)| t) {
                    None => break,
                    Some(Token::LBracket | Token::LBrace) => depth += 1,
                    Some(Token::RBracket | Token::RBrace) if depth > 0 => depth -= 1,
                    Some(Token::Comma | Token::RBracket) if depth == 0 => break,
                    _ => {}
                }
                i += 1;
            }
            if matches!(self.tokens.get(i).map(|(t, _)| t), Some(Token::Comma)) {
                i += 1;
            }
        }

        // Literals admit a dynamic `$var` key; patterns cannot bind through
        // one, so `parse_pattern_inner` passes `false` here.
        self.key_colon_at(i, /*allow_deref=*/ true)
    }

    /// True when `tokens[i]` is a map key followed by `:`.  The one shape test
    /// behind both lookaheads, so the two cannot drift on what a key is.
    fn key_colon_at(&self, i: usize, allow_deref: bool) -> bool {
        let is_key = matches!(
            self.tokens.get(i).map(|(t, _)| t),
            Some(Token::Word(Word::Plain(_)) | Token::SingleQuoted(_) | Token::Tag(_))
        ) || (allow_deref
            && matches!(self.tokens.get(i).map(|(t, _)| t), Some(Token::Deref(_))));
        is_key && matches!(self.tokens.get(i + 1).map(|(t, _)| t), Some(Token::Colon))
    }

    /// elem = atom | '...' atom
    fn parse_list_elems(&mut self) -> Result<Ast, ParseError> {
        let mut elems = Vec::new();

        self.parse_separated_until(&Token::RBracket, "list", |p| {
            if p.peek() == &Token::Spread {
                p.advance();
                let (sp, a) = p.capture_span(Self::parse_atom)?;
                elems.push(ListElem::Spread(Spanned::new(sp, a)));
            } else {
                let (sp, a) = p.capture_span(Self::parse_atom)?;
                elems.push(ListElem::Single(Spanned::new(sp, a)));
            }
            Ok(SepFlow::Cont)
        })?;

        Ok(Ast::List(elems))
    }

    fn parse_map_entries(&mut self) -> Result<Ast, ParseError> {
        let mut entries = Vec::new();
        // Bare `name` versus tag `` `name ``; a dynamic `$var` key is unknown
        // until runtime and so votes for neither alphabet.
        let mut alphabet: Option<bool> = None;

        self.parse_separated_until(&Token::RBracket, "map", |p| {
            if p.peek() == &Token::Spread {
                p.advance();
                let (sp, a) = p.capture_span(Self::parse_atom)?;
                entries.push(MapEntry::Spread(Spanned::new(sp, a)));
                return Ok(SepFlow::Cont);
            }
            let key_form = p.parse_map_key()?;
            if let MapKeyForm::Static(ref key) = key_form {
                p.check_key_alphabet(
                    &mut alphabet,
                    key.is_tag(),
                    "record literal mixes bare and tag keys — pick one alphabet",
                )?;
            }
            p.expect(&Token::Colon)?;
            let (val_span, val) = p.capture_span(Self::parse_atom)?;
            let value = Spanned::new(val_span, val);
            entries.push(match key_form {
                MapKeyForm::Static(key) => MapEntry::Entry { key, value },
                MapKeyForm::Deref(name) => MapEntry::Deref { name, value },
            });
            Ok(SepFlow::Cont)
        })?;

        Ok(Ast::Map(entries))
    }

    /// Lower the segments of a double-quoted string.  Each keeps the byte range
    /// the lexer gave it, and a `Force` segment passes that range to its inner
    /// block too, so the forced body has a span of its own.
    fn parse_interpolation_parts(parts: &[Spanned<StringPart>]) -> Result<Ast, ParseError> {
        if parts.len() == 1
            && let StringPart::Literal(s) = &parts[0].item
        {
            return Ok(Ast::Literal(s.clone()));
        }

        let mut ast_parts = Vec::new();
        for part in parts {
            let segment = match &part.item {
                StringPart::Literal(s) => Ast::Literal(s.clone()),
                StringPart::Variable(name) => Ast::Variable(name.clone()),
                StringPart::Force(tokens) => {
                    let stmts = parse_force_body(tokens.clone())?;
                    Ast::Force(Spanned::with_span(part.span, Box::new(Ast::Block(stmts))))
                }
                StringPart::Expr(tokens) => Ast::Expr(Box::new(parse_expr_block(tokens.clone())?)),
                StringPart::Index { name, keys } => deref_index_to_ast(name.clone(), keys.clone())?,
            };
            ast_parts.push(Spanned::with_span(part.span, segment));
        }

        Ok(Ast::Interpolation(ast_parts))
    }

    // ── Arithmetic (Pratt parser) ────────────────────────────────────

    /// Precedence-climbing loop.  It needs no depth guard of its own: every
    /// depth-growing recursion bottoms out in [`Self::parse_expr_atom`], and
    /// the binary right-hand side is bounded by the precedence ladder.
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

    /// The arithmetic grammar's `nested()` chokepoint, wrapping
    /// [`Self::parse_expr_operand`].  Parenthesised sub-expressions and the
    /// unary prefixes both come back here, so this guard bounds every
    /// depth-growing path inside `$[…]`.
    fn parse_expr_atom(&mut self) -> Result<Expr, ParseError> {
        self.nested(Self::parse_expr_operand)
    }

    fn parse_expr_operand(&mut self) -> Result<Expr, ParseError> {
        match self.peek().clone() {
            Token::LParen => {
                self.advance();
                let expr = self.parse_expr_prec(0)?;
                self.expect(&Token::RParen)?;
                Ok(expr)
            }
            Token::Deref(part) => {
                let result = match part {
                    StringPart::Variable(name) => Ok(Expr::Variable(name)),
                    StringPart::Index { name, keys } => {
                        Ok(Expr::Index(name.item, parse_index_keys(keys)?))
                    }
                    _ => Err(self
                        .error("unexpected `$…` form inside `$[…]` — use `$name` or `$name[key]`")),
                };
                self.advance();
                result
            }
            Token::Bang => {
                // Lifted rather than re-parsed, so the operand span crosses
                // into the expression grammar intact.
                let Ast::Force(body) = self.parse_bang()? else {
                    unreachable!("parse_bang yields Ast::Force by construction");
                };
                Ok(Expr::Force(body))
            }
            Token::Word(Word::Plain(s)) if s == "-" => {
                self.advance();
                let inner = self.parse_expr_atom()?;
                Ok(Expr::Negate(Box::new(inner)))
            }
            Token::Word(Word::Plain(s)) if s == "not" => {
                self.advance();
                // An atom, not an expression: `not` binds tighter than every
                // binary operator, so `not $x == 0` is `(not $x) == 0`.
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
                // `classify`, not a blanket `f64` parse, so `inf`, `nan`, and
                // `1e5` stay the strings they are outside `$[…]`.
                match WordLiteral::classify(&s) {
                    Some(WordLiteral::Int(n)) => {
                        self.advance();
                        Ok(Expr::Integer(n))
                    }
                    Some(WordLiteral::Float(f)) => {
                        self.advance();
                        Ok(Expr::Number(f))
                    }
                    _ => Err(self.error(format!(
                        "expected a number, variable, or `(…)` here, but found '{s}' \
                         — did you mean `${s}` to reference a variable?"
                    ))),
                }
            }
            // `<` and `>` arrive as `Redirect` tokens.  One in *operand*
            // position is usually a digit glued to the operator — `$[2>3]`
            // lexes `2>` as a file descriptor — so name the spacing.
            Token::Redirect { fd, kind, .. } => {
                let op = match kind {
                    RedirectMode::Read => Some("<"),
                    RedirectMode::Write => Some(">"),
                    _ => None,
                };
                Err(self.error(match (op, fd) {
                    (Some(op), Some(n)) => format!(
                        "unexpected `{n}{op}` in expression; a digit glued to `{op}` is read \
                         as a file-descriptor redirect — write `{n} {op} …` with spaces for a \
                         comparison"
                    ),
                    (Some(op), None) => format!(
                        "unexpected `{op}` in expression; `{op}` is a comparison operator and \
                         needs an operand on each side"
                    ),
                    (None, _) => {
                        format!("unexpected redirect in expression: {}", self.peek())
                    }
                }))
            }
            _ => Err(self.error(format!("unexpected token in expression: {}", self.peek()))),
        }
    }

    /// The operator at the cursor and its binding power, low to high: `||`,
    /// `&&`, comparison, add/sub, mul/div/mod.  The unary prefixes bind tighter
    /// than all of these.
    fn peek_expr_op(&self) -> Option<(InfixOp, u8)> {
        match self.peek() {
            Token::Word(Word::Plain(s)) => match s.as_str() {
                "||" => Some((InfixOp::Or, 1)),
                "&&" => Some((InfixOp::And, 2)),
                "+" => Some((InfixOp::Op(BinaryOp::Add), 4)),
                "-" => Some((InfixOp::Op(BinaryOp::Sub), 4)),
                "*" => Some((InfixOp::Op(BinaryOp::Mul), 5)),
                "/" => Some((InfixOp::Op(BinaryOp::Div), 5)),
                "%" => Some((InfixOp::Op(BinaryOp::Mod), 5)),
                "==" => Some((InfixOp::Op(BinaryOp::Eq), 3)),
                "!=" => Some((InfixOp::Op(BinaryOp::Ne), 3)),
                "<" => Some((InfixOp::Op(BinaryOp::Lt), 3)),
                ">" => Some((InfixOp::Op(BinaryOp::Gt), 3)),
                "<=" => Some((InfixOp::Op(BinaryOp::Le), 3)),
                ">=" => Some((InfixOp::Op(BinaryOp::Ge), 3)),
                _ => None,
            },
            Token::Word(Word::Slash(s)) if s == "/" => Some((InfixOp::Op(BinaryOp::Div), 5)),
            // A bare `<` or `>` lexes as a redirect; here it is a comparison.
            Token::Redirect {
                fd: None,
                kind: RedirectMode::Read,
                target_fd: None,
            } => Some((InfixOp::Op(BinaryOp::Lt), 3)),
            Token::Redirect {
                fd: None,
                kind: RedirectMode::Write,
                target_fd: None,
            } => Some((InfixOp::Op(BinaryOp::Gt), 3)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum InfixOp {
    Op(BinaryOp),
    And,
    Or,
}

/// Parse the pre-lexed key streams of `$name[k1][k2]…`, one `word` apiece,
/// through the ordinary [`Parser::parse_word`].  Their spans still address the
/// outer source, so a diagnostic from down here underlines the right column.
fn parse_index_keys(
    keys: Vec<Spanned<Vec<(Token, Span)>>>,
) -> Result<Vec<Spanned<Ast>>, ParseError> {
    keys.into_iter()
        .map(|key| {
            let ast = Parser::run_complete(key.item, Parser::parse_word)?;
            Ok(Spanned::with_span(key.span, ast))
        })
        .collect()
}

/// Build an [`Ast::Index`] from a lexer-fused `$name[k1][k2]…` deref.  Shared
/// by both consumers of a [`StringPart::Index`], bare and inside a string, so
/// the two cannot construct it differently.
fn deref_index_to_ast(
    name: Spanned<String>,
    keys: Vec<Spanned<Vec<(Token, Span)>>>,
) -> Result<Ast, ParseError> {
    Ok(Ast::Index {
        target: Spanned::with_span(name.span, Box::new(Ast::Variable(name.item))),
        keys: parse_index_keys(keys)?,
    })
}

/// Parse the pre-lexed body of `$[…]` as a Pratt expression.  The lexer has
/// already fused the block's adjacent `&` / `|` pairs into `&&` / `||` words.
fn parse_expr_block(tokens: Vec<(Token, Span)>) -> Result<Expr, ParseError> {
    Parser::run_complete(tokens, |p| p.parse_expr_prec(0))
}

/// Parse the pre-lexed body of a `!{…}` interpolation as a statement list.
fn parse_force_body(tokens: Vec<(Token, Span)>) -> Result<Vec<Stmt>, ParseError> {
    Parser::run_complete(tokens, Parser::parse_program)
}

/// Names no binding may take: a keyword by [`crate::syntax::is_keyword`], or a
/// value literal.  A `^name` head never reaches a pattern, so `^try` still
/// resolves through PATH.
fn is_reserved(s: &str) -> bool {
    crate::syntax::is_keyword(s) || matches!(s, "true" | "false" | "unit")
}

/// A map-literal key before its entry is built: a static label or a `$name`
/// resolved at runtime.
enum MapKeyForm {
    Static(MapKey),
    Deref(String),
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::tilde::TildePath;

    fn plain(s: &str) -> Ast {
        Ast::Word(Word::Plain(s.into()))
    }

    /// Span-free `Spanned`.  These fixtures compare shapes, never positions,
    /// so both sides of every assertion are normalised to `None` spans.
    fn sp(a: Ast) -> Spanned<Ast> {
        Spanned::synthetic(a)
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

    fn app(head: Head, args: Vec<Ast>) -> Ast {
        Ast::Call {
            head,
            args: args.into_iter().map(Spanned::synthetic).collect(),
            redirects: vec![],
        }
    }

    fn app_redir(head: Head, args: Vec<Ast>, redirects: Vec<Redirect>) -> Ast {
        Ast::Call {
            head,
            args: args.into_iter().map(Spanned::synthetic).collect(),
            redirects,
        }
    }

    /// Bare `Ast`s in the `Vec<Stmt>` shape a block, lambda, or pipeline body
    /// demands.
    fn body(asts: Vec<Ast>) -> Vec<Stmt> {
        asts.into_iter().map(Spanned::synthetic).collect()
    }

    /// A parsed program as the bare `Vec<Ast>` the fixtures are written in.
    fn unwrap_stmts(stmts: Vec<Stmt>) -> Vec<Ast> {
        stmts.into_iter().map(|s| strip_one(s.item)).collect()
    }

    /// Like [`unwrap_stmts`], but for a nested body, where the `Stmt` wrapper
    /// has to stay for the expected shape to typecheck.
    fn strip_stmts(stmts: Vec<Stmt>) -> Vec<Stmt> {
        stmts
            .into_iter()
            .map(|s| Spanned::synthetic(strip_one(s.item)))
            .collect()
    }

    fn strip_args(args: Vec<Ast>) -> Vec<Ast> {
        args.into_iter().map(strip_one).collect()
    }

    fn strip_spanned_args(args: Vec<Spanned<Ast>>) -> Vec<Spanned<Ast>> {
        args.into_iter()
            .map(|sp| Spanned::synthetic(strip_one(sp.item)))
            .collect()
    }

    fn strip_head(head: Head) -> Head {
        match head {
            Head::Value(ast) => Head::Value(Box::new(strip_one(*ast))),
            other => other,
        }
    }

    /// Drop every span and unwrap a lone head out of its `Call`, so a fixture
    /// can name the shape without predicting byte positions.
    fn strip_one(n: Ast) -> Ast {
        match n {
            Ast::Call {
                head,
                args,
                redirects,
            } if args.is_empty() && redirects.is_empty() => match head {
                Head::Bare(s) => plain(&s),
                Head::Path(s) => Ast::Word(Word::Slash(s)),
                Head::TildePath(path) => tilde_word(path),
                Head::Value(ast) => strip_one(*ast),
                Head::ExternalName(s) => app(Head::ExternalName(s), vec![]),
            },
            Ast::Return(None) => Ast::Return(None),
            Ast::Return(Some(value)) => {
                Ast::Return(Some(Spanned::synthetic_boxed(strip_one(*value.item))))
            }
            Ast::Index { target, keys } => Ast::Index {
                target: Spanned::synthetic_boxed(strip_one(*target.item)),
                keys: strip_spanned_args(keys),
            },
            Ast::Call {
                head,
                args,
                redirects,
            } => {
                let plain_args: Vec<Ast> = args.into_iter().map(|sp| sp.item).collect();
                app_redir(strip_head(head), strip_args(plain_args), redirects)
            }
            Ast::Scope { op, redirects } => Ast::Scope {
                op: strip_scope(op),
                redirects,
            },
            Ast::Block(body) => Ast::Block(strip_stmts(body)),
            Ast::Lambda { param, body } => Ast::Lambda {
                param: Spanned::synthetic(param.item),
                body: strip_stmts(body),
            },
            Ast::Pipeline(stages) => Ast::Pipeline(strip_stmts(stages)),
            Ast::Chain(parts) => Ast::Chain(strip_spanned_args(parts)),
            Ast::Interpolation(parts) => Ast::Interpolation(strip_spanned_args(parts)),
            Ast::List(elems) => Ast::List(
                elems
                    .into_iter()
                    .map(|e| match e {
                        ListElem::Single(a) => {
                            ListElem::Single(Spanned::synthetic(strip_one(a.item)))
                        }
                        ListElem::Spread(a) => {
                            ListElem::Spread(Spanned::synthetic(strip_one(a.item)))
                        }
                    })
                    .collect(),
            ),
            Ast::Map(entries) => Ast::Map(
                entries
                    .into_iter()
                    .map(|e| match e {
                        MapEntry::Entry { key, value } => MapEntry::Entry {
                            key,
                            value: Spanned::synthetic(strip_one(value.item)),
                        },
                        MapEntry::Deref { name, value } => MapEntry::Deref {
                            name,
                            value: Spanned::synthetic(strip_one(value.item)),
                        },
                        MapEntry::Spread(a) => {
                            MapEntry::Spread(Spanned::synthetic(strip_one(a.item)))
                        }
                    })
                    .collect(),
            ),
            Ast::Background(value) => {
                Ast::Background(Spanned::synthetic_boxed(strip_one(*value.item)))
            }
            Ast::Force(value) => Ast::Force(Spanned::synthetic_boxed(strip_one(*value.item))),
            Ast::Let { pattern, value } => Ast::Let {
                pattern: Spanned::synthetic(pattern.item),
                value: Spanned::synthetic_boxed(strip_one(*value.item)),
            },
            Ast::If { branches, else_ } => Ast::If {
                branches: branches
                    .into_iter()
                    .map(|b| IfBranch {
                        cond: Spanned::synthetic_boxed(strip_one(*b.cond.item)),
                        body: Spanned::synthetic_boxed(strip_one(*b.body.item)),
                    })
                    .collect(),
                else_: else_.map(|e| Spanned::synthetic_boxed(strip_one(*e.item))),
            },
            Ast::Case { scrutinee, table } => Ast::Case {
                scrutinee: Spanned::synthetic_boxed(strip_one(*scrutinee.item)),
                table: Spanned::synthetic_boxed(strip_one(*table.item)),
            },
            Ast::Tag { label, payload } => Ast::Tag {
                label,
                payload: payload.map(|p| Spanned::synthetic_boxed(strip_one(*p.item))),
            },
            Ast::Spread(value) => Ast::Spread(Spanned::synthetic_boxed(strip_one(*value.item))),
            other => other,
        }
    }

    fn strip_scope(op: ScopeAst) -> ScopeAst {
        let s = |a: Box<Ast>| Box::new(strip_one(*a));
        match op {
            ScopeAst::Try { body, handler } => ScopeAst::Try {
                body: s(body),
                handler: s(handler),
            },
            ScopeAst::Guard { body, cleanup } => ScopeAst::Guard {
                body: s(body),
                cleanup: s(cleanup),
            },
            ScopeAst::Within { opts, body } => ScopeAst::Within {
                opts: s(opts),
                body: s(body),
            },
            ScopeAst::Grant { caps, body } => ScopeAst::Grant {
                caps: s(caps),
                body: s(body),
            },
            ScopeAst::Audit { body } => ScopeAst::Audit { body: s(body) },
        }
    }

    #[test]
    fn parse_simple_command() {
        let ast = unwrap_stmts(parse("echo hello").unwrap());
        assert_eq!(ast, vec![app(bare_head("echo"), vec![plain("hello")])]);
    }

    #[test]
    fn parse_variable() {
        let ast = unwrap_stmts(parse("echo $x").unwrap());
        assert_eq!(
            ast,
            vec![app(bare_head("echo"), vec![Ast::Variable("x".into())])]
        );
    }

    #[test]
    fn parse_explicit_value_head_application() {
        let ast = unwrap_stmts(parse("$map $upper ['a']").unwrap());
        assert_eq!(
            ast,
            vec![app(
                value_head(Ast::Variable("map".into())),
                vec![
                    Ast::Variable("upper".into()),
                    Ast::List(vec![ListElem::Single(sp(Ast::Literal("a".into())))]),
                ],
            )]
        );
    }

    #[test]
    fn parse_explicit_value_head_without_args_remains_value() {
        let ast = unwrap_stmts(parse("$map").unwrap());
        assert_eq!(ast, vec![Ast::Variable("map".into())]);
    }

    #[test]
    fn parse_external_name_head_application() {
        let ast = unwrap_stmts(parse("^git status").unwrap());
        assert_eq!(ast, vec![app(external_head("git"), vec![plain("status")])]);
    }

    #[test]
    fn parse_external_name_head_without_args() {
        let ast = parse("^git").unwrap();
        match ast.as_slice() {
            [
                Stmt {
                    item: Ast::Call { head, args, .. },
                    ..
                },
            ] => {
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
        let ast = unwrap_stmts(parse("let x = hello").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Spanned::synthetic(Pattern::Name("x".into())),
                value: Spanned::synthetic_boxed(plain("hello")),
            }]
        );
    }

    #[test]
    fn parse_pipeline() {
        let ast = unwrap_stmts(parse("echo hello | upper").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Pipeline(body(vec![
                app(bare_head("echo"), vec![plain("hello")]),
                plain("upper"),
            ]))]
        );
    }

    #[test]
    fn parse_pipeline_quoted_literal_stage() {
        let ast = unwrap_stmts(parse("'abc' | blah").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Pipeline(body(vec![
                Ast::Literal("abc".into()),
                plain("blah"),
            ]))]
        );
    }

    #[test]
    fn parse_chain() {
        let ast = unwrap_stmts(parse("return true ? echo yes").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Chain(vec![
                sp(Ast::Return(Some(Spanned::synthetic_boxed(plain("true"))))),
                sp(app(bare_head("echo"), vec![plain("yes")])),
            ])]
        );
    }

    #[test]
    fn parse_let_rhs_chain() {
        // The whole chain binds to `x`, not just `a`.
        let ast = unwrap_stmts(parse("let x = a ? b").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Spanned::synthetic(Pattern::Name("x".into())),
                value: Spanned::synthetic_boxed(Ast::Chain(vec![sp(plain("a")), sp(plain("b")),])),
            }]
        );
    }

    #[test]
    fn parse_let_rhs_trailing_amp_backgrounds_whole_chain() {
        // The `&` wraps the whole chain, never just the last arm.
        let ast = unwrap_stmts(parse("let x = a ? b &").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Spanned::synthetic(Pattern::Name("x".into())),
                value: Spanned::synthetic_boxed(Ast::Background(Spanned::synthetic_boxed(
                    Ast::Chain(vec![sp(plain("a")), sp(plain("b")),])
                ),)),
            }]
        );
    }

    #[test]
    fn parse_let_rhs_per_arm_amp_rejected() {
        // A per-arm `&` belongs to statement chains only, so the RHS ends at
        // `a &` and the `?` is left stray.
        assert!(parse("let x = a & ? b").is_err());
    }

    #[test]
    fn parse_chain_arms_may_background_at_stmt_level() {
        // Where, at statement level, it is exactly what is allowed.
        let ast = unwrap_stmts(parse("a & ? b &").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Chain(vec![
                sp(Ast::Background(Spanned::synthetic_boxed(plain("a")))),
                sp(Ast::Background(Spanned::synthetic_boxed(plain("b")))),
            ])]
        );
    }

    #[test]
    fn parse_let_after_pipe_rejected() {
        let err = parse("cmd | let x = y").unwrap_err();
        assert!(
            err.message.contains("`let`"),
            "expected let-placement error, got: {}",
            err.message
        );
    }

    #[test]
    fn parse_let_after_question_rejected() {
        let err = parse("cmd ? let x = y").unwrap_err();
        assert!(
            err.message.contains("`let`"),
            "expected let-placement error, got: {}",
            err.message
        );
    }

    // ── Sub-token-stream parsers require EOF ─────────────────────────

    #[test]
    fn expr_block_rejects_trailing_input() {
        let err = parse("echo $[1 2 3]").unwrap_err();
        assert!(
            err.message.contains("trailing input"),
            "expected trailing-input error, got: {}",
            err.message
        );
    }

    /// `$[2>3]` lexes `2>` as a file descriptor, so `>` lands in operand
    /// position; the error must name the glued shape, not "redirect".
    #[test]
    fn glued_comparison_suggests_spacing() {
        let err = parse("return $[2>3]").unwrap_err();
        assert!(
            err.message.contains("`2>`") && err.message.contains("with spaces"),
            "expected a spacing hint naming `2>`, got: {}",
            err.message
        );

        // No glued digit to blame, so the message names the comparison.
        let err = parse("return $[< 3]").unwrap_err();
        assert!(
            err.message.contains("comparison operator"),
            "expected a comparison-operator error, got: {}",
            err.message
        );
    }

    /// A key stream holds one word; `$m[a b]` must reject, not index by `a`.
    #[test]
    fn index_keys_reject_trailing_input() {
        let err = parse("$m[a b]").unwrap_err();
        assert!(
            err.message.contains("trailing input"),
            "expected trailing-input error, got: {}",
            err.message
        );
    }

    /// A stray `}` is an error, not a stop that truncates the program.
    #[test]
    fn top_level_stray_rbrace_rejected() {
        let err = parse("echo a\n}\necho b").unwrap_err();
        assert!(
            err.message.contains("unmatched `}`"),
            "expected an unmatched-brace error, got: {}",
            err.message
        );
    }

    /// With statements after the stray brace, "trailing input" would be doubly
    /// wrong: it is not trailing, and the parse has not completed.
    #[test]
    fn mid_program_stray_rbrace_names_unmatched_brace() {
        let err = parse("{ let x = 1 } } let y = 2").unwrap_err();
        assert!(
            err.message.contains("unmatched `}`"),
            "expected an unmatched-brace error, got: {}",
            err.message
        );
    }

    #[test]
    fn well_formed_block_still_parses() {
        let ast = unwrap_stmts(parse("echo a; { echo b }").unwrap());
        assert_eq!(
            ast,
            vec![
                app(bare_head("echo"), vec![plain("a")]),
                Ast::Block(body(vec![app(bare_head("echo"), vec![plain("b")])])),
            ]
        );
    }

    #[test]
    fn parse_chain_continues_across_newline_before_question() {
        let ast = unwrap_stmts(parse("a\n? b").unwrap());
        assert_eq!(ast, vec![Ast::Chain(vec![sp(plain("a")), sp(plain("b"))])]);
    }

    #[test]
    fn parse_pipeline_continues_across_newline_before_pipe() {
        let ast = unwrap_stmts(parse("a\n| b").unwrap());
        assert_eq!(ast, vec![Ast::Pipeline(body(vec![plain("a"), plain("b")]))]);
    }

    #[test]
    fn parse_block_stmt() {
        let ast = unwrap_stmts(parse("{ echo hello }").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Block(body(vec![app(
                bare_head("echo"),
                vec![plain("hello")]
            )]))]
        );
    }

    #[test]
    fn parse_lambda_arg() {
        let ast = unwrap_stmts(parse("echo { |x| echo $x }").unwrap());
        assert_eq!(
            ast,
            vec![app(
                bare_head("echo"),
                vec![Ast::Lambda {
                    param: Spanned::synthetic(Pattern::Name("x".into())),
                    body: body(vec![app(
                        bare_head("echo"),
                        vec![Ast::Variable("x".into())]
                    )]),
                }],
            )]
        );
    }

    #[test]
    fn parse_return_stage() {
        let ast = unwrap_stmts(parse("return $x").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Return(Some(Spanned::synthetic_boxed(Ast::Variable(
                "x".into()
            ),)))]
        );
    }

    #[test]
    fn parse_return_unit_stage() {
        let ast = unwrap_stmts(parse("return").unwrap());
        assert_eq!(ast, vec![Ast::Return(None)]);
    }

    #[test]
    fn parse_return_force_argument() {
        let ast = unwrap_stmts(parse("return !{hostname}").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Return(Some(Spanned::synthetic_boxed(Ast::Force(
                Spanned::synthetic_boxed(Ast::Block(body(vec![plain("hostname"),])))
            ),)))]
        );
    }

    #[test]
    fn parse_list() {
        let ast = unwrap_stmts(parse("return [a, b, c]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Return(Some(Spanned::synthetic_boxed(Ast::List(
                vec![
                    ListElem::Single(sp(plain("a"))),
                    ListElem::Single(sp(plain("b"))),
                    ListElem::Single(sp(plain("c"))),
                ]
            ),)))]
        );
    }

    #[test]
    fn parse_map() {
        let ast = unwrap_stmts(parse("return [host: localhost, port: 8080]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Return(Some(Spanned::synthetic_boxed(Ast::Map(vec![
                MapEntry::Entry {
                    key: MapKey::Bare("host".into()),
                    value: sp(plain("localhost")),
                },
                MapEntry::Entry {
                    key: MapKey::Bare("port".into()),
                    value: sp(plain("8080")),
                },
            ]),)))]
        );
    }

    #[test]
    fn parse_command_substitution() {
        let ast = unwrap_stmts(parse("let name = !{hostname}").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Spanned::synthetic(Pattern::Name("name".into())),
                value: Spanned::synthetic_boxed(Ast::Force(Spanned::synthetic_boxed(Ast::Block(
                    body(vec![plain("hostname")])
                )),)),
            }]
        );
    }

    #[test]
    fn parse_arithmetic() {
        let ast = unwrap_stmts(parse("$[2 + 3]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Expr(Box::new(Expr::BinOp(
                Box::new(Expr::Integer(2)),
                BinaryOp::Add,
                Box::new(Expr::Integer(3)),
            )))]
        );
    }

    #[test]
    fn parse_arithmetic_precedence() {
        let ast = unwrap_stmts(parse("$[2 + 3 * 4]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Expr(Box::new(Expr::BinOp(
                Box::new(Expr::Integer(2)),
                BinaryOp::Add,
                Box::new(Expr::BinOp(
                    Box::new(Expr::Integer(3)),
                    BinaryOp::Mul,
                    Box::new(Expr::Integer(4)),
                )),
            )))]
        );
    }

    /// A float needs a `.`, so these three stay strings inside `$[…]` too.
    #[test]
    fn expr_atom_rejects_non_dotted_float_shapes() {
        for s in ["$[nan]", "$[inf]", "$[1e5]"] {
            let err = parse(s).unwrap_err();
            assert!(
                err.message.contains("expected a number"),
                "{s:?} should be rejected as a number, got: {}",
                err.message
            );
        }
    }

    #[test]
    fn expr_atom_accepts_dotted_float() {
        let ast = unwrap_stmts(parse("$[1.5]").unwrap());
        assert_eq!(ast, vec![Ast::Expr(Box::new(Expr::Number(1.5)))]);
    }

    /// `not $x == 0` is `(not $x) == 0`, never `not ($x == 0)`.
    #[test]
    fn not_binds_tighter_than_binary_op() {
        let ast = unwrap_stmts(parse("$[not $x == 0]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Expr(Box::new(Expr::BinOp(
                Box::new(Expr::Not(Box::new(Expr::Variable("x".into())))),
                BinaryOp::Eq,
                Box::new(Expr::Integer(0)),
            )))]
        );
    }

    #[test]
    fn parse_index() {
        let ast = unwrap_stmts(parse("echo $items[0]").unwrap());
        assert_eq!(
            ast,
            vec![app(
                bare_head("echo"),
                vec![Ast::Index {
                    target: Spanned::synthetic_boxed(Ast::Variable("items".into())),
                    keys: vec![Spanned::synthetic(plain("0"))],
                }],
            )]
        );
    }

    #[test]
    fn parse_postfix_index_on_list_literal() {
        let ast = unwrap_stmts(parse("return ['a'][0]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Return(Some(Spanned::synthetic_boxed(Ast::Index {
                target: Spanned::synthetic_boxed(Ast::List(vec![ListElem::Single(sp(
                    Ast::Literal("a".into())
                )),])),
                keys: vec![Spanned::synthetic(plain("0"))],
            },)))]
        );
    }

    #[test]
    fn parse_interpolation() {
        let ast = unwrap_stmts(parse("echo \"hello $name\"").unwrap());
        assert_eq!(
            ast,
            vec![app(
                bare_head("echo"),
                vec![Ast::Interpolation(vec![
                    sp(Ast::Literal("hello ".into())),
                    sp(Ast::Variable("name".into())),
                ])],
            )]
        );
    }

    #[test]
    fn parse_multiple_stmts() {
        let ast = unwrap_stmts(parse("x = 5\necho $x").unwrap());
        assert_eq!(ast.len(), 2);
    }

    #[test]
    fn parse_destructuring() {
        let ast = unwrap_stmts(parse("let [first, second] = [a, b]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Spanned::synthetic(Pattern::List {
                    elems: vec![
                        Pattern::Name("first".into()),
                        Pattern::Name("second".into()),
                    ],
                    rest: None,
                }),
                value: Spanned::synthetic_boxed(Ast::List(vec![
                    ListElem::Single(sp(plain("a"))),
                    ListElem::Single(sp(plain("b"))),
                ])),
            }]
        );
    }

    #[test]
    fn parse_rest_pattern() {
        let ast = unwrap_stmts(parse("let [head, ...rest] = $list").unwrap());
        match &ast[0] {
            Ast::Let { pattern, .. } => {
                assert_eq!(
                    pattern.item,
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
    fn rest_pattern_name_rejects_reserved_keyword() {
        let err = parse("let [...try] = $xs").unwrap_err();
        assert!(
            err.message.contains("reserved keyword"),
            "rest-pattern name should enforce the reserved-name guard: {err:?}"
        );
    }

    #[test]
    fn parse_wildcard_pattern() {
        let ast = unwrap_stmts(parse("let _ = hello").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Spanned::synthetic(Pattern::Wildcard),
                value: Spanned::synthetic_boxed(plain("hello")),
            }]
        );
    }

    #[test]
    fn parse_wildcard_in_destructuring() {
        let ast = unwrap_stmts(parse("let [_, x] = [a, b]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Spanned::synthetic(Pattern::List {
                    elems: vec![Pattern::Wildcard, Pattern::Name("x".into())],
                    rest: None,
                }),
                value: Spanned::synthetic_boxed(Ast::List(vec![
                    ListElem::Single(sp(plain("a"))),
                    ListElem::Single(sp(plain("b"))),
                ])),
            }]
        );
    }

    #[test]
    fn parse_command_with_lambda_arg() {
        let ast = unwrap_stmts(parse("for $items { |x| echo $x }").unwrap());
        match &ast[0] {
            Ast::Call { head, args, .. } => {
                assert_eq!(head, &bare_head("for"));
                assert_eq!(args.len(), 2); // $items and the lambda
                assert!(matches!(args[0].item, Ast::Variable(_)));
                assert!(matches!(args[1].item, Ast::Lambda { .. }));
            }
            _ => panic!("expected command"),
        }
    }

    #[test]
    fn parse_spread_in_list() {
        let ast = unwrap_stmts(parse("return [...$a, b]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Return(Some(Spanned::synthetic_boxed(Ast::List(
                vec![
                    ListElem::Spread(sp(Ast::Variable("a".into()))),
                    ListElem::Single(sp(plain("b"))),
                ]
            ),)))]
        );
    }

    #[test]
    fn parse_empty_map() {
        let ast = unwrap_stmts(parse("[:]").unwrap());
        assert_eq!(ast, vec![Ast::Map(vec![])]);
    }

    #[test]
    fn parse_empty_list() {
        let ast = unwrap_stmts(parse("[]").unwrap());
        assert_eq!(ast, vec![Ast::List(vec![])]);
    }

    #[test]
    fn parse_leading_spread_disambiguates_to_map() {
        // The `key: val` pair sits past the spread, where the lookahead has to
        // reach to call this a map.
        let ast = unwrap_stmts(parse("[...$d, k: 'v']").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Map(vec![
                MapEntry::Spread(sp(Ast::Variable("d".into()))),
                MapEntry::Entry {
                    key: MapKey::Bare("k".into()),
                    value: sp(Ast::Literal("v".into())),
                },
            ])]
        );
    }

    /// The inner `]` of the spread operand must not be read as the outer
    /// collection's close, or `[...[a: 1], b: 2]` would parse as a list.
    #[test]
    fn parse_leading_spread_of_nested_collection_disambiguates_to_map() {
        let ast = unwrap_stmts(parse("[...[a: 1], b: 2]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Map(vec![
                MapEntry::Spread(sp(Ast::Map(vec![MapEntry::Entry {
                    key: MapKey::Bare("a".into()),
                    value: sp(plain("1")),
                }]))),
                MapEntry::Entry {
                    key: MapKey::Bare("b".into()),
                    value: sp(plain("2")),
                },
            ])]
        );
    }

    /// Unterminated, the same lookahead must error rather than hang.
    #[test]
    fn parse_unterminated_leading_spread_errors_without_hang() {
        assert!(parse("[...[a: 1").is_err());
    }

    #[test]
    fn parse_leading_spread_disambiguates_to_list() {
        // Past the spread sits a bare element, so this one is a list.
        let ast = unwrap_stmts(parse("[...$xs, a]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::List(vec![
                ListElem::Spread(sp(Ast::Variable("xs".into()))),
                ListElem::Single(sp(plain("a"))),
            ])]
        );
    }

    #[test]
    fn parse_map_with_blocks() {
        // Standalone: a multiline map is a value, not a command.
        let src1 = "[\n    quit: { echo q },\n    help: { echo h },\n]";
        let ast1 = unwrap_stmts(parse(src1).unwrap());
        assert_eq!(ast1.len(), 1);
        assert!(matches!(&ast1[0], Ast::Map(_)));

        // And the same map in argument position.
        let src = "dispatch $action [\n    quit: { echo quitting },\n    help: { echo help },\n    _: { echo unknown },\n]";
        let ast = unwrap_stmts(parse(src).unwrap());
        match &ast[0] {
            Ast::Call { head, args, .. } => {
                assert_eq!(head, &bare_head("dispatch"));
                assert_eq!(args.len(), 2);
                assert!(matches!(&args[1].item, Ast::Map(_)));
            }
            _ => panic!("expected command, got {:?}", ast[0]),
        }
    }

    #[test]
    fn parse_newline_separates_statements_in_block_inside_map() {
        let src = "return [prompt: { let x = hi\nreturn \"$x> \" }]";
        let ast = unwrap_stmts(parse(src).unwrap());
        match &ast[0] {
            Ast::Return(Some(val)) => match val.item.as_ref() {
                Ast::Map(entries) => {
                    assert_eq!(entries.len(), 1);
                    let MapEntry::Entry { value, .. } = &entries[0] else {
                        panic!("expected map entry");
                    };
                    let Ast::Block(stmts) = &value.item else {
                        panic!("expected block in prompt entry");
                    };
                    assert!(matches!(stmts[0].item, Ast::Let { .. }));
                    assert!(matches!(stmts[1].item, Ast::Return(Some(_))));
                }
                _ => panic!("expected map"),
            },
            _ => panic!("expected return map"),
        }
    }

    #[test]
    fn parse_if_else_blocks_across_newline_with_explicit_else() {
        let src = "return [aliases: [ls: { |args| if $is-mac { echo a }\nelse { echo b } }]]";
        let ast = unwrap_stmts(parse(src).unwrap());
        let Ast::Return(Some(val)) = &ast[0] else {
            panic!("expected return");
        };
        let Ast::Map(entries) = val.item.as_ref() else {
            panic!("expected map");
        };
        let MapEntry::Entry {
            value: aliases_val, ..
        } = &entries[0]
        else {
            panic!("expected aliases entry");
        };
        let Ast::Map(alias_entries) = &aliases_val.item else {
            panic!("expected aliases map");
        };
        let MapEntry::Entry { value: ls_val, .. } = &alias_entries[0] else {
            panic!("expected ls entry");
        };
        let Ast::Lambda { body, .. } = &ls_val.item else {
            panic!("expected lambda");
        };
        assert_eq!(body.len(), 1);
        let Ast::If { branches, else_ } = &body[0].item else {
            panic!("expected Ast::If, got {:?}", body[0]);
        };
        assert_eq!(branches.len(), 1);
        assert!(matches!(branches[0].cond.item.as_ref(), Ast::Variable(s) if s == "is-mac"));
        assert!(matches!(branches[0].body.item.as_ref(), Ast::Block(_)));
        assert!(matches!(
            else_.as_ref().map(|b| b.item.as_ref()),
            Some(Ast::Block(_))
        ));
    }

    #[test]
    fn parse_redirect() {
        let ast = unwrap_stmts(parse("echo hello > out.txt").unwrap());
        match &ast[0] {
            Ast::Call {
                args, redirects, ..
            } => {
                assert_eq!(args.len(), 1);
                assert_eq!(redirects.len(), 1);
            }
            _ => panic!("expected command"),
        }
    }

    #[test]
    fn parse_herestring_redirect() {
        let ast = unwrap_stmts(parse("cat << #'body'#").unwrap());
        match &ast[0] {
            Ast::Call { redirects, .. } => {
                assert_eq!(redirects.len(), 1);
                assert_eq!(redirects[0].fd, 0);
                assert_eq!(redirects[0].mode, RedirectMode::HereString);
                assert_eq!(
                    redirects[0].target,
                    RedirectTarget::File(Box::new(Ast::Literal("body".into())))
                );
            }
            other => panic!("expected command, got {other:?}"),
        }
    }

    /// A here-string payload takes any value form a redirect operand does.
    #[test]
    fn parse_herestring_variable_payload() {
        let ast = unwrap_stmts(parse("cat << $body").unwrap());
        match &ast[0] {
            Ast::Call { redirects, .. } => {
                assert_eq!(redirects[0].mode, RedirectMode::HereString);
                assert_eq!(
                    redirects[0].target,
                    RedirectTarget::File(Box::new(Ast::Variable("body".into())))
                );
            }
            other => panic!("expected command, got {other:?}"),
        }
    }

    /// The bash-heredoc reflex earns an error naming the raw-string form, not
    /// a silent feed of the literal word `EOF`.
    #[test]
    fn herestring_bare_word_is_rejected() {
        for src in ["cat <<EOF", "cat << EOF"] {
            let err = parse(src).expect_err("bare word after `<<` must not parse");
            assert!(
                err.message.contains("ral has no heredocs") && err.message.contains("#' ... '#"),
                "for {src:?} got: {}",
                err.message
            );
        }
    }

    /// A path after `<<` gets the `< path` correction instead.
    #[test]
    fn herestring_path_word_is_rejected() {
        let err = parse("cat << ./body.txt").expect_err("path after `<<` must not parse");
        assert!(err.message.contains("use `< path`"), "got: {}", err.message);
    }

    /// `<<` always feeds stdin: fd 0 may be spelled out, anything else errors.
    #[test]
    fn herestring_fd_prefix() {
        let ast = unwrap_stmts(parse("cat 0<< #'x'#").unwrap());
        match &ast[0] {
            Ast::Call { redirects, .. } => assert_eq!(redirects[0].fd, 0),
            other => panic!("expected command, got {other:?}"),
        }
        let err = parse("cat 3<< #'x'#").expect_err("fd 3 herestring must not parse");
        assert!(
            err.message.contains("always feeds stdin"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn parse_tilde() {
        let ast = unwrap_stmts(parse("~").unwrap());
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
        let ast = unwrap_stmts(parse("cd ~").unwrap());
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
        let ast = unwrap_stmts(parse("~root").unwrap());
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
        let ast = unwrap_stmts(parse("~/foo/bar").unwrap());
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
            [
                Stmt {
                    item: Ast::Call { head, args, .. },
                    ..
                },
            ] => {
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
        let ast = unwrap_stmts(parse("[1,2]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::List(vec![
                ListElem::Single(sp(plain("1"))),
                ListElem::Single(sp(plain("2"))),
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
            [
                Stmt {
                    item: Ast::Call { head, args, .. },
                    ..
                },
            ] => {
                assert!(args.is_empty());
                assert_eq!(head, &path_head("./script"));
            }
            _ => panic!("expected zero-arg path app, got {ast:?}"),
        }
    }

    #[test]
    fn parse_tilde_with_space_is_bare() {
        // The space cuts the tilde loose: `foo` is a second argument, not a
        // suffix on `~`.
        let ast = unwrap_stmts(parse("echo ~ foo").unwrap());
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
        let ast = unwrap_stmts(parse("{ { echo inner } }").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Block(body(vec![Ast::Block(body(vec![app(
                bare_head("echo"),
                vec![plain("inner")]
            )]))]))]
        );
    }

    // ── Recursion-depth chokepoints ─────────────────────────────────────
    //
    // These cover the two sub-grammars that do not route through
    // `parse_primary` and so guard themselves.  Each uses a depth well past
    // the cap but far short of any real stack ceiling, so a lost guard shows
    // up as a missing error rather than a crash.

    #[test]
    fn deeply_nested_pattern_hits_nesting_cap() {
        let n = 200;
        let src = format!("let {}a{} = x", "[".repeat(n), "]".repeat(n));
        let err = parse(&src).unwrap_err();
        assert!(
            err.message.contains("too deep"),
            "deep pattern nesting should hit the cap, got: {}",
            err.message
        );
    }

    #[test]
    fn deeply_nested_unary_minus_hits_nesting_cap() {
        let src = format!("$[{}1]", "- ".repeat(200));
        let err = parse(&src).unwrap_err();
        assert!(
            err.message.contains("too deep"),
            "deep unary-minus nesting should hit the cap, got: {}",
            err.message
        );
    }

    #[test]
    fn deeply_nested_not_hits_nesting_cap() {
        let src = format!("$[{}$x]", "not ".repeat(200));
        let err = parse(&src).unwrap_err();
        assert!(
            err.message.contains("too deep"),
            "deep `not` nesting should hit the cap, got: {}",
            err.message
        );
    }

    #[test]
    fn parse_force_stmt_still_allowed() {
        let ast = unwrap_stmts(parse("!{echo hello}").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Force(Spanned::synthetic_boxed(Ast::Block(body(
                vec![app(bare_head("echo"), vec![plain("hello")])]
            ),)))]
        );
    }

    #[test]
    fn bare_bang_is_not_a_literal_word() {
        assert!(parse("echo !").is_err());
    }

    #[test]
    fn let_rhs_on_next_line() {
        let ast = unwrap_stmts(parse("let x =\necho hi").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Spanned::synthetic(Pattern::Name("x".into())),
                value: Spanned::synthetic_boxed(app(bare_head("echo"), vec![plain("hi")],)),
            }]
        );
    }

    #[test]
    fn let_rhs_on_next_line_multiple_newlines() {
        let ast = unwrap_stmts(parse("let x =\n\necho hi").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Spanned::synthetic(Pattern::Name("x".into())),
                value: Spanned::synthetic_boxed(app(bare_head("echo"), vec![plain("hi")],)),
            }]
        );
    }

    #[test]
    fn let_destructure_rhs_on_next_line() {
        let ast = unwrap_stmts(parse("let [a, b] =\n[1, 2]").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Spanned::synthetic(Pattern::List {
                    elems: vec![Pattern::Name("a".into()), Pattern::Name("b".into())],
                    rest: None,
                }),
                value: Spanned::synthetic_boxed(Ast::List(vec![
                    ListElem::Single(sp(plain("1"))),
                    ListElem::Single(sp(plain("2"))),
                ])),
            }]
        );
    }

    #[test]
    fn let_rhs_chain_continues_before_question() {
        let ast = unwrap_stmts(parse("let x = echo hi\n? echo bye").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Let {
                pattern: Spanned::synthetic(Pattern::Name("x".into())),
                value: Spanned::synthetic_boxed(Ast::Chain(vec![
                    sp(app(bare_head("echo"), vec![plain("hi")])),
                    sp(app(bare_head("echo"), vec![plain("bye")])),
                ])),
            }]
        );
    }

    #[test]
    fn pipeline_continuation_after_pipe() {
        let ast = unwrap_stmts(parse("echo hello |\nupper").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Pipeline(body(vec![
                app(bare_head("echo"), vec![plain("hello")]),
                plain("upper"),
            ]))]
        );
    }

    #[test]
    fn pipeline_continuation_before_pipe() {
        let ast = unwrap_stmts(parse("echo hello\n| upper").unwrap());
        assert_eq!(
            ast,
            vec![Ast::Pipeline(body(vec![
                app(bare_head("echo"), vec![plain("hello")]),
                plain("upper"),
            ]))]
        );
    }

    #[test]
    fn newline_terminates_command_args() {
        // Two statements, not one command with two arguments.
        let ast = unwrap_stmts(parse("echo hello\nworld").unwrap());
        assert_eq!(ast.len(), 2);
    }

    #[test]
    fn caret_is_not_a_continuation_token() {
        assert!(!needs_continuation("^"));
    }

    #[test]
    fn needs_continuation_on_unterminated_string() {
        assert!(needs_continuation("\"foo"));
    }

    #[test]
    fn needs_continuation_on_unterminated_string_with_inner_force() {
        // Nested unclosed forms are still one verdict, not a competing pair.
        assert!(needs_continuation("\"foo !{cmd"));
    }

    #[test]
    fn complete_program_does_not_need_continuation() {
        assert!(!needs_continuation("echo done"));
    }

    /// The lexer calls an unbalanced top-level `{` or `[` an unterminated
    /// delimiter, which reaches the REPL as a request for another line.
    #[test]
    fn needs_continuation_on_unbalanced_open_delimiters() {
        assert!(needs_continuation("let f = {"));
        assert!(needs_continuation("return [a, b"));
        assert!(needs_continuation("if true {"));
    }

    #[test]
    fn balanced_delimiters_do_not_need_continuation() {
        assert!(!needs_continuation("let f = { return 1 }"));
        assert!(!needs_continuation("return [a, b]"));
    }

    /// A comment running to end of input must not mask the open delimiter
    /// before it.
    #[test]
    fn needs_continuation_on_open_delim_then_comment_to_eof() {
        assert!(needs_continuation("let f = {# comment"));
        assert!(needs_continuation("return [a, b # comment"));
        assert!(needs_continuation("{# comment"));
        assert!(needs_continuation("[# comment"));
    }

    #[test]
    fn balanced_program_with_trailing_comment_does_not_need_continuation() {
        assert!(!needs_continuation("let f = { return 1 } # done"));
        assert!(!needs_continuation("echo done # done"));
    }

    #[test]
    fn needs_continuation_on_let_awaiting_rhs() {
        assert!(needs_continuation("let x ="));
        assert!(needs_continuation("let [a, b] ="));
    }

    /// A trailing `=` outside a binder is a plain-word argument, and a line
    /// that already parses must never ask for another.
    #[test]
    fn trailing_bare_equals_does_not_need_continuation() {
        assert!(parse("x =").is_ok());
        assert!(!needs_continuation("x ="));
        assert!(parse("echo a =").is_ok());
        assert!(!needs_continuation("echo a ="));
    }

    #[test]
    fn needs_continuation_on_dangling_continuation_token() {
        assert!(needs_continuation("echo hi |"));
        assert!(needs_continuation("echo a ?"));
        assert!(needs_continuation("if"));
        assert!(needs_continuation("if true x\nelsif"));
        assert!(needs_continuation("if true x\nelse"));
        // Condition parsed, body demanded, input gone.
        assert!(needs_continuation("if $c"));
        assert!(needs_continuation("if true a\nelsif $c"));
    }

    #[test]
    fn if_same_line_bare_block_is_error() {
        // A third block on the same line wants the `else` keyword.
        let err = parse("if $c { a } { b }").unwrap_err();
        assert!(
            err.message.contains("else"),
            "error should hint at `else`: {err:?}"
        );
    }

    #[test]
    fn if_newline_block_is_valid() {
        // On the next line it is a statement of its own.
        assert!(parse("if $c { a }\n{ b }").is_ok());
    }

    #[test]
    fn if_with_else_keyword_is_valid() {
        assert!(parse("if $c { a } else { b }").is_ok());
    }

    // ── Control operators (try / guard / within / grant / audit) ────────

    fn unwrap_single_scope(ast: Vec<Stmt>) -> (ScopeAst, Vec<Redirect>) {
        let stripped: Vec<_> = ast.into_iter().map(|s| s.item).collect();
        match stripped.as_slice() {
            [Ast::Scope { op, redirects, .. }] => (op.clone(), redirects.clone()),
            _ => panic!("expected a single Ast::Scope, got {stripped:?}"),
        }
    }

    fn unwrap_single_exec(ast: Vec<Stmt>) -> (Head, Vec<Ast>) {
        let stripped: Vec<_> = ast.into_iter().map(|s| s.item).collect();
        match stripped.as_slice() {
            [Ast::Call { head, args, .. }] => {
                (head.clone(), args.iter().map(|s| s.item.clone()).collect())
            }
            _ => panic!("expected a single Ast::Call, got {stripped:?}"),
        }
    }

    #[test]
    fn parse_try_two_blocks() {
        let (op, redirects) = unwrap_single_scope(parse("try { body } { handler }").unwrap());
        assert!(redirects.is_empty());
        match op {
            ScopeAst::Try { body, handler } => {
                assert!(matches!(*body, Ast::Block(_)));
                assert!(matches!(*handler, Ast::Block(_)));
            }
            _ => panic!("expected ScopeAst::Try, got {op:?}"),
        }
    }

    #[test]
    fn parse_try_body_then_lambda() {
        // The shape the prelude writes: a bound body, a lambda handler.
        let (op, redirects) =
            unwrap_single_scope(parse("try $body { |err| return unit }").unwrap());
        assert!(redirects.is_empty());
        match op {
            ScopeAst::Try { body, handler } => {
                assert!(matches!(*body, Ast::Variable(ref n) if n == "body"));
                assert!(matches!(*handler, Ast::Lambda { .. }));
            }
            _ => panic!("expected ScopeAst::Try, got {op:?}"),
        }
    }

    #[test]
    fn parse_try_with_trailing_redirect() {
        let (op, redirects) = unwrap_single_scope(parse("try { body } { handler } > out").unwrap());
        assert_eq!(redirects.len(), 1);
        match op {
            ScopeAst::Try { body, handler } => {
                assert!(matches!(*body, Ast::Block(_)));
                assert!(matches!(*handler, Ast::Block(_)));
            }
            _ => panic!("expected ScopeAst::Try, got {op:?}"),
        }
    }

    #[test]
    fn parse_try_with_two_trailing_redirects() {
        let (op, redirects) =
            unwrap_single_scope(parse("try { body } { handler } > out 2>&1").unwrap());
        assert_eq!(redirects.len(), 2);
        assert!(matches!(op, ScopeAst::Try { .. }));
    }

    #[test]
    fn parse_try_one_arg_is_error() {
        let err = parse("try { body }").unwrap_err();
        assert!(
            err.message.contains("try requires 2") && err.message.contains("got 1"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn parse_try_zero_args_is_error() {
        let err = parse("try").unwrap_err();
        assert!(
            err.message.contains("try requires 2") && err.message.contains("got 0"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn parse_try_three_args_is_error() {
        let err = parse("try { a } { b } { c }").unwrap_err();
        assert!(
            err.message.contains("try requires 2") && err.message.contains("got 3"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn parse_guard_two_blocks() {
        let (op, _) = unwrap_single_scope(parse("guard { body } { cleanup }").unwrap());
        assert!(matches!(op, ScopeAst::Guard { .. }));
    }

    #[test]
    fn parse_guard_one_arg_is_error() {
        let err = parse("guard { body }").unwrap_err();
        assert!(err.message.contains("guard requires 2"), "msg: {err}");
    }

    #[test]
    fn parse_within_opts_and_body() {
        let (op, _) = unwrap_single_scope(parse("within [dir: '/tmp'] { body }").unwrap());
        match op {
            ScopeAst::Within { opts, body } => {
                assert!(matches!(*opts, Ast::Map(_)));
                assert!(matches!(*body, Ast::Block(_)));
            }
            _ => panic!("expected ScopeAst::Within, got {op:?}"),
        }
    }

    #[test]
    fn parse_within_one_arg_is_error() {
        let err = parse("within [dir: '/tmp']").unwrap_err();
        assert!(err.message.contains("within requires 2"), "msg: {err}");
    }

    #[test]
    fn parse_grant_caps_and_body() {
        let (op, _) = unwrap_single_scope(parse("grant [exec: [:]] { body }").unwrap());
        assert!(matches!(op, ScopeAst::Grant { .. }));
    }

    #[test]
    fn parse_grant_zero_args_is_error() {
        let err = parse("grant").unwrap_err();
        assert!(err.message.contains("grant requires 2"), "msg: {err}");
    }

    #[test]
    fn parse_audit_one_block() {
        let (op, _) = unwrap_single_scope(parse("audit { body }").unwrap());
        assert!(matches!(op, ScopeAst::Audit { .. }));
    }

    #[test]
    fn parse_audit_two_args_is_error() {
        let err = parse("audit a b").unwrap_err();
        assert!(
            err.message.contains("audit requires 1") && err.message.contains("got 2"),
            "msg: {err}"
        );
    }

    #[test]
    fn parse_audit_zero_args_is_error() {
        let err = parse("audit").unwrap_err();
        assert!(err.message.contains("audit requires 1"), "msg: {err}");
    }

    #[test]
    fn parse_audit_with_trailing_redirect() {
        let (op, redirects) = unwrap_single_scope(parse("audit { body } > out").unwrap());
        assert!(matches!(op, ScopeAst::Audit { .. }));
        assert_eq!(redirects.len(), 1);
    }

    // ── Reserved-name binding rejection ─────────────────────────────────

    #[test]
    fn parse_let_try_rejected() {
        let err = parse("let try = 1").unwrap_err();
        assert!(err.message.contains("'try'"), "msg: {err}");
    }

    #[test]
    fn parse_let_within_rejected() {
        let err = parse("let within = 1").unwrap_err();
        assert!(err.message.contains("'within'"), "msg: {err}");
    }

    #[test]
    fn parse_let_guard_rejected() {
        assert!(parse("let guard = 1").is_err());
    }

    #[test]
    fn parse_let_grant_rejected() {
        assert!(parse("let grant = 1").is_err());
    }

    #[test]
    fn parse_let_audit_rejected() {
        assert!(parse("let audit = 1").is_err());
    }

    #[test]
    fn parse_lambda_param_named_try_rejected() {
        assert!(parse("let f = { |try| 1 }").is_err());
    }

    // ── ^try keeps external-only semantics ──────────────────────────────

    #[test]
    fn parse_external_try_still_valid() {
        let (head, args) = unwrap_single_exec(parse("^try arg").unwrap());
        assert_eq!(head, external_head("try"));
        assert_eq!(args.len(), 1);
    }

    #[test]
    fn parse_external_within_still_valid() {
        let (head, _args) = unwrap_single_exec(parse("^within").unwrap());
        assert_eq!(head, external_head("within"));
    }

    // ── Standalone redirect rejection ───────────────────────────────────

    #[test]
    fn parse_standalone_redirect_rejected() {
        let err = parse("> out.txt").unwrap_err();
        assert!(
            err.message.contains("redirect must follow a command"),
            "msg: {err}"
        );
    }

    #[test]
    fn parse_leading_redirect_after_newline_rejected() {
        let err = parse("echo hi\n> out").unwrap_err();
        assert!(
            err.message.contains("redirect must follow a command"),
            "msg: {err}"
        );
    }
}
