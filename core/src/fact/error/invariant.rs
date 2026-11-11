//! Portable invariant failure facts.

use super::shared::ValueShape;
use crate::variant;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvariantFailure {
    BareLambda,
    AppliedUnforcedValue(ValueShape),
    CaptureNotBytes(ValueShape),
    HandleInSeed,
    BoundaryWithoutSite(String),
    MissingEngineFactory,
}
variant!(InvariantFailure { BareLambda: "bare-lambda", AppliedUnforcedValue(ValueShape): "applied-unforced-value", CaptureNotBytes(ValueShape): "capture-not-bytes", HandleInSeed: "handle-in-seed", BoundaryWithoutSite(String): "boundary-without-site", MissingEngineFactory: "missing-engine-factory" });

impl InvariantFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::BareLambda => "tried to evaluate a bare lambda in computation position: a function value must be applied to arguments or held as a thunk".into(),
            Self::AppliedUnforcedValue(shape) => format!(
                "the evaluator applied {} without forcing it",
                shape.describe()
            ),
            Self::CaptureNotBytes(shape) => {
                format!("the value boundary was handed {} where a captured byte payload belongs", shape.describe())
            }
            Self::HandleInSeed => "a live handle cannot be sealed into an engine seed".into(),
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
            Self::BareLambda => vec!["either call this function with all its arguments, or hold it in a block with a parameter, such as `{ |x| f 1 $x }`, to pass it as a value".into()],
            Self::AppliedUnforcedValue(_) => vec!["the type checker forces every function before it is applied, so this is a fault in ral rather than in your program: please report the source that produced it".into()],
            Self::CaptureNotBytes(_) => vec!["only the type checker writes this step, so this is a fault in ral rather than in your program: please report the source that produced it".into()],
            Self::HandleInSeed | Self::BoundaryWithoutSite(_) | Self::MissingEngineFactory => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::datum::Datum;
    use crate::fact::Reason;

    #[test]
    fn invariant_description_and_advice_survive_reason_round_trip() {
        for (failure, description, advice) in [
            (
                InvariantFailure::BareLambda,
                "tried to evaluate a bare lambda in computation position: a function value must be applied to arguments or held as a thunk",
                "either call this function with all its arguments, or hold it in a block with a parameter, such as `{ |x| f 1 $x }`, to pass it as a value",
            ),
            (
                InvariantFailure::AppliedUnforcedValue(ValueShape::Function),
                "the evaluator applied a function without forcing it",
                "the type checker forces every function before it is applied, so this is a fault in ral rather than in your program: please report the source that produced it",
            ),
            (
                InvariantFailure::CaptureNotBytes(ValueShape::List),
                "the value boundary was handed a list where a captured byte payload belongs",
                "only the type checker writes this step, so this is a fault in ral rather than in your program: please report the source that produced it",
            ),
        ] {
            assert_eq!(failure.describe(), description);
            assert_eq!(failure.advice(), [advice]);
            let reason = Reason::Invariant(Box::new(failure));
            assert_eq!(reason.advice(), [advice]);
            let decoded = Reason::decode(&reason.clone().encode()).unwrap();
            assert_eq!(decoded, reason);
            assert_eq!(decoded.describe(), description);
            assert_eq!(decoded.advice(), [advice]);
        }
        for failure in [
            InvariantFailure::HandleInSeed,
            InvariantFailure::BoundaryWithoutSite("x".into()),
            InvariantFailure::MissingEngineFactory,
        ] {
            assert_eq!(failure.advice(), [] as [String; 0]);
        }
    }
}
