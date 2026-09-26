//! A thunk value, `⟨M, ρ|occ(M)⟩`: a computation over just the bindings it
//! mentions, built only by [`Closure::new`].

use std::sync::Arc;

use crate::ir::{Comp, Occ};

use super::env::Env;

/// A thunk value: `⟨M, ρ⟩` with `ρ` scrubbed of every name `M` does not
/// mention.  The fields are private, so every thunk value is scrubbed by one
/// of the constructors below.
#[derive(Debug, Clone)]
pub struct Closure {
    comp: Arc<Comp>,
    env: Env,
}

const _: () = assert!(
    std::mem::size_of::<Closure>() <= 24,
    "Closure must fit in 24 bytes"
);

impl Closure {
    /// Restricts `env` to `occ`, the occ of the node that owns `comp` — a
    /// `ThunkNode`'s for `force`, a `GroupNode`'s for the `Rec` rule.
    pub(crate) fn new(comp: Arc<Comp>, occ: &Occ, env: &Env) -> Self {
        Self {
            comp,
            env: env.restrict(occ),
        }
    }

    /// A closure over a closed body: the one constructor for hosts building
    /// a hook with no capture of its own.
    pub fn closed(comp: Comp) -> Self {
        Self {
            comp: Arc::new(comp),
            env: Env::new(),
        }
    }

    /// Over an `env` already restricted to what `comp` mentions.
    pub(crate) fn captured(comp: Arc<Comp>, env: Env) -> Self {
        Self { comp, env }
    }

    pub fn comp(&self) -> &Arc<Comp> {
        &self.comp
    }

    pub fn env(&self) -> &Env {
        &self.env
    }

    /// Both halves, leaving `self.env` empty so `Drop` below has nothing to dismantle.
    pub(crate) fn into_parts(mut self) -> (Arc<Comp>, Env) {
        let comp = Arc::clone(&self.comp);
        let env = std::mem::take(&mut self.env);
        (comp, env)
    }
}

impl Drop for Closure {
    /// Cuts the stream chain; see [`Env::dismantle`].
    fn drop(&mut self) {
        self.env.dismantle();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{CompKind, Val};
    use crate::source::Spanned;
    use crate::types::{Binding, Value};

    /// What the decode, the scrub and `Rec`'s siblings rely on: rebuilding a
    /// closure from its own halves keeps the root it was handed.
    #[test]
    fn a_closure_is_scrubbed_and_its_scrub_is_fixed() {
        let mut env = Env::new();
        for name in ["a", "b"] {
            env.bind(
                name.into(),
                Binding {
                    value: Value::Unit,
                    scheme: None,
                },
            );
        }
        let comp = Arc::new(Spanned::synthetic(CompKind::Return(Val::Variable(
            "a".into(),
        ))));
        let node = crate::ir::ThunkNode::new(Arc::clone(&comp));
        let closure = Closure::new(comp, node.occ(), &env);
        assert!(closure.env().binding("a").is_some());
        assert!(closure.env().binding("b").is_none());

        let again = Closure::new(Arc::clone(closure.comp()), node.occ(), closure.env());
        assert!(
            again.env().ptr_eq(closure.env()),
            "a scrubbed closure rescrubs to itself"
        );
    }
}
