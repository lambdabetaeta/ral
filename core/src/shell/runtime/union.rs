//! ρ seen together with Σ's prelude.

use super::signature::Signature;
use crate::ir::{Binding, Env, ValClos};
use crate::ty::Closed;

impl Env {
    /// ρ then Σ's prelude, projecting each name on first sight.
    pub(crate) fn fold_union<T>(
        &self,
        sig: &Signature,
        project: impl Fn(&Binding) -> T,
    ) -> Vec<(String, T)> {
        let mut seen = std::collections::HashSet::new();
        let mut result = Vec::with_capacity(self.len());
        for (k, b) in self.iter() {
            seen.insert(k.as_ref());
            result.push((k.to_string(), project(b)));
        }
        for name in sig.prelude_names() {
            if !seen.contains(name)
                && let Some(b) = sig.prelude_binding(name)
            {
                result.push((name.to_string(), project(b)));
            }
        }
        result
    }
    pub(crate) fn largest_shallow_size(&self, sig: &Signature) -> usize {
        let session = self.iter().map(|(_, b)| b.value.shallow_size());
        let prelude = sig
            .prelude_names()
            .filter(|name| self.binding(name).is_none())
            .filter_map(|name| sig.prelude_binding(name))
            .map(|b| b.value.shallow_size());
        session.chain(prelude).max().unwrap_or(0)
    }
    pub(crate) fn all_bindings(&self, sig: &Signature) -> Vec<(String, ValClos)> {
        self.fold_union(sig, |b| b.value.clone())
    }
    pub(crate) fn distinct_name_count(&self, sig: &Signature) -> usize {
        let mut seen: std::collections::HashSet<&str> = self.names().map(AsRef::as_ref).collect();
        seen.extend(sig.prelude_names());
        seen.len()
    }
    pub(crate) fn binding_schemes(&self, sig: &Signature) -> Vec<(String, Option<Closed>)> {
        self.fold_union(sig, |b| b.scheme.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::super::signature::lookup;
    use super::*;
    use std::sync::Arc;

    fn binding(value: ValClos) -> Binding {
        Binding {
            value,
            scheme: None,
        }
    }
    #[test]
    fn shadowed_prelude_name_round_trips_then_unset_reveals_it() {
        let mut sig = Signature::default();
        sig.install_prelude(Arc::new(
            std::iter::once(("map".to_string(), binding(ValClos::string("prelude-map")))).collect(),
        ));
        let mut env = Env::EMPTY;
        assert_eq!(
            lookup("map", &env, &sig),
            Some(&ValClos::string("prelude-map"))
        );
        env.bind("map".into(), binding(ValClos::int(3)));
        assert_eq!(lookup("map", &env, &sig), Some(&ValClos::int(3)));
        env.unset("map");
        assert_eq!(
            lookup("map", &env, &sig),
            Some(&ValClos::string("prelude-map"))
        );
    }
}
