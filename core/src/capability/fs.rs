//! The fs dimension: a [`Region`] per op, as [`super::exec`] holds the exec
//! dimension.
//!
//! Both readers of fs authority consume it: the in-process guard
//! (`crate::guard`) judges a path by the region, the OS projection
//! (`crate::sandbox::SandboxProjection`) renders its live allows and its
//! denies.  Agreement
//! between guard and sandbox profile is then structural — one fold per op,
//! two consumers — rather than a property two independent folds have to be
//! tested into.
//!
//! A restricted read region is the layers' meet joined with the [`floor`]:
//! the platform's fixed read-only system paths any program needs to start.
//! No layer's `read` narrows it; only a `deny` removes a floor path.
//!
//! Prefixes are re-frozen against the caller's [`Resolver`] here rather
//! than read off the frozen policy, so the caller decides how fresh the
//! answer is: composition is a statement about the policy, these about the
//! world.  The guard folds afresh on every check; the projection folds once,
//! at spawn, because that is when the OS profile is written.

use super::table::{Scope, Table};
use super::{FsPolicy, GrantStack, Meet, Verdict};
use crate::path::{FrozenPath, Polarity, Resolver};
use std::path::Path;
use std::sync::LazyLock;
use strum::IntoStaticStr;

/// Which fs region a check consults: the read or the write prefix set.
#[derive(IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum FsOp {
    Read,
    Write,
}

impl FsOp {
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

/// The read-only system paths admitted under every restricted `read`, frozen
/// once per process: they are root-owned, and the guard refolds on every check.
/// Those absent on this host are dropped.
pub(crate) fn floor() -> &'static [FrozenPath] {
    static FLOOR: LazyLock<Vec<FrozenPath>> = LazyLock::new(|| {
        #[cfg(windows)]
        let listed = ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)"]
            .map(|k| std::env::var(k).unwrap_or_default());
        #[cfg(target_os = "linux")]
        let listed = [
            "/bin",
            "/usr",
            "/lib",
            "/lib64",
            // `/dev`, `/proc` and `/sys/fs/cgroup` are absent: the envelope
            // mounts its own.  `/etc` and `/sys` wholesale are excluded: only what dynamic
            // linking, name resolution, user lookup, toolchain resolution and
            // hardware sizing need.  The rest of `/sys` describes the host,
            // not the envelope: `class/net` is the mounter's netns whatever
            // `--unshare-net` did, `class/dmi` and `bus` name the machine.  A
            // grant that wants it reads `/sys` by name.
            "/sys/devices/system/cpu",
            "/sys/kernel/mm/transparent_hugepage",
            "/etc/ld.so.conf",
            "/etc/ld.so.conf.d",
            "/etc/ld.so.cache",
            "/etc/resolv.conf",
            "/etc/nsswitch.conf",
            "/etc/hosts",
            "/etc/ssl",
            "/etc/ca-certificates",
            "/etc/pki",
            // getpwuid/getgrgid sit in libc startup paths: without these many
            // programs cannot even resolve HOME.
            "/etc/passwd",
            "/etc/group",
            // Debian/Ubuntu toolchain symlinks (cc → gcc-13, etc.).
            "/etc/alternatives",
            // Linuxbrew: a system prefix that happens to live under /home.
            "/home/linuxbrew/.linuxbrew",
        ];
        #[cfg(target_os = "macos")]
        let listed = [
            "/bin",
            "/usr",
            // Rosetta's runtime, the loader analogue: no grant should name it.
            "/Library/Apple/usr",
            "/Library/Developer/CommandLineTools",
            "/Applications/Xcode.app/Contents/Developer",
            "/opt/homebrew",
            "/lib",
            "/System",
            "/dev",
            "/private/var/db/dyld",
            // Wholesale: tools read whatever they read (gitconfig, paths.d,
            // zshenv, nix.conf, …); nothing user-secret sits here unprotected.
            "/private/etc",
            // xcode-select state; denied, build drivers report a broken install.
            "/private/var/select",
            // resolv.conf's target and the mDNSResponder socket.
            "/private/var/run",
        ];
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        let listed: [&str; 0] = [];
        (listed.iter())
            .filter(|p| crate::path::exists(p))
            .map(FrozenPath::from_surface)
            .collect()
    });
    &FLOOR
}

/// The stack's region for `op`: each opining layer's prefixes for `op` and
/// its denies, re-frozen, met; for `Read`, joined with the [`floor`], which
/// only a deny removes.  `None` exactly when no layer held an `fs` opinion,
/// so the guard is unrestricted and the projection needs no fs rules; a layer
/// that opined and admitted nothing denies (beyond the floor).
pub(crate) fn region(grants: &GrantStack, resolver: &Resolver, op: &FsOp) -> Option<Region> {
    let mut met = grants
        .fs()
        .map(|fs| {
            let allows = op.prefixes(fs).iter().map(|p| (p, Verdict::Allow));
            let denies = fs.deny_paths.iter().map(|p| (p, Verdict::Deny));
            (allows.chain(denies))
                .map(|(p, v)| (p.refreeze(resolver), v))
                .collect::<Region>()
        })
        .reduce(Meet::meet)?;
    if matches!(op, FsOp::Read) {
        met.extend(floor().iter().map(|p| (p.clone(), Verdict::Allow)));
    }
    Some(met)
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

    /// The region for `op` of one layer allowing `allow` for both ops.
    fn region_of(op: &FsOp, allow: &[FrozenPath], deny: &[FrozenPath]) -> Region {
        let stack = GrantStack::of(crate::capability::Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: allow.to_vec(),
                write_prefixes: allow.to_vec(),
                deny_paths: deny.to_vec(),
            }),
            ..Default::default()
        });
        region(&stack, &Resolver::shell_less(), op).expect("fs opined")
    }

    #[test]
    fn the_read_region_admits_the_floor_a_layer_never_named() {
        let read = region_of(&FsOp::Read, &[lit("/work")], &[]);
        for f in floor() {
            assert_eq!(read.verdict(f.real_path()), Verdict::Allow, "{f:?}");
        }
    }

    #[test]
    fn a_deny_refuses_a_floor_path() {
        let f = floor().first().expect("a host has a floor");
        let read = region_of(&FsOp::Read, &[lit("/work")], std::slice::from_ref(f));
        assert_eq!(read.verdict(f.real_path()), Verdict::Deny);
    }

    #[test]
    fn the_write_region_admits_no_floor_path() {
        let write = region_of(&FsOp::Write, &[lit("/work")], &[]);
        for f in floor() {
            assert_eq!(write.verdict(f.real_path()), Verdict::Deny, "{f:?}");
        }
    }

    #[test]
    fn an_empty_read_admits_exactly_the_floor() {
        let read = region_of(&FsOp::Read, &[], &[]);
        assert!(
            floor()
                .iter()
                .all(|f| read.verdict(f.real_path()) == Verdict::Allow)
        );
        assert_eq!(read.verdict(Path::new("/no/such/place")), Verdict::Deny);
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
