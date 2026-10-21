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

use super::exec::{ExecAllow, ExecDeny, ExecRules};
use super::fs::{FsAllow, FsDeny, FsRules};
use super::lattice::GrantPath;
use super::{Floor, FsOp, GrantStack, Meet, region};
use crate::path::FrozenPath;

/// The prefixes that are both exec-admitted and fs-writable under `stack`,
/// the live part of the meet of the two tables, each folded as the guard
/// folds it, so neither side's exclusions are lost.
///
/// `None` from either side, no layer opining on fs writes or no exec that
/// restricts, yields no finding: unrestricted is not "everything writable" or
/// "everything runnable".  A directory wholly excluded on either side holds
/// Both and is dropped; each writable island under a runnable directory is
/// reported.
pub fn deputy_prefixes(stack: &GrantStack, floor: &Floor) -> Vec<FrozenPath> {
    if stack.exec_is_top() {
        return Vec::new();
    }
    let runs = stack.exec().map(runnable).reduce(Meet::meet);
    let writes = region(stack, &FsOp::Write, floor);
    let (Some(runs), Some(writes)) = (runs, writes) else {
        return Vec::new();
    };
    let both = runs.meet(writes);
    both.live()
        .filter_map(|scope| match scope {
            FsAllow::Prefix(prefix) => Some(prefix.written.clone()),
            FsAllow::RunPlace(_) => None,
        })
        .collect()
}

/// One layer's runnable directories as an fs table: `every` and a directory
/// that holds everywhere are everywhere, a directory is its position, and
/// an excluded directory carries its holds.
fn runnable(exec: &ExecRules) -> FsRules {
    let allows = exec.allows().filter_map(|allow| match allow {
        ExecAllow::Any => Some(GrantPath::everywhere()),
        ExecAllow::Dir(dir) => Some(dir.clone()),
        ExecAllow::File { .. } | ExecAllow::Tool(_) => None,
    });
    let denies = exec.denies().filter_map(|deny| match deny {
        ExecDeny::Dir(dir) => Some(FsDeny(dir.clone())),
        ExecDeny::File(_) | ExecDeny::Name(_) => None,
    });
    FsRules::new(allows.map(FsAllow::Prefix), denies)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] a tempdir tree with a real link; fixture paths already in normal form"
)]
mod tests {
    use super::deputy_prefixes;
    use crate::capability::{
        Capabilities, Evidence, ExecGrant, ExecKey, Floor, FsPolicy, GrantStack,
    };
    use crate::path::{FrozenPath, RealPath, shown, test_absolute, utf8};
    use crate::test_helper::{layer, stack};

    fn lit(p: &str) -> FrozenPath {
        FrozenPath::from_surface(&test_absolute(p))
    }

    /// The surfaces a finding of `paths` reports, on this host, as shown.
    fn named(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| shown(test_absolute(p))).collect()
    }

    fn exec_dirs(dirs: &[(&str, Evidence)]) -> ExecGrant {
        (dirs.iter())
            .map(|&(d, e)| (ExecKey::Dir(lit(d)), e))
            .collect()
    }

    /// The grant of `dirs` with `every` set.
    fn every(dirs: &[(&str, Evidence)]) -> ExecGrant {
        exec_dirs(dirs).with_every()
    }

    fn fs(write: &[&str], deny: &[&str]) -> FsPolicy {
        FsPolicy {
            write_prefixes: write.iter().map(|p| lit(p)).collect(),
            deny_paths: deny.iter().map(|p| lit(p)).collect(),
            ..FsPolicy::default()
        }
    }

    fn found(exec: ExecGrant, fs: FsPolicy) -> Vec<String> {
        report(&stack([layer(Capabilities {
            exec: Some(exec),
            fs: Some(fs),
            ..Capabilities::default()
        })]))
    }

    /// The finding as the audit shows it.
    fn report(stack: &GrantStack) -> Vec<String> {
        (deputy_prefixes(stack, &Floor::default()).iter())
            .map(FrozenPath::shown)
            .collect()
    }

    const ALLOW: Evidence = Evidence::ALLOW;
    const DENY: Evidence = Evidence::DENY;

    #[test]
    fn symlinked_write_region_is_reported_via_resolved_form() {
        // `data` is a link: lexically disjoint from `bin`, resolved inside it.
        let tmp = tempfile::tempdir().unwrap();
        let root = RealPath::of_path(tmp.path()).unwrap();
        let root = root.as_path();
        std::fs::create_dir_all(root.join("bin/sub")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("bin/sub"), root.join("data")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(root.join("bin/sub"), root.join("data")).unwrap();
        let spelled = |name: &str| FrozenPath::from_surface(utf8(&root.join(name)).unwrap());
        let data = spelled("data");
        let stack = stack([layer(Capabilities {
            exec: Some(std::iter::once((ExecKey::Dir(spelled("bin")), ALLOW)).collect()),
            fs: Some(FsPolicy {
                write_prefixes: vec![data.clone()],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        })]);
        assert_eq!(report(&stack), [data.shown()]);
    }

    #[test]
    fn fs_none_is_invisible_even_with_an_exec_dir() {
        let stack = stack([layer(Capabilities {
            exec: Some(exec_dirs(&[("/x/bin", ALLOW)])),
            fs: None,
            ..Capabilities::default()
        })]);
        assert!(
            report(&stack).is_empty(),
            "an unrestricted fs dimension must not be read as \"everything writable\""
        );
    }

    #[test]
    fn two_innocent_layers_fold_into_a_finding() {
        let layer_a = layer(Capabilities {
            exec: Some(exec_dirs(&[("/x/bin", ALLOW)])),
            ..Capabilities::default()
        });
        let layer_b = layer(Capabilities {
            fs: Some(fs(&["/x/bin"], &[])),
            ..Capabilities::default()
        });
        assert!(
            report(&stack([layer_a.clone()])).is_empty()
                && report(&stack([layer_b.clone()])).is_empty(),
            "neither layer alone names both dimensions"
        );
        assert_eq!(report(&stack([layer_a, layer_b])), named(&["/x/bin"]));
    }

    #[test]
    fn compile_and_run_shape_fires_benignly() {
        let found = found(
            exec_dirs(&[("/work/target/debug", ALLOW)]),
            fs(&["/work"], &[]),
        );
        assert_eq!(found, named(&["/work/target/debug"]));
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
        assert_eq!(found, named(&["/work/a", "/work/b"]));
    }

    #[test]
    fn an_every_layer_does_not_erase_the_findings_of_another() {
        let stack = stack([
            layer(Capabilities {
                exec: Some(every(&[])),
                ..Capabilities::default()
            }),
            layer(Capabilities {
                exec: Some(exec_dirs(&[("/work", ALLOW)])),
                fs: Some(fs(&["/work/a"], &[])),
                ..Capabilities::default()
            }),
        ]);
        assert_eq!(report(&stack), named(&["/work/a"]));
    }

    #[test]
    fn an_every_layer_excluding_a_subdirectory_still_finds_the_write_region() {
        let found = found(every(&[("/work/private", DENY)]), fs(&["/work"], &[]));
        assert_eq!(found, named(&["/work"]));
    }

    #[test]
    fn an_authored_root_directory_finds_every_writable_prefix() {
        let found = found(exec_dirs(&[("/", ALLOW)]), fs(&["/a", "/b"], &[]));
        assert_eq!(found, named(&["/a", "/b"]));
    }

    #[test]
    fn an_exec_that_denotes_top_has_no_finding() {
        assert_eq!(found(every(&[]), fs(&["/work"], &[])), Vec::<String>::new());
    }
}
