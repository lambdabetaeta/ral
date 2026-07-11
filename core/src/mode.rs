//! The pipeline-mode lattice that schemes serialise and the annotation
//! pass grounds.
//!
//! A pipeline stage has a computation type `F[I,O] A`: the modes `I` and
//! `O` classify the two byte channels — `None` for a value edge (no byte
//! stream), `Bytes` for a raw byte channel, or a unification variable
//! `Var` resolved during inference.  Connection requires the producer's
//! output mode to *equal* the consumer's input mode; a value cannot
//! silently cross a byte edge (`docs/SPEC.md` §4.2.1, §20.4).
//!
//! The static Hindley–Milner checker in [`mod@crate::typecheck`] is the sole
//! engine over these modes: it validates a whole program ahead of time and
//! grounds each stage's variable into a [`Wire`], the two-valued image the
//! evaluator reads off the IR.  The equality rule itself lives on the
//! checker's `Unifier` (`crate::typecheck::unify`).
//!
//! `PipeMode`, `ModeVar`, and `PipeSpec` are serialized: they ride inside
//! a [`crate::typecheck::Scheme`] and are baked into the prelude as a
//! postcard blob (see the schema-evolution note atop [`crate`]).
//! Their serde derives are load-bearing.

/// Unification variable for pipeline modes.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ModeVar(pub u32);

/// The I/O mode of one end of a pipeline stage.
///
/// `None` — no byte stream (value edge).
/// `Bytes` — raw byte channel (external commands, `to-X`/`from-X`).
/// `Var` — unification variable, resolved during inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PipeMode {
    None,
    Bytes,
    Var(ModeVar),
}

/// Pipeline specification: the input and output modes of a command —
/// `F[input, output]` in the CBPV computation type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PipeSpec {
    pub input: PipeMode,
    pub output: PipeMode,
}

impl PipeSpec {
    /// Pure: no pipeline I/O on either end.
    pub const fn none() -> Self {
        Self {
            input: PipeMode::None,
            output: PipeMode::None,
        }
    }
    /// Decoder: consumes a byte stream, produces a value (no byte output).
    pub const fn decode() -> Self {
        Self {
            input: PipeMode::Bytes,
            output: PipeMode::None,
        }
    }
}

/// A ground I/O mode: the two-valued image of a [`PipeMode`] with the
/// `Var` arm removed.
///
/// `Empty` — the `∅` mode, no byte stream (value edge).
/// `Bytes` — the byte-channel mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ByteMode {
    Bytes,
    Empty,
}

impl From<ByteMode> for PipeMode {
    /// Lift a ground mode back into the lattice: `Bytes` to the byte
    /// channel, `Empty` to the `∅` value edge.  This is the inverse of
    /// the grounding rule (`Bytes → Bytes`, `None`/`Var → Empty`) on the
    /// values the rule preserves, so a wire-derived [`PipeSpec`] reads the
    /// same as the spec it was grounded from.
    fn from(mode: ByteMode) -> Self {
        match mode {
            ByteMode::Bytes => Self::Bytes,
            ByteMode::Empty => Self::None,
        }
    }
}

/// The checker's instantiated, ground verdict for one pipeline stage:
/// the input and output [`ByteMode`]s of a `F[input, output]` computation
/// type with no variable arm.
///
/// Where a [`PipeSpec`] admits a `PipeMode::Var`
/// resolved during inference, a `Wire` is the two-valued image of that spec
/// once the variable is grounded — so "annotations are ground" is a fact of
/// the type rather than an invariant the reader must trust.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Wire {
    pub input: ByteMode,
    pub output: ByteMode,
}

impl Wire {
    /// The elaborator's placeholder: both edges `Empty`, the same default
    /// the annotation pass grounds an unconstrained mode to.  A pipeline
    /// is born carrying one [`Wire::EMPTY`] per stage; the checker
    /// overwrites it with the inferred wire before evaluation.
    pub const EMPTY: Self = Self {
        input: ByteMode::Empty,
        output: ByteMode::Empty,
    };

    /// Lift the ground wire back into a [`PipeSpec`] over the lattice, so a
    /// consumer that already reads `PipeSpec` modes (pipeline staging's
    /// boundary and kind decisions) reads the checker's verdict unchanged.
    pub fn spec(self) -> PipeSpec {
        PipeSpec {
            input: self.input.into(),
            output: self.output.into(),
        }
    }
}

/// A mode-unification failure: a `None`/`Bytes` clash the checker's
/// `unify_mode` raises and each caller maps onto its own diagnostic.
///
/// The
/// static checker's [`crate::typecheck::TypeErrorKind::ModeMismatch`], or
/// the install-time `alias_arm_scheme` rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeMismatch {
    pub left: PipeMode,
    pub right: PipeMode,
}
