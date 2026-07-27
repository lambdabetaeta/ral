//! The confused-deputy verdict: a pure predicate over a folded
//! [`Capabilities`] frame.
//!
//! `design/grant.md`'s third concession names the shape in prose: a
//! prefix that is both `exec`-admitted and `fs`-writable is an escape
//! hatch — drop a binary there, the next call admits it.
//! [`deputy_prefixes`] turns that into a theorem, judged with
//! [`covers`](crate::path::covers) — the same resolved-form predicate
//! [`meet_prefixes`](crate::path::meet_prefixes) folds over — so
//! overlap survives a symlinked write region and never fires across
//! namespaces.
//!
//! **It reports; it does not deny.** Write-`cwd:` plus exec under
//! `cwd:` is the compile-and-run workflow every agent profile needs
//! (`cargo build && ./target/debug/app` *is* this shape), so a finding
//! marks a property worth surfacing, not a policy to reject.
//!
//! **Judged on the folded frame, not a layer.** A base grants exec
//! under `/usr/bin`, an overlay grants write under `/usr/bin`, and
//! neither layer alone is a deputy — only their meet is. So this takes
//! a single already-folded `Capabilities`, not the stack.
//!
//! **The residue is stated, not hidden.** The predicate can only fire
//! where **both** dimensions are restricted: a frame with `fs: None`
//! leaves every exec-admitted prefix writable and undetectable, so
//! `None` on either `exec` or `fs` yields no finding, deliberately. A
//! symlink created *after* the fold, pointing into an exec-admitted
//! tree, stays open under the same stability hypothesis as TOCTOU
//! (`design/grant.md`'s second concession).

use crate::path::{NormalizedPrefix, covers};
use crate::types::Capabilities;

/// The exec-admitted directory prefixes that are also writable under the
/// folded `caps` — the confused-deputy escape hatch.
///
/// Empty whenever `caps` carries no finding, including whenever `exec`
/// or `fs` is `None` (see the module doc's static residue).
///
/// Two directory prefixes never partially overlap: one contains the
/// other, or they are disjoint. So containment in either direction is
/// the escape-hatch condition, and the contained (narrower) prefix — the
/// region that is simultaneously writable and exec-admitted — is what's
/// reported.
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
        // /data lexically diverges from /usr/bin, but resolves inside it —
        // the drop-a-binary escape the module doc names.
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
    fn empty_capabilities_has_no_finding() {
        assert!(deputy_prefixes(&Capabilities::default()).is_empty());
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
        // `write: cwd:` plus exec under `cwd:` — the workflow every
        // agent profile needs, and exactly the shape the concession
        // warns about. The verdict fires; nothing here denies it.
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
