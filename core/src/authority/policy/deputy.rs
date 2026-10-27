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
use super::{Baseline, FsOp, GrantStack, Meet, region};
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
pub fn deputy_prefixes(stack: &GrantStack, baseline: &Baseline) -> Vec<FrozenPath> {
    if stack.exec_is_top() {
        return Vec::new();
    }
    let runs = stack.exec().map(runnable).reduce(Meet::meet);
    let writes = region(stack, &FsOp::Write, baseline);
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
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::deputy_prefixes;
    use crate::authority::policy::{
        Baseline, Capabilities, ExecGrant, ExecKey, FsPolicy, GrantStack,
    };
    use crate::path::{FrozenPath, shown, utf8};
    use crate::test_helper::{Tree, layer, stack};

    /// A tempdir, whose real path the `/`-rooted names of a test lie under:
    /// what a grant names need not exist, the walk ends where the disk does.
    struct Disk {
        _dir: tempfile::TempDir,
        tree: Tree,
    }

    impl Disk {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let tree = Tree::lay(dir.path(), std::iter::empty::<&str>());
            Self { _dir: dir, tree }
        }

        /// The spelling of `/`-rooted `p`; `/` itself is the universal prefix.
        fn lit(&self, p: &str) -> FrozenPath {
            if p == "/" {
                FrozenPath::root()
            } else {
                FrozenPath::from_surface(&self.tree.at(p))
            }
        }

        /// The surfaces a finding of `paths` reports, as shown.
        fn named(&self, paths: &[&str]) -> Vec<String> {
            paths.iter().map(|p| shown(self.tree.at(p))).collect()
        }

        fn exec_dirs(&self, allow: &[&str], deny: &[&str]) -> ExecGrant {
            let keys = |dirs: &[&str]| {
                dirs.iter()
                    .map(|d| ExecKey::Dir(self.lit(d)))
                    .collect::<Vec<_>>()
            };
            ExecGrant::of(keys(allow), keys(deny))
        }

        /// The grant excluding `deny`, with `every` set.
        fn every(&self, deny: &[&str]) -> ExecGrant {
            self.exec_dirs(&[], deny).with_every()
        }

        fn fs(&self, write: &[&str], deny: &[&str]) -> FsPolicy {
            FsPolicy {
                write_prefixes: write.iter().map(|p| self.lit(p)).collect(),
                deny_paths: deny.iter().map(|p| self.lit(p)).collect(),
                ..FsPolicy::default()
            }
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
        (deputy_prefixes(stack, &Baseline::default()).iter())
            .map(FrozenPath::shown)
            .collect()
    }

    #[test]
    fn symlinked_write_region_is_reported_via_resolved_form() {
        // `data` is a link: lexically disjoint from `bin`, resolved inside it.
        let disk = Disk::new();
        let root = disk.tree.root();
        std::fs::create_dir_all(root.join("bin/sub")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("bin/sub"), root.join("data")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(root.join("bin/sub"), root.join("data")).unwrap();
        let spelled = |name: &str| FrozenPath::from_surface(utf8(&root.join(name)).unwrap());
        let data = spelled("data");
        let stack = stack([layer(Capabilities {
            exec: Some(ExecGrant::of([ExecKey::Dir(spelled("bin"))], [])),
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
        let disk = Disk::new();
        let stack = stack([layer(Capabilities {
            exec: Some(disk.exec_dirs(&["/x/bin"], &[])),
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
        let disk = Disk::new();
        let layer_a = layer(Capabilities {
            exec: Some(disk.exec_dirs(&["/x/bin"], &[])),
            ..Capabilities::default()
        });
        let layer_b = layer(Capabilities {
            fs: Some(disk.fs(&["/x/bin"], &[])),
            ..Capabilities::default()
        });
        assert!(
            report(&stack([layer_a.clone()])).is_empty()
                && report(&stack([layer_b.clone()])).is_empty(),
            "neither layer alone names both dimensions"
        );
        assert_eq!(report(&stack([layer_a, layer_b])), disk.named(&["/x/bin"]));
    }

    #[test]
    fn compile_and_run_shape_fires_benignly() {
        let disk = Disk::new();
        let found = found(
            disk.exec_dirs(&["/work/target/debug"], &[]),
            disk.fs(&["/work"], &[]),
        );
        assert_eq!(found, disk.named(&["/work/target/debug"]));
    }

    #[test]
    fn a_wholly_exec_excluded_directory_is_not_found() {
        let disk = Disk::new();
        let exec = disk.exec_dirs(&["/work"], &["/work"]);
        assert_eq!(found(exec, disk.fs(&["/work"], &[])), Vec::<String>::new());
    }

    #[test]
    fn a_wholly_fs_excluded_directory_is_not_found() {
        let disk = Disk::new();
        let exec = disk.exec_dirs(&["/work"], &[]);
        assert_eq!(
            found(exec, disk.fs(&["/work"], &["/work"])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_write_region_wholly_inside_an_exec_exclusion_is_not_found() {
        let disk = Disk::new();
        let exec = disk.exec_dirs(&["/"], &["/work"]);
        assert_eq!(
            found(exec, disk.fs(&["/work/a"], &[])),
            Vec::<String>::new()
        );
    }

    #[test]
    fn two_writable_islands_under_one_runnable_directory_are_both_found() {
        let disk = Disk::new();
        let found = found(
            disk.exec_dirs(&["/work"], &[]),
            disk.fs(&["/work/a", "/work/b"], &[]),
        );
        assert_eq!(found, disk.named(&["/work/a", "/work/b"]));
    }

    #[test]
    fn an_every_layer_does_not_erase_the_findings_of_another() {
        let disk = Disk::new();
        let stack = stack([
            layer(Capabilities {
                exec: Some(disk.every(&[])),
                ..Capabilities::default()
            }),
            layer(Capabilities {
                exec: Some(disk.exec_dirs(&["/work"], &[])),
                fs: Some(disk.fs(&["/work/a"], &[])),
                ..Capabilities::default()
            }),
        ]);
        assert_eq!(report(&stack), disk.named(&["/work/a"]));
    }

    #[test]
    fn an_every_layer_excluding_a_subdirectory_still_finds_the_write_region() {
        let disk = Disk::new();
        let found = found(disk.every(&["/work/private"]), disk.fs(&["/work"], &[]));
        assert_eq!(found, disk.named(&["/work"]));
    }

    #[test]
    fn an_authored_root_directory_finds_every_writable_prefix() {
        let disk = Disk::new();
        let found = found(disk.exec_dirs(&["/"], &[]), disk.fs(&["/a", "/b"], &[]));
        assert_eq!(found, disk.named(&["/a", "/b"]));
    }

    #[test]
    fn an_exec_that_denotes_top_has_no_finding() {
        let disk = Disk::new();
        assert_eq!(
            found(disk.every(&[]), disk.fs(&["/work"], &[])),
            Vec::<String>::new()
        );
    }
}
