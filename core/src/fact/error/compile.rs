//! Portable compile failure facts.

use super::shared::Count;
use crate::text::plural;
use crate::{record, variant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileFailure {
    pub source: String,
    pub phase: CompilePhase,
}
record!(CompileFailure {
    source: "source",
    phase: "phase"
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeErrorCount {
    pub count: Count,
}
record!(TypeErrorCount { count: "count" });

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompilePhase {
    Parse,
    Types(TypeErrorCount),
    Use,
}
variant!(CompilePhase { Parse: "parse", Types(TypeErrorCount): "types", Use: "use" });

impl CompileFailure {
    pub fn describe(&self) -> String {
        match &self.phase {
            CompilePhase::Parse => format!("loaded source {:?} did not parse", self.source),
            CompilePhase::Types(errors) => format!(
                "loaded source {:?} failed typechecking ({})",
                self.source,
                plural(errors.count.to_u64(), "error")
            ),
            CompilePhase::Use => format!("loaded source {:?} could not link a module", self.source),
        }
    }
}

impl TypeErrorCount {
    pub fn describe(&self) -> String {
        format!("count: {}", self.count)
    }
}

impl CompilePhase {
    pub fn describe(&self) -> String {
        match self {
            Self::Parse => "parse".to_owned(),
            Self::Types(value) => format!("types: {}", value.describe()),
            Self::Use => "use".to_owned(),
        }
    }
}
