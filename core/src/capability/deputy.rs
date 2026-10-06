//! The confused-deputy verdict: the prefixes a grant makes both
//! `exec`-admitted and `fs`-writable, where a binary dropped now is
//! admitted on the next call.
//!
//! Within one projection that is no escalation — the dropped binary is
//! spawned under the same confinement as the process that wrote it — and
//! `cargo build && ./target/debug/app` requires the shape, so this
//! reports and never denies.  What a finding locates is where a write
//! becomes runnable; only a runner outside the projection turns that into
//! an escape.  Takes the stack: an exec-granting base and a write-granting
//! overlay are each innocent alone, and only the stack's meet-fold of both
//! dimensions is a deputy.

use super::fs::Region;
use super::table::Scope;
use crate::path::{Allow, FrozenPath};
use crate::types::{ExecKey, GrantStack, Meet, Verdict};
use std::collections::BTreeSet;

/// The exec-admitted directory prefixes that are also writable under `stack`.
///
/// Each dimension folds as a [`Region`] of allows across every opining
/// layer; `None` — no layer opined — means unrestricted, not "everything
/// writable", so it yields no finding.  Directory prefixes never partly
/// overlap, so containment either way fires, and the narrower prefix —
/// the region that is both — is what's reported.
pub fn deputy_prefixes(stack: &GrantStack) -> Vec<FrozenPath> {
    let admits = |prefix: &FrozenPath| (prefix.clone(), Verdict::Allow);
    let dirs = (stack.exec())
        .map(|exec| {
            (exec.0.iter())
                .filter_map(|(key, v)| match key {
                    ExecKey::Dir(dir) if !v.is_denied() => Some(admits(dir)),
                    _ => None,
                })
                .collect::<Region>()
        })
        .reduce(Meet::meet);
    let writes = (stack.fs())
        .map(|fs| fs.write_prefixes.iter().map(admits).collect::<Region>())
        .reduce(Meet::meet);
    let (Some(dirs), Some(writes)) = (dirs, writes) else {
        return Vec::new();
    };
    let covers = |a: &FrozenPath, b: &FrozenPath| a.holds::<Allow>(b.own());
    let found: BTreeSet<&FrozenPath> = (dirs.live())
        .filter_map(|dir| {
            (writes.live())
                .find(|w| covers(w, dir) || covers(dir, w))
                .map(|w| if covers(w, dir) { dir } else { w })
        })
        .collect();
    found.into_iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::deputy_prefixes;
    use crate::path::FrozenPath;
    use crate::types::{Capabilities, ExecGrant, ExecKey, FsPolicy, GrantStack, Verdict};
    use std::collections::BTreeMap;

    fn exec_dir(dir: FrozenPath) -> ExecGrant {
        ExecGrant(BTreeMap::from([(ExecKey::Dir(dir), Verdict::Allow)]))
    }

    fn fs_write(prefix: &str) -> FsPolicy {
        FsPolicy {
            write_prefixes: vec![FrozenPath::from_surface(prefix)],
            ..FsPolicy::default()
        }
    }

    #[test]
    fn symlinked_write_region_is_reported_via_resolved_form() {
        // `/data` is a symlink: lexically disjoint from `/usr/bin`, resolved inside it.
        let stack = GrantStack::of(Capabilities {
            exec: Some(exec_dir(FrozenPath::for_test("/usr/bin", "/usr/bin"))),
            fs: Some(FsPolicy {
                write_prefixes: vec![FrozenPath::for_test("/data", "/usr/bin/sub")],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        });
        assert_eq!(
            deputy_prefixes(&stack),
            vec![FrozenPath::for_test("/data", "/usr/bin/sub")]
        );
    }

    #[test]
    fn fs_none_is_invisible_even_with_an_exec_dir() {
        let stack = GrantStack::of(Capabilities {
            exec: Some(exec_dir(FrozenPath::from_surface("/usr/bin"))),
            fs: None,
            ..Capabilities::default()
        });
        assert!(
            deputy_prefixes(&stack).is_empty(),
            "an unrestricted fs dimension must not be read as \"everything writable\""
        );
    }

    #[test]
    fn two_innocent_layers_fold_into_a_finding() {
        let layer_a = Capabilities {
            exec: Some(exec_dir(FrozenPath::from_surface("/usr/bin"))),
            ..Capabilities::default()
        };
        let layer_b = Capabilities {
            fs: Some(fs_write("/usr/bin")),
            ..Capabilities::default()
        };
        assert!(
            deputy_prefixes(&GrantStack::of(layer_a.clone())).is_empty()
                && deputy_prefixes(&GrantStack::of(layer_b.clone())).is_empty(),
            "neither layer alone names both dimensions"
        );
        let mut stack = GrantStack::of(layer_a);
        stack.push(layer_b);
        assert_eq!(
            deputy_prefixes(&stack),
            vec![FrozenPath::from_surface("/usr/bin")]
        );
    }

    #[test]
    fn compile_and_run_shape_fires_benignly() {
        let stack = GrantStack::of(Capabilities {
            exec: Some(exec_dir(FrozenPath::from_surface("/work/target/debug"))),
            fs: Some(fs_write("/work")),
            ..Capabilities::default()
        });
        assert_eq!(
            deputy_prefixes(&stack),
            vec![FrozenPath::from_surface("/work/target/debug")]
        );
    }
}
