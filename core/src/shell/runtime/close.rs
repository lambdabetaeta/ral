//! Formation of a value closure from syntax under Σ.

use super::Error;
use super::signature::{Signature, lookup};
use crate::fact::{EvalFailure, Reason};
use crate::ir::{Env, Val, ValClos};

impl ValClos {
    /// Close one layer, pulling Σ's answers into ρ for direct names only.
    pub(crate) fn close(val: &Val, env: &Env, sig: &Signature) -> Result<Self, Error> {
        if let Val::Variable(name) = val {
            return lookup(name, env, sig)
                .cloned()
                .ok_or_else(|| missing(name, env, sig));
        }
        let mut captured = match val.terminal() {
            Val::Thunk(node) => env.restrict(node.occ()),
            Val::List(node) => env.restrict(node.occ()),
            Val::Record(node) | Val::Map(node) => env.restrict(node.occ()),
            Val::Variable(_)
            | Val::Unit
            | Val::Bool(_)
            | Val::Int(_)
            | Val::Float(_)
            | Val::String(_)
            | Val::Bytes(_)
            | Val::Builtin(_)
            | Val::Handle(_) => Env::EMPTY,
            Val::Variant { .. } => unreachable!("the tag spine was peeled above"),
        };
        val.for_each_direct(&mut |name| {
            if captured.binding(name).is_none() {
                let binding = env
                    .binding(name)
                    .or_else(|| sig.binding(name))
                    .cloned()
                    .ok_or_else(|| missing(name, env, sig))?;
                captured.bind(name.clone(), binding);
            }
            Ok(())
        })?;
        Ok(Self::from_parts(val.clone(), captured))
    }
}

fn missing(name: &str, env: &Env, sig: &Signature) -> Error {
    let hint = match name {
        "STATUS" => None,
        _ => crate::typecheck::hyphen_boundary(name, |p| lookup(p, env, sig).is_some())
            .map(crate::typecheck::hyphen_boundary_hint),
    };
    let error = Error::new(Reason::Evaluation(Box::new(EvalFailure::MissingBinding(
        name.to_string(),
    ))));
    match hint {
        Some(hint) => error.with_hint(hint),
        None => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::terminal::{InteractiveMode, TerminalState};
    use crate::ir::{Binding, VClosRef, View};
    use crate::source::Spanned;
    use crate::ty::{Binders, Ty};
    use std::sync::Arc;

    fn names(names: &[&str]) -> Val {
        Val::list(
            names
                .iter()
                .map(|name| Spanned::synthetic(Val::Variable((*name).into()))),
        )
    }
    fn first(value: &ValClos) -> Option<ValClos> {
        let View::List(items) = value.view() else {
            panic!("a list")
        };
        items.get(0).map(VClosRef::into_owned)
    }
    #[test]
    fn terminal_records_match_their_closed_scheme_in_every_mode() {
        let scheme = ValClos::terminal_scheme();
        let site = crate::ty::Site::snapshot(
            &crate::typecheck::Unifier::new(),
            &scheme.ty,
            crate::ty::UnitId::fresh(),
        );
        for (mode, text) in [
            (InteractiveMode::Auto, "auto"),
            (InteractiveMode::Minimal, "minimal"),
            (InteractiveMode::Full, "full"),
        ] {
            let terminal = TerminalState {
                mode,
                ..TerminalState::default()
            };
            let value = ValClos::from(&terminal);
            site.admit(&value, &crate::ty::Fixings::default())
                .expect("TERMINAL matches its scheme");
            let View::Map(fields) = value.view() else {
                panic!("a record")
            };
            assert_eq!(
                fields.get("mode").map(VClosRef::into_owned),
                Some(ValClos::string(text))
            );
        }
    }
    #[test]
    fn a_literal_naming_a_signature_name_captures_its_value() {
        let mut sig = Signature::default();
        sig.install_prelude(Arc::new(
            std::iter::once((
                "k".into(),
                Binding {
                    value: ValClos::int(7),
                    scheme: None,
                },
            ))
            .collect(),
        ));
        let value = ValClos::close(&names(&["k"]), &Env::EMPTY, &sig).unwrap();
        assert_eq!(value.parts().1.get("k"), Some(&ValClos::int(7)));
        assert_eq!(first(&value), Some(ValClos::int(7)));
    }
    #[test]
    fn a_literal_captures_prelude_and_value_builtin_names_with_their_schemes() {
        let shell = crate::test_helper::core_shell();
        let mut sig = (*shell.sig).clone();
        let k_scheme = Binders::close(|_| Ty::int());
        sig.install_prelude(Arc::new(
            std::iter::once((
                "k".into(),
                Binding {
                    value: ValClos::int(7),
                    scheme: Some(k_scheme.clone()),
                },
            ))
            .collect(),
        ));
        let value = ValClos::close(&names(&["k", "has", "true"]), &Env::EMPTY, &sig).unwrap();
        let env = value.parts().1;
        let has = shell.lookup_builtin("has").unwrap();
        let scheme_of = |name| env.binding(name).unwrap().scheme.as_ref();
        assert_eq!(scheme_of("k"), Some(&k_scheme));
        assert_eq!(scheme_of("has"), Some(has.decl.scheme()));
        assert_eq!(scheme_of("true"), Some(&Binders::close(|_| Ty::bool())));
    }
    #[test]
    fn a_component_shares_the_whole_capture() {
        let mut env = Env::EMPTY;
        for (name, n) in [("a", 1), ("b", 2)] {
            env.bind(
                name.into(),
                Binding {
                    value: ValClos::int(n),
                    scheme: None,
                },
            );
        }
        let val = Val::list([
            Spanned::synthetic(names(&["a"])),
            Spanned::synthetic(Val::Variable("b".into())),
        ]);
        let value = ValClos::close(&val, &env, &Signature::default()).unwrap();
        let child = first(&value).unwrap();
        assert!(child.parts().1.ptr_eq(value.parts().1));
        assert_eq!(child.parts().1.get("b"), Some(&ValClos::int(2)));
    }
}
