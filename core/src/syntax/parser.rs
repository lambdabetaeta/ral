//! Parser: token stream → AST.
//!
//! Recursive-descent over the [`crate::syntax::lexer`] output.  The grammar is
//! statement-oriented: a *program* is a sequence of statements separated
//! by newlines or `;`.  A *statement* is either a [binding][Parser::parse_binding_opt]
//! or a [chain][Parser::parse_chain] of `?`-separated `bg-pipeline`s; a
//! *bg-pipeline* is a [pipeline][Parser::parse_pipeline] with an optional
//! trailing `&`; a *pipeline* is `|`-connected stages; a *stage* is
//! `return`, `if`, `case`, or a command.  `|`, `?`, `,`, and `=` are
//! continuation tokens — newlines around them are absorbed.
//!
//! The let-RHS chain is intentionally narrower than the statement chain —
//! see [`Parser::parse_binding_opt`].
//!
//! Each statement-list element is wrapped in an [`Stmt`] that carries the
//! source span of the statement's first token.  The elaborator stamps that
//! span on the IR it emits, so the parser never has to thread a span
//! through every constructor.
//!
//! Arithmetic inside `$[...]` is parsed by a small Pratt sub-parser
//! ([`Parser::parse_expr_prec`]) over the token stream the *outer*
//! lexer produced for the expression block: no re-lex hop, no raw
//! substring round-trip.  The lexer also fuses `&&` / `||` for us
//! (see `lexer::Lexer::scan_expr_block`) so Pratt sees the same
//! bare-word logical operators it would outside any nesting.

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
///
/// Each arm names a production the parser was midway through when it ran
/// out of tokens.  The lexer-origin arms mirror the still-open
/// [`LexErrorKind`] kinds (see [`LexErrorKind::is_incomplete`]); the
/// parser-origin arm covers a binder that has its `=` but no right-hand
/// side yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incompleteness {
    /// A string, balanced `{}` / `[]`, or `$(…)` ran past end of input.
    UnclosedLexeme,
    /// A `let` binding consumed its `=` but reached end of input before
    /// the right-hand side.
    BinderAwaitingRhs,
    /// A continuation token — a pipeline `|`, a chain `?`, or an
    /// `if` / `elsif` / `else` keyword — was consumed, then the input ran
    /// out before the stage, branch, or body it demands.
    AwaitingContinuation,
}

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    /// Byte range of the offending token (or the opening delimiter, for
    /// lexer-originating errors).  `None` when the parser had no token
    /// to point at — practically rare given that the lexer always emits
    /// an `Eof` token, but kept optional per the source.rs idiom.
    pub span: Option<Span>,
    /// Set when the failure originated in the lexer; carries enough
    /// structure for the diagnostic layer to draw multi-label reports
    /// (opening delimiter + EOF position + nested-form note).  `None`
    /// for parser-originating errors, which still render as a single
    /// label.
    pub lex_kind: Option<LexErrorKind>,
    /// Set when the failure is the input merely running short of a
    /// complete program.  Drives REPL line continuation: the parser's own
    /// signal that the user is mid-typing.
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
/// Returns `Err` if lexing fails, or if the token stream is not a valid
/// program — an unexpected token, an unclosed construct, or input that ends
/// before a production completes.
pub fn parse(source: &str) -> Result<Vec<Stmt>, ParseError> {
    parse_with(source, crate::source::FileId::DUMMY)
}

/// Returns `true` when `input` is incomplete and the user's next line should
/// be joined to it before parsing.
///
/// This runs the real parser and asks
/// whether it failed because the input ran short — an open `'…'`, `"…"`,
/// `!{…}`, `$(…)`, or an unbalanced `{` / `[`, or a `let` still awaiting
/// its right-hand side.  The verdict is the parser's own structured
/// [`Incompleteness`] signal.
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
/// Returns `Err` if lexing fails, or if the token stream is not a valid
/// program — an unexpected token, an unclosed construct, or input that ends
/// before a production completes.
pub fn parse_with(source: &str, file: crate::source::FileId) -> Result<Vec<Stmt>, ParseError> {
    let tokens = lexer::lex_with(source, file)?;
    Parser::run_complete(tokens, Parser::parse_program)
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
    /// Current depth of recursive descent through nesting-introducing
    /// productions.  Each of the three mutually-recursive sub-grammars
    /// passes through one guarded chokepoint per level —
    /// [`Parser::parse_primary`] (values), [`Parser::parse_expr_atom`]
    /// (arithmetic), and [`Parser::parse_pattern`] (patterns) — so this
    /// one counter bounds them all.  Bumped and decremented by
    /// [`Parser::nested`] — closure-style so the decrement is always
    /// paired with its increment on every error-return path.
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

    /// Parse a whole token stream and require that `body` consumed every
    /// token up to `Eof`.  This is the completion contract every
    /// sub-token-stream site shares: a `body` production that stops early
    /// — `parse_program` halting at a stray `RBrace`, `parse_word`
    /// returning after one word, the Pratt parser settling on one
    /// expression — must not silently drop the remainder.  The first
    /// leftover token is reported as trailing input, spanning that token —
    /// except a leftover `RBrace`, which is named as an unmatched `}`
    /// rather than folded into the generic message, since neither "trailing"
    /// nor "after the parse completed" holds when the brace sits mid-program.
    ///
    /// It is the only constructor reachable from the sub-stream sites
    /// (`parse_force_body`, `parse_expr_block`, `parse_index_keys`) and
    /// from `parse_with` at the top level, where it additionally promotes
    /// a depth-0 `RBrace` from a stop condition into an error.
    fn run_complete<T>(
        tokens: Vec<(Token, Span)>,
        body: impl FnOnce(&mut Self) -> Result<T, ParseError>,
    ) -> Result<T, ParseError> {
        let mut parser = Self::new(tokens);
        let value = body(&mut parser)?;
        if parser.peek() != &Token::Eof {
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

    /// Run `body` under one extra level of recursive descent.  Fails
    /// with a friendly diagnostic when the cap is exceeded — the
    /// message names the construct ("nesting too deep") and the limit,
    /// so a reader knows it isn't a syntax problem but a resource
    /// limit.  Using a closure rather than an RAII guard sidesteps the
    /// borrow-checker conflict between holding a `&mut self.depth`
    /// and calling further `&mut self` methods inside the body.
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
        self.tokens
            .get(self.pos)
            .map_or(&Token::Eof, |(t, _)| t)
    }

    /// Span of the current token, or — once the cursor has run past the
    /// last token — the EOF token's span.  The lexer always emits an
    /// `Eof`, so this only falls back to a synthetic point span on a
    /// truly empty token vector (a state real callers never produce).
    fn span(&self) -> Span {
        self.tokens
            .get(self.pos)
            .or_else(|| self.tokens.last())
            .map_or_else(|| Span::point(crate::source::FileId::DUMMY, 0), |(_, s)| *s)
    }

    fn advance(&mut self) -> &Token {
        let tok = self
            .tokens
            .get(self.pos)
            .map_or(&Token::Eof, |(t, _)| t);
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

    /// Like [`Self::error`] but flags the failure as the input running
    /// short of a complete program — the REPL reads another line rather
    /// than reporting the error.
    fn incomplete(&self, why: Incompleteness, message: impl Into<String>) -> ParseError {
        ParseError {
            incompleteness: Some(why),
            ..self.error(message)
        }
    }

    /// Like [`Self::error`] but attaches an explicit `span` rather than
    /// the span of the current token.
    fn error_at(span: Span, message: impl Into<String>) -> ParseError {
        ParseError {
            message: message.into(),
            span: Some(span),
            lex_kind: None,
            incompleteness: None,
        }
    }

    /// Guard the point just after a continuation token (`|`, `?`, `if`,
    /// `elsif`, `else`) has been consumed and is about to demand a stage,
    /// branch, or body.  If the input ran out here, report it as
    /// awaiting-continuation so the REPL reads another line instead of
    /// erroring on the dangling operator.
    fn require_continuation(&self, what: &str) -> Result<(), ParseError> {
        if self.peek() == &Token::Eof {
            return Err(self.incomplete(
                Incompleteness::AwaitingContinuation,
                format!("expected {what} after the continuation"),
            ));
        }
        Ok(())
    }

    /// Capture the byte span covered by `parse`.  Returns `(span, value)`
    /// where `span` runs from the byte of the token at the current
    /// position to the end of the last token `parse` consumed —
    /// collapsing the open-coded
    /// `let start = self.span(); let v = …; let span = start.join(self.prev_byte_span())`
    /// recipe at most per-construct span-capture sites (App args,
    /// Index keys, If cond, Case operands, Return value, …).  Call
    /// sites lift the result into whatever shape (`Spanned<Box<Ast>>`,
    /// `Spanned<Ast>`, raw `Span`) the surrounding AST node expects.
    ///
    /// Sites that intentionally keep the recipe inline:
    /// [`Self::parse_command`] (redirect-interleaved arg loop with
    /// an outer span) and [`Self::parse_control_op`] (multi-output
    /// production where threading through a closure adds friction).
    fn capture_span<T>(
        &mut self,
        parse: impl FnOnce(&mut Self) -> Result<T, ParseError>,
    ) -> Result<(Span, T), ParseError> {
        let start = self.span();
        let v = parse(self)?;
        let span = start.join(self.prev_byte_span());
        Ok((span, v))
    }

    /// Drive a comma-separated list terminated by `end`.  The closure
    /// parses one item and signals whether the run continues (`Cont`)
    /// or that item closes it (`Stop`).  A trailing comma before `end`
    /// is allowed in either case.  `label` names the construct ("list",
    /// "map pattern", …) for the error message on a missing separator.
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
            // `parse_stmt` stops at (but does not consume) the trailing
            // newline, so the captured span runs from the first token
            // of the statement to its last — never a separator.
            // Underlining the full statement is essential for
            // diagnostics whose offending Val sub-expression has no
            // span of its own.
            let (span, kind) = self.capture_span(Self::parse_stmt)?;
            stmts.push(Spanned::new(span, kind));
            self.skip_newlines();
        }
        Ok(stmts)
    }

    /// stmt = binding | chain
    ///
    /// A `let` binding is a *statement*, never a pipeline stage or chain
    /// branch — its RHS already absorbs an entire pipeline-and-chain, and
    /// embedding it deeper would produce an `Ast::Let` in expression
    /// position (which the elaborator cannot lower).  Catching it here is
    /// what keeps `parse_stage`'s leading-`let` rejection truly defensive.
    ///
    /// The trailing newline is *not* consumed — that's `parse_program`'s
    /// job — so callers computing the statement's span see only the
    /// statement's own tokens.
    fn parse_stmt(&mut self) -> Result<Ast, ParseError> {
        match self.parse_binding_opt()? {
            Some(binding) => Ok(binding),
            None => self.parse_chain(),
        }
    }

    /// chain = bg-pipeline (NL? '?' bg-pipeline)*
    ///
    /// Used for the statement-level `?`-chain.  Each arm may carry its
    /// own trailing `&` (handled inside `parse_bg_pipeline`).  The
    /// `let`-RHS chain has its own narrower variant, [`parse_chain_no_bg`],
    /// that rejects per-arm `&`.
    fn parse_chain(&mut self) -> Result<Ast, ParseError> {
        self.parse_chain_of(Self::parse_bg_pipeline)
    }

    /// Variant of `parse_chain` for `let` RHS: arms are bare pipelines, so
    /// per-arm `&` is rejected — see [`Self::parse_binding_opt`].
    fn parse_chain_no_bg(&mut self) -> Result<Ast, ParseError> {
        self.parse_chain_of(Self::parse_pipeline)
    }

    /// Shared shape of both chain parsers: one or more arms separated by
    /// `?`-continuations.  A singleton chain collapses to the bare arm —
    /// `Ast::Chain` is reserved for 2+ branches so downstream passes can
    /// pattern-match on its presence without a length guard.
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

    /// Consume `?` with at most one preceding newline (the continuation
    /// rule in §1).  Rewinds and returns false if no `?` follows.
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
    ///
    /// `|` is a continuation token: a newline before or after `|` is ignored.
    fn parse_pipeline(&mut self) -> Result<Ast, ParseError> {
        let (first_span, first) = self.capture_span(Self::parse_stage)?;
        let mut stages = vec![Spanned::new(first_span, first)];

        // `|` is a continuation token: newlines are ignored on either side.
        while self.eat_continuation(&Token::Pipe) {
            self.require_continuation("a pipeline stage")?;
            let (span, stage) = self.capture_span(Self::parse_stage)?;
            stages.push(Spanned::new(span, stage));
        }

        if stages.len() == 1 {
            // Single stage — unwrap the Stmt and return the stage directly.
            Ok(stages.remove(0).item)
        } else {
            Ok(Ast::Pipeline(stages))
        }
    }

    /// Consume `tok` surrounded by any number of newlines on either side.
    /// This is the "fully continuation token" rule from SPEC §1: newlines
    /// before *and* after are absorbed.  Returns true on success; rewinds
    /// on miss.
    ///
    /// The narrower `?` rule — at most one leading newline, zero trailing
    /// — has its own helper, [`Self::eat_chain_question`].
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
        // Capture the inner pipeline's span *before* checking for `&`,
        // so a wrapped `Background` underlines just the pipeline (not
        // the `&` token).
        let (inner_span, node) = self.capture_span(Self::parse_pipeline)?;
        if self.peek() == &Token::Ampersand {
            self.advance();
            return Ok(Ast::Background(Spanned::boxed(inner_span, node)));
        }
        Ok(node)
    }

    /// stage = return-stage | if-stage | case-stage | command | atom-stage
    ///
    /// `let` is a statement, not a stage — `parse_stmt` peels it off
    /// before reaching here.  Seeing it now means the caller embedded
    /// a binding in pipeline or chain position (`cmd | let x = …`,
    /// `cmd ? let x = …`); reject it with a clear error rather than
    /// mis-parse `let` as a command head.
    fn parse_stage(&mut self) -> Result<Ast, ParseError> {
        // The plain-word head of the stage decides which dedicated form
        // (if any) takes over before falling through to `parse_command`.
        // Each special form is named once here, so the dispatch table
        // doubles as a list of reserved stage keywords.
        match self.peek().as_plain_word() {
            // `let` is a statement, not a stage — `parse_stmt` peels it
            // off before reaching here.  Reaching this arm means the
            // caller embedded a binding in pipeline or chain position
            // (`cmd | let x = …`, `cmd ? let x = …`).
            Some("let") => Err(self.error(
                "`let` is a statement, not a pipeline stage or chain branch — \
                 move the binding to its own line, or wrap the consumer in a \
                 block: `{ let x = …; … }`",
            )),
            // `return` lifts a value into a computation — not an
            // implicit control-flow escape.
            Some("return") => self.parse_return_stage(),
            // `if`: syntactic stage form, bespoke AST node.
            Some("if") => self.parse_if(),
            // `case <scrutinee> [<handlers>]`: tag-keyed record of
            // thunks as the table; the typing rule is bespoke, so it
            // gets a dedicated AST node rather than going through the
            // regular command path.
            Some("case") => self.parse_case(),
            // Control-operator stage forms (`try`/`guard`/`within`/
            // `grant`/`audit`).  The `^try` external-only form is
            // unaffected — it begins with `Token::Caret` and falls
            // through to `parse_command` via the default arm.
            Some(name) => match ScopeAst::lookup_keyword(name) {
                Some(kw) => self.parse_control_op(kw),
                None => self.parse_command(),
            },
            None => self.parse_command(),
        }
    }

    /// Parse a fixed-arity control operator described by `kw`: the
    /// bare-head keyword, followed by exactly `kw.arity` function-
    /// atom operands and an optional run of trailing I/O redirects.
    /// `kw.build` destructures the arity-validated operand vector
    /// into the matching [`ScopeAst`]; the result is wrapped in
    /// [`Ast::Scope`] carrying any trailing redirects.
    ///
    /// Each operand is an atom, not a call argument: the operands fill
    /// fixed positions rather than a splat-able argument list, so a
    /// spread (`...x`) is meaningless here and is rejected outright —
    /// using `parse_arg` would admit an `Ast::Spread` that has no
    /// control-operator lowering.
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

    /// Collect a (possibly empty) run of trailing I/O redirects.  Used
    /// by [`parse_control_op`], whose operand arity is fixed: any
    /// `Token::Redirect` after the last operand attaches to the whole
    /// scope form.  `parse_command` uses its own arg/redirect-
    /// interleaved loop because plain commands accept redirects
    /// anywhere among their arguments.
    fn collect_trailing_redirects(&mut self) -> Result<Vec<Redirect>, ParseError> {
        let mut redirects = Vec::new();
        while !self.at_cmd_end() && matches!(self.peek(), Token::Redirect { .. }) {
            redirects.push(self.parse_redirect()?);
        }
        Ok(redirects)
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
        let (scrut_span, scrutinee) = self.capture_span(Self::parse_atom)?;
        self.skip_newlines();
        let (table_span, table) = self.capture_span(Self::parse_atom)?;
        Ok(Ast::Case {
            scrutinee: Spanned::boxed(scrut_span, scrutinee),
            table: Spanned::boxed(table_span, table),
        })
    }

    /// if = 'if' atom atom [elsif atom atom]* [else atom]
    ///
    /// Both branches are atoms: blocks, force expressions, variables — any value.
    /// The typechecker ensures they are thunks.  When no else branch is given
    /// the condition is evaluated for side effects only (type Unit).
    ///
    /// The leading `if` and every `elsif` collapse into one `branches`
    /// vector on the surface — they parse the same way and have the
    /// same semantics, only the keyword differs.
    fn parse_if(&mut self) -> Result<Ast, ParseError> {
        self.advance(); // consume 'if'
        self.skip_newlines();
        self.require_continuation("the `if` condition")?;
        let mut branches = vec![self.parse_if_branch()?];
        let mut else_ = None;

        loop {
            // Allow the elsif/else keywords on the next line.
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

        Ok(Ast::If { branches, else_ })
    }

    /// Parse one `cond body` pair — shared by the leading `if` arm and
    /// every `elsif` arm.
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
    /// Returns `None` if the next token is not `let`, so callers can fall
    /// through to the chain-statement path.  The let RHS is intentionally
    /// narrower than a top-level [`parse_chain`]: per-arm `&` is rejected
    /// (a chain arm here is a bare `pipeline`), and a single trailing `&`
    /// backgrounds the *whole* RHS.  This avoids the ambiguity where
    /// `let x = a ? b &` would otherwise read as backgrounding only `b`.
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
        // Allow the RHS to start on the next line: `let x =\n  expr`.
        self.skip_newlines();
        if self.peek() == &Token::Eof {
            return Err(self.incomplete(
                Incompleteness::BinderAwaitingRhs,
                "expected the right-hand side of the `let` binding",
            ));
        }
        // `inner_span` covers the chain itself; the wrapping `Background`
        // (if `&` follows) underlines just the chain, while the
        // outer `value_span` extends to include the `&` token so the
        // `Spanned<Box<Ast>>` on `Let.value` covers the full RHS.
        let (inner_span, mut value) = self.capture_span(Self::parse_chain_no_bg)?;
        // Trailing `&` backgrounds the whole RHS — see the fn doc.
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

    /// Parse a pattern (binding LHS or lambda params).
    ///
    /// The *pattern* grammar's `nested()` chokepoint, and its sole entry:
    /// list and map patterns recurse back through here for each element,
    /// so one guard at the top bounds the whole pattern recursion — the
    /// pattern-grammar analogue of [`Self::parse_primary`].
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

        // Empty list []
        if self.peek() == &Token::RBracket {
            self.advance();
            return Ok(Pattern::List {
                elems: vec![],
                rest: None,
            });
        }

        // Peek to determine if this is a map pattern: KEY ':' …  The key
        // alphabet matches map literals minus the dynamic `$var` key,
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
            // Rest pattern: ...name — terminal, must be the last element.
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
        // Mirror the literal side: bare and tag alphabets cannot mix.
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
            // Optional default: = atom
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

    /// pkey = IDENT | QUOTED | TAG  (map-pattern keys)
    ///
    /// Returns a [`MapKey`] carrying the parsed label.  Map-literal keys
    /// also accept `$deref`; that lives in [`Self::parse_map_key`].
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

    /// mapkey = IDENT | QUOTED | deref | TAG  (map-literal keys)
    ///
    /// Returns the parsed key form so the caller can construct either
    /// a [`MapEntry::Entry`] (for static keys) or a [`MapEntry::Deref`]
    /// (for `$name`) after the `:` and value are consumed.
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

    /// Enforce that `is_tag` matches the previously-seen alphabet (or
    /// record it on the first key).  Used by both map literals and map
    /// patterns to reject alphabet mixing such as `` [host: ..., `dev: ...] ``.
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
    /// The *value* grammar's `nested()` chokepoint: every nested value
    /// form (`{ … }`, `[ … ]`, `!{ … }`, `$[ … ]` via `parse_word`'s
    /// `Token::Bang`/`Token::Expr` arms) passes through here, so one depth
    /// check bounds the value recursion.  The sibling sub-grammars carry
    /// their own chokepoints — [`Self::parse_expr_atom`] (arithmetic) and
    /// [`Self::parse_pattern`] (patterns) — since neither routes through a
    /// primary.  The matching cap on *lexer* nesting lives in
    /// [`lexer::Lexer::scan_token_group`].
    fn parse_primary(&mut self) -> Result<Ast, ParseError> {
        self.nested(|p| match p.peek() {
            Token::LBrace => p.parse_block(),
            Token::LBracket => p.parse_collection(),
            _ => p.parse_word(),
        })
    }

    /// `atom = primary ('[' word ']')*`
    ///
    /// Postfix indexing is folded directly into a single
    /// `Ast::Index { target, keys }` node — building nested
    /// one-key-at-a-time `Index` chains here and flattening afterwards
    /// would do the same work twice.  Variable indexing
    /// (`$name[key]`) is already resolved by the lexer via adjacency
    /// and arrives as an `Ast::Index` from `parse_primary`; further
    /// postfix `[k]`s extend its keys list in place.
    fn parse_atom(&mut self) -> Result<Ast, ParseError> {
        let (node_span, node) = self.capture_span(Self::parse_primary)?;
        let mut new_keys: Vec<Spanned<Ast>> = Vec::new();
        while self.peek() == &Token::LBracket && self.next_token_is_adjacent() {
            // Span the whole `[key]` — including the brackets — so the
            // caret highlights the part the user wrote, not just the
            // inner word.
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
        // Merge into an existing `Ast::Index` if `parse_primary` produced
        // one (lexer-emitted `$name[k]` arrives this way), so the result
        // is a single flat `Index` node rather than nested.
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
                let default_fd =
                    u32::from(!matches!(mode, RedirectMode::Read | RedirectMode::HereString));
                let target = if let Some(tfd) = target_fd {
                    RedirectTarget::Fd(tfd)
                } else {
                    let (word_span, word) = self.capture_span(Self::parse_word)?;
                    if mode == RedirectMode::HereString
                        && let Ast::Word(w) = &word
                    {
                        let message: String = match w {
                            Word::Plain(_) => {
                                "ral has no heredocs: `<<` feeds a string to \
                                 stdin. Use a raw string: `cmd << #' ... '#`, \
                                 which may use newlines"
                                    .into()
                            }
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
    /// `...x` in argument position becomes [`Ast::Spread`], distinct
    /// from `[...x]` (a list literal containing a spread); the
    /// elaborator uses the distinction to splice `x`'s elements into
    /// the call's argument list rather than treat the list as one arg.
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

        // Argument-less, redirect-less heads that are really values
        // (`$x`, a literal `true`/`false`/`unit`) skip the `Ast::Call`
        // wrapper so downstream passes see the raw value.  Everything
        // else becomes a `Call`, regardless of whether the arg / redirect
        // lists happen to be empty.  `return` cannot reach here — it is a
        // dedicated stage form intercepted by `parse_stage`.
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

    /// Byte span of the most recently consumed token.  Used to compute the
    /// end of an `Ast::Call` once all args have been parsed.  Falls back to
    /// the current-position span at the start of input.
    fn prev_byte_span(&self) -> Span {
        self.tokens
            .get(self.pos.saturating_sub(1))
            .map_or_else(|| self.span(), |(_, s)| *s)
    }

    /// Check if we've reached the end of a command's argument list.
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

    /// True at boundaries where a backtick tag should remain nullary instead of
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
    /// Consumes the `!` itself so the captured span runs from `!` to
    /// the end of the forced primary.  Both callers (`parse_word`'s
    /// `Token::Bang` arm and `parse_expr_atom`'s — `$[!{…}]` inside
    /// `$[…]`) leave the `!` token unconsumed when delegating here.
    ///
    /// Postfix `[k]` indexing is intentionally left to the outer
    /// `parse_atom` so that `!{cmd}[k]` parses as `Index(Force(Block), k)`
    /// — force first, then index — rather than the incorrect
    /// `Force(Index(Block, k))`.
    fn parse_bang(&mut self) -> Result<Ast, ParseError> {
        let (span, inner) = self.capture_span(|p| {
            p.advance(); // consume `!`
            p.parse_primary()
        })?;
        Ok(Ast::Force(Spanned::boxed(span, inner)))
    }

    /// Parse a block: { program } or lambda: { |params| body }
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
                // Currying desugar: { |x y z| body } → { |x| { |y| { |z| body } } }.
                // Each intermediate lambda's body is a single synthetic
                // statement wrapping the next-nested lambda.  We reuse the
                // span of the original body's first statement (so
                // diagnostics inside the curried form still point at user
                // code); when the body is empty, fall back to the current
                // parser position.
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
        // Skip leading spread entries to find the first keyed element.  The
        // spread operand is a whole atom and may nest brackets/braces
        // (`...[a: 1]`), so the scan tracks depth: only a `,` or `]` at the
        // operand's own level ends it, not the inner `]` of a nested
        // collection.
        while matches!(self.tokens.get(i).map(|(t, _)| t), Some(Token::Spread)) {
            i += 1;
            let mut depth = 0usize;
            loop {
                match self.tokens.get(i).map(|(t, _)| t) {
                    Some(Token::LBracket | Token::LBrace) => depth += 1,
                    Some(Token::RBracket | Token::RBrace) if depth > 0 => depth -= 1,
                    Some(Token::Comma | Token::RBracket) | None if depth == 0 => break,
                    _ => {}
                }
                i += 1;
            }
            if matches!(self.tokens.get(i).map(|(t, _)| t), Some(Token::Comma)) {
                i += 1;
            }
        }

        // A map literal additionally admits a dynamic `$var` key, so the
        // lookahead permits a `Deref` head here; map *patterns* do not
        // (you cannot bind through a dynamic key), so `parse_pattern_inner`
        // passes `allow_deref = false`.
        self.key_colon_at(i, /*allow_deref=*/ true)
    }

    /// True when `tokens[i]` is a static map key — a bare word, a quoted
    /// string, or a backtick tag (plus a `$var` deref when `allow_deref`)
    /// — immediately followed by `:` at `i + 1`.  The single shape test
    /// shared by the map-literal lookahead ([`Self::is_map_ahead`]) and
    /// the map-pattern lookahead ([`Self::parse_pattern_inner`]).
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
        // Track the alphabet of static keys (literal `name` vs tag `` `name ``)
        // so that mixing them in one literal is rejected at parse time.
        // Dynamic `$var` keys do not contribute to the alphabet decision.
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

    /// Parse interpolation parts from a double-quoted string.  Each
    /// part carries its own byte range (computed by the lexer in
    /// `scan_double_quoted`); we transfer that range onto the
    /// surrounding `Spanned<Ast>`, and on `Force` segments also onto
    /// the inner block so the forced body has a real span of its own.
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

    /// Precedence-climbing loop.  The depth-growing recursions —
    /// parenthesised sub-expressions and unary prefixes — all bottom out
    /// in [`Self::parse_expr_atom`], which carries the `nested()` guard,
    /// so this loop needs none of its own: its only self-recursion is the
    /// binary right-hand side, bounded by the fixed precedence ladder.
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

    /// One operand of the arithmetic grammar — and that grammar's single
    /// `nested()` chokepoint.  Parenthesised sub-expressions recurse via
    /// [`Self::parse_expr_prec`] and the unary prefixes `-` / `not` recurse
    /// straight back here, so guarding this one entry bounds every
    /// depth-growing path inside `$[…]`.  The body lives in
    /// [`Self::parse_expr_operand`]; this wrapper exists only to apply the
    /// guard, mirroring [`Self::parse_primary`] and [`Self::parse_pattern`].
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
                // `parse_bang` always returns `Ast::Force`; lift the
                // inner `Spanned<Box<Ast>>` directly into `Expr::Force`
                // so the force-operand span survives into the expression
                // grammar.
                let Ast::Force(body) = self.parse_bang()? else {
                    unreachable!("parse_bang yields Ast::Force by construction");
                };
                Ok(Expr::Force(body))
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
                    other => {
                        Expr::BinOp(Box::new(Expr::Integer(0)), BinaryOp::Sub, Box::new(other))
                    }
                })
            }
            Token::Word(Word::Plain(s)) if s == "not" => {
                self.advance();
                // `not` is a prefix operator binding tighter than any binary
                // op (see `peek_expr_op`'s precedence table): it takes a single
                // atom, so `not $x == 0` parses as `(not $x) == 0`.
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
                // Share the numeric shape rule with bare-word literals
                // ([`WordLiteral::classify`]): an integer must `i64`-parse,
                // a float must carry a `.`.  A blanket `f64` parse here
                // would silently admit `inf`, `nan`, and `1e5` as numbers
                // inside `$[…]` while the rest of the language reads them
                // as strings.  `true` / `false` are taken by the arms above.
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
            // `<` / `>` reach the expression grammar as `Redirect` tokens
            // ([`Self::peek_expr_op`] reads them as comparison operators).
            // Hitting one in *operand* position means the comparison was
            // misplaced — most often a digit glued to the operator, since
            // `$[2>3]` lexes `2` as a file descriptor (`2>`) rather than the
            // operand of `>`.  Point at the spacing fix instead of naming the
            // bare `redirect` token.
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

    fn peek_expr_op(&self) -> Option<(InfixOp, u8)> {
        // Precedence (low → high): ||=1, &&=2, comparison=3, add/sub=4,
        // mul/div/mod=5.  Unary `-` / `not` bind tighter than any binary
        // and are handled in `parse_expr_atom`.
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
            // < and > are lexed as Redirect tokens, handle them as expr operators
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

/// Parse the pre-lexed key streams of `$name[k1][k2]…` into AST nodes.
///
/// The lexer has already lexed each `[k]` body — there is no re-lex
/// here.  Each stream contains the tokens for a single `word` (an
/// identifier, a `'quoted'` literal, a `$var` deref, etc.) and we use
/// the regular [`Parser::parse_word`] entry point so the grammar stays
/// in one place.  Inner-token spans already attribute to the outer
/// source, so any diagnostic from the sub-parser underlines the right
/// column; [`Parser::peek`] yields `Eof` past the slice end, so no
/// sentinel is needed.
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

/// Build an [`Ast::Index`] from a lexer-fused `$name[k1][k2]…` deref: the
/// `$name` head becomes the indexed target and each key stream is parsed
/// by [`parse_index_keys`].  Shared by the two sites that consume a
/// [`StringPart::Index`] — `parse_word`'s `Token::Deref` arm and the
/// double-quoted interpolation arm — so both construct it the same way.
fn deref_index_to_ast(
    name: Spanned<String>,
    keys: Vec<Spanned<Vec<(Token, Span)>>>,
) -> Result<Ast, ParseError> {
    Ok(Ast::Index {
        target: Spanned::with_span(name.span, Box::new(Ast::Variable(name.item))),
        keys: parse_index_keys(keys)?,
    })
}

/// Parse the pre-lexed body of `$[…]` as a Pratt expression.  The
/// lexer fused `&&`/`||` already (see [`lexer::Lexer::scan_expr_block`]),
/// so the parser sees the same shape it would for a bare-word `&&`
/// outside any nesting — no fusion happens here.
fn parse_expr_block(tokens: Vec<(Token, Span)>) -> Result<Expr, ParseError> {
    Parser::run_complete(tokens, |p| p.parse_expr_prec(0))
}

/// Parse the pre-lexed body of a `!{…}` interpolation as a statement
/// list.  No re-lex: the tokens already came out of the outer lexer.
fn parse_force_body(tokens: Vec<(Token, Span)>) -> Result<Vec<Stmt>, ParseError> {
    Parser::run_complete(tokens, Parser::parse_program)
}

/// Keywords and value literals that may not be used as binding names: a
/// keyword ([`crate::syntax::is_keyword`] — control flow plus the
/// control-operator names) or a value literal (`true`, `false`, `unit`).
/// The `^name` head form bypasses this predicate — `^try` parses as
/// `Head::ExternalName("try")` and dispatches to PATH lookup.
fn is_reserved(s: &str) -> bool {
    crate::syntax::is_keyword(s) || matches!(s, "true" | "false" | "unit")
}

/// Parsed map-literal key — either a static label (bare or tag, via
/// [`MapKey`]) or a runtime deref (`$name`).  The map-literal parser
/// produces this from one token and then constructs the matching
/// [`MapEntry`] variant after parsing the `:` and value.
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

    /// Wrap an `Ast` in a synthetic `Spanned` — used by test fixtures
    /// to construct list / map / chain / interpolation elements which
    /// the parser produces as `Spanned<Ast>` but tests don't predict
    /// concrete positions for.
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

    /// Construct an `Ast::Call` with no redirects — the parser tests
    /// do not inspect spans, and `strip_one` already drops them, so
    /// this keeps the expected-AST literals readable.
    fn app(head: Head, args: Vec<Ast>) -> Ast {
        Ast::Call {
            head,
            args: args.into_iter().map(Spanned::synthetic).collect(),
            redirects: vec![],
        }
    }

    /// Construct an `Ast::Call` with explicit redirects, for tests
    /// that need to verify redirect attachment.
    fn app_redir(head: Head, args: Vec<Ast>, redirects: Vec<Redirect>) -> Ast {
        Ast::Call {
            head,
            args: args.into_iter().map(Spanned::synthetic).collect(),
            redirects,
        }
    }

    /// Wrap a list of bare `Ast`s into the `Vec<Stmt>` shape that block /
    /// lambda / pipeline bodies demand.  Spans are normalised to `None` —
    /// `strip_stmts` matches that on the parsed side, so expected-AST
    /// equality holds.
    fn body(asts: Vec<Ast>) -> Vec<Stmt> {
        asts.into_iter().map(Spanned::synthetic).collect()
    }

    /// Unwrap `Stmt` wrappers down to `Ast` and normalise lone-atom
    /// `Call`s, for top-level test assertions.  The `Stmt` span field
    /// is not inspected by these tests, so dropping it yields the
    /// `Vec<Ast>` shape the fixtures compare against.
    fn unwrap_stmts(stmts: Vec<Stmt>) -> Vec<Ast> {
        stmts.into_iter().map(|s| strip_one(s.item)).collect()
    }

    /// Like [`unwrap_stmts`] but for nested statement sequences (block /
    /// lambda bodies, pipeline stages) — keeps the `Stmt` wrapper so the
    /// expected AST shape stays well-typed, but normalises spans to
    /// `None` so equality holds against synthetic Stmts built by
    /// [`body`].
    fn strip_stmts(stmts: Vec<Stmt>) -> Vec<Stmt> {
        stmts
            .into_iter()
            .map(|s| Spanned::synthetic(strip_one(s.item)))
            .collect()
    }

    fn strip_args(args: Vec<Ast>) -> Vec<Ast> {
        args.into_iter().map(strip_one).collect()
    }

    /// Normalise a `Vec<Spanned<Ast>>` (used by `Ast::Call.args` and
    /// `Ast::Index.keys` after the `Spanned<T>` refactor) — drops the
    /// span info and recurses through `strip_one` on each item.  Tests
    /// reconstruct equivalent fixtures with `None` spans, so the
    /// parse-vs-fixture comparison still holds without callers having
    /// to predict positions.
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

    fn strip_one(n: Ast) -> Ast {
        match n {
            // Unwrap Call { head, args: [], redirects: [] } → head atom
            // (lone bare/path/tilde/value in command position).
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
        // `let x = a ? b` binds the *chain* to x, not just `a`.
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
        // The trailing `&` on a let RHS must wrap the whole chain, never
        // greedily attach to only the last arm.
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
        // `let x = a & ? b` is not legal: per-arm `&` is reserved for
        // statement-level chains.  The parser stops after `a &`, then sees
        // a stray `?`.
        assert!(parse("let x = a & ? b").is_err());
    }

    #[test]
    fn parse_chain_arms_may_background_at_stmt_level() {
        // At the statement level, every chain arm may carry its own `&`.
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

    // ── A6: sub-token-stream parsers require EOF ─────────────────────

    /// F4: an expression block `$[…]` with trailing tokens after the
    /// first expression must reject, not silently drop `2 3`.
    #[test]
    fn expr_block_rejects_trailing_input() {
        let err = parse("echo $[1 2 3]").unwrap_err();
        assert!(
            err.message.contains("trailing input"),
            "expected trailing-input error, got: {}",
            err.message
        );
    }

    /// A numeric literal glued to a comparison operator inside `$[…]`
    /// (`2>3`) lexes the digit as a file descriptor, so the `>` lands in
    /// operand position.  The error must name the `2>` shape and point at
    /// the spacing fix rather than reporting a bare "redirect" token.
    #[test]
    fn glued_comparison_suggests_spacing() {
        let err = parse("return $[2>3]").unwrap_err();
        assert!(
            err.message.contains("`2>`") && err.message.contains("with spaces"),
            "expected a spacing hint naming `2>`, got: {}",
            err.message
        );

        // A bare operator with no left operand reports the comparison, not a
        // file descriptor (there is no glued digit to blame).
        let err = parse("return $[< 3]").unwrap_err();
        assert!(
            err.message.contains("comparison operator"),
            "expected a comparison-operator error, got: {}",
            err.message
        );
    }

    /// F4: an index key stream `$m[k]` with trailing tokens after the
    /// first word must reject, not index by `a` alone.
    #[test]
    fn index_keys_reject_trailing_input() {
        let err = parse("$m[a b]").unwrap_err();
        assert!(
            err.message.contains("trailing input"),
            "expected trailing-input error, got: {}",
            err.message
        );
    }

    /// F5: a stray top-level `}` must be a parse error rather than a
    /// silent stop condition that truncates the program, and must be
    /// named as an unmatched brace rather than folded into the generic
    /// trailing-input message (neither "trailing" nor "after the parse
    /// completed" is true when statements follow the brace).
    #[test]
    fn top_level_stray_rbrace_rejected() {
        let err = parse("echo a\n}\necho b").unwrap_err();
        assert!(
            err.message.contains("unmatched `}`"),
            "expected an unmatched-brace error, got: {}",
            err.message
        );
    }

    /// The same shape after a well-formed block, with more statements
    /// following the stray brace — the "trailing input" wording would be
    /// doubly wrong here since the brace isn't trailing and the parse
    /// hasn't completed.
    #[test]
    fn mid_program_stray_rbrace_names_unmatched_brace() {
        let err = parse("{ let x = 1 } } let y = 2").unwrap_err();
        assert!(
            err.message.contains("unmatched `}`"),
            "expected an unmatched-brace error, got: {}",
            err.message
        );
    }

    /// A well-formed block body still parses — `run_complete` only fires
    /// on tokens the body genuinely left unconsumed.
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

    /// F11: an expression atom shares the numeric shape rule with
    /// bare-word literals — a float needs a `.`, so `inf`/`nan`/`1e5`
    /// are not numbers inside `$[…]` (they would be `String`s elsewhere).
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

    /// A dotted float still parses inside `$[…]`.
    #[test]
    fn expr_atom_accepts_dotted_float() {
        let ast = unwrap_stmts(parse("$[1.5]").unwrap());
        assert_eq!(ast, vec![Ast::Expr(Box::new(Expr::Number(1.5)))]);
    }

    /// `not` binds tighter than any binary operator, so `not $x == 0`
    /// parses as `(not $x) == 0`, not `not ($x == 0)`.
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
        // `[...$d, k: v]` starts with a spread, then has a `key: val` pair —
        // the lookahead must look past the spread to see that this is a map.
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

    /// F14: the lookahead past a leading spread must track nesting — the
    /// spread operand `[a: 1]` is a nested collection whose inner `]`
    /// must not be mistaken for the outer list's close.  `[...[a: 1], b: 2]`
    /// is a map.
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

    #[test]
    fn parse_leading_spread_disambiguates_to_list() {
        // `[...$xs, a]` starts with a spread, then a bare element — list.
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
        // First test: multiline map standalone parses as a value form.
        let src1 = "[\n    quit: { echo q },\n    help: { echo h },\n]";
        let ast1 = unwrap_stmts(parse(src1).unwrap());
        assert_eq!(ast1.len(), 1);
        assert!(matches!(&ast1[0], Ast::Map(_)));

        // Second test: multiline map as command argument
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

    /// `<< $var` feeds a stored string; the payload word admits the same
    /// value forms as any other redirect operand.
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

    /// The bash-heredoc reflex `<<EOF` (and `<< EOF`) is a targeted parse
    /// error naming the raw-string form, not a silent feed of the literal
    /// word `EOF`.
    #[test]
    fn herestring_bare_word_is_rejected() {
        for src in ["cat <<EOF", "cat << EOF"] {
            let err = parse(src).expect_err("bare word after `<<` must not parse");
            assert!(
                err.message.contains("ral has no heredocs")
                    && err.message.contains("#' ... '#"),
                "for {src:?} got: {}",
                err.message
            );
        }
    }

    /// A path after `<<` gets the `< path` correction instead of the
    /// heredoc message.
    #[test]
    fn herestring_path_word_is_rejected() {
        let err = parse("cat << ./body.txt").expect_err("path after `<<` must not parse");
        assert!(
            err.message.contains("use `< path`"),
            "got: {}",
            err.message
        );
    }

    /// `<<` always feeds stdin: fd 0 may be spelled explicitly, anything
    /// else is an error.
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
        // ~ as an argument to a command should parse as Tilde, not be wrapped in Command
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
        // "~ foo" — space between ~ and word means ~ is standalone, foo is a separate arg
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
    // The three mutually-recursive sub-grammars each descend through one
    // `nested()` guard (`parse_primary` / `parse_expr_atom` /
    // `parse_pattern`).  These exercise the two sub-grammars whose
    // recursion does *not* route through `parse_primary`, so adversarial
    // depth rejects cleanly rather than overflowing the call stack.  The
    // depth used sits well above the cap (64) but far below any real stack
    // ceiling, so a regression surfaces as a missing error, not a crash.

    /// Nested destructuring patterns recurse through `parse_pattern`; deep
    /// nesting must hit the cap, not overflow the stack.
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

    /// A long run of unary `-` recurses through `parse_expr_atom`; deep
    /// nesting must hit the cap, not overflow the stack.
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

    /// A long run of `not` recurses through `parse_expr_atom`; deep nesting
    /// must hit the cap, not overflow the stack.
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
        // `let x =\n expr` — newline between = and RHS is allowed.
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
        // Multiple blank lines between = and RHS are also fine.
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
        // Destructuring pattern with newline before RHS.
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
        // cmd1 |\ncmd2 — pipe at end of line continues.
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
        // cmd1\n| cmd2 — pipe at start of next line continues.
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
        // echo hello\nworld — two separate statements, not one command.
        let ast = unwrap_stmts(parse("echo hello\nworld").unwrap());
        assert_eq!(ast.len(), 2);
    }

    #[test]
    fn caret_is_not_a_continuation_token() {
        assert!(!needs_continuation("^"));
    }

    #[test]
    fn needs_continuation_on_unterminated_string() {
        // Plain unterminated double quote: the REPL must offer another line.
        assert!(needs_continuation("\"foo"));
    }

    #[test]
    fn needs_continuation_on_unterminated_string_with_inner_force() {
        // Outer string + inner unclosed `!{...}` is still "user is
        // mid-typing"; both unclosed pieces mean continuation.
        assert!(needs_continuation("\"foo !{cmd"));
    }

    #[test]
    fn complete_program_does_not_need_continuation() {
        assert!(!needs_continuation("echo done"));
    }

    /// F14: an unbalanced top-level `{` / `[` is "user is mid-typing" —
    /// the lexer now reports it as an unterminated delimiter, so the REPL
    /// offers another line instead of erroring on the spot.
    #[test]
    fn needs_continuation_on_unbalanced_open_delimiters() {
        assert!(needs_continuation("let f = {"));
        assert!(needs_continuation("return [a, b"));
        assert!(needs_continuation("if true {"));
    }

    /// A balanced program still terminates — no spurious continuation.
    #[test]
    fn balanced_delimiters_do_not_need_continuation() {
        assert!(!needs_continuation("let f = { return 1 }"));
        assert!(!needs_continuation("return [a, b]"));
    }

    /// Bug (a): a comment that runs to EOF must not mask an open
    /// delimiter.  An open `{` / `[` followed only by a trailing comment
    /// is still "user is mid-typing".
    #[test]
    fn needs_continuation_on_open_delim_then_comment_to_eof() {
        assert!(needs_continuation("let f = {# comment"));
        assert!(needs_continuation("return [a, b # comment"));
        assert!(needs_continuation("{# comment"));
        assert!(needs_continuation("[# comment"));
    }

    /// Bug (a), companion: a *balanced* program with a trailing comment
    /// still terminates — the comment alone must not trigger a spurious
    /// continuation.
    #[test]
    fn balanced_program_with_trailing_comment_does_not_need_continuation() {
        assert!(!needs_continuation("let f = { return 1 } # done"));
        assert!(!needs_continuation("echo done # done"));
    }

    /// Bug (b): a `let` whose RHS is missing is incomplete — the REPL
    /// must offer another line for the right-hand side.
    #[test]
    fn needs_continuation_on_let_awaiting_rhs() {
        assert!(needs_continuation("let x ="));
        assert!(needs_continuation("let [a, b] ="));
    }

    /// Bug (b): a trailing bare `=` that is *not* a `let` binder marks
    /// the end of a complete command (`=` is a plain-word argument), so
    /// it must NOT be flagged as needing continuation — otherwise the
    /// REPL hangs on a line that already parses.
    #[test]
    fn trailing_bare_equals_does_not_need_continuation() {
        assert!(parse("x =").is_ok());
        assert!(!needs_continuation("x ="));
        assert!(parse("echo a =").is_ok());
        assert!(!needs_continuation("echo a ="));
    }

    /// A dangling continuation operator or control keyword leaves the
    /// parser midway through a production it has committed to — the REPL
    /// reads the stage, branch, or body on the next line.
    #[test]
    fn needs_continuation_on_dangling_continuation_token() {
        assert!(needs_continuation("echo hi |"));
        assert!(needs_continuation("echo a ?"));
        assert!(needs_continuation("if"));
        assert!(needs_continuation("if true x\nelsif"));
        assert!(needs_continuation("if true x\nelse"));
        // Condition parsed, body demanded but input ran out.
        assert!(needs_continuation("if $c"));
        assert!(needs_continuation("if true a\nelsif $c"));
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

    // ── Control operators (try / guard / within / grant / audit) ────────

    /// Helper: assert the parsed program is a single `Ast::Scope` and
    /// destructure it.
    fn unwrap_single_scope(ast: Vec<Stmt>) -> (ScopeAst, Vec<Redirect>) {
        let stripped: Vec<_> = ast.into_iter().map(|s| s.item).collect();
        match stripped.as_slice() {
            [Ast::Scope { op, redirects, .. }] => (op.clone(), redirects.clone()),
            _ => panic!("expected a single Ast::Scope, got {stripped:?}"),
        }
    }

    /// Helper: assert the parsed program is a single `Ast::Call` and
    /// destructure it.
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
        // Prelude shape: `try $body { |err| ... }`.
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
