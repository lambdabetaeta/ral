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

use crate::path::{NormalizedPrefix, covers, meet_prefixes};
use crate::types::GrantStack;

/// The exec-admitted directory prefixes that are also writable under `stack`.
///
/// Each dimension folds by [`meet_prefixes`] across every opining layer;
/// `None` — no layer opined — means unrestricted, not "everything
/// writable", so it yields no finding.  Directory prefixes never partly
/// overlap, so containment either way fires, and the narrower prefix —
/// the region that is both — is what's reported.
pub fn deputy_prefixes(stack: &GrantStack) -> Vec<NormalizedPrefix> {
    let allow_dirs = stack.exec().fold(None, |acc: Option<Vec<_>>, exec| {
        let dirs: Vec<NormalizedPrefix> = exec.allow_dirs.iter().cloned().collect();
        Some(match acc {
            Some(prev) => meet_prefixes(&prev, &dirs),
            None => dirs,
        })
    });
    let write_prefixes = stack.fs().fold(None, |acc: Option<Vec<_>>, fs| {
        Some(match acc {
            Some(prev) => meet_prefixes(&prev, &fs.write_prefixes),
            None => fs.write_prefixes.clone(),
        })
    });
    let (Some(allow_dirs), Some(write_prefixes)) = (allow_dirs, write_prefixes) else {
        return Vec::new();
    };
    let mut found: Vec<NormalizedPrefix> = allow_dirs
        .iter()
        .filter_map(|dir| {
            write_prefixes
                .iter()
                .find(|w| covers(w, dir) || covers(dir, w))
                .map(|w| {
                    if covers(w, dir) {
                        dir.clone()
                    } else {
                        w.clone()
                    }
                })
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::deputy_prefixes;
    use crate::path::{Namespace, NormalizedPrefix};
    use crate::types::{Capabilities, ExecMap, FsPolicy, GrantStack};
    use std::collections::BTreeSet;

    fn exec_dir(dir: &str) -> ExecMap {
        ExecMap {
            allow_dirs: BTreeSet::from([NormalizedPrefix::from_surface(dir)]),
            ..ExecMap::default()
        }
    }

    fn fs_write(prefix: &str) -> FsPolicy {
        FsPolicy {
            write_prefixes: vec![NormalizedPrefix::from_surface(prefix)],
            ..FsPolicy::default()
        }
    }

    #[test]
    fn symlinked_write_region_is_reported_via_resolved_form() {
        // `/data` is a symlink: lexically disjoint from `/usr/bin`, resolved inside it.
        let stack = GrantStack::of(Capabilities {
            exec: Some(ExecMap {
                allow_dirs: BTreeSet::from([NormalizedPrefix::for_test(
                    "/usr/bin",
                    "/usr/bin",
                    Namespace::Host,
                )]),
                ..ExecMap::default()
            }),
            fs: Some(FsPolicy {
                write_prefixes: vec![NormalizedPrefix::for_test(
                    "/data",
                    "/usr/bin/sub",
                    Namespace::Host,
                )],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        });
        assert_eq!(
            deputy_prefixes(&stack),
            vec![NormalizedPrefix::for_test(
                "/data",
                "/usr/bin/sub",
                Namespace::Host
            )]
        );
    }

    #[test]
    fn cross_namespace_overlap_is_not_reported() {
        let stack = GrantStack::of(Capabilities {
            exec: Some(ExecMap {
                allow_dirs: BTreeSet::from([NormalizedPrefix::for_test(
                    "/usr/bin",
                    "/usr/bin",
                    Namespace::Host,
                )]),
                ..ExecMap::default()
            }),
            fs: Some(FsPolicy {
                write_prefixes: vec![NormalizedPrefix::for_test(
                    "/usr/bin",
                    "/usr/bin",
                    Namespace::Guest,
                )],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        });
        assert!(
            deputy_prefixes(&stack).is_empty(),
            "a shared spelling across namespaces names different machines, not an overlap"
        );
    }

    #[test]
    fn fs_none_is_invisible_even_with_an_exec_dir() {
        let stack = GrantStack::of(Capabilities {
            exec: Some(exec_dir("/usr/bin")),
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
            exec: Some(exec_dir("/usr/bin")),
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
            vec![NormalizedPrefix::from_surface("/usr/bin")]
        );
    }

    #[test]
    fn compile_and_run_shape_fires_benignly() {
        let stack = GrantStack::of(Capabilities {
            exec: Some(exec_dir("/work/target/debug")),
            fs: Some(fs_write("/work")),
            ..Capabilities::default()
        });
        assert_eq!(
            deputy_prefixes(&stack),
            vec![NormalizedPrefix::from_surface("/work/target/debug")]
        );
    }
}
