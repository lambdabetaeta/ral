//! Elaboration: surface AST into the call-by-push-value IR of [`crate::ir`].
//!
//! CBPV splits inert `Val` from effectful `Comp`.  Where the IR wants a `Val`
//! but the source has an effectful sub-expression, the elaborator binds it to
//! a fresh `%varN` and substitutes a `Val::Variable`; those pending bindings ride
//! a mutable *binds* accumulator threaded through `elab_expr`, which
//! `wrap_binds` folds into a `Comp::Bind` chain at a statement boundary.
//! A context that may not run — an `if` arm, a `?` chain arm, a pipeline stage
//! — must therefore hand its subtree a fresh accumulator, or the untaken arm's
//! effects escape the guard.
//!
//! The other job is command dispatch.  A bare head that is lexically bound
//! becomes `Force(Variable)` applied to its arguments; an unbound one becomes
//! `Exec` against the command namespace.  Every other head shape (`^name`,
//! `./x`, `~/x`, `$f`, `{ … }`) declares which it is syntactically.

use crate::ir::{
    Args, Assembly, CaseArm, CommandName, CommandWord, Comp, CompKind, Exec, GroupNode,
    HandlerArmV, Name, OptionsV, Pattern, Phrase, Redirects, Unchecked, Val, ValListElem,
    ValMapEntry, ValRecordEntry, synthetic,
};
use crate::prelude_manifest;
use crate::source::Span;
use crate::source::Spanned;
use crate::source::WithSpan;
use crate::syntax::ast::{
    self, Ast, Head, IfBranch, ListElem, MapEntry, Options, RecordEntry, ScopeAst, Stmt, Word,
    WordLiteral,
};
use crate::syntax::group::{self, Def};
use crate::syntax::parser::ParseError;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, LazyLock};

/// State threaded through the elaboration pass.
struct Elaborator {
    /// Fresh-name counter for `gensym`.
    counter: usize,
    /// Prelude exports, in scope beneath every lexical frame.  Shared and never
    /// mutated, so an elaboration bumps a refcount rather than cloning the set.
    prelude: Arc<HashSet<String>>,
    /// Bound names, innermost last; the base frame holds the caller's bindings,
    /// the one above it the top-level statements' own.
    lexical_scopes: Vec<HashSet<Name>>,
    /// The stretches being elaborated, innermost last.
    stretches: Vec<StretchGraph>,
    /// Attached to every emitted `Comp`; narrowed and restored by `with_span`,
    /// which every traversal that knows a tighter byte range wraps its body in.
    current_span: Option<Span>,
    /// The script file's path as shown, `None` where there is no
    /// self-location to bake (the REPL, `-c`, virtual sources).  What
    /// `$SCRIPT` resolves to.
    script: Option<String>,
    /// Elaboration's one failure path, checked by `elaborate` once the walk is
    /// done — a single slot beats threading `Result` through every traversal.
    error: Option<ParseError>,
}

/// A stretch under elaboration (SPEC §5.7): the references that resolve to
/// its members are the edges of its dependency graph.
struct StretchGraph {
    /// The frame of `lexical_scopes` the stretch binds into.
    frame: usize,
    /// Each member's index, by name.
    members: HashMap<Name, usize>,
    /// The member being elaborated.
    current: usize,
    /// `edges[i]`: the members member `i` refers to.
    edges: Vec<Vec<usize>>,
    /// The names that block definitions further down the list, past the
    /// stretch, define.
    later: HashSet<Name>,
}

/// Wrap a `CompKind` using the elaborator's current span.
macro_rules! comp {
    ($self:expr, $kind:expr) => {
        Spanned::with_span($self.current_span, $kind)
    };
}

impl WithSpan for Elaborator {
    fn span_slot(&mut self) -> &mut Option<Span> {
        &mut self.current_span
    }
}

impl Elaborator {
    /// `bindings` are names already live in the caller (REPL definitions, say);
    /// `script` is the file the source was read from, if any.
    fn new_with_bindings(bindings: impl IntoIterator<Item = Name>, script: Option<&Path>) -> Self {
        Self {
            counter: 0,
            prelude: prelude_scope(),
            lexical_scopes: vec![bindings.into_iter().collect(), HashSet::new()],
            stretches: Vec::new(),
            current_span: None,
            script: script.map(crate::path::shown),
            error: None,
        }
    }

    /// Elaborate a `$name` reference.  `SCRIPT` bakes to a string literal
    /// rather than a runtime lookup: self-location is lexical (bash's
    /// `BASH_SOURCE`, not `$0`'s caller-site), and a compile-time literal is
    /// lexicality by construction.
    fn variable_val(&mut self, name: &str) -> Val {
        if name != "SCRIPT" {
            self.refer(name);
            return Val::Variable(name.into());
        }
        if let Some(s) = &self.script {
            return Val::String(s.clone().into());
        }
        self.refuse(
            self.current_span,
            "$SCRIPT: no script name here (the REPL, `-c`, and preloaded sources have none)",
        );
        Val::Unit
    }

    /// A name for a hoisted temporary.
    fn gensym(&mut self) -> Name {
        self.counter += 1;
        synthetic("var", self.counter)
    }

    /// Elaboration's one failure: the first refusal stands.
    fn refuse(&mut self, span: Option<Span>, message: &str) {
        self.error
            .get_or_insert_with(|| ParseError::new(span, message));
    }

    /// The one door into scope, hence the one place `SCRIPT` is refused as
    /// a binder; `span` is the binder's own.
    fn bind(&mut self, span: Option<Span>, names: impl IntoIterator<Item = Name>) {
        for name in names {
            if &*name == "SCRIPT" {
                self.refuse(
                    span,
                    "$SCRIPT names the file being compiled, not a name you can bind; choose \
                     another name",
                );
            }
            self.lexical_scopes
                .last_mut()
                .expect("lexical_scopes is initialised non-empty and never popped past 1")
                .insert(name);
        }
    }

    fn bind_pattern(&mut self, pat: &Spanned<Pattern>) {
        self.bind(pat.span, pat.item.names().into_iter().cloned());
    }

    /// A `{ |param| body }` binder together with the statements it scopes:
    /// the body elaborates inside the frame the param's names open.  Both
    /// readings of that spelling — the lambda it denotes and the `case` arm
    /// that is a branch rather than a function — get their scope from here,
    /// so it is stated once.
    fn elab_binder_scope(&mut self, param: &Spanned<Pattern>, body: &[Stmt]) -> (Pattern, Comp) {
        let body = self.with_new_scope(|this| {
            this.bind_pattern(param);
            this.stmts_nested(body)
        });
        (param.item.clone(), body)
    }

    /// A fresh frame, so the body's `let`s shadow rather than leak outward.
    fn with_new_scope<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved_span = self.current_span;
        self.lexical_scopes.push(HashSet::new());
        let out = f(self);
        self.lexical_scopes.pop();
        self.current_span = saved_span;
        out
    }

    fn is_bound(&self, name: &str) -> bool {
        self.lexical_scopes
            .iter()
            .rev()
            .any(|scope| scope.contains(name))
            || self.prelude.contains(name)
    }

    /// Note a reference to `name`: refused if a block further down the list
    /// defines it past the stretch and the body does not bind it above; an
    /// edge if it resolves to a member of the stretch under elaboration.  Each
    /// list has a frame of its own, so a frame below a stretch's is outside it.
    fn refer(&mut self, name: &str) {
        let frame = self.lexical_scopes.iter().rposition(|s| s.contains(name));
        if self
            .stretches
            .iter()
            .any(|s| s.later.contains(name) && frame.is_none_or(|f| f < s.frame))
        {
            self.refuse(
                self.current_span,
                &format!(
                    "a block further down defines `{name}`, but a statement between them \
                     hides it from this block; move that definition up, so that only block \
                     definitions stand between them, or rename it"
                ),
            );
        }
        let Some(frame) = frame else {
            return;
        };
        if let Some(stretch) = self.stretches.iter_mut().rev().find(|s| s.frame == frame)
            && let Some(&member) = stretch.members.get(name)
        {
            stretch.edges[stretch.current].push(member);
        }
    }

    /// Yield the `Val` the parent consumes: `Return(v)` passes through, and
    /// anything else is bound to a fresh `%varN` pushed onto `binds`.
    fn hoist(&mut self, comp: Comp, binds: &mut Vec<(Pattern, Comp)>) -> Val {
        if let CompKind::Return(v) = comp.item {
            v
        } else {
            let name = self.gensym();
            binds.push((Pattern::Name(name.clone()), comp));
            Val::Variable(name)
        }
    }

    #[allow(clippy::wrong_self_convention)]
    fn to_val(&mut self, ast: &Ast, binds: &mut Vec<(Pattern, Comp)>) -> Val {
        let comp = self.elab_expr(ast, binds);
        self.hoist(comp, binds)
    }
}

/// Fold `binds` into a chain of `Comp::Bind` nodes around `inner`, the first
/// binding outermost so the chain runs in the order the hoists were pushed.
fn wrap_binds(span: Option<Span>, binds: Vec<(Pattern, Comp)>, inner: Comp) -> Comp {
    binds
        .into_iter()
        .rev()
        .fold(inner, |rest, (pattern, comp)| {
            Spanned::with_span(span, CompKind::bind(pattern, comp, rest))
        })
}

/// The prelude's exported names, built once and shared by refcount thereafter.
fn prelude_scope() -> Arc<HashSet<String>> {
    static PRELUDE: LazyLock<Arc<HashSet<String>>> = LazyLock::new(|| {
        Arc::new(
            prelude_manifest::PRELUDE_EXPORTS
                .iter()
                .map(ToString::to_string)
                .collect(),
        )
    });
    Arc::clone(&PRELUDE)
}

/// The value a bare word denotes, by the shape rules of
/// [`WordLiteral::classify`].
///
/// Eager and type-blind: a numeric-looking word meant as argv data is read as
/// a number, and stringifies back unchanged only where its source was already
/// canonical (`007` ⇒ `7`, `1.50` ⇒ `1.5`).
pub(crate) fn word_val(s: &str) -> Val {
    match WordLiteral::classify(s) {
        Some(WordLiteral::Bool(b)) => Val::Bool(b),
        Some(WordLiteral::Int(n)) => Val::Int(n),
        Some(WordLiteral::Float(f)) => Val::Float(f),
        None => Val::String(s.into()),
    }
}

/// Elaborate a top-level statement sequence into unchecked phrases.
///
/// Each `let` becomes a `Define`, the members of a recursive group sharing
/// one `Rec` group, and everything else a `Run`.
///
/// `bindings` are the names already live in the calling environment (a REPL's
/// accumulated definitions, say); the prelude is always in scope.  `script` is
/// the file the source was read from, `None` for a virtual source; shown, it
/// is the value `$SCRIPT` resolves to.
///
/// # Errors
/// `$SCRIPT` referenced where there is no `script`, or bound by a pattern.
pub fn elaborate(
    ast: &[Stmt],
    bindings: impl IntoIterator<Item = Name>,
    script: Option<&Path>,
) -> Result<Unchecked, ParseError> {
    let mut elaborator = Elaborator::new_with_bindings(bindings, script);
    let mut phrases = Vec::new();
    let parts = group::parts(ast);
    for (k, part) in parts.iter().enumerate() {
        match part {
            group::Part::Stmt(stmt) => phrases.push(elaborator.toplevel_phrase(stmt)),
            group::Part::Stretch(defs) => {
                let bindings = elaborator.elab_stretch(defs, &parts[k + 1..]);
                phrases.extend(bindings.into_iter().map(|(span, name, rhs)| {
                    let define = Phrase::Define {
                        pattern: Arc::new(Pattern::Name(name)),
                        comp: Arc::new(rhs),
                        schemes: (),
                    };
                    Spanned::with_span(span, define)
                }));
            }
        }
    }
    if let Some(e) = elaborator.error {
        return Err(e);
    }
    Ok(phrases)
}

mod branches;
mod calls;
mod expr;
mod statements;

#[cfg(test)]
mod tests;
