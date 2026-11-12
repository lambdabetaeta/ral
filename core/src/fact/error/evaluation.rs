//! Portable evaluation failure facts.

use super::shared::{Count, CountRule, Found, ValueShape};
use crate::data::Finite;
use crate::text::plural;
use crate::{label, record, variant};
use strum::{IntoStaticStr, VariantArray};

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum KeyOrField {
    Key,
    Field,
}
label!(KeyOrField);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingEntry {
    pub kind: KeyOrField,
    pub key: String,
    pub available: Vec<String>,
}
record!(MissingEntry {
    kind: "kind",
    key: "key",
    available: "available"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexBounds {
    pub index: Count,
    pub length: Count,
}
record!(IndexBounds {
    index: "index",
    length: "length"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Field(String),
    Key(String),
    Position(Count),
}
variant!(Access { Field(String): "field", Key(String): "key", Position(Count): "position" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unreadable {
    pub access: Access,
    pub found: Found,
}
record!(Unreadable {
    access: "access",
    found: "found"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum CallOrForce {
    Call,
    Force,
}
label!(CallOrForce);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyFailure {
    pub operation: CallOrForce,
    pub found: Found,
}
record!(ApplyFailure {
    operation: "operation",
    found: "found"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternLength {
    pub expected: CountRule,
    pub got: Count,
}
record!(PatternLength {
    expected: "expected",
    got: "got"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseArmFailure {
    pub tag: String,
    pub handled: Vec<String>,
}
record!(CaseArmFailure {
    tag: "tag",
    handled: "handled"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum EqualityOrOrder {
    Equality,
    Order,
}
label!(EqualityOrOrder);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareFailure {
    pub operation: String,
    pub relation: EqualityOrOrder,
    pub left: ValueShape,
    pub right: ValueShape,
}
record!(CompareFailure {
    operation: "operation",
    relation: "relation",
    left: "left",
    right: "right"
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum DivOrMod {
    Div,
    Mod,
}
label!(DivOrMod);

#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr, VariantArray)]
#[strum(serialize_all = "kebab-case")]
pub enum BinaryArithmeticOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Modulo,
}
label!(BinaryArithmeticOp);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegerBinaryOverflow {
    pub operator: BinaryArithmeticOp,
    pub left: i64,
    pub right: i64,
}
record!(IntegerBinaryOverflow {
    operator: "operator",
    left: "left",
    right: "right"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloatBinaryOverflow {
    pub operator: BinaryArithmeticOp,
    pub left: Finite,
    pub right: Finite,
}
record!(FloatBinaryOverflow {
    operator: "operator",
    left: "left",
    right: "right"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegerOverflow {
    Negate(i64),
    Binary(IntegerBinaryOverflow),
}
variant!(IntegerOverflow { Negate(i64): "negate", Binary(IntegerBinaryOverflow): "binary" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloatOverflow {
    Negate(Finite),
    Binary(FloatBinaryOverflow),
}
variant!(FloatOverflow { Negate(Finite): "negate", Binary(FloatBinaryOverflow): "binary" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundUnrepresentable {
    pub value: Finite,
    pub places: i64,
}
record!(RoundUnrepresentable {
    value: "value",
    places: "places"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArithmeticFailure {
    ZeroDivisor(DivOrMod),
    IntegerOverflow(IntegerOverflow),
    FloatOverflow(FloatOverflow),
    ModuloNeedsIntegers,
    RoundUnrepresentable(RoundUnrepresentable),
}
variant!(ArithmeticFailure { ZeroDivisor(DivOrMod): "zero-divisor", IntegerOverflow(IntegerOverflow): "integer-overflow", FloatOverflow(FloatOverflow): "float-overflow", ModuloNeedsIntegers: "modulo-needs-integers", RoundUnrepresentable(RoundUnrepresentable): "round-unrepresentable" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalFailure {
    WithheldHandle(String),
    MissingBinding(String),
    MissingEntry(MissingEntry),
    IndexBounds(IndexBounds),
    Unreadable(Unreadable),
    DuplicateMapKey(String),
    Apply(ApplyFailure),
    Interpolate(ValueShape),
    PatternLength(PatternLength),
    CaseArm(CaseArmFailure),
    Arithmetic(ArithmeticFailure),
    Compare(CompareFailure),
}
variant!(EvalFailure { WithheldHandle(String): "withheld-handle", MissingBinding(String): "missing-binding", MissingEntry(MissingEntry): "missing-entry", IndexBounds(IndexBounds): "index-bounds", Unreadable(Unreadable): "unreadable", DuplicateMapKey(String): "duplicate-map-key", Apply(ApplyFailure): "apply", Interpolate(ValueShape): "interpolate", PatternLength(PatternLength): "pattern-length", CaseArm(CaseArmFailure): "case-arm", Arithmetic(ArithmeticFailure): "arithmetic", Compare(CompareFailure): "compare" });

impl KeyOrField {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl MissingEntry {
    pub fn describe(&self) -> String {
        format!("{} {:?} not found", self.kind.describe(), self.key)
    }
}

impl IndexBounds {
    pub fn describe(&self) -> String {
        format!(
            "index {} out of bounds for list of length {}",
            self.index, self.length
        )
    }
}

impl Access {
    pub fn describe(&self) -> String {
        match self {
            Self::Field(value) => format!("the field {value:?}"),
            Self::Key(value) => format!("the key {value:?}"),
            Self::Position(value) => format!("the position {value}"),
        }
    }
}

impl Unreadable {
    pub fn describe(&self) -> String {
        format!(
            "cannot read {} from {}",
            self.access.describe(),
            self.found.describe()
        )
    }
}

impl CallOrForce {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl ApplyFailure {
    pub fn describe(&self) -> String {
        match self.operation {
            CallOrForce::Call => format!("cannot call {}", self.found.describe()),
            CallOrForce::Force => {
                format!("cannot force {}: ! requires a Block", self.found.describe())
            }
        }
    }
}

impl PatternLength {
    pub fn describe(&self) -> String {
        match self.expected {
            CountRule::Exactly(n) => {
                format!("need {}, got {}", plural(n.to_u64(), "value"), self.got)
            }
            CountRule::AtLeast(n) => format!(
                "need at least {}, got {}",
                plural(n.to_u64(), "value"),
                self.got
            ),
        }
    }
}

impl CaseArmFailure {
    pub fn describe(&self) -> String {
        format!(
            "no case arm handles `{}`; handled: {}",
            self.tag,
            self.handled.join(", ")
        )
    }
}

impl EqualityOrOrder {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl CompareFailure {
    pub fn describe(&self) -> String {
        format!(
            "{}: cannot compare {} with {}",
            self.operation,
            self.left.describe(),
            self.right.describe()
        )
    }
}

impl DivOrMod {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl BinaryArithmeticOp {
    pub fn describe(&self) -> String {
        <&str>::from(*self).replace('-', " ")
    }
}

impl IntegerBinaryOverflow {
    pub fn describe(&self) -> String {
        format!(
            "integer {} overflow for {} and {}",
            self.operator.describe(),
            self.left,
            self.right
        )
    }
}

impl FloatBinaryOverflow {
    pub fn describe(&self) -> String {
        format!(
            "floating-point {} overflow for {} and {}",
            self.operator.describe(),
            self.left,
            self.right
        )
    }
}

impl IntegerOverflow {
    pub fn describe(&self) -> String {
        match self {
            Self::Negate(value) => format!("negating {value} overflows the integer range"),
            Self::Binary(value) => value.describe(),
        }
    }
}

impl FloatOverflow {
    pub fn describe(&self) -> String {
        match self {
            Self::Negate(value) => format!("negating {value} overflows the Float range"),
            Self::Binary(value) => value.describe(),
        }
    }
}

impl RoundUnrepresentable {
    pub fn describe(&self) -> String {
        format!(
            "rounding {} to {} places overflows while scaling; use fewer places",
            self.value, self.places
        )
    }
}

impl ArithmeticFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::ZeroDivisor(DivOrMod::Div) => "division by zero".into(),
            Self::ZeroDivisor(DivOrMod::Mod) => "modulo by zero".into(),
            Self::IntegerOverflow(cause) => cause.describe(),
            Self::FloatOverflow(cause) => cause.describe(),
            Self::ModuloNeedsIntegers => "% requires Int operands".into(),
            Self::RoundUnrepresentable(cause) => cause.describe(),
        }
    }
}

impl EvalFailure {
    pub fn describe(&self) -> String {
        match self {
            Self::WithheldHandle(title) => format!(
                "handle `{title}` was spawned by another shell and is withheld here; await it where it was spawned and pass its result instead"
            ),
            Self::MissingBinding(name) => format!("undefined variable: ${name}"),
            Self::MissingEntry(cause) => cause.describe(),
            Self::IndexBounds(cause) => cause.describe(),
            Self::Unreadable(cause) => cause.describe(),
            Self::DuplicateMapKey(key) => {
                format!("map literal: key {key:?} given twice; which did you mean?")
            }
            Self::Apply(cause) => cause.describe(),
            Self::Interpolate(shape) => {
                format!("cannot interpolate {} in string", shape.describe())
            }
            Self::PatternLength(cause) => cause.describe(),
            Self::CaseArm(cause) => cause.describe(),
            Self::Arithmetic(cause) => cause.describe(),
            Self::Compare(cause) => cause.describe(),
        }
    }
}

impl EvalFailure {
    pub(crate) const NO_STATUS_REGISTER: &str = "there is no status register: a failure raises an \
        error record: catch it with `try` and read `$e[status]` and `$e[message]` from \
        the handler's argument";

    pub fn advice(&self) -> Vec<String> {
        match self {
            Self::MissingBinding(name) if name == "STATUS" => vec![Self::NO_STATUS_REGISTER.into()],
            Self::MissingBinding(_) => {
                vec!["check spelling, or ensure the variable is defined before this line".into()]
            }
            Self::MissingEntry(cause) => vec![if cause.available.is_empty() {
                match cause.kind {
                    KeyOrField::Key => "the map is empty".into(),
                    KeyOrField::Field => "the record is empty".into(),
                }
            } else {
                format!(
                    "available: {}",
                    cause
                        .available
                        .iter()
                        .map(|key| format!("{key:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }],
            Self::IndexBounds(cause) => vec![match cause.length.to_u64() {
                0 => "the list is empty".into(),
                n => format!("valid indices: 0..{}", n - 1),
            }],
            Self::Unreadable(cause) => vec![
                match cause.access {
                    Access::Field(_) => "a field is read from a record",
                    Access::Key(_) => "a computed key reads a map",
                    Access::Position(_) => "a position is read from a list",
                }
                .into(),
            ],
            Self::Apply(cause) => vec![
                match cause.operation {
                    CallOrForce::Call => "only Blocks, Lambdas, and natives can be called",
                    CallOrForce::Force => "wrap in a block: !{ expr }",
                }
                .into(),
            ],
            Self::Interpolate(shape) => vec![
                match shape {
                    ValueShape::Unit => "`()` is nothing to print; to print the text, write '()'",
                    ValueShape::Bytes => {
                        "render with `str $b` (lossy UTF-8), or decode with `bytes-to-string $b`"
                    }
                    _ => "use str to convert, or index into the value",
                }
                .into(),
            ],
            Self::Compare(cause) => vec![
                match cause.relation {
                    EqualityOrOrder::Equality => {
                        "equality is defined on scalars, strings, bytes, lists, maps, and variants"
                    }
                    EqualityOrOrder::Order => "ordering is defined on numbers and strings",
                }
                .into(),
            ],
            Self::PatternLength(cause) => cause.advice(),
            Self::Arithmetic(ArithmeticFailure::ModuloNeedsIntegers) => {
                vec!["use int to convert".into()]
            }
            _ => Vec::new(),
        }
    }
}

impl PatternLength {
    pub fn advice(&self) -> Vec<String> {
        let got = self.got.to_u64();
        match self.expected {
            CountRule::Exactly(n) if got < n.to_u64() => {
                vec!["the list has too few elements for the named bindings; use fewer bindings or supply more elements".into()]
            }
            CountRule::Exactly(n) if got > n.to_u64() => {
                vec!["there are more elements; use [..., ...rest] to capture them".into()]
            }
            CountRule::AtLeast(n) if got < n.to_u64() => {
                vec!["the list has too few elements for the named bindings".into()]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::datum::Datum;

    fn projection(failure: &EvalFailure, description: &str, advice: &[&str]) {
        assert_eq!(failure.describe(), description);
        assert_eq!(failure.advice(), advice);
        let decoded = EvalFailure::decode(&failure.clone().encode()).unwrap();
        assert_eq!(&decoded, failure);
        assert_eq!(decoded.describe(), description);
        assert_eq!(decoded.advice(), advice);
    }

    #[test]
    fn withheld_handle_names_its_origin_and_recovery() {
        projection(
            &EvalFailure::WithheldHandle("worker".into()),
            "handle `worker` was spawned by another shell and is withheld here; await it where it was spawned and pass its result instead",
            &[],
        );
    }

    #[test]
    fn pattern_length_advice_only_explains_unsatisfied_counts() {
        for (expected, got, description, advice) in [
            (
                CountRule::Exactly(Count::from_u64(2)),
                1,
                "need 2 values, got 1",
                vec![
                    "the list has too few elements for the named bindings; use fewer bindings or supply more elements",
                ],
            ),
            (
                CountRule::Exactly(Count::from_u64(1)),
                2,
                "need 1 value, got 2",
                vec!["there are more elements; use [..., ...rest] to capture them"],
            ),
            (
                CountRule::Exactly(Count::from_u64(1)),
                1,
                "need 1 value, got 1",
                vec![],
            ),
            (
                CountRule::AtLeast(Count::from_u64(2)),
                1,
                "need at least 2 values, got 1",
                vec!["the list has too few elements for the named bindings"],
            ),
            (
                CountRule::AtLeast(Count::from_u64(1)),
                1,
                "need at least 1 value, got 1",
                vec![],
            ),
            (
                CountRule::AtLeast(Count::from_u64(0)),
                1,
                "need at least 0 values, got 1",
                vec![],
            ),
        ] {
            projection(
                &EvalFailure::PatternLength(PatternLength {
                    expected,
                    got: Count::from_u64(got),
                }),
                description,
                &advice,
            );
        }
    }
}
