//! The user handler stack.
//!
//! [`HandlerFrame`] is one frame of the user handler stack.  All frames
//! share one flat `Vec<HandlerFrame>` in [`HandlerStack`], ordered
//! innermost-last (last-pushed-wins).  Scoped `within` handlers are removed
//! by handle; aliases are removed by name and carry an explicit
//! `removable_by_unalias` bit.

use super::value::Value;
use crate::typecheck;
use std::borrow::Cow;
use std::fmt;

use super::flow::Settled;

/// Opaque handle returned by [`HandlerStack::push`].
///
/// Generational — allocated from a monotonic counter on [`HandlerStack`],
/// one per push.  Passing the handle back to [`HandlerStack::remove_by_handle`]
/// locates the frame by identity rather than index, so removal is robust
/// to sibling alias removals that would shift array indices between push
/// and paired pop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHandle(pub(crate) u64);

/// Calling convention of a handler invocation — fixed by the surface
/// form at install time, never inferred from the value's runtime shape.
///
/// A per-name handler (`within [handlers: …]`) and an alias are always
/// [`Unary`]: a unary lambda `{ |args| … }` invoked with the command's
/// argument list.  A catch-all (`within [handler: …]`) is always
/// [`CatchAll`]: a binary lambda `{ |name args| … }` invoked with the
/// command name and the argument list.  The install boundary rejects any
/// value that does not match the required arity.
///
/// [`Unary`]: HandlerArity::Unary
/// [`CatchAll`]: HandlerArity::CatchAll
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum HandlerArity {
    /// Catch-all: thunk receives `(name, args)`.
    CatchAll,
    /// Per-name lambda: thunk receives `(args)`.
    Unary,
}

/// One user handler entry — the unit of installation in a
/// [`HandlerFrame`].  Builtins are represented by
/// [`BuiltinEntry`](super::BuiltinEntry), not by this type.
#[derive(Clone)]
pub struct HandlerEntry {
    pub name: Cow<'static, str>,
    /// Calling convention for the dispatch site.  Read directly off the
    /// entry rather than inferred from the thunk shape per call.
    pub arity: HandlerArity,
    pub thunk: Value,
    /// The arm's closed scheme, stored at install for persistent (alias)
    /// frames so the next turn's check sees the alias as the installing
    /// turn did.  `None` on `within [handlers: …]` entries, whose frames
    /// never outlive their turn.
    pub scheme: Option<crate::typecheck::Scheme>,
}

impl HandlerEntry {
    /// Build a per-name entry for a user-defined `within [handlers: …]`
    /// or `alias` thunk.  Always [`HandlerArity::Unary`]: a per-name
    /// handler's calling convention is fixed by its surface form, so its
    /// thunk is a unary lambda `{ |args| … }` invoked with the command's
    /// argument list.  The caller validates at the install boundary that
    /// the thunk is in fact a unary lambda.
    pub fn ral_per_name(name: String, thunk: Value) -> Self {
        Self {
            name: Cow::Owned(name),
            arity: HandlerArity::Unary,
            thunk,
            scheme: None,
        }
    }

    /// Vet `thunk` as the handler body for `name` and build the entry to
    /// install — the single gate shared by `alias` and `within [handlers:
    /// …]` so the name-conflict check, the shape check, and the
    /// mode-preservation check are each written once regardless of which
    /// install path is calling.
    ///
    /// Checked in order: `name` must be free of both a lexical binding
    /// and a builtin (a handler needs a head of its own to dispatch on);
    /// `thunk` must be a unary lambda `{ |args| … }`, enforced by
    /// [`validate_handler_arity`]; its body must preserve the head's
    /// pipeline mode, enforced by [`crate::typecheck::alias_arm_scheme`].
    /// `role` names the diagnostic and picks whether the inferred scheme
    /// is persisted on the entry — an alias frame outlives its
    /// installing turn and needs it seeded for the next turn's check; a
    /// `within [handlers: …]` frame is popped before the turn ends and
    /// needs none.
    ///
    /// # Errors
    /// Returns `Err` if `name` is already a lexical binding in scope, if
    /// `name` is a builtin, if `thunk` is not a unary lambda (per
    /// [`validate_handler_arity`]), or if the arm's body changes the head's
    /// pipeline mode (per [`crate::typecheck::alias_arm_scheme`]).
    pub fn vet(
        name: String,
        thunk: Value,
        session_schemes: crate::typecheck::SessionSchemes,
        role: HandlerRole,
    ) -> Settled<Self> {
        let label = role.label();
        if session_schemes.bindings.iter().any(|(n, _)| n == &name) {
            return Err(super::coerce::sig(format!(
                "{label}: `{name}` is a lexical binding in this scope; handler names must \
                 be free of lexical bindings and builtins"
            )));
        }
        if crate::builtins::is_builtin(&name) {
            return Err(super::coerce::sig(format!(
                "{label}: `{name}` is a builtin; handler names must be free of lexical \
                 bindings and builtins"
            )));
        }
        validate_handler_arity(&thunk, 1, &format!("{label}: `{name}`"))?;
        let Value::Lambda { param, body, .. } = &thunk else {
            unreachable!("validate_handler_arity guarantees a unary lambda");
        };
        let scheme = crate::typecheck::alias_arm_scheme(&name, param, body, session_schemes)
            .map_err(|m| {
                use crate::typecheck::fmt_mode;
                super::coerce::sig(format!(
                    "{label}: `{name}`'s body changes the head's pipeline mode ({} vs {}); \
                     a handler reinterprets a head and must preserve its modes — match the \
                     existing head's modes or add a codec",
                    fmt_mode(&m.left),
                    fmt_mode(&m.right),
                ))
            })?;
        let mut entry = Self::ral_per_name(name, thunk);
        if role.persists_scheme() {
            entry.scheme = Some(scheme);
        }
        Ok(entry)
    }
}

/// Which install path is vetting a handler entry through
/// [`HandlerEntry::vet`] — picks the diagnostic's label and whether the
/// arm's inferred scheme is persisted on the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerRole {
    /// `alias NAME { |args| … }` — the frame outlives its installing
    /// turn, so its scheme is persisted for the next turn's check.
    Alias,
    /// `within [handlers: …]` — the frame is popped before the turn
    /// ends, so no scheme needs to survive it.
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

/// Validate that a handler thunk's surface form matches the required
/// calling convention: it must be a lambda of exactly `arity` arguments.
///
/// The calling convention of a handler is fixed by its surface form, not
/// inferred from its runtime shape, so this is the single gate at every
/// install boundary (`alias`, `within [handlers: …]`, `within [handler:
/// …]`).  A non-lambda value or a lambda of the wrong arity is rejected
/// with a message that names what was wrong and `context` (e.g. ``alias:
/// `greet` ``) so the diagnostic points at the offending install site.
///
/// # Errors
/// Returns `Err` if `value` is not a lambda, or is a lambda whose
/// curry-chain arity is not exactly `arity`.
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

/// One frame of the handler stack.
///
/// Per-name entries are checked before the catch-all within the same
/// frame.  If a frame has neither a matching entry nor a catch-all,
/// command lookup falls through to the next handler frame.  Scoped
/// handlers and aliases share this frame shape, with an explicit
/// removability bit for `unalias`.
#[derive(Debug, Clone)]
pub struct HandlerFrame {
    pub entries: Vec<HandlerEntry>,
    /// Catch-all handler: `within [handler: thunk]`.  `None` on alias
    /// frames.  Its arity is implicit ([`HandlerArity::CatchAll`]).
    pub catch_all: Option<Value>,
    /// Opaque identity for paired push / remove.
    pub handle: FrameHandle,
    /// True only for frames installed by `alias` and removable by
    /// `unalias`; scoped `within` frames are removed by handle.
    pub removable_by_unalias: bool,
}

impl HandlerFrame {
    /// Whether this frame is the alias frame for `name`: it carries the
    /// `removable_by_unalias` bit and has exactly one per-name entry
    /// matching `name` with no catch-all.  The single shape predicate
    /// shared by alias removal and alias presence queries.
    pub fn is_alias_for(&self, name: &str) -> bool {
        self.removable_by_unalias
            && self.catch_all.is_none()
            && self.entries.len() == 1
            && self.entries[0].name == name
    }
}

/// The handler stack.
///
/// A flat `Vec<HandlerFrame>` with last-pushed-wins ordering — the
/// innermost frame is at the highest index.  Scoped `within` handlers
/// are removed by handle; aliases are removed by walking for the
/// matching removable name.
///
/// The two-pass [`HandlerStack::lookup`] rule (`per-name across all
/// frames, then catch-all across all frames`) ensures any per-name
/// handler beats any catch-all regardless of stack position.
///
/// No `Serialize` / `Deserialize`: frames carry `Value`, which holds
/// closures that must be interned through `serial::InternCtx` at IPC
/// boundaries; the wire mirror in `subprocess::WireHandlerFrame` handles
/// that conversion field-by-field.
#[derive(Debug, Clone, Default)]
pub struct HandlerStack {
    frames: Vec<HandlerFrame>,
    next_handle: u64,
}

impl HandlerStack {
    /// Allocate a new handle, append a frame, and return the handle.
    ///
    /// Covers scoped `within` installation.  The caller is responsible
    /// for removing the frame via [`Self::remove_by_handle`].
    pub fn push(&mut self, entries: Vec<HandlerEntry>, catch_all: Option<Value>) -> FrameHandle {
        self.push_frame(HandlerFrame {
            entries,
            catch_all,
            handle: FrameHandle(u64::MAX),
            removable_by_unalias: false,
        })
    }

    /// Install an alias frame removable by [`Self::remove_alias`].
    pub fn push_alias(&mut self, entries: Vec<HandlerEntry>) -> FrameHandle {
        self.push_frame(HandlerFrame {
            entries,
            catch_all: None,
            handle: FrameHandle(u64::MAX),
            removable_by_unalias: true,
        })
    }

    /// Append a complete [`HandlerFrame`], minting a fresh handle from
    /// this stack's counter and preserving every other field — notably
    /// `removable_by_unalias`, so a wire-hydrated alias frame stays
    /// removable by `unalias`.  The frame's incoming `handle` is
    /// discarded; identity belongs to the receiving stack.
    pub fn push_frame(&mut self, mut frame: HandlerFrame) -> FrameHandle {
        let handle = FrameHandle(self.next_handle);
        self.next_handle += 1;
        frame.handle = handle;
        self.frames.push(frame);
        handle
    }

    /// Remove the frame with the given handle (walk innermost-first;
    /// usually near the top).  Returns the removed frame, or `None` if
    /// no frame carries that handle.  Used by `with_handlers`'s paired
    /// pop.
    pub fn remove_by_handle(&mut self, handle: FrameHandle) -> Option<HandlerFrame> {
        let pos = self.frames.iter().rposition(|f| f.handle == handle)?;
        Some(self.frames.remove(pos))
    }

    /// Remove the innermost alias frame for `name` (see
    /// [`HandlerFrame::is_alias_for`]).  Returns the removed frame, or
    /// `None` if no such frame is installed.
    ///
    /// Selection turns on the `removable_by_unalias` bit, which `push`
    /// clears on scoped `within` frames; only frames installed by
    /// `alias` carry it.  A `within [handlers: [foo: t]]` frame is thus
    /// excluded by construction even when it shares the one-entry,
    /// no-catch-all shape of an alias for `foo`.
    pub fn remove_alias(&mut self, name: &str) -> Option<HandlerFrame> {
        let pos = self.frames.iter().rposition(|f| f.is_alias_for(name))?;
        Some(self.frames.remove(pos))
    }

    /// Walk the stack in two passes, returning the winning handler for
    /// `name`.  Returns the matched entry together with `depth` — the
    /// count of frames from the top to (and including) the matched
    /// frame, used by self-masking invocation to locate and lift the
    /// matched frame for the dynamic extent of the body.
    ///
    /// **Pass 1 — per-name:** scan all frames innermost-first.  The
    /// first frame that has an explicit entry whose name equals `name`
    /// wins immediately.
    ///
    /// **Pass 2 — catch-all:** if no per-name entry was found anywhere,
    /// scan all frames innermost-first again.  The first frame that
    /// carries a catch-all thunk wins; the synthesized `HandlerEntry`
    /// has `arity = CatchAll` and `thunk` set to the catch-all value.
    ///
    /// Returning `None` means the name is not handled by the stack at
    /// all (the caller falls through to external command lookup).
    pub fn lookup(&self, name: &str) -> Option<(HandlerEntry, usize)> {
        // Pass 1: per-name match across all frames.
        for (depth, frame) in self.frames.iter().rev().enumerate() {
            if let Some(entry) = frame.entries.iter().find(|e| e.name == name) {
                return Some((entry.clone(), depth + 1));
            }
        }
        // Pass 2: catch-all match across all frames.
        for (depth, frame) in self.frames.iter().rev().enumerate() {
            if let Some(thunk) = &frame.catch_all {
                return Some((
                    HandlerEntry {
                        name: Cow::Owned(name.to_string()),
                        arity: HandlerArity::CatchAll,
                        thunk: thunk.clone(),
                        scheme: None,
                    },
                    depth + 1,
                ));
            }
        }
        None
    }

    /// All per-name handler entries installed across the stack,
    /// innermost first.  Duplicates are not de-duplicated.
    pub fn entries(&self) -> impl Iterator<Item = &HandlerEntry> {
        self.frames.iter().rev().flat_map(|f| f.entries.iter())
    }

    /// The (name, scheme) pairs of installed alias arms, outermost first
    /// — the alias half of the next turn's check seed.
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

    /// Lift the matched frame off the stack and return it.  Pair with
    /// [`Self::restore_matched`].  Only the matched frame is removed —
    /// frames newer or older than it stay in place, so outer handlers
    /// for *other* names remain visible inside the running body.
    /// `depth` is the value returned alongside the match by
    /// [`Self::lookup`].  The frame carries its own `handle`, which
    /// `restore_matched` reads to find the insertion point.
    pub fn strip_matched(&mut self, depth: usize) -> HandlerFrame {
        let index = self.frames.len() - depth;
        self.frames.remove(index)
    }

    /// Re-insert the frame previously taken by [`Self::strip_matched`]
    /// at its correct position, using its handle to find the insertion
    /// point that preserves the original relative ordering.
    ///
    /// The frame must go back *under* any frames newer than it (frames
    /// with higher handle values) and *over* anything older.  Since
    /// handles are monotonically allocated, we find the rightmost frame
    /// whose handle is strictly older and insert after it.  In practice
    /// this walks at most a few entries — only frames pushed during the
    /// matched body's own execution will have newer handles.
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
    /// Build a `HandlerStack` from a raw frame vec, assigning new handles.
    /// Used at IPC boundaries where deserialized frames arrive without
    /// handles (the wire format does not carry them).
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
