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

use super::fs::{FsScope, Region};
use super::{Evidence, ExecGrant, ExecKey, FsOp, GrantStack, Meet, region};
use crate::path::{FrozenPath, Resolver};
use std::collections::BTreeSet;

/// The prefixes that are both exec-admitted and fs-writable under `stack`,
/// the live part of the meet of the two regions, each folded as the guard
/// folds it, so neither side's exclusions are lost.
///
/// `None` from either side, no layer opining on fs writes or no exec that
/// restricts, yields no finding: unrestricted is not "everything writable" or
/// "everything runnable".  A directory wholly excluded on either side holds
/// Both and is dropped; each writable island under a runnable directory is
/// reported.
pub fn deputy_prefixes(stack: &GrantStack, resolver: &Resolver) -> Vec<FrozenPath> {
    if stack.exec_is_top() {
        return Vec::new();
    }
    let runs = stack
        .exec()
        .map(|exec| runnable(exec, resolver))
        .reduce(Meet::meet);
    let writes = region(stack, resolver, &FsOp::Write);
    let (Some(runs), Some(writes)) = (runs, writes) else {
        return Vec::new();
    };
    let both = runs.meet(writes);
    let found: BTreeSet<&FsScope> = both.live().collect();
    found
        .into_iter()
        .filter_map(|scope| match scope {
            FsScope::Prefix(prefix) => Some(prefix.clone()),
            FsScope::Place(_) => None,
        })
        .collect()
}

/// One layer's runnable directories as a region; `every` is the root.  The
/// positive half of an entry stays as frozen and the negative half is
/// re-resolved, as `fs::region` treats its own.
fn runnable(exec: &ExecGrant, resolver: &Resolver) -> Region {
    let root = exec
        .every()
        .then(|| (FsScope::Prefix(FrozenPath::root()), Evidence::ALLOW));
    let dirs = exec.iter().filter_map(|(key, e)| match key {
        ExecKey::Dir(dir) => Some((dir, e)),
        _ => None,
    });
    let halves = dirs.flat_map(|(dir, e)| {
        [
            (e.positive).then(|| (FsScope::Prefix(dir.clone()), Evidence::ALLOW)),
            (e.negative).then(|| (FsScope::Prefix(dir.refreeze(resolver)), Evidence::DENY)),
        ]
    });
    root.into_iter().chain(halves.flatten()).collect()
}

#[cfg(test)]
mod tests {
    use super::deputy_prefixes;
    use crate::capability::{
        Axis, Capabilities, Evidence, ExecGrant, ExecKey, FsPolicy, GrantStack,
    };
    use crate::path::{FrozenPath, Resolver};

    fn lit(p: &str) -> FrozenPath {
        FrozenPath::for_test(p, p)
    }

    fn exec_dirs(dirs: &[(&str, Evidence)]) -> ExecGrant {
        (dirs.iter())
            .map(|&(d, e)| (ExecKey::Dir(lit(d)), e))
            .collect()
    }

    /// The grant of `dirs` with `every` set, as `join_top` makes it, its one
    /// name entry only there to make it non-empty.
    fn every(dirs: &[(&str, Evidence)]) -> ExecGrant {
        let bait = (ExecKey::Name("x".into()), Evidence::ALLOW);
        std::iter::once(bait)
            .chain(dirs.iter().map(|&(d, e)| (ExecKey::Dir(lit(d)), e)))
            .collect::<ExecGrant>()
            .join_top()
            .expect("non-empty")
    }

    fn fs(write: &[&str], deny: &[&str]) -> FsPolicy {
        FsPolicy {
            write_prefixes: write.iter().map(|p| lit(p)).collect(),
            deny_paths: deny.iter().map(|p| lit(p)).collect(),
            ..FsPolicy::default()
        }
    }

    fn found(exec: ExecGrant, fs: FsPolicy) -> Vec<String> {
        let stack = GrantStack::of(Capabilities {
            exec: Some(exec),
            fs: Some(fs),
            ..Capabilities::default()
        });
        report(&stack)
    }

    fn report(stack: &GrantStack) -> Vec<String> {
        (deputy_prefixes(stack, &Resolver::shell_less()).iter())
            .map(|p| p.as_str().to_string())
            .collect()
    }

    const ALLOW: Evidence = Evidence::ALLOW;
    const DENY: Evidence = Evidence::DENY;

    #[test]
    fn symlinked_write_region_is_reported_via_resolved_form() {
        // `/data` is a symlink: lexically disjoint from `/usr/bin`, resolved inside it.
        let stack = GrantStack::of(Capabilities {
            exec: Some(exec_dirs(&[("/usr/bin", ALLOW)])),
            fs: Some(FsPolicy {
                write_prefixes: vec![FrozenPath::for_test("/data", "/usr/bin/sub")],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        });
        assert_eq!(report(&stack), ["/data"]);
    }

    #[test]
    fn fs_none_is_invisible_even_with_an_exec_dir() {
        let stack = GrantStack::of(Capabilities {
            exec: Some(exec_dirs(&[("/usr/bin", ALLOW)])),
            fs: None,
            ..Capabilities::default()
        });
        assert!(
            report(&stack).is_empty(),
            "an unrestricted fs dimension must not be read as \"everything writable\""
        );
    }

    #[test]
    fn two_innocent_layers_fold_into_a_finding() {
        let layer_a = Capabilities {
            exec: Some(exec_dirs(&[("/usr/bin", ALLOW)])),
            ..Capabilities::default()
        };
        let layer_b = Capabilities {
            fs: Some(fs(&["/usr/bin"], &[])),
            ..Capabilities::default()
        };
        assert!(
            report(&GrantStack::of(layer_a.clone())).is_empty()
                && report(&GrantStack::of(layer_b.clone())).is_empty(),
            "neither layer alone names both dimensions"
        );
        let mut stack = GrantStack::of(layer_a);
        stack.push(layer_b);
        assert_eq!(report(&stack), ["/usr/bin"]);
    }

    #[test]
    fn compile_and_run_shape_fires_benignly() {
        let found = found(
            exec_dirs(&[("/work/target/debug", ALLOW)]),
            fs(&["/work"], &[]),
        );
        assert_eq!(found, ["/work/target/debug"]);
    }

    #[test]
    fn a_wholly_exec_excluded_directory_is_not_found() {
        let exec = exec_dirs(&[("/work", ALLOW), ("/work", DENY)]);
        assert_eq!(found(exec, fs(&["/work"], &[])), Vec::<String>::new());
    }

    #[test]
    fn a_wholly_fs_excluded_directory_is_not_found() {
        let exec = exec_dirs(&[("/work", ALLOW)]);
        assert_eq!(
            found(exec, fs(&["/work"], &["/work"])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_write_region_wholly_inside_an_exec_exclusion_is_not_found() {
        let exec = exec_dirs(&[("/", ALLOW), ("/work", DENY)]);
        assert_eq!(found(exec, fs(&["/work/a"], &[])), Vec::<String>::new());
    }

    #[test]
    fn two_writable_islands_under_one_runnable_directory_are_both_found() {
        let found = found(
            exec_dirs(&[("/work", ALLOW)]),
            fs(&["/work/a", "/work/b"], &[]),
        );
        assert_eq!(found, ["/work/a", "/work/b"]);
    }

    #[test]
    fn an_every_layer_does_not_erase_the_findings_of_another() {
        let mut stack = GrantStack::of(Capabilities {
            exec: Some(every(&[])),
            ..Capabilities::default()
        });
        stack.push(Capabilities {
            exec: Some(exec_dirs(&[("/work", ALLOW)])),
            fs: Some(fs(&["/work/a"], &[])),
            ..Capabilities::default()
        });
        assert_eq!(report(&stack), ["/work/a"]);
    }

    #[test]
    fn an_every_layer_excluding_a_subdirectory_still_finds_the_write_region() {
        let found = found(every(&[("/work/private", DENY)]), fs(&["/work"], &[]));
        assert_eq!(found, ["/work"]);
    }

    #[test]
    fn an_authored_root_directory_finds_every_writable_prefix() {
        let found = found(exec_dirs(&[("/", ALLOW)]), fs(&["/a", "/b"], &[]));
        assert_eq!(found, ["/a", "/b"]);
    }

    #[test]
    fn an_exec_that_denotes_top_has_no_finding() {
        assert_eq!(found(every(&[]), fs(&["/work"], &[])), Vec::<String>::new());
    }
}
