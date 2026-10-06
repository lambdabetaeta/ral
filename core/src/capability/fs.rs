//! The fs dimension: a [`Region`] per op, as [`super::exec`] holds the exec
//! dimension.
//!
//! Both readers of fs authority consume it: the in-process guard in
//! [`super::enforce`] judges a path by the region, the OS projection in
//! [`super::sandbox`] renders its live allows and its denies.  Agreement
//! between guard and sandbox profile is then structural — one fold per op,
//! two consumers — rather than a property two independent folds have to be
//! tested into.
//!
//! Prefixes are re-frozen against the caller's [`Resolver`] here rather
//! than read off the frozen policy, so the caller decides how fresh the
//! answer is: composition is a statement about the policy, these about the
//! world.  The guard folds afresh on every check; the projection folds once,
//! at spawn, because that is when the OS profile is written.

use super::table::{Scope, Table};
use crate::path::{FrozenPath, Polarity, Resolver};
use crate::types::{FsPolicy, GrantStack, Meet, Verdict};
use std::path::Path;

/// Which fs region a check consults: the read or the write prefix set.
pub enum FsOp {
    Read,
    Write,
}

impl FsOp {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
        }
    }

    fn prefixes<'a>(&self, fs: &'a FsPolicy) -> &'a [FrozenPath] {
        match self {
            Self::Read => &fs.read_prefixes,
            Self::Write => &fs.write_prefixes,
        }
    }
}

/// Fs authority over one op: prefixes judged by their resolved form, a deny
/// outranking every allow.
pub(crate) type Region = Table<FrozenPath>;

impl Scope for FrozenPath {
    type Subject<'a> = &'a Path;
    type Rank = (bool, usize);

    /// A deny outranks every allow; the deeper prefix ranks above among each.
    fn rank(&self, verdict: &Verdict) -> (bool, usize) {
        (verdict.is_denied(), self.depth())
    }

    fn holds<P: Polarity>(&self, path: &Path) -> bool {
        self.contains::<P>(path)
    }

    fn own(&self) -> &Path {
        self.real_path()
    }
}

/// The stack's region for `op`: each opining layer's prefixes for `op` and
/// its denies, re-frozen, met.  `None` exactly when no layer held an `fs`
/// opinion, so the guard is unrestricted and the projection needs no fs
/// rules; a layer that opined and admitted nothing denies.
pub(super) fn region(grants: &GrantStack, resolver: &Resolver, op: &FsOp) -> Option<Region> {
    grants
        .fs()
        .map(|fs| {
            let allows = op.prefixes(fs).iter().map(|p| (p, Verdict::Allow));
            let denies = fs.deny_paths.iter().map(|p| (p, Verdict::Deny));
            (allows.chain(denies))
                .map(|(p, v)| (p.refreeze(resolver), v))
                .collect()
        })
        .reduce(Meet::meet)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test fixtures build the access-side path from a literal already in normal form"
)]
mod tests {
    use super::*;

    /// A divergent `surface`/`resolved` pair — what a symlink freezes to,
    /// without touching disk.
    fn p(surface: &str, resolved: &str) -> FrozenPath {
        FrozenPath::for_test(surface, resolved)
    }

    /// The ordinary, no-symlink case: both forms coincide.
    fn lit(s: &str) -> FrozenPath {
        p(s, s)
    }

    fn table(allows: &[FrozenPath], denies: &[FrozenPath]) -> Region {
        let allows = allows.iter().map(|p| (p.clone(), Verdict::Allow));
        allows
            .chain(denies.iter().map(|p| (p.clone(), Verdict::Deny)))
            .collect()
    }

    fn live(region: &Region) -> Vec<&str> {
        region.live().map(FrozenPath::as_str).collect()
    }

    #[test]
    fn the_meet_keeps_the_deeper_prefix_of_each_overlapping_pair() {
        let met = table(&[lit("/a"), lit("/b")], &[]).meet(table(&[lit("/a/x"), lit("/c")], &[]));
        assert_eq!(live(&met), ["/a/x"]);
    }

    /// Security regression.  A grant that lexically nests under a shallower
    /// ceiling but resolves outside it through a symlink must not survive
    /// the meet: the survivor would reach the OS sandbox, where `bwrap
    /// --bind` follows the source symlink and Seatbelt matches lexically, so
    /// a spawned child could read the link's target.
    #[test]
    fn a_symlinked_grant_cannot_escape_a_shallower_ceiling() {
        let met = table(&[lit("/a")], &[]).meet(table(&[p("/a/link", "/x")], &[]));
        assert!(live(&met).is_empty(), "{met:?}");
    }

    /// Positive control: the meet narrows, it does not blanket-deny.
    #[test]
    fn legitimate_nesting_survives_the_meet() {
        let met = table(&[lit("/a")], &[]).meet(table(&[lit("/a/sub")], &[]));
        assert_eq!(live(&met), ["/a/sub"]);
    }

    /// T3: the deny relation folds a spelling the allow relation keeps
    /// apart, and a refusal by the fold names the deny.
    #[test]
    fn a_deny_holds_every_spelling_and_an_allow_only_its_own() {
        let access = Path::new("/d/secrets/f");
        let region = table(&[lit("/")], &[lit("/d/Secrets")]);
        assert_eq!(region.verdict(access), Verdict::Deny);
        assert_eq!(region.verdict(Path::new("/d/other")), Verdict::Allow);
        if cfg!(not(windows)) {
            assert_eq!(region.respelled(access), Some(&lit("/d/Secrets")));
            assert_eq!(
                table(&[lit("/d/Secrets")], &[]).verdict(access),
                Verdict::Deny
            );
        }
    }

    /// T3: the projection drops an allow a deny holds in another spelling,
    /// and keeps one a narrower deny leaves standing.
    #[test]
    fn the_live_allows_are_those_no_deny_holds() {
        let region = table(
            &[lit("/d/secrets/f"), lit("/d/other"), lit("/e")],
            &[lit("/d/Secrets"), lit("/e/f")],
        );
        assert_eq!(live(&region), ["/d/other", "/e"]);
    }

    /// A prefix is judged by where it resolves, whatever it spells.
    #[test]
    fn a_prefix_holds_by_its_resolved_form_not_its_surface() {
        let file = Path::new("/a/file");
        assert_eq!(
            table(&[p("/a", "/elsewhere")], &[]).verdict(file),
            Verdict::Deny
        );
        assert_eq!(
            table(&[p("/link", "/a")], &[]).verdict(file),
            Verdict::Allow
        );
    }

    /// The refusal cites the deepest deny that holds the path only by a fold.
    #[test]
    fn the_deepest_folded_deny_is_named() {
        let region = table(&[lit("/")], &[lit("/D"), lit("/D/Secrets")]);
        if cfg!(not(windows)) {
            assert_eq!(
                region.respelled(Path::new("/d/secrets/f")),
                Some(&lit("/D/Secrets"))
            );
        }
    }

    /// The empty table is the fail-closed meet, so it must cover nothing.
    #[test]
    fn the_empty_table_covers_nothing() {
        assert_eq!(Region::default().verdict(Path::new("/a")), Verdict::Deny);
    }

    /// Regression: the ceiling survives a re-freeze without shrinking to a
    /// drive.  `Resolver::resolve` anchors a driveless path to a cwd, so the
    /// universal root once came back as the root of whichever drive the
    /// process ran from: a session launched from `D:` lost a `%TEMP%` on
    /// `C:`, the shape GitHub's Windows runners have.
    #[test]
    fn the_root_survives_a_re_freeze_as_the_universal_prefix() {
        let root = FrozenPath::root().refreeze(&Resolver::shell_less());
        assert_eq!(root.real_path(), Path::new("/"), "got {root:?}");
        let region = table(&[root], &[]);
        // A drive spelling is a path only on Windows.
        let mut paths = vec!["/etc/hosts"];
        if cfg!(windows) {
            paths.extend([r"C:\Users\someone\Temp\x", r"D:\a\repo\y"]);
        }
        for path in paths {
            assert_eq!(region.verdict(Path::new(path)), Verdict::Allow, "{path}");
        }
    }

    /// Gated because the case- and separator-folding branch of
    /// `path_within` fires only under a real `cfg!(windows)` build.
    #[cfg(windows)]
    #[test]
    fn the_meet_admits_a_windows_case_and_separator_variant() {
        let met = table(&[lit(r"C:\work")], &[]).meet(table(&[lit("c:/WORK/sub")], &[]));
        assert!(!live(&met).is_empty());
    }

    /// Same, through the `\\?\`-verbatim spelling `std::fs::canonicalize`
    /// returns on Windows.
    #[cfg(windows)]
    #[test]
    fn the_meet_admits_a_windows_verbatim_prefix_variant() {
        let met = table(&[lit(r"C:\work")], &[]).meet(table(&[lit(r"\\?\C:\work\sub")], &[]));
        assert!(!live(&met).is_empty());
    }
}
