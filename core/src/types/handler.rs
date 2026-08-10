//! The user handler stack — one flat `Vec<HandlerFrame>` shared by `alias` and
//! `within [handlers: …]`, ordered innermost-last.  Scoped frames come off by
//! handle, alias frames by name.

use super::builtin::BuiltinEntry;
use super::value::Value;
use crate::typecheck;
use std::borrow::Cow;
use std::fmt;

use super::flow::Settled;

/// Frame identity, minted from a monotonic counter on [`HandlerStack`].
///
/// Removal finds the frame by handle rather than index, so an alias dropped
/// between a push and its paired pop cannot shift the wrong frame out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHandle(pub(crate) u64);

/// Calling convention of a handler — fixed by its surface form at install,
/// never inferred from the thunk at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HandlerArity {
    /// `within [handler: …]` — the thunk receives `(name, args)`.
    CatchAll,
    /// `alias` or `within [handlers: …]` — the thunk receives `(args)`.
    Unary,
}

/// One user handler — the unit of installation in a [`HandlerFrame`].
/// Builtins are `BuiltinEntry` instead.
#[derive(Clone)]
pub struct HandlerEntry {
    pub name: Cow<'static, str>,
    pub arity: HandlerArity,
    pub thunk: Value,
    /// The arm's closed scheme, kept only on alias entries: their frames
    /// outlive the installing run and must seed the next run's check.
    pub scheme: Option<crate::typecheck::Scheme>,
}

impl HandlerEntry {
    /// Build a per-name entry, unary by construction.  Vetting the thunk's
    /// shape belongs to the caller, at the install boundary.
    pub fn ral_per_name(name: String, thunk: Value) -> Self {
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
    /// `thunk` not a unary lambda, its body disagreeing with the head about
    /// where their payload lives, or — under a byte-routed head — still
    /// returning a value instead of `Unit`.
    pub fn vet(
        name: String,
        thunk: Value,
        session_schemes: crate::typecheck::SessionSchemes,
        role: HandlerRole,
    ) -> Settled<Self> {
        let label = role.label();
        validate_handler_arity(&thunk, 1, &format!("{label}: `{name}`"))?;
        let Value::Lambda { param, body, .. } = &thunk else {
            unreachable!("validate_handler_arity guarantees a unary lambda");
        };
        let scheme = crate::typecheck::alias_arm_scheme(&name, param, body, session_schemes)
            .map_err(|failure| {
                use crate::typecheck::{PinFailure, fmt_route, fmt_ty};
                let msg = match failure {
                    PinFailure::Route(m) => format!(
                        "{label}: `{name}`'s body and the head it reinterprets disagree \
                         about where their payload lives — the arm's is {}, the head's is \
                         {}; a handler must agree with the head it reinterprets, so match \
                         its route or add a codec",
                        fmt_route(&m.left),
                        fmt_route(&m.right),
                    ),
                    PinFailure::ByteHeadReturnsValue(ty) => format!(
                        "{label}: `{name}`'s payload is its stdout, so an arm installed \
                         under it has no separate value to return; its return type must be \
                         Unit, and `{name}`'s body returns {}",
                        fmt_ty(&ty),
                    ),
                };
                super::coerce::sig(msg)
            })?;
        let mut entry = Self::ral_per_name(name, thunk);
        if role.persists_scheme() {
            entry.scheme = Some(scheme);
        }
        Ok(entry)
    }
}

/// Which install path is calling [`HandlerEntry::vet`] — picks the diagnostic's
/// label and whether the inferred scheme is kept on the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerRole {
    /// `alias NAME { |args| … }` — the frame outlives its installing run, so
    /// its scheme seeds the next run's check.
    Alias,
    /// `within [handlers: …]` — the frame is popped before the run ends.
    Scoped,
}

impl HandlerRole {
    fn label(self) -> &'static str {
        match self {
            Self::Alias => "alias",
            Self::Scoped => "within handlers",
        }
    }

    fn persists_scheme(self) -> bool {
        matches!(self, Self::Alias)
    }
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
pub fn validate_handler_arity(value: &Value, arity: usize, context: &str) -> Settled<()> {
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

/// One frame of the handler stack, shared shape for scoped handlers and
/// aliases.
#[derive(Debug, Clone)]
pub struct HandlerFrame {
    pub entries: Vec<HandlerEntry>,
    /// `within [handler: thunk]`; `None` on alias frames.
    pub catch_all: Option<Value>,
    pub handle: FrameHandle,
    /// Set only by `alias`; scoped `within` frames come off by handle instead.
    pub removable_by_unalias: bool,
}

impl HandlerFrame {
    /// Whether this frame is *the* alias frame for `name` — the one shape
    /// predicate behind both [`HandlerStack::remove_alias`] and
    /// `Shell::has_alias`.
    pub fn is_alias_for(&self, name: &str) -> bool {
        self.removable_by_unalias
            && self.catch_all.is_none()
            && self.entries.len() == 1
            && self.entries[0].name == name
    }
}

/// One [`HandlerStack::lookup`] hit: a user frame, masked by depth and run
/// through the ordinary handler calling convention, or a base frame, called
/// directly with no masking and no adapter.
#[derive(Debug, Clone)]
pub enum HandlerLookup {
    Frame(Box<HandlerEntry>, usize),
    Base(BuiltinEntry),
}

/// The handler stack: the innermost frame sits at the highest index.
///
/// No `Serialize` / `Deserialize` — frames carry `Value`, whose closures must
/// be interned through `serial::InternCtx` to cross an IPC boundary, which
/// `subprocess`'s `WireHandlerFrame` does field by field.
///
/// `base` is a permanent layer below every run frame — manifest rows, not
/// `HandlerFrame`s, so [`Self::strip_matched`] and [`Self::remove_alias`],
/// which index `frames` alone, cannot reach it.  It never crosses the wire:
/// the wire form is a `Vec<HandlerFrame>`, and a receiving shell's own boot
/// installs its base layer.
#[derive(Debug, Clone, Default)]
pub struct HandlerStack {
    frames: Vec<HandlerFrame>,
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
            handle: FrameHandle(u64::MAX),
            removable_by_unalias: false,
        })
    }

    /// Push a frame that `unalias` can remove.
    pub fn push_alias(&mut self, entries: Vec<HandlerEntry>) -> FrameHandle {
        self.push_frame(HandlerFrame {
            entries,
            catch_all: None,
            handle: FrameHandle(u64::MAX),
            removable_by_unalias: true,
        })
    }

    /// Append a whole frame, minting a fresh handle and keeping every other
    /// field — notably `removable_by_unalias`, so a wire-hydrated alias stays
    /// removable.  The incoming handle is discarded: identity belongs to the
    /// receiving stack.
    pub fn push_frame(&mut self, mut frame: HandlerFrame) -> FrameHandle {
        let handle = FrameHandle(self.next_handle);
        self.next_handle += 1;
        frame.handle = handle;
        self.frames.push(frame);
        handle
    }

    /// Remove the frame carrying `handle`, searching innermost-first.
    pub fn remove_by_handle(&mut self, handle: FrameHandle) -> Option<HandlerFrame> {
        let pos = self.frames.iter().rposition(|f| f.handle == handle)?;
        Some(self.frames.remove(pos))
    }

    /// Remove the innermost alias frame for `name`.  The `removable_by_unalias`
    /// bit excludes a `within [handlers: [foo: t]]` frame by construction, even
    /// though it shares an alias's one-entry, no-catch-all shape.
    pub fn remove_alias(&mut self, name: &str) -> Option<HandlerFrame> {
        let pos = self.frames.iter().rposition(|f| f.is_alias_for(name))?;
        Some(self.frames.remove(pos))
    }

    /// The winning handler for `name`: a run-frame per-name entry (with its
    /// depth from the top, what [`Self::strip_matched`] masks by); else a
    /// base frame; else a run-frame catch-all.  So any per-name handler
    /// beats any catch-all whatever their relative depth, and a catch-all
    /// never sees a base frame's name.  `None` falls through to external
    /// command lookup.
    pub fn lookup(&self, name: &str) -> Option<HandlerLookup> {
        for (depth, frame) in self.frames.iter().rev().enumerate() {
            if let Some(entry) = frame.entries.iter().find(|e| e.name == name) {
                return Some(HandlerLookup::Frame(Box::new(entry.clone()), depth + 1));
            }
        }
        if let Some(entry) = self.base.iter().find(|e| e.name == name) {
            return Some(HandlerLookup::Base(entry.clone()));
        }
        for (depth, frame) in self.frames.iter().rev().enumerate() {
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

    /// Install base handler frames — manifest rows for variadic/optional
    /// builtins.
    pub(crate) fn install_base(&mut self, entries: &[BuiltinEntry]) {
        self.base.extend(entries.iter().cloned());
    }

    /// Every per-name entry on the stack, innermost first.  A shadowed name
    /// appears once per frame that binds it.
    pub fn entries(&self) -> impl Iterator<Item = &HandlerEntry> {
        self.frames.iter().rev().flat_map(|f| f.entries.iter())
    }

    /// The installed alias arms' schemes, outermost first — the alias half of
    /// the seed `Shell::session_schemes` hands the next run's check.
    pub fn alias_schemes(&self) -> Vec<(String, typecheck::Scheme)> {
        self.frames
            .iter()
            .filter(|f| f.removable_by_unalias)
            .flat_map(|f| f.entries.iter())
            .filter_map(|entry| {
                entry
                    .scheme
                    .clone()
                    .map(|scheme| (entry.name.as_ref().to_string(), scheme))
            })
            .collect()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, HandlerFrame> {
        self.frames.iter()
    }

    /// Lift the frame at `depth`, as returned by [`Self::lookup`], off the
    /// stack; pair with [`Self::restore_matched`].  Only that frame goes, so
    /// outer handlers for *other* names stay visible to the running body.
    pub fn strip_matched(&mut self, depth: usize) -> HandlerFrame {
        let index = self.frames.len() - depth;
        self.frames.remove(index)
    }

    /// Put a frame taken by [`Self::strip_matched`] back where it was.
    ///
    /// Handles are monotonic, so inserting after the rightmost strictly older
    /// handle restores the original order; the only newer frames are those the
    /// masked body pushed itself.
    pub fn restore_matched(&mut self, frame: HandlerFrame) {
        let insert_at = self
            .frames
            .iter()
            .rposition(|f| f.handle.0 < frame.handle.0)
            .map_or(0, |i| i + 1);
        self.frames.insert(insert_at, frame);
    }
}

impl<'a> IntoIterator for &'a HandlerStack {
    type Item = &'a HandlerFrame;
    type IntoIter = std::slice::Iter<'a, HandlerFrame>;
    fn into_iter(self) -> Self::IntoIter {
        self.frames.iter()
    }
}

impl From<Vec<HandlerFrame>> for HandlerStack {
    /// Assigns fresh handles, the wire format not carrying them: this is how
    /// frames hydrated from a subprocess envelope acquire identity.
    fn from(v: Vec<HandlerFrame>) -> Self {
        let mut stack = Self::default();
        for frame in v {
            stack.push_frame(frame);
        }
        stack
    }
}

impl From<HandlerStack> for Vec<HandlerFrame> {
    fn from(s: HandlerStack) -> Self {
        s.frames
    }
}
