//! The exec boundary's refused set: the shapes `execve(2)` has no argument
//! for, and the idiom that lowers each.
//!
//! Rendering an argv *inside* the shell is total but for `()`: every value has
//! a text form, so `echo [a: 1]` prints a map and a handler arm receives one as
//! a word.  An operating-system argument is narrower: it is one word, and the
//! shapes below have no single word to give.
//!
//! Stated once, as a verdict on a [`Head`], and read from both sides of that
//! boundary.  The checker maps an argument's *type* into this set before the
//! spawn (the argv rule in `typecheck::infer`) and `runtime::command::vet` maps
//! the *value* at the spawn, through `Value::heads`.  The one match is
//! wildcard-free on purpose: a new head has to be given a verdict, so the
//! static gate and its runtime backstop cannot disagree about one argument.

use super::{Head, Ty};

/// A shape the exec boundary refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusedArg {
    /// Nothing: there is no word to pass, and no text to print.
    Unit,
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
    /// The refusal a shape earns at the spawn, or `None` when it renders.  The
    /// one classification: a type's head and a value's both come through it.
    pub(crate) fn of_head(head: Head) -> Option<Self> {
        match head {
            Head::Unit => Some(Self::Unit),
            Head::List => Some(Self::List),
            // A record is a map at run time, and is refused as the map it is.
            Head::Map | Head::Record => Some(Self::Map),
            Head::Thunk => Some(Self::Block),
            Head::Handle => Some(Self::Handle),
            Head::Bytes => Some(Self::Bytes),
            Head::Bool | Head::Int | Head::Float | Head::String | Head::Variant => None,
        }
    }

    /// The same refusal read off a type, so the checker can raise it before the
    /// spawn.  `ty` must be resolved: a variable is not yet a shape, and says
    /// nothing rather than guessing at one.
    pub(crate) fn of_ty(ty: &Ty) -> Option<Self> {
        Head::of(ty).and_then(Self::of_head)
    }

    /// How to lower this shape into arguments `cmd` can receive.  One sentence
    /// per shape, wherever the refusal was raised, so a user who meets the
    /// static error and the pre-spawn one meets one language.
    pub(crate) fn remedy(self, cmd: &str) -> String {
        match self {
            Self::Unit => {
                "`()` is nothing, not a word: if the text is meant, write `'()'`".to_string()
            }
            Self::List => format!("use '...' to spread a list into arguments: {cmd} ...$xs"),
            Self::Map => format!(
                "a map is fields rather than one word: pass a field, as in \
                 `{cmd} $m[name]`, or render the whole of it with `{cmd} !{{to-json $m}}`"
            ),
            Self::Block => format!(
                "a block is a computation, not a word: run it and pass what it \
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
