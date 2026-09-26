//! A thunk value, `⟨M, ρ|occ(M)⟩`: a computation over just the bindings it
//! mentions, built only by [`Closure::new`].

use std::sync::Arc;

use crate::ir::{Comp, referenced_names};

use super::env::Env;

/// A thunk value: `⟨M, ρ⟩` with `ρ` scrubbed of every name `M` does not
/// mention.  The fields are private and `new` is the only constructor, so
/// every thunk value is scrubbed.
#[derive(Debug, Clone)]
pub struct Closure {
    comp: Arc<Comp>,
    env: Env,
}

impl Closure {
    pub fn new(comp: Arc<Comp>, env: &Env) -> Self {
        let env = env.restrict(referenced_names(&comp));
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
        let closure = Closure::new(comp, &env);
        assert!(closure.env().session_binding("a").is_some());
        assert!(closure.env().session_binding("b").is_none());

        let again = Closure::new(Arc::clone(closure.comp()), closure.env());
        assert!(
            again
                .env()
                .bindings_root()
                .ptr_eq(closure.env().bindings_root()),
            "a scrubbed closure rescrubs to itself"
        );
    }
}
