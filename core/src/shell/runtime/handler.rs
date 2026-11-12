//! The user handler stack, ordered innermost-last. Scoped frames come off by
//! handle; host-installed handler frames also come off by name.

use super::value::ValClos;
use super::{Break, Env, Shell, ill_typed};
use crate::builtin::BuiltinEntry;
use crate::fact::{BlockRole, Expected, InvalidCause, InvalidFailure, Reason, ShapeFailure};
use crate::ir::Comp;
use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use super::flow::Settled;

/// Frame identity, minted from a monotonic counter on [`HandlerStack`].
///
/// Removal finds the frame by handle rather than index, so a handler dropped
/// between a push and its paired pop cannot shift the wrong frame out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHandle(pub(crate) u64);

/// Calling convention of a handler — fixed by its surface form at install,
/// never inferred from the thunk at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum HandlerArity {
    /// `within [handler: …]` — the thunk receives `(name, args)`.
    CatchAll,
    /// A named handler — the thunk receives `(args)`.
    Unary,
}

/// One user handler — the unit of installation in a [`HandlerFrame`].
/// Builtins are `BuiltinEntry` instead.
#[derive(Clone)]
pub struct HandlerEntry {
    pub name: Cow<'static, str>,
    pub(crate) arity: HandlerArity,
    pub(crate) thunk: ValClos,
}

impl HandlerEntry {
    /// Build a per-name entry, unary by construction. The caller has checked the thunk.
    pub(crate) fn ral_per_name(name: String, thunk: ValClos) -> Self {
        Self {
            name: Cow::Owned(name),
            arity: HandlerArity::Unary,
            thunk,
        }
    }

    /// Vet `thunk` as the persistent handler for `name` (rc, manifest) and build
    /// its entry. A name that a binding or a native holds installs, and no bare
    /// head reaches it; `^name` bypasses every handler.
    ///
    /// # Errors
    /// `thunk` not a block, or not an arm by its type: one parameter, the
    /// argv, producing what its head produces (`crate::typecheck::arm_stands_in`).
    pub(crate) fn vet(name: String, thunk: ValClos, shell: &Shell) -> Settled<Self> {
        let (comp, env) = arm_closure(&thunk, &name)?;
        crate::typecheck::arm_stands_in(&name, comp, shell.closure_schemes(env))
            .map_err(|errors| refused_arm(&name, &errors, comp.span, shell))?;
        Ok(Self::ral_per_name(name, thunk))
    }
}

/// A refused arm, in the checker's own sentences, caret on the arm when it
/// has a span.
fn refused_arm(
    subject: &str,
    errors: &[crate::typecheck::TypeError],
    span: Option<crate::source::Span>,
    shell: &Shell,
) -> Break {
    Break::from(ill_typed(
        "handler",
        BlockRole::Handler(subject.into()),
        errors,
    ))
    .locate("handler", span, shell)
}

/// An arm's thunk, excluding builtin η residuals before checking its type.
///
/// # Errors
/// `value` is not a non-builtin block.
fn arm_closure<'a>(value: &'a ValClos, subject: &str) -> Settled<(&'a Arc<Comp>, &'a Env)> {
    value.block().ok_or_else(|| {
        super::Error::new(Reason::Invalid(Box::new(
            InvalidFailure::new(
                "handler".into(),
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Closure,
                    found: value.found(),
                }),
            )
            .field(subject.into()),
        )))
        .into()
    })
}

impl fmt::Debug for HandlerEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HandlerEntry")
            .field("name", &self.name)
            .field("arity", &self.arity)
            .field("thunk", &self.thunk)
            .finish_non_exhaustive()
    }
}

/// What installed a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum FrameKind {
    /// `within [handlers: …, handler: …]`: popped by handle when the scope ends.
    Within,
    /// Host-installed: retained between units.
    Handler,
}

/// One frame of the handler stack, shared by scoped and persistent handlers.
#[derive(Debug, Clone)]
pub(crate) struct HandlerFrame {
    pub(crate) entries: Vec<HandlerEntry>,
    /// `within [handler: thunk]`; `None` on persistent handler frames.
    pub(crate) catch_all: Option<ValClos>,
    pub(crate) kind: FrameKind,
}

impl HandlerFrame {
    /// Whether this is the persistent handler frame for `name`.
    pub(crate) fn is_handler_for(&self, name: &str) -> bool {
        self.kind == FrameKind::Handler && self.entries.first().is_some_and(|e| e.name == name)
    }
}

/// One [`HandlerStack::lookup`] hit: a user frame, masked by depth and run
/// through the ordinary handler calling convention, or a base frame, called
/// directly with no masking and no adapter.
#[derive(Debug, Clone)]
pub(crate) enum HandlerLookup {
    Frame(Box<HandlerEntry>, usize),
    Base(BuiltinEntry),
}

/// The handler stack: the innermost frame sits at the highest index, each
/// beside the handle this stack minted for it.
///
/// No `Serialize` / `Deserialize`: `marshal` writes frames and their captures.
///
/// `base` is a permanent layer below every run frame — manifest rows, not
/// `HandlerFrame`s, so [`Self::strip_matched`] and [`Self::remove_handler`],
/// which index `frames` alone, cannot reach it.  It never crosses the wire:
/// the wire form is a `Vec` of frames, and a receiving shell's own boot
/// installs its base layer.
#[derive(Debug, Clone, Default)]
pub(crate) struct HandlerStack {
    frames: Vec<(FrameHandle, HandlerFrame)>,
    base: Vec<BuiltinEntry>,
    next_handle: u64,
}

impl HandlerStack {
    /// A stack over `base`, each of `frames` pushed through
    /// [`Self::push_frame`], so the handles are this stack's.
    pub(crate) fn over(base: Vec<BuiltinEntry>, frames: Vec<HandlerFrame>) -> Self {
        let mut stack = Self {
            base,
            ..Self::default()
        };
        for frame in frames {
            stack.push_frame(frame);
        }
        stack
    }

    /// Push a scoped `within` frame.  `Shell::with_handlers` owns the paired
    /// [`Self::remove_by_handle`].
    pub fn push(&mut self, entries: Vec<HandlerEntry>, catch_all: Option<ValClos>) -> FrameHandle {
        self.push_frame(HandlerFrame {
            entries,
            catch_all,
            kind: FrameKind::Within,
        })
    }

    /// Push a host-installed handler frame.
    pub(crate) fn push_handler(&mut self, entries: Vec<HandlerEntry>) -> FrameHandle {
        self.push_frame(HandlerFrame {
            entries,
            catch_all: None,
            kind: FrameKind::Handler,
        })
    }

    /// Append a whole frame under a fresh handle: identity belongs to this
    /// stack, so a wire-hydrated handler stays removable by the host.
    pub(crate) fn push_frame(&mut self, frame: HandlerFrame) -> FrameHandle {
        let handle = FrameHandle(self.next_handle);
        self.next_handle += 1;
        self.frames.push((handle, frame));
        handle
    }

    /// Remove the frame carrying `handle`, searching innermost-first.
    pub(crate) fn remove_by_handle(&mut self, handle: FrameHandle) -> Option<HandlerFrame> {
        let pos = self.frames.iter().rposition(|(h, _)| *h == handle)?;
        Some(self.frames.remove(pos).1)
    }

    /// Remove the innermost persistent handler frame for `name`.
    /// [`FrameKind`] keeps scoped frames out, even with the same shape.
    pub fn remove_handler(&mut self, name: &str) -> Option<HandlerFrame> {
        let pos = self
            .frames
            .iter()
            .rposition(|(_, f)| f.is_handler_for(name))?;
        Some(self.frames.remove(pos).1)
    }

    /// The winning handler for `name`: a run-frame per-name entry (with its
    /// depth from the top, what [`Self::strip_matched`] masks by); else a
    /// base frame; else a run-frame catch-all.  So any per-name handler
    /// beats any catch-all whatever their relative depth, and a catch-all
    /// never sees a base frame's name.  `None` falls through to external
    /// command lookup.
    pub(crate) fn lookup(&self, name: &str) -> Option<HandlerLookup> {
        for (depth, (_, frame)) in self.frames.iter().rev().enumerate() {
            if let Some(entry) = frame.entries.iter().find(|e| e.name == name) {
                return Some(HandlerLookup::Frame(Box::new(entry.clone()), depth + 1));
            }
        }
        if let Some(entry) = self.base.iter().find(|e| e.decl.name == name) {
            return Some(HandlerLookup::Base(entry.clone()));
        }
        for (depth, (_, frame)) in self.frames.iter().rev().enumerate() {
            if let Some(thunk) = &frame.catch_all {
                return Some(HandlerLookup::Frame(
                    Box::new(HandlerEntry {
                        name: Cow::Owned(name.to_string()),
                        arity: HandlerArity::CatchAll,
                        thunk: thunk.clone(),
                    }),
                    depth + 1,
                ));
            }
        }
        None
    }

    /// Every per-name entry on the stack, innermost first.  A shadowed name
    /// appears once per frame that binds it.
    pub(crate) fn entries(&self) -> impl Iterator<Item = &HandlerEntry> {
        self.iter().rev().flat_map(|f| f.entries.iter())
    }

    /// Installed per-name handlers, outermost first, for checker diagnostics.
    pub(crate) fn handled_names(&self) -> Vec<String> {
        self.iter()
            .flat_map(|frame| frame.entries.iter())
            .map(|entry| entry.name.to_string())
            .collect()
    }

    /// The frames, outermost first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &HandlerFrame> {
        self.frames.iter().map(|(_, frame)| frame)
    }

    /// Lift the frame at `depth`, as returned by [`Self::lookup`], off the
    /// stack; pair with [`Self::restore_matched`].  Only that frame goes, so
    /// outer handlers for *other* names stay visible to the running body.
    pub(crate) fn strip_matched(&mut self, depth: usize) -> (FrameHandle, HandlerFrame) {
        let index = self.frames.len() - depth;
        self.frames.remove(index)
    }

    /// Put a frame taken by [`Self::strip_matched`] back where it was.
    ///
    /// Handles are monotonic, so inserting after the rightmost strictly older
    /// handle restores the original order; the only newer frames are those the
    /// masked body pushed itself.
    pub(crate) fn restore_matched(&mut self, (handle, frame): (FrameHandle, HandlerFrame)) {
        let insert_at = self
            .frames
            .iter()
            .rposition(|(h, _)| h.0 < handle.0)
            .map_or(0, |i| i + 1);
        self.frames.insert(insert_at, (handle, frame));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fact::{DataStep, IllTyped};

    #[test]
    fn a_refused_arm_is_one_ill_typed_fact_located_at_its_span() {
        let mut shell = crate::test_helper::core_shell();
        let file = shell.install_script_context(
            &crate::source::SourceOrigin::named("handler.ral"),
            "handler",
        );
        let span = Some(crate::source::Span::new(file, 0, 7));
        let error = crate::typecheck::TypeError {
            pos: span,
            kind: crate::typecheck::TypeErrorKind::RecursiveRow,
            reason: None,
            weak: None,
            unit: None,
        };
        let Break::Error(refused) = refused_arm("echo", std::slice::from_ref(&error), span, &shell)
        else {
            panic!("handler refusal")
        };
        let Reason::Invalid(failure) = &refused.reason else {
            panic!("invalid")
        };
        assert_eq!(failure.operation, "handler");
        assert_eq!(
            failure.cause,
            InvalidCause::IllTyped(
                IllTyped::new(BlockRole::Handler("echo".into()), vec![error.diagnostic()]).unwrap(),
            )
        );
        assert_eq!(
            refused.prov.as_ref().and_then(crate::error::Prov::span),
            span
        );
    }

    #[test]
    fn a_residual_is_refused_as_a_handler_arm() {
        let mut shell = crate::test_helper::core_shell();
        let entry = shell.lookup_builtin("map").expect("map builtin");
        let function = crate::shell::runtime::block_over(&shell.env);
        let residual = crate::shell::evaluator::machine::apply(
            ValClos::builtin(Arc::new(entry)),
            vec![function],
            &crate::shell::runtime::Mooring::adrift(),
            &mut shell,
        )
        .expect("one argument leaves map's η residual");
        assert!(residual.residual().is_some());
        let Err(Break::Error(error)) = shell.install_handler("echo".into(), residual.clone())
        else {
            panic!("a residual is not a handler closure")
        };
        let Reason::Invalid(failure) = error.reason else {
            panic!("invalid handler closure")
        };
        assert_eq!(failure.operation, "handler");
        assert_eq!(failure.path, [DataStep::Field("echo".into())]);
        assert_eq!(
            failure.cause,
            InvalidCause::Shape(ShapeFailure {
                expected: Expected::Closure,
                found: residual.found(),
            })
        );
        assert!(!shell.has_handler("echo"));
    }

    #[test]
    fn a_handler_closure_expectation_excludes_natives() {
        let shell = crate::test_helper::core_shell();
        let native = crate::shell::runtime::lookup("length", &shell.env, &shell.sig).unwrap();
        assert_eq!(native.shape(), crate::fact::ValueShape::Function);
        let Err(Break::Error(error)) = arm_closure(native, "length") else {
            panic!("a native is not a handler closure")
        };
        let Reason::Invalid(failure) = error.reason else {
            panic!("invalid handler closure")
        };
        assert_eq!(failure.operation, "handler");
        assert_eq!(failure.path, [DataStep::Field("length".into())]);
        assert_eq!(
            failure.cause,
            InvalidCause::Shape(ShapeFailure {
                expected: Expected::Closure,
                found: native.found(),
            })
        );
    }
}
