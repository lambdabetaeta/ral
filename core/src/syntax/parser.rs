//! Recursive-descent parser: [`crate::syntax::lexer`] tokens → AST.
//!
//! The grammar is statement-oriented: a program is statements separated by
//! newlines or `;`; a statement is a `let` binding or a `?`-chain of
//! `|`-pipelines; a stage is `return`, `if`, `case`, a control operator, or a
//! command.  Newlines bend around continuations — freely either side of `|`
//! and `?`, any number after a binder's `=` — and inside `[…]` the lexer drops
//! them.
//!
//! Each [`Stmt`] carries the span of its own tokens and the elaborator stamps
//! that span on the IR it emits, so no constructor here threads a span.
//! The body of `$[…]` is a Pratt sub-parser over the tokens the lexer already
//! produced for the block — no re-lex, no substring round trip — whose
//! operands are the ordinary atoms of the value grammar.

use crate::ir::{
    ArithOp, BinaryOp, MapPatternEntry, Pattern, Redirect, Redirects, StdinSource, WriteMode,
};
use crate::source::{Span, Spanned};
use crate::syntax::CONTROL_OPERATORS;
use crate::syntax::ast::{
    Ast, CaseArm, HandlerArm, Head, IfBranch, ListElem, MapEntry, Options, RecordEntry, ScopeAst,
    Stmt, Word, WordLiteral,
};
use crate::syntax::lexer::{
    self, LexError, LexErrorKind, Lexeme, Operator, RedirectOp, StringPart, Token,
};
use crate::syntax::numeral::{self, Shape};
use crate::text::plural;
use std::fmt;

// ── Parse Error ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    /// The offending token, or the opening delimiter for a lexer error.
    pub span: Option<Span>,
    pub(crate) kind: ParseErrorKind,
    /// The parse failed because the input *stopped short* rather than being
    /// malformed — an unclosed lexeme, a `let` still awaiting its right-hand
    /// side, or a consumed `|` / `?` / `if` / `elsif` / `else` whose stage,
    /// branch, or body never arrived.  Drives REPL line continuation.
    pub(crate) incomplete: bool,
}

/// The structure the diagnostic layer needs to draw more than one label.
#[derive(Debug, Clone)]
pub(crate) enum ParseErrorKind {
    Plain,
    Lex(LexErrorKind),
    /// Two atoms with nothing between them: the unit before, the first atom
    /// after it, and the whole touching run.
    Touching {
        first: Span,
        second: Span,
        run: Span,
    },
}

impl ParseError {
    pub(crate) fn new(span: Option<Span>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span,
            kind: ParseErrorKind::Plain,
            incomplete: false,
        }
    }

    /// The REPL reads another line instead of reporting it.
    pub(crate) fn incomplete(self) -> Self {
        Self {
            incomplete: true,
            ..self
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error: {}", self.message)
    }
}

impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        Self {
            message: e.kind.message(),
            span: Some(e.span),
            incomplete: e.kind.is_incomplete(),
            kind: ParseErrorKind::Lex(e.kind),
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
/// Runs the real parser and reads its own verdict rather than guessing from
/// the raw text.
pub fn needs_continuation(input: &str) -> bool {
    matches!(
        parse(input),
        Err(ParseError {
            incomplete: true,
            ..
        })
    )
}

/// Parse `source` into a statement list, attributing spans to `file`.
///
/// # Errors
/// Returns `Err` if lexing fails or the tokens do not form a valid program.
pub(crate) fn parse_with(
    source: &str,
    file: crate::source::FileId,
) -> Result<Vec<Stmt>, ParseError> {
    let tokens = lexer::lex_with(source, file)?;
    Parser::run_complete(
        tokens,
        Span::point(file, 0),
        trailing_input,
        Parser::parse_program,
    )
}

// ── Parser ───────────────────────────────────────────────────────────────

/// Which keys a `[k: v]` accepts: a literal admits a data key, `"…"` or
/// `$var`; a pattern cannot bind through one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyAlphabet {
    Label,
    Data,
}

/// Loop-body verdict for [`Parser::parse_separated_until`]: keep going after
/// this item, or treat it as the last one.
enum SepFlow {
    Cont,
    Stop,
}

/// A control operator's operands for the arity message: how many it takes,
/// what they are, and how many have been read.
#[derive(Clone, Copy)]
struct Form {
    name: &'static str,
    desc: &'static str,
    arity: usize,
    seen: usize,
}

impl Form {
    fn new(name: &'static str, desc: &'static str, arity: usize) -> Self {
        Self {
            name,
            desc,
            arity,
            seen: 0,
        }
    }

    fn arity_error(self, got: usize) -> String {
        format!(
            "{name} requires {args} ({desc}); got {got}",
            name = self.name,
            args = plural(self.arity, "argument"),
            desc = self.desc,
        )
    }
}

struct Parser {
    tokens: Vec<Lexeme>,
    pos: usize,
    /// Where the stream came from: the `$[…]` or `$(…)` token for a
    /// sub-stream, the file's start at the top level.  A sub-stream carries
    /// no `Eof`, so an empty one has no token to point at.
    at: Span,
    /// Descent depth, maintained only by [`Parser::nested`].  Values,
    /// arithmetic, and patterns each pass through one guarded chokepoint per
    /// level, so this one counter bounds all three.
    depth: usize,
}

impl Parser {
    /// Parse a token stream and require that `body` consumed all of it.  The
    /// sole constructor, so no entry point — top level or sub-stream — can let
    /// a production that stops early silently drop the remainder.  `leftover`
    /// says what an unconsumed token means for this particular stream.
    fn run_complete<T>(
        tokens: Vec<Lexeme>,
        at: Span,
        leftover: fn(&Token) -> String,
        body: impl FnOnce(&mut Self) -> Result<T, ParseError>,
    ) -> Result<T, ParseError> {
        let mut parser = Self {
            tokens,
            pos: 0,
            at,
            depth: 0,
        };
        let value = body(&mut parser)?;
        if parser.peek() != &Token::Eof {
            let message = leftover(parser.peek());
            return Err(parser.error(message));
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
        self.peek_at(0)
    }

    /// The token `ahead` of the cursor, `Eof` past the end.
    fn peek_at(&self, ahead: usize) -> &Token {
        self.tokens
            .get(self.pos + ahead)
            .map_or(&Token::Eof, |l| &l.token)
    }

    /// Span of the current token, of the last one past the end, or — for an
    /// empty sub-stream, which has no tokens at all — of the `$[…]` / `$(…)`
    /// this stream came out of.
    fn span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .or_else(|| self.tokens.last())
            .map_or(self.at, |l| l.span)
    }

    fn advance(&mut self) -> &Token {
        let tok = self.tokens.get(self.pos).map_or(&Token::Eof, |l| &l.token);
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    /// Consume `tok` if it is next.
    fn eat(&mut self, tok: &Token) -> bool {
        let hit = self.peek() == tok;
        if hit {
            self.advance();
        }
        hit
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
            Token::Newline | Token::Semi | Token::Eof | Token::RBrace
        )
    }

    /// Soft newlines only: continuation contexts may not cross a `;`.
    fn skip_newlines(&mut self) {
        while self.peek() == &Token::Newline {
            self.advance();
        }
    }

    fn skip_separators(&mut self) {
        while matches!(self.peek(), Token::Newline | Token::Semi) {
            self.advance();
        }
    }

    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError::new(Some(self.span()), message)
    }

    /// Called just after consuming a `|`, `?`, `if`, `elsif`, or `else` that
    /// now demands a stage, branch, or body: end of input here is the user
    /// mid-typing, not a dangling operator.
    fn require_continuation(&self, what: &str) -> Result<(), ParseError> {
        if self.peek() == &Token::Eof {
            return Err(self
                .error(format!("expected {what} after the continuation"))
                .incomplete());
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

    /// [`Self::capture_span`], kept with what it parsed.
    fn spanned<T>(
        &mut self,
        parse: impl FnOnce(&mut Self) -> Result<T, ParseError>,
    ) -> Result<Spanned<T>, ParseError> {
        let (span, item) = self.capture_span(parse)?;
        Ok(Spanned::new(span, item))
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
            if self.eat(end) {
                return Ok(());
            }
            match item(self)? {
                SepFlow::Cont => {
                    if !self.eat(&Token::Comma) && self.peek() != end {
                        return Err(self.error(format!("expected ',' or '{end}' in {label}")));
                    }
                }
                SepFlow::Stop => {
                    self.eat(&Token::Comma);
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
        self.skip_separators();
        while self.peek() != &Token::Eof && self.peek() != &Token::RBrace {
            // `parse_stmt` leaves the separator, so this span never swallows
            // one.  Underlining the whole statement is the only anchor a
            // diagnostic has when the guilty sub-expression carries no span.
            stmts.push(self.spanned(Self::parse_stmt)?);
            self.skip_separators();
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

    /// chain = pipeline (NL* '?' NL* pipeline)*
    ///
    /// A singleton chain collapses to its bare arm, so `Ast::Chain` always
    /// means two or more branches and downstream passes need no length guard.
    /// A trailing `?` is a continuation exactly as a trailing `|` is, so the
    /// REPL's continuation prompt is telling the truth rather than baiting the
    /// user.
    fn parse_chain(&mut self) -> Result<Ast, ParseError> {
        let mut arms = vec![self.spanned(Self::parse_pipeline)?];
        while self.eat_continuation(&Token::Question) {
            self.require_continuation("a chain branch")?;
            arms.push(self.spanned(Self::parse_pipeline)?);
        }
        Ok(if arms.len() == 1 {
            arms.remove(0).item
        } else {
            Ast::Chain(arms)
        })
    }

    /// pipeline = stage ('|' stage)*
    fn parse_pipeline(&mut self) -> Result<Ast, ParseError> {
        let mut stages = vec![self.spanned(Self::parse_stage)?];

        while self.eat_continuation(&Token::Pipe) {
            if self.peek() == &Token::Pipe {
                return Err(self.error(NO_OR_OR));
            }
            self.require_continuation("a pipeline stage")?;
            stages.push(self.spanned(Self::parse_stage)?);
        }

        if stages.len() == 1 {
            Ok(stages.remove(0).item)
        } else {
            Ok(Ast::Pipeline(stages))
        }
    }

    /// Consume `tok` with any number of newlines on either side; rewinds and
    /// returns false on a miss.
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

    /// stage = return-stage | if-stage | case-stage | control-op | command
    ///
    /// The dispatch below is also the list of words reserved in stage position.
    ///
    /// Every arm must leave the cursor at [`Self::at_cmd_end`] — checked once
    /// here rather than trusted of each arm, so a fixed-shape form that stops
    /// short (like `case`'s scrutinee-then-arms) can't hand its leftover
    /// tokens to `parse_program` as a silent, separator-less next statement.
    ///
    /// A keyword is a word like any other for the words rule: `return'x'` and
    /// `try{a}{b}` are refused before dispatch.
    fn parse_stage(&mut self) -> Result<Ast, ParseError> {
        let head = self.peek().as_plain_word().map(str::to_owned);
        if head.as_deref().is_some_and(is_stage_keyword) {
            let keyword = self.span();
            if self.atom_starts_at(1, keyword.end) {
                self.advance();
                return Err(self.touching(keyword));
            }
        }
        let (span, (form, redirects)) = self.capture_span(|p| match head.as_deref() {
            // `parse_stmt` peels `let` off first, so arriving here means a
            // binding embedded in pipeline or chain position.
            Some("let") => Err(p.error(
                "`let` is a statement, not a pipeline stage or chain branch: \
                 move the binding to its own line, or wrap the consumer in a \
                 block: `{ let x = …; … }`",
            )),
            Some(word @ ("else" | "elsif")) => Err(p.error(format!(
                "`{word}` continues an `if` on the line above; there is none here"
            ))),
            Some("return") => p.trailed(Self::parse_return_stage),
            Some("if") => p.trailed(Self::parse_if),
            Some("case") => p.trailed(Self::parse_case),
            // `^try` and friends stay external: a `Token::Caret` head yields
            // no plain word, so it falls to `parse_command` below.
            Some(name) if CONTROL_OPERATORS.contains(&name) => {
                p.trailed(|p| p.parse_control_op(name))
            }
            _ => p.parse_command(),
        })?;
        let stage = if redirects.is_empty() {
            form
        } else {
            Ast::Redirected {
                stage: Spanned::boxed(span, form),
                redirects: Box::new(redirects),
            }
        };
        if self.at_cmd_end() {
            return Ok(stage);
        }
        let found = self.peek().clone();
        Err(self.error(format!(
            "unexpected {found} after `{name}`; a new statement needs a separator (newline or ';')",
            name = head.as_deref().unwrap_or("this"),
        )))
    }

    /// A stage form followed by its trailing redirects.
    fn trailed(
        &mut self,
        parse: impl FnOnce(&mut Self) -> Result<Ast, ParseError>,
    ) -> Result<(Ast, Redirects<Ast>), ParseError> {
        let form = parse(self)?;
        Ok((form, self.collect_trailing_redirects()?))
    }

    /// Only a fixed-arity form can collect redirects at the end like this;
    /// `parse_command` interleaves them, since a command takes them anywhere.
    fn collect_trailing_redirects(&mut self) -> Result<Redirects<Ast>, ParseError> {
        let mut redirects = Redirects::default();
        while !self.at_cmd_end() && self.at_redirect() {
            self.parse_redirect_into(&mut redirects)?;
        }
        Ok(redirects)
    }

    /// The control operator `name` and exactly its operands, each an atom or
    /// the form's own option bracket — not an argument: `parse_arg` would
    /// admit an `Ast::Spread` that these fixed positions have no lowering for.
    fn parse_control_op(&mut self, name: &str) -> Result<Ast, ParseError> {
        self.advance(); // consume the head name
        let op = match name {
            "try" => {
                let mut form = Form::new("try", "body, handler", 2);
                let body = Box::new(self.operand(&mut form)?);
                let handler = Box::new(self.operand(&mut form)?);
                self.no_more_operands(form)?;
                ScopeAst::Try { body, handler }
            }
            "guard" => {
                let mut form = Form::new("guard", "body, cleanup", 2);
                let body = Box::new(self.operand(&mut form)?);
                let cleanup = Box::new(self.operand(&mut form)?);
                self.no_more_operands(form)?;
                ScopeAst::Guard { body, cleanup }
            }
            "within" => {
                let mut form = Form::new("within", "options, body", 2);
                self.operand_slot(&mut form)?;
                let (opts, handlers) = self.parse_options(form, true, "dir: $d")?;
                let body = Box::new(self.operand(&mut form)?);
                self.no_more_operands(form)?;
                ScopeAst::Within {
                    opts,
                    handlers,
                    body,
                }
            }
            "grant" => {
                let mut form = Form::new("grant", "capabilities, body", 2);
                self.operand_slot(&mut form)?;
                let (caps, _) = self.parse_options(form, false, "net: $n")?;
                let body = Box::new(self.operand(&mut form)?);
                self.no_more_operands(form)?;
                ScopeAst::Grant { caps, body }
            }
            "audit" => {
                let mut form = Form::new("audit", "body", 1);
                let body = Box::new(self.operand(&mut form)?);
                self.no_more_operands(form)?;
                ScopeAst::Audit { body }
            }
            _ => unreachable!("parse_stage admits only CONTROL_OPERATORS"),
        };
        Ok(Ast::Scope(op))
    }

    /// Room for one more operand, counted into `form`: refuses a missing one
    /// and a spread.
    fn operand_slot(&self, form: &mut Form) -> Result<(), ParseError> {
        if self.at_cmd_end() || self.at_redirect() {
            return Err(self.error(form.arity_error(form.seen)));
        }
        if self.peek() == &Token::Spread {
            return Err(self.error(format!(
                "`{name}` takes its operands by position ({desc}); \
                 a spread `...` has no meaning here",
                name = form.name,
                desc = form.desc,
            )));
        }
        form.seen += 1;
        Ok(())
    }

    fn operand(&mut self, form: &mut Form) -> Result<Ast, ParseError> {
        self.operand_slot(form)?;
        self.parse_atom()
    }

    /// A surplus operand is an arity error, counted one past the form's own.
    fn no_more_operands(&self, form: Form) -> Result<(), ParseError> {
        if self.at_cmd_end() || self.at_redirect() {
            return Ok(());
        }
        Err(self.error(form.arity_error(form.arity + 1)))
    }

    /// `options = '[' ']' | '[' item (',' item)* ']'`
    ///
    /// A form's option bracket, which is the form's syntax and not a
    /// collection literal: `[]` here is the empty option set rather than the
    /// empty list, and `[:]` — a map — names no options at all.  The values
    /// may be computed, but the option names are written out, the way `case`
    /// writes its arms: a bundle assembled elsewhere, a spread and a computed
    /// key are refused here.
    ///
    /// Where the form takes arms, `handlers:` is lifted out of the bracket:
    /// its labels are the names it binds in the body, so they are syntax and
    /// no computed table can spell them.
    fn parse_options(
        &mut self,
        form: Form,
        arms: bool,
        example: &str,
    ) -> Result<(Options, Option<Vec<HandlerArm>>), ParseError> {
        let written = || {
            format!(
                "`{name}` takes its options written in its own bracket. The values may be \
                 bound (`{name} [{example}]`) but the option names are written here, \
                 the way `case` writes its arms.",
                name = form.name,
            )
        };
        if self.peek() != &Token::LBracket {
            return Err(self.error(written()));
        }
        let open = self.span();
        self.advance(); // consume `[`
        if self.eat(&Token::RBracket) {
            self.end_unit(open.join(self.prev_byte_span()))?;
            return Ok((Vec::new(), None));
        }
        if self.peek() == &Token::Colon {
            return Err(self.error(format!(
                "`{name}` takes its options by name ({operands_desc}), so `[:]` (a map, \
                 whose keys are data) names none of them; write `[]` for no options",
                name = form.name,
                operands_desc = form.desc,
            )));
        }
        let mut items = Vec::new();
        self.parse_separated_until(&Token::RBracket, "the options", |p| {
            items.push(p.parse_collection_item()?);
            Ok(SepFlow::Cont)
        })?;
        self.end_unit(open.join(self.prev_byte_span()))?;

        let mut options: Options = Vec::new();
        let mut handlers = None;
        for item in items {
            match item {
                CollectionItem::Spread(base) => return Err(ParseError::new(base.span, written())),
                CollectionItem::Entry {
                    key: MapKeyForm::Label(key),
                    value,
                } if arms && key.item == "handlers" => {
                    if handlers.is_some() {
                        return Err(ParseError::new(
                            value.span,
                            "`handlers:` is written once; put every arm in the one list",
                        ));
                    }
                    handlers = Some(Self::handler_arms(form, value)?);
                }
                CollectionItem::Entry {
                    key: MapKeyForm::Label(key),
                    value,
                } => {
                    if options.iter().any(|(seen, _)| *seen == key.item) {
                        return Err(ParseError::new(
                            value.span,
                            format!(
                                "`{key}` is written twice in `{name}`'s options; write it once",
                                key = key.item,
                                name = form.name,
                            ),
                        ));
                    }
                    options.push((key.item, value));
                }
                CollectionItem::Entry {
                    key: MapKeyForm::Data(key),
                    ..
                } => {
                    return Err(ParseError::new(
                        key.span,
                        format!(
                            "`{name}`'s options are named in writing, so a key that is data \
                             (`\"…\"` or `$var`) cannot be one; write the name out, as in \
                             `{name} [{example}]`",
                            name = form.name,
                        ),
                    ));
                }
                CollectionItem::Elem(item) => {
                    return Err(ParseError::new(
                        item.span,
                        format!(
                            "`{name}` takes `option: value` entries ({operands_desc}); \
                             this one has no name",
                            name = form.name,
                            operands_desc = form.desc,
                        ),
                    ));
                }
            }
        }
        Ok((options, handlers))
    }

    /// `handlers: [name: arm, …]` — the arm list, read as syntax.  The table
    /// is written out here for the same reason `case`'s arms are: the labels
    /// are the names bound in the body, and a table assembled elsewhere hides
    /// them.  An arm's *value* is any atom, the name being what is syntax.
    fn handler_arms(form: Form, value: Spanned<Ast>) -> Result<Vec<HandlerArm>, ParseError> {
        let spelling = format!(
            "write the arms out (`{name} [handlers: [deploy: {{ |args| … }}]] …`) \
             since each name is bound in the body",
            name = form.name,
        );
        let entries = match value.item {
            Ast::List(ref elems) if elems.is_empty() => Vec::new(),
            Ast::Record(entries) => entries,
            // `[:]` is a map, not the arm list's own empty; give an explicit
            // error rather than let it fall through as a type mismatch.
            Ast::Map(ref entries) if entries.is_empty() => {
                return Err(ParseError::new(
                    value.span,
                    "the empty handler set is `handlers: []`; `[:]` is a map, \
                     and an arm's name is not data",
                ));
            }
            Ast::Map(_) => {
                return Err(ParseError::new(
                    value.span,
                    format!("`handlers:` takes named arms, not a map; {spelling}"),
                ));
            }
            _ => {
                return Err(ParseError::new(
                    value.span,
                    format!("`handlers:` is an arm list, not a value; {spelling}"),
                ));
            }
        };
        let mut arms: Vec<HandlerArm> = Vec::new();
        for entry in entries {
            match entry {
                RecordEntry::Field { key, value } => {
                    if arms.iter().any(|arm| arm.name == key.item) {
                        return Err(ParseError::new(
                            value.span,
                            format!(
                                "`{key}` already has an arm; one name, one arm",
                                key = key.item
                            ),
                        ));
                    }
                    arms.push(HandlerArm {
                        name: key.item,
                        value,
                    });
                }
                RecordEntry::Spread(base) => {
                    return Err(ParseError::new(
                        base.span,
                        format!("`handlers:` spreads no other table; {spelling}"),
                    ));
                }
            }
        }
        Ok(arms)
    }

    /// case = 'case' atom '[' arm (',' arm)* trailing-comma? ']'
    ///
    /// The scrutinee is any atom; the arms are not an atom at all: each is a
    /// literal label paired with a body, so the set of alternatives is a fact
    /// the parser establishes and nothing downstream can widen.  Everything
    /// that would hide that set — a computed table in place of the list, a
    /// spread, a repeated tag — is refused here, where no payload is yet in
    /// flight.
    fn parse_case(&mut self) -> Result<Ast, ParseError> {
        self.advance(); // consume `case`
        self.skip_newlines();
        let (scrut_span, scrutinee) = self.capture_span(Self::parse_atom)?;
        self.skip_newlines();
        self.require_continuation("the `case` arms")?;
        if self.peek() != &Token::LBracket {
            let found = self.peek().clone();
            return Err(self.error(format!(
                "`case` wants its arms here, one per tag, written out: \
                 case $x [`ok: {{ |v| … }}, `err: {{ |e| … }}]: but found {found}. \
                 The arms are syntax, not a record: a table assembled elsewhere hides \
                 alternatives `case` must see to prove it covers every tag."
            )));
        }
        self.advance(); // consume `[`
        if self.peek() == &Token::RBracket {
            return Err(self.error(
                "`case` needs at least one arm: what should it do with the value? \
                 Write one arm per tag, as in case $x [`ok: { |v| … }].",
            ));
        }
        let mut arms: Vec<CaseArm> = Vec::new();
        self.parse_separated_until(&Token::RBracket, "`case` arms", |p| {
            let arm = p.parse_case_arm()?;
            if let Some(prev) = arms.iter().find(|a| a.tag.item == arm.tag.item) {
                let label = &arm.tag.item;
                let span = arm.tag.span.or(prev.tag.span).unwrap_or_else(|| p.span());
                return Err(ParseError::new(
                    Some(span),
                    format!(
                        "this `case` already has a `{label} arm, and exactly one \
                         computation may run per tag: merge the two bodies, or give \
                         the second arm the tag you meant."
                    ),
                ));
            }
            arms.push(arm);
            Ok(SepFlow::Cont)
        })?;
        Ok(Ast::Case {
            scrutinee: Spanned::boxed(scrut_span, scrutinee),
            arms,
        })
    }

    /// arm = TAG ':' (lambda | atom)
    ///
    /// The tag is read straight off the token, never through `parse_atom`:
    /// the tag grammar takes the next adjacent atom as a payload, and would
    /// swallow the arm's binder as one.  The body is any atom — a function
    /// named elsewhere is an arm like any other, since what must be syntax is
    /// the *set* of alternatives, not how each one is spelled.  A brace form
    /// is read here rather than by `parse_atom`, so an arm that forgot its
    /// binder or took two is told so where it stands.
    fn parse_case_arm(&mut self) -> Result<CaseArm, ParseError> {
        if self.peek() == &Token::Spread {
            return Err(self.error(
                "a `case` lists its arms one by one, so a `...` spread has no meaning \
                 here: an arm spliced in from elsewhere is an alternative `case` cannot \
                 see, and so cannot prove it covers. Write it out as an arm of its own.",
            ));
        }
        let tag_span = self.span();
        let Token::Tag(label) = self.peek().clone() else {
            let found = self.peek().clone();
            return Err(self.error(format!(
                "a `case` arm is labelled by a tag, as in \
                 case $x [`some: {{ |p| … }}]: but found {found}."
            )));
        };
        self.advance();
        if self.peek() != &Token::Colon {
            let found = self.peek().clone();
            return Err(self.error(format!(
                "expected `:` after the `{label} arm's tag, found {found}."
            )));
        }
        self.advance(); // consume `:`
        let (body_span, body) = if self.peek() == &Token::LBrace {
            self.capture_span(|p| p.nested(|p| p.parse_case_arm_lambda(&label)))?
        } else {
            self.capture_span(Self::parse_atom)?
        };
        Ok(CaseArm {
            tag: Spanned::new(tag_span, label),
            body: Spanned::boxed(body_span, body),
        })
    }

    /// lambda = '{' '|' binder '|' program '}' — an arm's own body.
    ///
    /// `parse_block` would accept both shapes this rejects: a plain thunk,
    /// which binds nothing, and a multi-parameter lambda, which it curries.
    /// Neither is an arm, and each is worth its own sentence.
    fn parse_case_arm_lambda(&mut self, label: &str) -> Result<Ast, ParseError> {
        self.advance(); // consume `{`
        if self.peek() != &Token::Pipe {
            return Err(self.error(format!(
                "the `{label} arm must bind its payload: write {{ |p| … }}, \
                 or {{ |_| … }} where the tag carries nothing."
            )));
        }
        self.advance(); // consume the opening `|`
        let (param_span, param) = self.parse_binder()?;
        if self.peek() != &Token::Pipe {
            return Err(self.error(format!(
                "a `case` arm binds exactly one payload, so the `{label} arm takes one \
                 parameter: destructure it in place if it carries several fields, \
                 as in {{ |[head: h, tail: t]| … }}."
            )));
        }
        self.advance(); // consume the closing `|`
        let body = self.parse_program()?;
        self.expect(&Token::RBrace)?;
        Ok(Ast::Lambda {
            param: Spanned::new(param_span, param),
            body,
        })
    }

    /// if = 'if' atom atom ('elsif' atom atom)* ('else' atom)?
    ///
    /// Conditions and bodies are any atom; the elaborator demands that a body be
    /// a block, or a name holding one.
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
                    let keyword = self.span();
                    self.advance();
                    self.end_unit(keyword)?;
                    self.skip_newlines();
                    self.require_continuation("the `elsif` condition")?;
                    branches.push(self.parse_if_branch()?);
                }
                tok if tok.as_plain_word() == Some("else") => {
                    let keyword = self.span();
                    self.advance();
                    self.end_unit(keyword)?;
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
                            self.error("unexpected `{` after `if`: did you mean `else { … }`?")
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

        if self.at_cmd_end() || self.at_redirect() {
            return Ok(Ast::Return(None));
        }
        if let Some(word) = self.peek().as_plain_word().filter(|w| begins_statement(w)) {
            return Err(self.error(format!(
                "`return` takes one value, and `{word}` begins a statement: put the \
                 `return` inside each branch, or write `return !{{{word} …}}`"
            )));
        }

        let (val_span, val) = self.capture_span(Self::parse_atom)?;
        if !(self.at_cmd_end() || self.at_redirect()) {
            return Err(self.error("return expects at most one value argument"));
        }
        Ok(Ast::Return(Some(Spanned::boxed(val_span, val))))
    }

    /// binding = 'let' pattern '=' chain
    ///
    /// `None` when the next token is not `let`, so the caller falls through to
    /// the chain statement.
    fn parse_binding_opt(&mut self) -> Result<Option<Ast>, ParseError> {
        if self.peek().as_plain_word() != Some("let") {
            return Ok(None);
        }
        self.advance(); // consume 'let'
        let (pattern_span, pattern) = self.parse_binder()?;
        match self.peek() {
            tok if tok.as_plain_word() == Some("=") => {
                self.advance();
            }
            _ => {
                return Err(self.glued_equals().unwrap_or_else(|| {
                    self.error("expected '=' after the binding name in `let`")
                }));
            }
        }
        // The RHS may start on the next line: `let x =\n  expr`.
        self.skip_newlines();
        if self.peek() == &Token::Eof {
            return Err(self
                .error("expected the right-hand side of the `let` binding")
                .incomplete());
        }
        let (value_span, value) = self.capture_span(Self::parse_chain)?;
        Ok(Some(Ast::Let {
            pattern: Spanned::new(pattern_span, pattern),
            value: Spanned::boxed(value_span, value),
        }))
    }

    /// A complete binder — a `let` LHS or one lambda parameter — with its
    /// span. The one place duplicate names are refused: a pattern binds all
    /// its names at once, so a repeat within it is ambiguous, whereas a
    /// repeat across curried parameters is ordinary shadowing.
    fn parse_binder(&mut self) -> Result<(Span, Pattern), ParseError> {
        let (span, pattern) = self.capture_span(Self::parse_pattern)?;
        if let Some(name) = pattern.duplicate_name() {
            return Err(ParseError::new(
                Some(span),
                format!("pattern binds `{name}` more than once"),
            ));
        }
        Ok((span, pattern))
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
            Token::Word(Word::Plain(name)) if is_reserved(name) => {
                Err(p.error(crate::syntax::reserved_keyword_message(name)))
            }
            Token::Word(Word::Plain(name)) if lexer::is_ident(name) => {
                let name = name.clone();
                p.advance();
                Ok(Pattern::Name(name.into()))
            }
            _ => Err(p.glued_equals().unwrap_or_else(|| {
                p.error(
                    "expected a pattern: a name like `x`, `_` to ignore, \
                     or a destructuring `[a, b]` / `[host: h, port: p]`",
                )
            })),
        })
    }

    /// The bash `x=5` where `let x = 5` is meant: a word with the `=` inside.
    fn glued_equals(&self) -> Option<ParseError> {
        let Token::Word(Word::Plain(word) | Word::Slash(word)) = self.peek() else {
            return None;
        };
        word.contains('=').then(|| {
            self.error(format!(
                "`{word}` is one word: `let` wants spaces around `=`, as in `let x = 5`"
            ))
        })
    }

    fn parse_pattern_inner(&mut self) -> Result<Pattern, ParseError> {
        self.expect(&Token::LBracket)?;

        if self.eat(&Token::RBracket) {
            return Ok(Pattern::List {
                elems: vec![],
                rest: None,
            });
        }

        // Same key alphabet as a map literal minus the data keys, which a
        // pattern cannot bind through.
        let is_map = self.key_colon_here(KeyAlphabet::Label);

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
            if p.eat(&Token::Spread) {
                p.attached(SPREAD_ATTACHES)?;
                let Token::Word(Word::Plain(name)) = p.peek().clone() else {
                    return Err(p.error("expected name after '...'"));
                };
                if is_reserved(&name) {
                    return Err(p.error(crate::syntax::reserved_keyword_message(&name)));
                }
                if !lexer::is_ident(&name) {
                    return Err(p.error(
                        "rest pattern `...name` needs a plain identifier after the dots, \
                         e.g. `...rest`",
                    ));
                }
                p.advance();
                let closes = |ahead| p.peek_at(ahead) == &Token::RBracket;
                if !(closes(0) || p.peek() == &Token::Comma && closes(1)) {
                    return Err(p.error("`...rest` takes the remaining elements, so it comes last"));
                }
                rest = Some(name.into());
                return Ok(SepFlow::Stop);
            }
            elems.push(p.parse_pattern()?);
            Ok(SepFlow::Cont)
        })?;

        Ok(Pattern::List { elems, rest })
    }

    fn parse_map_pattern(&mut self) -> Result<Pattern, ParseError> {
        let mut entries = Vec::new();

        self.parse_separated_until(&Token::RBracket, "map pattern", |p| {
            let (key_span, key) = p.capture_span(Self::parse_static_key)?;
            // Parsed keys, not token spellings: `[a: x, 'a': y]` repeats and
            // `[a: [a: x]]` does not.  A record has one value per field, so a
            // repeated label is a mistake about the pattern, not about the
            // value it is matched against — and it needs no types to see.
            if entries.iter().any(|e: &MapPatternEntry| e.key == key) {
                return Err(ParseError::new(
                    Some(key_span),
                    format!(
                        "this pattern binds '{key}' twice, and a record has one value per \
                         field: keep whichever '{key}' you meant"
                    ),
                ));
            }
            p.expect(&Token::Colon)?;
            let pattern = p.parse_pattern()?;
            entries.push(MapPatternEntry { key, pattern });
            Ok(SepFlow::Cont)
        })?;

        Ok(Pattern::Map(entries))
    }

    /// pkey = IDENT | 'QUOTED' — the label alphabet.  Literals additionally
    /// admit data keys, which is [`Self::parse_map_key`]'s job.
    fn parse_static_key(&mut self) -> Result<String, ParseError> {
        match self.peek().clone() {
            Token::Word(Word::Plain(k)) if lexer::is_ident(&k) => {
                self.advance();
                Ok(k)
            }
            Token::SingleQuoted(k) => {
                self.advance();
                Ok(k)
            }
            Token::Tag(label) => Err(self.error(format!(
                "`` `{label} `` names a variant, so it cannot be a key: a tag says \
                 *which of* several alternatives, and a key says *which part of* one \
                 value. Write the field as `{label}`, or, if these are the arms of a \
                 match, write them as case $x [`{label}: {{ |p| … }}, …]."
            ))),
            _ => Err(self.error("expected a key: a name or a 'quoted string'")),
        }
    }

    /// mapkey = IDENT | 'QUOTED' | "STRING" | deref, a literal's key alphabet.
    /// The form is returned rather than the entry, because which entry to
    /// build is only settled once the `:` and value are consumed.
    fn parse_map_key(&mut self) -> Result<MapKeyForm, ParseError> {
        match self.peek().clone() {
            Token::Word(Word::Plain(k)) if lexer::is_ident(&k) => {
                Ok(MapKeyForm::Label(self.spanned(Self::parse_static_key)?))
            }
            Token::SingleQuoted(_) | Token::Tag(_) => {
                Ok(MapKeyForm::Label(self.spanned(Self::parse_static_key)?))
            }
            Token::DoubleQuoted(parts) => Ok(MapKeyForm::Data(self.spanned(|p| {
                let at = p.span();
                p.advance();
                Self::parse_interpolation_parts(&parts, at)
            })?)),
            Token::Variable { name, .. } => Ok(MapKeyForm::Data(self.spanned(|p| {
                p.advance();
                Ok(Ast::Variable(name))
            })?)),
            Token::Word(Word::Plain(k)) if WordLiteral::classify(&k).is_some() => Err(self.error(
                "map keys must be identifiers or quoted strings, not numbers; use '0': val",
            )),
            _ => {
                Err(self.error("expected a key: a name, a 'quoted string', a \"string\", or $var"))
            }
        }
    }

    /// primary = word | tag | block | collection
    ///
    /// The value grammar's `nested()` chokepoint.  Expressions and patterns
    /// guard their own prefixes too, at [`Self::parse_expr_operand`] and
    /// [`Self::parse_pattern`]; the lexer caps its delimiter nesting likewise.
    fn parse_primary(&mut self) -> Result<Ast, ParseError> {
        self.nested(|p| match p.peek() {
            Token::LBrace => p.parse_block(),
            Token::LBracket => p.parse_collection(),
            Token::LParen => p.parse_unit(),
            _ => p.parse_word(),
        })
    }

    /// `unit-literal = '(' ')'` — the whole of what `(` opens out here.  Parenthesised
    /// sub-expressions live in the arithmetic grammar of `$[…]` and nowhere else.
    fn parse_unit(&mut self) -> Result<Ast, ParseError> {
        self.expect(&Token::LParen)?;
        if self.eat(&Token::RParen) {
            return Ok(Ast::Unit);
        }
        Err(self.error(
            "parentheses group only inside `$[…]`; here `()` is the unit value. Did you \
             mean `$[…]` for arithmetic, or `!{…}` to run a command inline?",
        ))
    }

    /// An atom, and no atom touching it: the words rule.
    fn parse_atom(&mut self) -> Result<Ast, ParseError> {
        let (span, atom) = self.capture_span(Self::parse_atom_unchecked)?;
        self.end_unit(span)?;
        Ok(atom)
    }

    /// `atom = primary ('[' word ']')*`
    ///
    /// A run of postfix keys becomes one flat `Ast::Index`, never a nest of
    /// single-key ones.  `$(name)` marks the end of a name and takes none,
    /// forced or not.
    fn parse_atom_unchecked(&mut self) -> Result<Ast, ParseError> {
        let delimited = self.at_delimited_name();
        let (node_span, node) = self.capture_span(Self::parse_primary)?;
        if delimited {
            return Ok(node);
        }
        let mut new_keys: Vec<Spanned<Ast>> = Vec::new();
        while self.peek() == &Token::LBracket && self.next_token_is_adjacent() {
            // Brackets included, so the caret underlines what the user wrote.
            let (span, k) = self.capture_span(|p| {
                p.advance();
                if p.peek() == &Token::RBracket {
                    return Err(p.error("an index needs a key: `$x[name]` or `$x[0]`"));
                }
                let k = p.parse_word()?;
                p.expect(&Token::RBracket)?;
                Ok(k)
            })?;
            new_keys.push(Spanned::new(span, k));
        }
        if new_keys.is_empty() {
            return Ok(node);
        }
        Ok(Ast::Index {
            target: Spanned::boxed(node_span, node),
            keys: new_keys,
        })
    }

    /// `$(name)` or `!$(name)` ahead.
    fn at_delimited_name(&self) -> bool {
        let at = self.pos + usize::from(self.peek() == &Token::Bang);
        matches!(
            self.tokens.get(at),
            Some(Lexeme {
                token: Token::Variable {
                    delimited: true,
                    ..
                },
                ..
            })
        )
    }

    /// The token `ahead` of the cursor starts an atom exactly at byte `end`.
    fn atom_starts_at(&self, ahead: usize, end: u32) -> bool {
        self.tokens
            .get(self.pos + ahead)
            .is_some_and(|l| is_atom_start(&l.token) && l.span.start == end)
    }

    /// Nothing separates the token at the cursor from the one after it.
    fn touches_next(&self) -> bool {
        let end = self.span().end;
        self.tokens
            .get(self.pos + 1)
            .is_some_and(|l| l.span.start == end)
    }

    /// An atom begins where `unit` ends.
    fn touches_after(&self, unit: Span) -> bool {
        self.atom_starts_at(0, unit.end)
    }

    /// The words rule, checked once at the end of a unit: nothing may begin
    /// where it ends.
    fn end_unit(&mut self, unit: Span) -> Result<(), ParseError> {
        if self.touches_after(unit) {
            return Err(self.touching(unit));
        }
        Ok(())
    }

    /// The atom ahead touches `first`.  The run is parsed rather than
    /// scanned, so `!{ a b }` and `[…]` inside it stay balanced; an error
    /// inside the run is reported instead.
    fn touching(&mut self, first: Span) -> ParseError {
        let mut run = || -> Result<ParseError, ParseError> {
            let (second, _) = self.capture_span(Self::parse_arg_unchecked)?;
            let mut last = second;
            while self.touches_after(last) {
                (last, _) = self.capture_span(Self::parse_arg_unchecked)?;
            }
            Ok(ParseError {
                kind: ParseErrorKind::Touching {
                    first,
                    second,
                    run: first.join(last),
                },
                ..ParseError::new(
                    Some(second),
                    "words touch: whitespace separates words, and nothing joins them",
                )
            })
        };
        run().unwrap_or_else(|e| e)
    }

    /// A marker (`^`, `...`, `!`) must touch what it marks.
    fn attached(&self, message: &str) -> Result<(), ParseError> {
        if self.next_token_is_adjacent() {
            Ok(())
        } else {
            Err(self.error(message))
        }
    }

    /// No gap since the previous token — what separates `$xs[0]` (an index)
    /// from `cmd $xs [0]` (a second argument).
    fn next_token_is_adjacent(&self) -> bool {
        let Some(Lexeme {
            span: prev_span, ..
        }) = self.tokens.get(self.pos.saturating_sub(1))
        else {
            return false;
        };
        let Some(Lexeme {
            span: next_span, ..
        }) = self.tokens.get(self.pos)
        else {
            return false;
        };
        prev_span.end == next_span.start
    }

    fn parse_redirect(&mut self) -> Result<Redirect<Ast>, ParseError> {
        let op_span = self.span();
        match self.peek().clone() {
            Token::StderrToStdout => {
                self.advance();
                Ok(Redirect::StderrToStdout)
            }
            Token::Redirect { stderr, op } => {
                self.advance();
                if self.at_cmd_end() {
                    return Err(ParseError::new(Some(op_span), redirect_needs_a_target(op)));
                }
                let (word_span, word) = self.capture_span(Self::parse_word)?;
                let redirect = redirect_word(stderr, op, word);
                if let Redirect::Stdin(StdinSource::Here(Ast::Word(w))) = &redirect {
                    let message = match w {
                        Word::Plain(_) => crate::syntax::NO_HEREDOCS,
                        Word::Slash(_) | Word::Tilde(_) => {
                            "`<<` feeds a string to stdin, not a file: \
                             to read a file into stdin, use `< path`"
                        }
                    };
                    return Err(ParseError::new(Some(word_span), message));
                }
                self.end_unit(word_span)?;
                Ok(redirect)
            }
            _ => Err(self.error("expected redirect")),
        }
    }

    /// One redirect, bound into `into`; a clash is reported at the whole
    /// second redirect.
    fn parse_redirect_into(&mut self, into: &mut Redirects<Ast>) -> Result<(), ParseError> {
        let (span, redirect) = self.capture_span(Self::parse_redirect)?;
        into.bind(redirect)
            .map_err(|m| ParseError::new(Some(span), m))
    }

    fn at_redirect(&self) -> bool {
        matches!(self.peek(), Token::Redirect { .. } | Token::StderrToStdout)
    }

    /// arg = atom | '...' atom
    ///
    /// The [`Ast::Spread`] node tells the elaborator to splice `x`'s elements
    /// into the argument list; `[...x]` is a literal and stays one argument.
    fn parse_arg(&mut self) -> Result<Ast, ParseError> {
        self.arg_of(Self::parse_atom)
    }

    /// [`Self::parse_arg`] without the words rule on its atom, which
    /// [`Self::touching`] needs to read the run it refuses.
    fn parse_arg_unchecked(&mut self) -> Result<Ast, ParseError> {
        self.arg_of(Self::parse_atom_unchecked)
    }

    fn arg_of(
        &mut self,
        atom: fn(&mut Self) -> Result<Ast, ParseError>,
    ) -> Result<Ast, ParseError> {
        if self.eat(&Token::Spread) {
            if self.at_cmd_end() {
                return Err(ParseError::new(
                    Some(self.prev_byte_span()),
                    SPREAD_NEEDS_A_VALUE,
                ));
            }
            self.attached(SPREAD_ATTACHES)?;
            let (span, inner) = self.capture_span(atom)?;
            Ok(Ast::Spread(Spanned::boxed(span, inner)))
        } else {
            atom(self)
        }
    }

    fn parse_head(&mut self) -> Result<Head, ParseError> {
        if self.eat(&Token::Caret) {
            let caret = self.prev_byte_span();
            self.attached("`^` attaches to the name it marks: write `^ls`")?;
            return match self.peek().clone() {
                Token::Word(Word::Plain(name)) => {
                    self.advance();
                    self.end_unit(caret.join(self.prev_byte_span()))?;
                    Ok(Head::ExternalName(name))
                }
                Token::Word(Word::Slash(_) | Word::Tilde(_)) => {
                    Err(self.error("'^' expects a bare command name, not a path"))
                }
                _ => Err(self.error("expected bare command name after '^'")),
            };
        }
        // `$[…]` is a value wherever it stands, whatever it unboxes to.
        if matches!(self.peek(), Token::Expr(_)) {
            return Ok(Head::Value(Box::new(self.parse_atom()?)));
        }
        // A literal word (`42`, `true`) and a lone `~` are values too.
        Ok(match self.parse_atom()? {
            Ast::Word(Word::Slash(s)) => Head::Path(s),
            Ast::Word(Word::Plain(s)) if WordLiteral::classify(&s).is_none() => Head::Bare(s),
            Ast::Word(Word::Tilde(path)) if path.suffix.is_some() => Head::TildePath(path),
            other => Head::Value(Box::new(other)),
        })
    }

    /// command = head (arg | redir)*
    fn parse_command(&mut self) -> Result<(Ast, Redirects<Ast>), ParseError> {
        if self.at_redirect() {
            return Err(self.error("redirect must follow a command"));
        }
        let headless = match self.peek() {
            Token::Pipe if self.peek_at(1) == &Token::Pipe => Some(NO_OR_OR),
            Token::Pipe => Some("a pipeline needs a command before `|`"),
            Token::Question => Some("`?` needs a command before it to fall back from"),
            _ => None,
        };
        if let Some(message) = headless {
            return Err(self.error(message));
        }
        let head = self.parse_head()?;
        let mut args: Vec<Spanned<Ast>> = Vec::new();
        let mut redirects = Redirects::default();
        while !self.at_cmd_end() {
            if self.at_redirect() {
                self.parse_redirect_into(&mut redirects)?;
            } else {
                args.push(self.spanned(Self::parse_arg)?);
            }
        }

        // A value head with nothing applied sheds the `Ast::Call` wrapper,
        // so downstream passes see the value itself.
        if args.is_empty()
            && let Head::Value(value) = head
        {
            return Ok((*value, redirects));
        }
        Ok((Ast::Call { head, args }, redirects))
    }

    /// Byte span of the last consumed token — where the production that just
    /// finished ends.  Falls back to the current one at input start.
    fn prev_byte_span(&self) -> Span {
        self.tokens
            .get(self.pos.saturating_sub(1))
            .map_or_else(|| self.span(), |l| l.span)
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
                let at = self.span();
                self.advance();
                Self::parse_interpolation_parts(&parts, at)
            }
            Token::Variable { name, .. } => {
                self.advance();
                Ok(Ast::Variable(name))
            }
            Token::Expr(tokens) => {
                let at = self.span();
                self.advance();
                parse_expr_block(tokens, at)
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
                | Token::Semi
                | Token::Eof
                | Token::RBrace
                | Token::RBracket
                | Token::RParen
                | Token::Pipe
                | Token::Question
                | Token::Comma
                | Token::Colon
                | Token::Spread
                | Token::Redirect { .. }
                | Token::StderrToStdout
        )
    }

    /// force = '!' variable index* | '!' primary — both callers leave the `!`
    /// for us, so the span starts there.
    ///
    /// A dereference reaches over its keys, so `!$p[tail]` forces the field
    /// that `$p[tail]` names.  Any other primary is forced first: leaving its
    /// postfix `[k]` to the enclosing `parse_atom` makes `!{cmd}[k]` select
    /// from the forced result.
    fn parse_bang(&mut self) -> Result<Ast, ParseError> {
        let (span, inner) = self.capture_span(|p| {
            p.advance(); // consume `!`
            p.attached("`!` attaches to what it forces: write `!{…}` or `!$x`")?;
            if matches!(p.peek(), Token::Variable { .. }) {
                p.parse_atom_unchecked()
            } else {
                p.parse_primary()
            }
        })?;
        Ok(Ast::Force(Spanned::boxed(span, inner)))
    }

    /// block = '{' program '}' | '{' '|' pattern+ '|' program '}'
    fn parse_block(&mut self) -> Result<Ast, ParseError> {
        self.expect(&Token::LBrace)?;
        if self.peek() == &Token::Newline && self.peek_at(1) == &Token::Pipe {
            self.advance();
            return Err(self.error(
                "a function's parameters open on the same line as `{`: \
                 write `{ |x|` and break the line after it",
            ));
        }
        if self.eat(&Token::Pipe) {
            let mut params: Vec<Spanned<Pattern>> = Vec::new();
            while self.peek() != &Token::Pipe {
                if self.at_stmt_end() {
                    return Err(self.error("expected `|` to close the parameter list: `{ |x| … }`"));
                }
                let (sp, p) = self.parse_binder()?;
                params.push(Spanned::new(sp, p));
            }
            self.expect(&Token::Pipe)?;
            if params.is_empty() {
                return Err(
                    self.error("lambda requires at least one parameter: use { } for thunks")
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

    /// collection = list | record | map — `[]` is the empty list and `[:]` the
    /// empty map, an entry with both sides erased.  Otherwise the items are read
    /// in one pass and each lifts the literal to at least its own kind: a label
    /// makes a record, a data key a map.
    fn parse_collection(&mut self) -> Result<Ast, ParseError> {
        self.expect(&Token::LBracket)?;

        if self.eat(&Token::RBracket) {
            return Ok(Ast::List(vec![]));
        }
        if self.eat(&Token::Colon) {
            if self.eat(&Token::RBracket) {
                return Ok(Ast::Map(vec![]));
            }
            return Err(self.error(
                "`[:]` is the empty map, and the only literal that opens with `:`; a map \
                 on written keys quotes them, `[\"a\": 1, \"b\": 2]`: a \"quoted\" key is \
                 data, where a bare `a:` or `'a':` is a record's label",
            ));
        }

        let mut items = Vec::new();
        self.parse_separated_until(&Token::RBracket, "collection", |p| {
            items.push(p.parse_collection_item()?);
            Ok(SepFlow::Cont)
        })?;
        items
            .into_iter()
            .try_fold(Literal::List(Vec::new()), Literal::push)?
            .into_ast()
    }

    /// item = '...' atom | mapkey ':' atom | atom
    fn parse_collection_item(&mut self) -> Result<CollectionItem, ParseError> {
        if self.eat(&Token::Spread) {
            self.attached(SPREAD_ATTACHES)?;
            return Ok(CollectionItem::Spread(self.spanned(Self::parse_atom)?));
        }
        if self.key_colon_here(KeyAlphabet::Data) {
            let key = self.parse_map_key()?;
            self.expect(&Token::Colon)?;
            return Ok(CollectionItem::Entry {
                key,
                value: self.spanned(Self::parse_atom)?,
            });
        }
        if let Token::Word(Word::Plain(word)) = self.peek()
            && let Some(key) = word.strip_suffix(':')
            && self.touches_next()
        {
            return Err(self.error(format!(
                "`{word}` is one word; for the key `{key}`, put a space after the \
                 colon: `{key}: value`"
            )));
        }
        Ok(CollectionItem::Elem(self.spanned(Self::parse_atom)?))
    }

    /// True when the next token is a map key followed by `:`.  The one shape
    /// test behind literal and pattern alike, so the two cannot drift on what a
    /// key is.  A tag is key-*shaped* but is no key: it is admitted only so
    /// [`Self::parse_static_key`] gets to say why.
    fn key_colon_here(&self, alphabet: KeyAlphabet) -> bool {
        let at = |i: usize| self.tokens.get(self.pos + i).map(|l| &l.token);
        let is_key = match at(0) {
            Some(Token::Word(Word::Plain(_)) | Token::SingleQuoted(_) | Token::Tag(_)) => true,
            Some(Token::Variable { .. } | Token::DoubleQuoted(_)) => alphabet == KeyAlphabet::Data,
            _ => false,
        };
        is_key && matches!(at(1), Some(Token::Colon))
    }

    /// Lower the segments of a double-quoted string.  A splice is the tokens
    /// it would be outside the string, and parses as the same atom; their
    /// spans still address the outer source.
    fn parse_interpolation_parts(
        parts: &[Spanned<StringPart>],
        at: Span,
    ) -> Result<Ast, ParseError> {
        match parts {
            [] => return Ok(Ast::Literal(String::new())),
            [
                Spanned {
                    item: StringPart::Literal(s),
                    ..
                },
            ] => return Ok(Ast::Literal(s.clone())),
            _ => {}
        }

        let mut ast_parts = Vec::new();
        for part in parts {
            let segment = match &part.item {
                StringPart::Literal(s) => Ast::Literal(s.clone()),
                StringPart::Splice(tokens) => Self::run_complete(
                    tokens.clone(),
                    part.span.unwrap_or(at),
                    trailing_input,
                    Self::parse_atom,
                )?,
            };
            ast_parts.push(Spanned::with_span(part.span, segment));
        }

        Ok(Ast::Interpolation(ast_parts))
    }

    // ── Expression blocks (Pratt parser) ────────────────────────────

    /// Precedence-climbing loop over [`Token::Op`]s, the tokens the lexer emits
    /// for operators inside `$[…]` and nowhere else.  It needs no depth guard
    /// of its own: every depth-growing recursion bottoms out in
    /// [`Self::parse_expr_operand`], and the binary right-hand side is bounded
    /// by the precedence ladder.
    fn parse_expr_prec(&mut self, min_prec: u8) -> Result<Spanned<Box<Ast>>, ParseError> {
        let start = self.span();
        let (span, first) = self.capture_span(Self::parse_expr_operand)?;
        let mut left = Spanned::boxed(span, first);

        loop {
            let (op, prec) = match *self.peek() {
                Token::Op(Operator::Assign) => return Err(self.error(ASSIGN_IS_NO_OPERATOR)),
                Token::Op(op) => (op, precedence(op)),
                _ => break,
            };
            if prec < min_prec {
                break;
            }
            let at = self.span();
            self.advance(); // consume operator token
            if self.peek() == &Token::Eof {
                return Err(ParseError::new(
                    Some(at),
                    format!("`{op}` needs an operand on its right"),
                ));
            }
            let right = self.parse_expr_prec(prec + 1)?;
            let node = match op {
                Operator::And => Ast::And(left, right),
                Operator::Or => Ast::Or(left, right),
                Operator::Binary(o) => {
                    if !matches!(o, BinaryOp::Eq(_)) {
                        numeric_operand(&left)?;
                        numeric_operand(&right)?;
                    }
                    Ast::Binary(left, o, right)
                }
                Operator::Assign => return Err(self.error(ASSIGN_IS_NO_OPERATOR)),
            };
            left = Spanned::boxed(start.join(self.prev_byte_span()), node);
        }

        Ok(left)
    }

    /// The prefix operator `name`, at `at`, has its operand ahead.
    fn operand_after(&self, at: Span, name: &str) -> Result<(), ParseError> {
        if self.peek() == &Token::Eof {
            return Err(ParseError::new(
                Some(at),
                format!("`{name}` needs an operand"),
            ));
        }
        Ok(())
    }

    /// operand = '(' expr ')' | '-' operand | 'not' operand | atom
    ///
    /// The expression grammar's `nested()` chokepoint: parenthesised
    /// sub-expressions and the unary prefixes both come back here, so this
    /// guard bounds every depth-growing path inside `$[…]`.  An operand is
    /// any atom — the typechecker, not the parser, says which values `+`
    /// or `==` accept.
    fn parse_expr_operand(&mut self) -> Result<Ast, ParseError> {
        self.nested(|p| match p.peek().clone() {
            // `()` is the unit literal, an atom; anything else `(` opens
            // here is a grouped sub-expression.
            Token::LParen if p.tokens.get(p.pos + 1).map(|l| &l.token) != Some(&Token::RParen) => {
                let (span, expr) = p.capture_span(|p| {
                    p.advance();
                    let expr = p.parse_expr_prec(0)?;
                    p.expect(&Token::RParen)?;
                    Ok(expr)
                })?;
                p.end_unit(span)?;
                Ok(*expr.item)
            }
            Token::Op(Operator::Binary(BinaryOp::Arith(ArithOp::Sub))) => {
                let minus = p.span();
                p.advance();
                p.operand_after(minus, "-")?;
                let (span, inner) = p.capture_span(Self::parse_expr_operand)?;
                let inner = Spanned::boxed(span, inner);
                numeric_operand(&inner)?;
                Ok(Ast::Negate(inner))
            }
            Token::Word(Word::Plain(s)) if s == "not" => {
                let word = p.span();
                p.advance();
                p.end_unit(word)?;
                p.operand_after(word, "not")?;
                // An operand, not an expression: `not` binds tighter than
                // every binary operator, so `not $x == 0` is `(not $x) == 0`.
                let (span, inner) = p.capture_span(Self::parse_expr_operand)?;
                Ok(Ast::Not(Spanned::boxed(span, inner)))
            }
            Token::Op(Operator::Assign) => Err(p.error(ASSIGN_IS_NO_OPERATOR)),
            Token::Op(op) => Err(p.error(format!(
                "`{op}` is an operator and needs an operand on each side"
            ))),
            _ => p.parse_atom(),
        })
    }
}

/// Eliminate a word-taking redirect token into the three streams.  The
/// lexer has already refused every fd spelling but `2`, which is `stderr`.
fn redirect_word<T>(stderr: bool, op: RedirectOp, word: T) -> Redirect<T> {
    match (stderr, op) {
        (_, RedirectOp::Read) => Redirect::Stdin(StdinSource::File(word)),
        (_, RedirectOp::HereString) => Redirect::Stdin(StdinSource::Here(word)),
        (false, RedirectOp::Write(mode)) => Redirect::Stdout(mode, word),
        // Stderr streams: staging diagnostics for an atomic commit would
        // withhold them until the frame settles.
        (true, RedirectOp::Write(mode)) => Redirect::Stderr(
            match mode {
                WriteMode::Write => WriteMode::Stream,
                other => other,
            },
            word,
        ),
    }
}

/// What a word-taking redirect is missing when nothing follows it.
fn redirect_needs_a_target(op: RedirectOp) -> &'static str {
    match op {
        RedirectOp::Write(_) => "`>` needs a file to write to",
        RedirectOp::Read => "`<` needs a file to read",
        RedirectOp::HereString => "`<<` needs a string to feed",
    }
}

/// A bare non-numeral word is a string in `$[…]` as everywhere, so under an
/// operator that wants numbers it is a certain type error — and almost always
/// a dropped `$`.  Refused here so the error can say so; `==` and `!=` accept
/// strings and are exempt.
fn numeric_operand(operand: &Spanned<Box<Ast>>) -> Result<(), ParseError> {
    match &*operand.item {
        Ast::Word(Word::Plain(w)) if WordLiteral::classify(w).is_none() => Err(ParseError::new(
            operand.span,
            if lexer::is_ident(w) {
                format!("`{w}` is the string '{w}' here, not a number: did you mean `${w}`?")
            } else if let Some(mantissa) = exponent_only(w) {
                format!(
                    "`{w}` is the string '{w}' here, not a number; a float needs a point: write `{mantissa}.0{}`",
                    &w[mantissa.len()..]
                )
            } else {
                format!("`{w}` is the string '{w}' here, not a number")
            },
        )),
        _ => Ok(()),
    }
}

/// The integer mantissa of an exponent-only spelling such as `1e5`.
fn exponent_only(w: &str) -> Option<&str> {
    let (n, Shape::Int) = numeral::prefix(w)? else {
        return None;
    };
    let (mantissa, exp) = w.split_at(n);
    let digits = exp.strip_prefix(['e', 'E'])?.trim_start_matches(['+', '-']);
    (!digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit())).then_some(mantissa)
}

/// Binding power, low to high: `||`, `&&`, comparison, add/sub, mul/div/mod.
/// The unary prefixes bind tighter than all of these.  `=` binds nothing: it
/// is refused wherever it stands.
fn precedence(op: Operator) -> u8 {
    match op {
        Operator::Assign => 0,
        Operator::Or => 1,
        Operator::And => 2,
        Operator::Binary(BinaryOp::Eq(_) | BinaryOp::Compare(_)) => 3,
        Operator::Binary(BinaryOp::Arith(ArithOp::Add | ArithOp::Sub)) => 4,
        Operator::Binary(BinaryOp::Arith(ArithOp::Mul | ArithOp::Div | ArithOp::Mod)) => 5,
    }
}

const NO_OR_OR: &str = "ral has no `||`: `a ? b` runs `b` when `a` fails; inside \
     `$[…]`, `||` is the Boolean connective";

const ASSIGN_IS_NO_OPERATOR: &str = "`=` is not an operator in `$[…]`: did you mean `==`?";

const SPREAD_ATTACHES: &str = "`...` attaches to the value it spreads: write `...$xs`";

const SPREAD_NEEDS_A_VALUE: &str = "`...` spreads the value after it: write `...$xs`";

const KEYED_ELEM_ERROR: &str = "this collection has `key: value` entries, so every entry \
     needs a key: or drop the keys to make it a list";

/// A bracket literal as its items arrive. Each item lifts it to at least its
/// own kind along list < record < map, so no item meets a kind already ruled out.
enum Literal {
    List(Vec<ListElem>),
    Record(Vec<RecordEntry>),
    Map(Vec<MapEntry>),
}

impl Literal {
    fn push(self, item: CollectionItem) -> Result<Self, ParseError> {
        match (self, item) {
            (Self::List(mut elems), CollectionItem::Elem(a)) => {
                elems.push(ListElem::Single(a));
                Ok(Self::List(elems))
            }
            (Self::List(mut elems), CollectionItem::Spread(a)) => {
                elems.push(ListElem::Spread(a));
                Ok(Self::List(elems))
            }
            (Self::List(elems), entry @ CollectionItem::Entry { .. }) => {
                let items = elems
                    .into_iter()
                    .map(|elem| match elem {
                        ListElem::Spread(a) => Ok(RecordEntry::Spread(a)),
                        ListElem::Single(a) => Err(ParseError::new(a.span, KEYED_ELEM_ERROR)),
                    })
                    .collect::<Result<_, _>>()?;
                Self::Record(items).push(entry)
            }
            (Self::Record(mut items), CollectionItem::Spread(a)) => {
                items.push(RecordEntry::Spread(a));
                Ok(Self::Record(items))
            }
            (
                Self::Record(mut items),
                CollectionItem::Entry {
                    key: MapKeyForm::Label(key),
                    value,
                },
            ) => {
                items.push(RecordEntry::Field { key, value });
                Ok(Self::Record(items))
            }
            (Self::Record(items), entry @ CollectionItem::Entry { .. }) => {
                let entries = items
                    .into_iter()
                    .map(|item| match item {
                        RecordEntry::Spread(a) => MapEntry::Spread(a),
                        RecordEntry::Field { key, value } => MapEntry::Entry {
                            key: key.map(Ast::Literal),
                            value,
                        },
                    })
                    .collect();
                Self::Map(entries).push(entry)
            }
            (Self::Record(_) | Self::Map(_), CollectionItem::Elem(a)) => {
                Err(ParseError::new(a.span, KEYED_ELEM_ERROR))
            }
            (Self::Map(mut entries), CollectionItem::Spread(a)) => {
                entries.push(MapEntry::Spread(a));
                Ok(Self::Map(entries))
            }
            (Self::Map(mut entries), CollectionItem::Entry { key, value }) => {
                entries.push(map_entry(key, value));
                Ok(Self::Map(entries))
            }
        }
    }

    /// Read against the bracket's *final* classification, never while it is
    /// being built: `push` walks list → record → map, so `[...[:], ...[:], x:
    /// 1, $k: 2]` is a map and its two spreads are entries, not bases.
    fn into_ast(self) -> Result<Ast, ParseError> {
        match self {
            Self::List(elems) => Ok(Ast::List(elems)),
            Self::Record(entries) => record_literal(entries),
            Self::Map(entries) => Ok(Ast::Map(entries)),
        }
    }
}

/// A record literal with a spread is an *update* of one base: `[...$r, k: v]`
/// replaces the fields `r` has.  Two bases would be a merge, and which of two
/// unknown remainders wins is a question the literal cannot answer; a spread
/// after a field would read as though the field came first.  Shared with a
/// form's option bracket, whose entries are a record's too.
fn record_literal(entries: Vec<RecordEntry>) -> Result<Ast, ParseError> {
    let mut spreads = entries.iter().enumerate().filter_map(|(i, e)| match e {
        RecordEntry::Spread(a) => Some((i, a)),
        RecordEntry::Field { .. } => None,
    });
    let first = spreads.next();
    if let Some((_, second)) = spreads.next() {
        return Err(ParseError::new(
            second.span,
            "a record can be written over one other record, not two: \
             write out the fields you need from this one, as in \
             `[...$base, y: $other[y]]`, or merge them in a block",
        ));
    }
    if let Some((i, spread)) = first
        && i > 0
    {
        return Err(ParseError::new(
            spread.span,
            "a record's spread comes first: `[...$r, k: v]`",
        ));
    }
    Ok(Ast::Record(entries))
}

fn map_entry(key: MapKeyForm, value: Spanned<Ast>) -> MapEntry {
    MapEntry::Entry {
        key: key.into_ast(),
        value,
    }
}

impl MapKeyForm {
    fn into_ast(self) -> Spanned<Ast> {
        match self {
            Self::Label(key) => key.map(Ast::Literal),
            Self::Data(key) => key,
        }
    }
}

/// What a token the parse never reached means for a whole program or a
/// `$(…)` splice.
fn trailing_input(found: &Token) -> String {
    // `parse_program` stops at `}` without consuming it, so a leftover one
    // means an unmatched brace — which may sit mid-program, where "trailing
    // input" would be doubly false.
    if *found == Token::RBrace {
        return "unmatched `}`: no enclosing block is open".into();
    }
    format!("trailing input: unexpected {found} after the parse completed")
}

/// Parse the pre-lexed body of `$[…]` as one expression.  `at` is the `$[…]`
/// token itself, the only span an empty body can be reported against.
fn parse_expr_block(tokens: Vec<Lexeme>, at: Span) -> Result<Ast, ParseError> {
    if tokens.is_empty() {
        return Err(ParseError::new(
            Some(at),
            "`$[…]` holds an expression: `$[1 + 2]`, `$[$n > 0]`",
        ));
    }
    Parser::run_complete(
        tokens,
        at,
        |found| format!("expected an operator before {found}: `$[…]` holds one expression"),
        |p| Ok(*p.parse_expr_prec(0)?.item),
    )
}

/// The tokens an atom may begin with.
fn is_atom_start(tok: &Token) -> bool {
    matches!(
        tok,
        Token::Word(_)
            | Token::SingleQuoted(_)
            | Token::DoubleQuoted(_)
            | Token::Variable { .. }
            | Token::Expr(_)
            | Token::Bang
            | Token::Tag(_)
            | Token::LBrace
            | Token::LBracket
            | Token::LParen
            | Token::Spread
    )
}

/// The words that open a stage and take what follows them: no atom may touch
/// one.
fn is_stage_keyword(word: &str) -> bool {
    matches!(word, "return" | "if" | "case" | "else" | "elsif") || CONTROL_OPERATORS.contains(&word)
}

/// The words that begin a statement of their own, which no value can be.
fn begins_statement(word: &str) -> bool {
    matches!(word, "if" | "case" | "let") || CONTROL_OPERATORS.contains(&word)
}

/// Names no binding may take: a keyword by [`crate::syntax::is_keyword`], or a
/// value literal.  A `^name` head never reaches a pattern, so `^try` still
/// resolves through PATH.
fn is_reserved(s: &str) -> bool {
    crate::syntax::is_keyword(s) || matches!(s, "true" | "false")
}

/// A literal's key before its entry is built: a label, which a record may
/// hold, or data (`"…"`, `$name`), which makes the literal a map.
enum MapKeyForm {
    Label(Spanned<String>),
    Data(Spanned<Ast>),
}

/// One item of a bracketed literal, read before the literal's shape is known.
enum CollectionItem {
    Spread(Spanned<Ast>),
    Elem(Spanned<Ast>),
    Entry {
        key: MapKeyForm,
        value: Spanned<Ast>,
    },
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
