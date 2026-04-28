//! Head classification: given a command name, decide what kind of computation
//! it represents.  Used by both the static type checker and the runtime mode
//! inferencer so the classification logic lives in exactly one place.
//!
//! The prelude exports that appear in the tables below are defined in
//! prelude.ral.  Their classification cannot be derived from their ral source
//! (which the runtime does not parse) so it is stated here explicitly.

use crate::builtins::{BuiltinCompHint, builtin_comp_hint};

use crate::prelude_manifest;

const PRELUDE_STREAMING_REDUCER_FNS: &[&str] = &["map-lines", "filter-lines", "each-line"];
const PRELUDE_BRANCH_FNS: &[&str] = &["if", "cond"];
const PRELUDE_LAST_THUNK_FNS: &[&str] = &["for"];

/// Classification of a command head by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadKind {
    /// External command or bytes-mode builtin — stdin/stdout are byte streams.
    Bytes,
    /// Pure value-output builtin — no stdio, returns a ral value.
    Value,
    /// Branching construct — output is the union of thunk-argument outputs.
    Branches,
    /// Loop or scope construct — output is the last thunk argument's output.
    LastThunk,
    /// Decoding stage — reads bytes from stdin, produces a ral value.
    DecodeToValue,
    /// Encoding stage — accepts a ral value, writes bytes to stdout.
    EncodeToBytes,
    /// Streaming reducer — consumes bytes line-by-line, emits bytes, returns Unit.
    StreamingReducer,
    /// Diverging — never returns normally; unifies with any type.
    Never,
    /// Unknown external command — defaults to bytes-in / bytes-out.
    External,
}

/// Prelude-name → kind table.  Walked in order; first match wins.
const PRELUDE_TABLE: &[(&[&str], HeadKind)] = &[
    (PRELUDE_STREAMING_REDUCER_FNS, HeadKind::StreamingReducer),
    (PRELUDE_BRANCH_FNS, HeadKind::Branches),
    (PRELUDE_LAST_THUNK_FNS, HeadKind::LastThunk),
    (&["within", "grant"], HeadKind::LastThunk),
];

/// Classify `name` into a `HeadKind`.
///
/// The order of checks matches the original `ty::head_sig` dispatch:
/// 1. C-level builtins (from the builtin registry hint table).
/// 2. Prelude-defined commands with known byte/branch/loop/decode semantics.
/// 3. Prelude-defined commands that produce plain values.
/// 4. Everything else: unknown external command.
pub fn head_kind(name: &str) -> HeadKind {
    if let Some(hint) = builtin_comp_hint(name) {
        return match hint {
            BuiltinCompHint::Bytes => HeadKind::Bytes,
            BuiltinCompHint::Value => HeadKind::Value,
            BuiltinCompHint::LastThunk => HeadKind::LastThunk,
            BuiltinCompHint::DecodeToValue => HeadKind::DecodeToValue,
            BuiltinCompHint::EncodeToBytes => HeadKind::EncodeToBytes,
            BuiltinCompHint::Never => HeadKind::Never,
        };
    }
    for (names, kind) in PRELUDE_TABLE {
        if names.contains(&name) {
            return *kind;
        }
    }
    if prelude_manifest::PRELUDE_EXPORTS.contains(&name) {
        return HeadKind::Value;
    }
    HeadKind::External
}
