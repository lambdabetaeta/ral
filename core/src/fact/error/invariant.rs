//! Portable invariant failure facts.

use super::shared::ValueShape;
use crate::variant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvariantFailure {
    AppliedUnforcedValue(ValueShape),
    CaptureNotBytes(ValueShape),
    BoundaryWithoutSite(String),
    MissingEngineFactory,
}
variant!(InvariantFailure { AppliedUnforcedValue(ValueShape): "applied-unforced-value", CaptureNotBytes(ValueShape): "capture-not-bytes", BoundaryWithoutSite(String): "boundary-without-site", MissingEngineFactory: "missing-engine-factory" });

impl InvariantFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::AppliedUnforcedValue(shape) => format!(
                "the evaluator applied {} without forcing it",
                shape.describe()
            ),
            Self::CaptureNotBytes(shape) => {
                format!(
                    "the value boundary was handed {} where a captured byte payload belongs",
                    shape.describe()
                )
            }
            Self::BoundaryWithoutSite(name) => {
                format!("builtin {name:?} crossed a boundary without a call site")
            }
            Self::MissingEngineFactory => "the host has no engine factory".into(),
        }
    }
}

impl InvariantFailure {
    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::AppliedUnforcedValue(_) => vec!["the type checker forces every function before it is applied, so this is a fault in ral rather than in your program: please report the source that produced it".into()],
            Self::CaptureNotBytes(_) => vec!["only the type checker writes this step, so this is a fault in ral rather than in your program: please report the source that produced it".into()],
            Self::BoundaryWithoutSite(_) | Self::MissingEngineFactory => Vec::new(),
        }
    }
}
