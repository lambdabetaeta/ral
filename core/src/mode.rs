//! The pipeline-mode lattice: the ends of a stage's computation type
//! `F[I,O,R] A` — `None` for a value edge, `Bytes` for a raw byte channel,
//! `Var` for a variable inference resolves.
//!
//! Connecting two stages demands *equality* of the producer's result mode and
//! the consumer's input, so no value silently crosses a byte edge; that rule
//! lives on the checker's `Unifier::unify_mode`, whose annotation pass grounds
//! every stage into the [`Wire`] the evaluator reads off the IR.  A stage's
//! `output` takes no part in that: chatter escapes the pipeline rather than
//! riding its wire.
//!
//! These types ride inside a [`crate::typecheck::Scheme`] into the
//! postcard-baked prelude, which carries no schema of its own; the serde
//! derives are load-bearing.

/// Unification variable for pipeline modes.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ModeVar(pub u32);

/// The I/O mode of one end of a pipeline stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PipeMode {
    None,
    Bytes,
    Var(ModeVar),
}

/// A command's computation type: `F[input, output, result]`.  `output` and
/// `result` are the two byte conduits out and name different streams —
/// `output` is chatter, the bytes that *escape* to whoever is watching;
/// `result` is the payload, which belongs to whoever consumes the
/// computation.  Neither bounds the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PipeSpec {
    pub input: PipeMode,
    /// The chatter that escapes: bytes a discarded statement wrote, seen by
    /// the nearest enclosing visible stream and never the payload of anything.
    pub output: PipeMode,
    /// Which conduit carries this computation's payload: `Bytes` for the byte
    /// channel, `None` for the return value. Ground at every source-tree
    /// node — a payload decision pins an unresolved variable to `None`;
    /// variables appear in declared signature slots and shape expectations,
    /// quantified if never consulted.
    pub result: PipeMode,
}

impl PipeSpec {
    /// Pure: no byte channel on either end.
    pub const fn none() -> Self {
        Self {
            input: PipeMode::None,
            output: PipeMode::None,
            result: PipeMode::None,
        }
    }
    /// Decoder: bytes in, value out.
    pub const fn decode() -> Self {
        Self {
            input: PipeMode::Bytes,
            output: PipeMode::None,
            result: PipeMode::None,
        }
    }
}

/// A ground I/O mode: [`PipeMode`] with the `Var` arm removed, `Empty`
/// standing for the `∅` value edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ByteMode {
    Bytes,
    Empty,
}

impl From<ByteMode> for PipeMode {
    /// Inverse of the checker's grounding (`Bytes → Bytes`, `None`/`Var →
    /// Empty`) on the modes it preserves, so a wire-derived [`PipeSpec`] reads
    /// like the spec it was grounded from.
    fn from(mode: ByteMode) -> Self {
        match mode {
            ByteMode::Bytes => Self::Bytes,
            ByteMode::Empty => Self::None,
        }
    }
}

/// The checker's ground verdict for one stage: a [`PipeSpec`] whose variable
/// arm is already resolved, so "annotations are ground" is a fact of the type
/// rather than an invariant the reader must trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Wire {
    pub input: ByteMode,
    pub output: ByteMode,
    pub result: ByteMode,
}

impl Wire {
    /// The elaborator's placeholder, one per stage, overwritten with the
    /// inferred wire before evaluation; `Empty` is also what grounding gives
    /// an unconstrained mode.
    pub const EMPTY: Self = Self {
        input: ByteMode::Empty,
        output: ByteMode::Empty,
        result: ByteMode::Empty,
    };

    /// Lift back into the lattice for `runtime::pipeline::resolve`, which
    /// takes its boundary and kind decisions on [`PipeSpec`] modes.
    pub fn spec(self) -> PipeSpec {
        PipeSpec {
            input: self.input.into(),
            output: self.output.into(),
            result: self.result.into(),
        }
    }
}

/// A `None`/`Bytes` clash raised by `Unifier::unify_mode`.
///
/// Each caller maps it onto its own diagnostic: the checker's
/// [`crate::typecheck::TypeErrorKind::ModeMismatch`], or a rejected handler
/// arm in `typecheck::alias_arm_scheme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeMismatch {
    pub left: PipeMode,
    pub right: PipeMode,
}
