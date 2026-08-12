//! The exec boundary's refused set: the shapes `execve(2)` has no argument
//! for, and the idiom that lowers each.
//!
//! Rendering an argv *inside* the shell is total — [`Value::render_argv`] gives
//! every value a text form, so `echo [a: 1]` prints a map and a handler arm
//! receives one as a word.  An operating-system argument is narrower: it is one
//! word, and the values below have no single word to give.
//!
//! Stated once, and read from both sides of that boundary.  Shape is exactly
//! what a type states, so the checker maps an argument's *type* into this set
//! before the spawn (the argv rule in `typecheck::infer`) and
//! `runtime::command::vet` maps the *value* at the spawn.  The two matches
//! below are wildcard-free on purpose: a new `Value` or `Ty` constructor has to
//! be given a verdict on both sides at once, so the static gate and its runtime
//! backstop cannot drift into disagreeing about the same argument.

use super::Value;
use crate::typecheck::Ty;

/// A shape the exec boundary refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusedArg {
    /// Several arguments in the costume of one.
    List,
    /// A map or a record: fields, rather than a word.
    Map,
    /// A block, a lambda, or a partly applied native — a computation that has
    /// not run.
    Block,
    /// A concurrent block, possibly still running.
    Handle,
    /// Bytes are a channel, not a word.
    Bytes,
}

impl RefusedArg {
    /// The refusal a value earns at the spawn, or `None` when it renders.
    pub(crate) fn of_value(value: &Value) -> Option<Self> {
        match value {
            Value::List(_) => Some(Self::List),
            Value::Map(_) => Some(Self::Map),
            Value::Lambda { .. } | Value::Block { .. } | Value::Native { .. } => Some(Self::Block),
            Value::Handle(_) => Some(Self::Handle),
            Value::Bytes(_) => Some(Self::Bytes),
            Value::Unit
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::String(_)
            | Value::Variant { .. } => None,
        }
    }

    /// The same refusal read off a type, so the checker can raise it before the
    /// spawn.  `ty` must be resolved: a variable is not yet a shape, and says
    /// nothing rather than guessing at one.
    pub(crate) fn of_ty(ty: &Ty) -> Option<Self> {
        match ty {
            Ty::List(_) => Some(Self::List),
            // A record is a map at run time, and is refused as the map it is.
            Ty::Map(_) | Ty::Record(_) => Some(Self::Map),
            Ty::Thunk(_) => Some(Self::Block),
            Ty::Handle(_) => Some(Self::Handle),
            Ty::Bytes => Some(Self::Bytes),
            Ty::Unit
            | Ty::Bool
            | Ty::Int
            | Ty::Float
            | Ty::String
            | Ty::Variant(_)
            | Ty::Var(_) => None,
        }
    }

    /// How to lower this shape into arguments `cmd` can receive.  One sentence
    /// per shape, wherever the refusal was raised, so a user who meets the
    /// static error and the pre-spawn one meets one language.
    pub(crate) fn remedy(self, cmd: &str) -> String {
        match self {
            Self::List => format!("use '...' to spread a list into arguments: {cmd} ...$xs"),
            Self::Map => format!(
                "a map is fields rather than one word — pass a field, as in \
                 `{cmd} $m[name]`, or render the whole of it with `{cmd} !{{to-json $m}}`"
            ),
            Self::Block => format!(
                "a block is a computation, not a word — run it and pass what it \
                 gives, as in `{cmd} !{{!$b}}`"
            ),
            Self::Handle => format!(
                "await the concurrent block first (`let r = await $h`), then pass a \
                 field of the result, as in `{cmd} $r[value]`"
            ),
            Self::Bytes => {
                "pipe binary data via stdin with to-bytes, or decode to string first".to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RefusedArg, Ty};
    use crate::types::Value;

    /// The two readings of one boundary agree — the property the shared set
    /// exists to hold.  Every pair a test can name without an `Env`; the three
    /// `Ty::Thunk` stands for and `Handle` need one, and are paired end to end
    /// instead (`core/tests/typecheck.rs`, `core/tests/argv_convention.rs`).
    #[test]
    fn a_value_and_its_type_earn_the_same_verdict() {
        let pairs: [(Value, Ty); 9] = [
            (Value::Unit, Ty::Unit),
            (Value::Bool(true), Ty::Bool),
            (Value::Int(1), Ty::Int),
            (Value::Float(1.0), Ty::Float),
            (Value::String("x".into()), Ty::String),
            (Value::Bytes(vec![1]), Ty::Bytes),
            (Value::List(vec![].into()), Ty::List(Box::new(Ty::String))),
            (Value::map(vec![]), Ty::Map(Box::new(Ty::Int))),
            // A tagged value renders, so neither side refuses it.
            (
                Value::Variant {
                    label: "ok".into(),
                    payload: None,
                },
                Ty::Variant(crate::typecheck::Row::Empty),
            ),
        ];
        for (value, ty) in pairs {
            assert_eq!(
                RefusedArg::of_value(&value),
                RefusedArg::of_ty(&ty),
                "{value:?} and {ty:?} must earn the same verdict"
            );
        }
    }

    /// A record is a map at run time, so it is refused as one.
    #[test]
    fn a_record_is_refused_as_the_map_it_is() {
        assert_eq!(
            RefusedArg::of_ty(&Ty::Record(crate::typecheck::Row::Empty)),
            Some(RefusedArg::Map)
        );
    }

    /// A variable is not a shape: the runtime keeps that question.
    #[test]
    fn a_variable_says_nothing() {
        assert_eq!(
            RefusedArg::of_ty(&Ty::Var(crate::typecheck::TyVar(0))),
            None
        );
    }
}
