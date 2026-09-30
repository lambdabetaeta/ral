//! Capture, decided by syntax: which commands a `let` binds the output of.
//!
//! A command is a computation of type `F Unit`: it writes, and returns nothing.
//! `let x = hostname` binds the text `hostname` writes because the checker
//! wraps the command in the capture coercion the machine already has, and it
//! decides that once, before any type is inferred, from two facts alone: the
//! syntax of the right-hand side, and the class of each head it meets.  No
//! type variable and no run-time frame is consulted.

use super::env::HandlerBinding;
use super::infer::Inferencer;
use super::scheme::Scheme;
use crate::ir::{CommandName, CommandWord, Comp, CompKind, Val};
use crate::types::{BuiltinEntry, Convention, Output};
use std::sync::Arc;

/// What a bare command head resolves to, by the lookup order the checker and
/// the runtime share: binding, value builtin, handler, external.
pub(super) enum HeadClass {
    /// A lexical or session binding, called at its scheme.
    Binding(Arc<Scheme>),
    /// A value row of the builtin table, applied at its scheme.
    Value(BuiltinEntry),
    /// A handler in scope — a user arm, else a base frame such as `echo` —
    /// standing in for the head it names; `output` is that head's.
    Arm {
        handler: HandlerBinding,
        output: Output,
    },
    /// Anything else, and always `^name`, `./p` and `~/p`.
    External,
}

impl Inferencer<'_> {
    pub(super) fn head_class(&self, name: &str) -> HeadClass {
        if let Some(scheme) = self.env.lookup_binding(name) {
            return HeadClass::Binding(Arc::clone(scheme));
        }
        if let Some(entry) = self.env.builtins.value(name) {
            return HeadClass::Value(entry);
        }
        if let Some(handler) = self.env.lookup_handler(name) {
            let output = self
                .env
                .builtins
                .get(name)
                .filter(|frame| frame.convention == Convention::Argv)
                .map_or(Output::Writes, |frame| frame.output);
            return HeadClass::Arm {
                handler: handler.clone(),
                output,
            };
        }
        HeadClass::External
    }

    /// Whether the call `name args` writes: its head is no binding, and is
    /// either not a value row or a `Writes` row applied at its arity.
    pub(super) fn head_writes(&self, name: &str, args: &crate::ir::Args) -> bool {
        match self.head_class(name) {
            HeadClass::Binding(_) => false,
            HeadClass::Value(entry) => {
                entry.output == Output::Writes
                    && crate::ir::args::positional(args)
                        .is_some_and(|given| given.len() == entry.fixed_arity())
            }
            HeadClass::Arm { output, .. } => output == Output::Writes,
            HeadClass::External => true,
        }
    }

    /// The name of the command `exec` runs, when it writes.
    fn writer_name(&self, exec: &crate::ir::Exec) -> Option<String> {
        match &exec.head {
            CommandWord::Name(CommandName::Bare(name)) => {
                self.head_writes(name, &exec.args).then(|| name.to_string())
            }
            CommandWord::External(name)
            | CommandWord::Name(name @ (CommandName::Path(_) | CommandName::TildePath(_))) => {
                Some(name.written().into_owned())
            }
        }
    }

    /// `⟦·⟧`: the commands a `let` with right-hand side `rhs` captures, found
    /// by following the positions its *result* comes from — a sequence's tail,
    /// a pipeline's final stage, a forced literal block, the arms of a form —
    /// and stopping at every other node.
    pub(super) fn capture_sites(&mut self, rhs: &Comp) {
        match &rhs.item {
            CompKind::Exec(exec) => {
                if self.writer_name(exec).is_some() {
                    self.ctx
                        .captured
                        .insert(std::ptr::from_ref::<Comp>(rhs) as usize);
                }
            }
            CompKind::Pipeline { stages, .. } => {
                if let Some(last) = stages.last() {
                    self.capture_sites(last);
                }
            }
            CompKind::Force(Val::Thunk(node)) => self.capture_sites(node.shape()),
            CompKind::Bind { rest, .. } => self.capture_sites(rest),
            CompKind::If { then, else_, .. } => {
                self.capture_arm(&then.item);
                self.capture_arm(&else_.item);
            }
            CompKind::Case { arms, .. } => {
                for arm in arms {
                    self.capture_arm(&arm.body.item);
                }
            }
            CompKind::Try { body, handler } => {
                self.capture_arm(body);
                self.capture_arm(handler);
            }
            CompKind::Within { body, .. }
            | CompKind::Grant { body, .. }
            | CompKind::Guard { body, .. } => self.capture_arm(body),
            CompKind::Capture(_)
            | CompKind::Redirect { .. }
            | CompKind::Force(_)
            | CompKind::Lam { .. }
            | CompKind::Return(_)
            | CompKind::Assemble(_)
            | CompKind::App { .. }
            | CompKind::Binary(..)
            | CompKind::Negate(_)
            | CompKind::Not(_)
            | CompKind::Index { .. }
            | CompKind::Interpolation(_)
            | CompKind::Rec { .. }
            | CompKind::Observe(_)
            | CompKind::Audit { .. }
            | CompKind::Decode(_) => {}
        }
    }

    /// `⟦{ M }⟧ᵥ` and `⟦{ |p| M }⟧ᵥ`: a literal thunk is syntax at the site, and
    /// a thunk in hand is not.
    fn capture_arm(&mut self, arm: &Val) {
        let Val::Thunk(node) = arm else {
            return;
        };
        match &node.shape().item {
            CompKind::Lam { body, .. } => self.capture_sites(body),
            _ => self.capture_sites(node.shape()),
        }
    }

    /// The command a computation's tail runs, when it writes — the arm whose
    /// value a join of `()` against another type most likely meant to capture.
    pub(super) fn tail_writer(&self, comp: &Comp) -> Option<String> {
        match &comp.item {
            CompKind::Exec(exec) => self.writer_name(exec),
            CompKind::Pipeline { stages, .. } => stages.last().and_then(|s| self.tail_writer(s)),
            CompKind::Bind { rest, .. } => self.tail_writer(rest),
            CompKind::Lam { body, .. } => self.tail_writer(body),
            _ => None,
        }
    }

    /// [`Self::tail_writer`] of a literal arm.
    pub(super) fn arm_writer(&self, arm: &Val) -> Option<String> {
        match arm {
            Val::Thunk(node) => self.tail_writer(node.shape()),
            _ => None,
        }
    }
}
