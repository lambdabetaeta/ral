//! The user handler stack — one flat stack of [`HandlerFrame`]s shared by `alias` and
//! `within [handlers: …]`, ordered innermost-last.  Scoped frames come off by
//! handle, alias frames by name.

use super::builtin::BuiltinEntry;
use super::value::Value;
use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;
use strum::IntoStaticStr;

use super::flow::Settled;

/// Frame identity, minted from a monotonic counter on [`HandlerStack`].
///
/// Removal finds the frame by handle rather than index, so an alias dropped
/// between a push and its paired pop cannot shift the wrong frame out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameHandle(pub(crate) u64);

/// Calling convention of a handler — fixed by its surface form at install,
/// never inferred from the thunk at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum HandlerArity {
    /// `within [handler: …]` — the thunk receives `(name, args)`.
    CatchAll,
    /// `alias` or `within [handlers: …]` — the thunk receives `(args)`.
    Unary,
}

/// One user handler — the unit of installation in a [`HandlerFrame`].
/// Builtins are `BuiltinEntry` instead.
#[derive(Clone)]
pub(crate) struct HandlerEntry {
    pub name: Cow<'static, str>,
    pub(crate) arity: HandlerArity,
    pub(crate) thunk: Value,
    /// The arm's closed scheme, kept only on alias entries: their frames
    /// outlive the installing run and must seed the next run's check.
    pub(crate) scheme: Option<Arc<crate::ty::Scheme>>,
}

impl HandlerEntry {
    /// Build a per-name entry, unary by construction.  Vetting the thunk's
    /// shape belongs to the caller, at the install boundary.
    pub(crate) fn ral_per_name(name: String, thunk: Value) -> Self {
        Self {
            name: Cow::Owned(name),
            arity: HandlerArity::Unary,
            thunk,
            scheme: None,
        }
    }

    /// Vet `thunk` as the body for `name` and build the entry to install —
    /// the one gate shared by `Shell::install_alias` and `within [handlers:
    /// …]`, so each check is written once.
    ///
    /// A name already a lexical binding or a native installs fine: bare
    /// heads never reach the new frame, but `^name` does, and resolution
    /// order alone is what decides that.
    ///
    /// # Errors
    /// `thunk` not a unary lambda, or its body returning something other than
    /// what the head it stands in for returns (`Inferencer::stands_in`).
    pub(crate) fn vet(
        name: String,
        thunk: Value,
        session_schemes: crate::typecheck::SessionSchemes,
        role: HandlerRole,
    ) -> Settled<Self> {
        let label = <&str>::from(role);
        validate_handler_arity(&thunk, 1, &format!("{label}: `{name}`"))?;
        let Value::Thunk(closure) = &thunk else {
            unreachable!("validate_handler_arity guarantees a unary lambda");
        };
        let Some((param, body)) = closure.comp().arrow() else {
            unreachable!("validate_handler_arity guarantees a unary lambda");
        };
        let scheme = crate::typecheck::alias_arm_scheme(&name, param, body, session_schemes)
            .map_err(|error| refused_arm(label, &error, body.span))?;
        let mut entry = Self::ral_per_name(name, thunk);
        if role.persists_scheme() {
            entry.scheme = Some(Arc::new(scheme));
        }
        Ok(entry)
    }
}

/// Which install path is calling [`HandlerEntry::vet`] — picks the diagnostic's
/// label and whether the inferred scheme is kept on the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub(crate) enum HandlerRole {
    /// `alias NAME { |args| … }` — the frame outlives its installing run, so
    /// its scheme seeds the next run's check.
    Alias,
    /// `within [handlers: …]` — the frame is popped before the run ends.
    #[strum(serialize = "within handlers")]
    Scoped,
}

impl HandlerRole {
    fn persists_scheme(self) -> bool {
        matches!(self, Self::Alias)
    }
}

/// A refused arm, in the checker's own sentence, caret on the arm when it has
/// a span.
pub(crate) fn refused_arm(
    label: &str,
    error: &crate::typecheck::TypeError,
    span: Option<crate::source::Span>,
) -> crate::types::Break {
    let sentence = error.hint().unwrap_or_else(|| error.kind.render_message());
    let mut error = crate::types::Error::new(format!("{label}: {sentence}"));
    error.span = span;
    error.into()
}

/// Check that `value` is a lambda of exactly `arity` arguments.
///
/// This is the gate at every install boundary (`alias`, `within [handlers:
/// …]`, `within [handler: …]`), a handler's calling convention being fixed
/// by its surface form. `context` names the offending install site in the
/// message.
///
/// # Errors
/// `value` is not a lambda, or its curry-chain arity is not `arity`.
pub(crate) fn validate_handler_arity(value: &Value, arity: usize, context: &str) -> Settled<()> {
    let form = match arity {
        1 => "a unary lambda `{ |args| ... }`",
        2 => "a binary lambda `{ |name args| ... }`",
        n => unreachable!("handler arity must be 1 or 2, got {n}"),
    };
    match value.lambda_arity() {
        Some(found) if found == arity => Ok(()),
        Some(found) => Err(super::coerce::sig(format!(
            "{context} must be {form}, got a lambda taking {found} argument(s)"
        ))),
        None => Err(super::coerce::sig(format!(
            "{context} must be {form}, got a {}",
            value.type_name()
        ))),
    }
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

/// What installed a frame: `unalias` removes only an [`Self::Alias`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum FrameKind {
    /// `within [handlers: …, handler: …]`: popped by handle when the scope ends.
    Within,
    /// `alias`: outlives its run.
    Alias,
}

/// One frame of the handler stack, shared shape for scoped handlers and
/// aliases.
#[derive(Debug, Clone)]
pub(crate) struct HandlerFrame {
    pub(crate) entries: Vec<HandlerEntry>,
    /// `within [handler: thunk]`; `None` on alias frames.
    pub(crate) catch_all: Option<Value>,
    pub(crate) kind: FrameKind,
}

impl HandlerFrame {
    /// Whether this frame is *the* alias frame for `name` — the one shape
    /// predicate behind both [`HandlerStack::remove_alias`] and
    /// `Shell::has_alias`.
    pub(crate) fn is_alias_for(&self, name: &str) -> bool {
        self.kind == FrameKind::Alias && self.entries.first().is_some_and(|e| e.name == name)
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
/// No `Serialize` / `Deserialize` — frames carry `Value`, whose closures must
/// be interned through `seed`'s `InternCtx` to cross an IPC boundary, which
/// `seed`'s `WireHandlerFrame` does field by field.
///
/// `base` is a permanent layer below every run frame — manifest rows, not
/// `HandlerFrame`s, so [`Self::strip_matched`] and [`Self::remove_alias`],
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
    /// Push a scoped `within` frame.  `Shell::with_handlers` owns the paired
    /// [`Self::remove_by_handle`].
    pub fn push(&mut self, entries: Vec<HandlerEntry>, catch_all: Option<Value>) -> FrameHandle {
        self.push_frame(HandlerFrame {
            entries,
            catch_all,
            kind: FrameKind::Within,
        })
    }

    /// Push a frame that `unalias` can remove.
    pub(crate) fn push_alias(&mut self, entries: Vec<HandlerEntry>) -> FrameHandle {
        self.push_frame(HandlerFrame {
            entries,
            catch_all: None,
            kind: FrameKind::Alias,
        })
    }

    /// Append a whole frame under a fresh handle: identity belongs to this
    /// stack, so a wire-hydrated alias stays removable by `unalias`.
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

    /// Remove the innermost alias frame for `name`.  The [`FrameKind`] excludes
    /// a `within [handlers: [foo: t]]` frame by construction, even though it
    /// shares an alias's one-entry, no-catch-all shape.
    pub fn remove_alias(&mut self, name: &str) -> Option<HandlerFrame> {
        let pos = self
            .frames
            .iter()
            .rposition(|(_, f)| f.is_alias_for(name))?;
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
                        scheme: None,
                    }),
                    depth + 1,
                ));
            }
        }
        None
    }

    /// Install base handler frames — the manifest's argv half.
    pub(crate) fn install_base(&mut self, entries: &[BuiltinEntry]) {
        self.base.extend(entries.iter().cloned());
    }

    /// Every per-name entry on the stack, innermost first.  A shadowed name
    /// appears once per frame that binds it.
    pub(crate) fn entries(&self) -> impl Iterator<Item = &HandlerEntry> {
        self.iter().rev().flat_map(|f| f.entries.iter())
    }

    /// The installed alias arms' schemes, outermost first — the alias half of
    /// the seed `Shell::session_schemes` hands the next run's check.
    pub(crate) fn alias_schemes(&self) -> Vec<(String, Arc<crate::ty::Scheme>)> {
        self.iter()
            .filter(|f| f.kind == FrameKind::Alias)
            .flat_map(|f| f.entries.iter())
            .filter_map(|entry| {
                entry
                    .scheme
                    .clone()
                    .map(|scheme| (entry.name.as_ref().to_string(), scheme))
            })
            .collect()
    }

    /// The frames, outermost first.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &HandlerFrame> {
        self.frames.iter().map(|(_, frame)| frame)
    }

    /// Every value a frame holds — each entry's `thunk`, then its
    /// `catch_all` — outermost frame first.
    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut Value> {
        self.frames.iter_mut().flat_map(|(_, frame)| {
            frame
                .entries
                .iter_mut()
                .map(|entry| &mut entry.thunk)
                .chain(frame.catch_all.as_mut())
        })
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
