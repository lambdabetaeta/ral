//! The confused-deputy verdict: a prefix that is both `exec`-admitted
//! and `fs`-writable is an escape hatch — drop a binary there and the
//! next call admits it.
//!
//! It reports; it does not deny — `cargo build && ./target/debug/app`
//! *is* this shape, so a finding names a property worth surfacing, not
//! a policy to reject.  Callers must fold first: an exec-granting base
//! and a write-granting overlay are each innocent alone, and only their
//! meet is a deputy.

use crate::path::{NormalizedPrefix, covers};
use crate::types::Capabilities;

/// The exec-admitted directory prefixes that are also writable under the
/// folded `caps`.
///
/// `None` on either dimension means unrestricted, not "everything
/// writable", so it yields no finding.  Directory prefixes never partly
/// overlap, so containment either way fires, and the narrower prefix —
/// the region that is both — is what's reported.
pub fn deputy_prefixes(caps: &Capabilities) -> Vec<NormalizedPrefix> {
    let (Some(exec), Some(fs)) = (&caps.exec, &caps.fs) else {
        return Vec::new();
    };
    let mut found: Vec<NormalizedPrefix> = exec
        .allow_dirs
        .iter()
        .filter_map(|dir| {
            fs.write_prefixes
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
    use crate::types::{Capabilities, ExecMap, FsPolicy};
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
        let caps = Capabilities {
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
        };
        assert_eq!(
            deputy_prefixes(&caps),
            vec![NormalizedPrefix::for_test(
                "/data",
                "/usr/bin/sub",
                Namespace::Host
            )]
        );
    }

    #[test]
    fn cross_namespace_overlap_is_not_reported() {
        let caps = Capabilities {
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
        };
        assert!(
            deputy_prefixes(&caps).is_empty(),
            "a shared spelling across namespaces names different machines, not an overlap"
        );
    }

    #[test]
    fn fs_none_is_invisible_even_with_an_exec_dir() {
        let caps = Capabilities {
            exec: Some(exec_dir("/usr/bin")),
            fs: None,
            ..Capabilities::default()
        };
        assert!(
            deputy_prefixes(&caps).is_empty(),
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
            deputy_prefixes(&layer_a).is_empty() && deputy_prefixes(&layer_b).is_empty(),
            "neither layer alone names both dimensions"
        );
        let folded = layer_a.meet(layer_b);
        assert_eq!(
            deputy_prefixes(&folded),
            vec![NormalizedPrefix::from_surface("/usr/bin")]
        );
    }

    #[test]
    fn compile_and_run_shape_fires_benignly() {
        let caps = Capabilities {
            exec: Some(exec_dir("/work/target/debug")),
            fs: Some(fs_write("/work")),
            ..Capabilities::default()
        };
        assert_eq!(
            deputy_prefixes(&caps),
            vec![NormalizedPrefix::from_surface("/work/target/debug")]
        );
    }
}
