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
//! No layer's `read` narrows it; only a `deny` removes a floor path.  Writing
//! reads, and running reads: each layer reads its own write prefixes and the
//! run places of the exec meet over it and the layers beneath it.
//!
//! Allowances are frozen, as decoded: only exclusions are re-resolved
//! against the live disk here, so a symlink put in an allowance's place
//! redirects nothing while an exclusion follows its name.  The guard folds
//! afresh on every check; the projection folds once, at spawn, because that is
//! when the OS profile is written.

use super::exec::ExecRules;
use super::table::{Scope, Table, Withheld};
use super::{Evidence, FsPolicy, GrantStack, Join, Meet};
use crate::guard::freeze::live_homebrew_roots;
use crate::path::{FrozenPath, Polarity, RealPath};
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
    /// What a layer allows for this op: writing reads, so a read includes the
    /// write prefixes.
    fn prefixes<'a>(&self, fs: &'a FsPolicy) -> impl Iterator<Item = &'a FrozenPath> {
        let reads: &[FrozenPath] = match self {
            Self::Read => &fs.read_prefixes,
            Self::Write => &[],
        };
        reads.iter().chain(&fs.write_prefixes)
    }
}

/// A key of an fs region: an authored prefix, or a place exec runs.  Both are
/// prefixes of a real path, so a place reads as a prefix whether exec runs it
/// as a file or a directory, and containment is transitive across every pair.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FsScope {
    Prefix(FrozenPath),
    Place(RealPath),
}

impl std::fmt::Display for FsScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prefix(prefix) => f.write_str(prefix.as_str()),
            Self::Place(place) => place.fmt(f),
        }
    }
}

/// Rosetta's runtime, the loader analogue: always readable, always admitted to exec.
#[cfg(target_os = "macos")]
pub(crate) const ROSETTA: &str = "/Library/Apple/usr";

/// Fs evidence over one op: prefixes judged by their resolved form, an
/// exclusion beating every allowance.  Only the read region holds a place, and
/// no place is negative.
pub(crate) type Region = Table<FsScope>;

impl Scope for FsScope {
    type Subject<'a> = &'a Path;

    fn stores(&self, evidence: Evidence) -> bool {
        matches!(self, Self::Prefix(_)) || !evidence.negative
    }

    fn holds<P: Polarity>(&self, path: &Path) -> bool {
        match self {
            Self::Prefix(prefix) => prefix.contains::<P>(path),
            Self::Place(place) => place.contains::<P>(path),
        }
    }

    fn own(&self) -> &Path {
        match self {
            Self::Prefix(prefix) => prefix.real_path(),
            Self::Place(place) => place.as_path(),
        }
    }
}

/// What loading needs is not authored, so an entry that cannot be held as a
/// grant, resolving through a non-Unicode name, is dropped.
fn freeze_floor(paths: impl IntoIterator<Item = impl AsRef<Path>>) -> Vec<FrozenPath> {
    paths
        .into_iter()
        .filter_map(|p| FrozenPath::freeze(p).ok())
        .collect()
}

/// The read-only system paths admitted under every restricted `read`, frozen
/// once per process, with the Homebrew prefixes `homebrew` expands to.  Those
/// absent on this host are dropped.
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
        ];
        #[cfg(target_os = "macos")]
        let listed = [
            "/bin",
            "/usr",
            // Rosetta's runtime, the loader analogue: no grant should name it.
            ROSETTA,
            "/Library/Developer/CommandLineTools",
            "/Applications/Xcode.app/Contents/Developer",
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
        let brew = live_homebrew_roots();
        let present = (listed.iter().map(ToString::to_string)).filter(|p| crate::path::exists(p));
        freeze_floor(present.chain(brew))
    });
    &FLOOR
}

/// `acc` met with `next`, the first `next` standing alone.
fn meet_onto<T: Meet>(acc: &mut Option<T>, next: T) {
    *acc = Some(match acc.take() {
        Some(held) => held.meet(next),
        None => next,
    });
}

/// The stack's region for `op`: each opining layer's allowances for `op` and
/// its exclusions, the exclusions re-frozen, met; for `Read`, joined with the
/// [`floor`], which only an exclusion removes.  `None` exactly when no layer
/// held an `fs` opinion, so the guard is unrestricted and the projection needs
/// no fs rules; a layer that opined and admitted nothing withholds support
/// beyond the floor.
pub(crate) fn region(grants: &GrantStack, op: &FsOp) -> Option<Region> {
    fold(grants, op, false)
}

/// The read region where each layer also reads the run places of the exec meet
/// over it and the layers beneath it: no layer makes anything readable for
/// those beneath it.
pub(crate) fn running_region(grants: &GrantStack) -> Option<Region> {
    fold(grants, &FsOp::Read, true)
}

fn fold(grants: &GrantStack, op: &FsOp, places: bool) -> Option<Region> {
    let mut run: Option<ExecRules> = None;
    let mut met: Option<Region> = None;
    let last = grants.into_iter().rposition(|l| l.fs.is_some())?;
    for layer in grants.into_iter().take(last + 1) {
        if places && let Some(exec) = &layer.exec {
            meet_onto(&mut run, ExecRules::compile(exec));
        }
        let Some(fs) = &layer.fs else { continue };
        let allows = op
            .prefixes(fs)
            .map(|p| (FsScope::Prefix(p.clone()), Evidence::ALLOW));
        let denies = (fs.deny_paths.iter()).map(|p| (FsScope::Prefix(p.live()), Evidence::DENY));
        let placed = (run.iter().flat_map(ExecRules::places))
            .map(|p| (FsScope::Place(p.clone()), Evidence::ALLOW));
        meet_onto(&mut met, allows.chain(denies).chain(placed).collect());
    }
    let met = met?;
    Some(match op {
        FsOp::Read => met.join(
            floor()
                .iter()
                .map(|p| (FsScope::Prefix(p.clone()), Evidence::ALLOW))
                .collect(),
        ),
        FsOp::Write => met,
    })
}

impl GrantStack {
    /// The read judgment: may `path` be read?  Two calls of [`region`]: a
    /// permit or an exclusion of the first, which compiles no exec table, is
    /// final with the same witness; only a missing allowance asks again with
    /// the run places, which add positive evidence alone.
    pub(crate) fn read(&self, path: &Path) -> Result<(), Withheld<FsScope>> {
        let Some(region) = region(self, &FsOp::Read) else {
            return Ok(());
        };
        match region.withheld(path) {
            None => Ok(()),
            Some(Withheld::MissingAllowance) => {
                let placed = running_region(self).expect("a region held an fs opinion before");
                placed.withheld(path).map_or(Ok(()), Err)
            }
            Some(excluded) => Err(excluded),
        }
    }

    /// The write judgment: may `path` be written?
    pub(crate) fn write(&self, path: &Path) -> Result<(), Withheld<FsScope>> {
        let region = region(self, &FsOp::Write);
        region.and_then(|r| r.withheld(path)).map_or(Ok(()), Err)
    }

    /// Whether only an exclusion withholds the read of `path`, which is all a
    /// bundled tool needs of ral's binary.
    pub(crate) fn read_not_excluded(&self, path: &Path) -> Result<(), Withheld<FsScope>> {
        let withheld = region(self, &FsOp::Read).and_then(|r| r.withheld(path));
        match withheld {
            Some(excluded @ Withheld::Excluded { .. }) => Err(excluded),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "test fixtures build the access-side path from a literal already in normal form"
)]
mod tests {
    use super::*;
    use crate::capability::{Capabilities, ExecGrant, ExecKey};
    use crate::path::{Allow, Deny};

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
        let allows = allows
            .iter()
            .map(|p| (FsScope::Prefix(p.clone()), Evidence::ALLOW));
        allows
            .chain(
                denies
                    .iter()
                    .map(|p| (FsScope::Prefix(p.clone()), Evidence::DENY)),
            )
            .collect()
    }

    fn live(region: &Region) -> Vec<String> {
        region.live().map(ToString::to_string).collect()
    }

    fn permits(region: &Region, path: &str) -> bool {
        region.evidence(Path::new(path)).permits()
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

    /// the exclusion relation folds a spelling the allowance relation
    /// keeps apart, and a refusal by the fold names the exclusion.
    #[test]
    fn an_exclusion_holds_every_spelling_and_an_allowance_only_its_own() {
        let access = Path::new("/d/secrets/f");
        let region = table(&[lit("/")], &[lit("/d/Secrets")]);
        assert!(!permits(&region, "/d/secrets/f"));
        assert!(permits(&region, "/d/other"));
        if cfg!(not(windows)) {
            assert_eq!(
                region.withheld(access),
                Some(Withheld::Excluded {
                    scope: FsScope::Prefix(lit("/d/Secrets")),
                    respelled: true
                })
            );
            assert!(!permits(&table(&[lit("/d/Secrets")], &[]), "/d/secrets/f"));
        }
    }

    /// the projection drops an allowance an exclusion holds in another
    /// spelling, and keeps one a narrower exclusion leaves standing.
    #[test]
    fn the_live_allows_are_those_no_exclusion_holds() {
        let region = table(
            &[lit("/d/secrets/f"), lit("/d/other"), lit("/e")],
            &[lit("/d/Secrets"), lit("/e/f")],
        );
        assert_eq!(live(&region), ["/d/other", "/e"]);
    }

    /// A prefix is judged by where it resolves, whatever it spells.
    #[test]
    fn a_prefix_holds_by_its_resolved_form_not_its_surface() {
        assert!(!permits(&table(&[p("/a", "/elsewhere")], &[]), "/a/file"));
        assert!(permits(&table(&[p("/link", "/a")], &[]), "/a/file"));
    }

    /// The refusal cites an exclusion that holds the path, never one that does
    /// not, and the same one every time.
    #[test]
    fn the_witness_is_an_exclusion_that_holds_the_path() {
        let region = table(&[lit("/")], &[lit("/D"), lit("/D/Secrets"), lit("/E")]);
        let path = Path::new("/d/secrets/f");
        let Some(Withheld::Excluded { scope, .. }) = region.withheld(path) else {
            panic!("an exclusion holds the path");
        };
        if cfg!(not(windows)) {
            assert!(scope.holds::<Deny>(path) && !scope.holds::<Allow>(path));
        }
        assert_eq!(region.withheld(path), region.withheld(path));
    }

    /// The empty table is the fail-closed meet: it supports nothing, and is
    /// not an exclusion.
    #[test]
    fn the_empty_table_supports_nothing() {
        let region = Region::default();
        assert_eq!(region.evidence(Path::new("/a")), Evidence::NEITHER);
        assert_eq!(
            region.withheld(Path::new("/a")),
            Some(Withheld::MissingAllowance)
        );
    }

    /// Regression: the ceiling survives a re-freeze without shrinking to a
    /// drive.  A resolver anchors a driveless path to a cwd, so the
    /// universal root once came back as the root of whichever drive the
    /// process ran from: a session launched from `D:` lost a `%TEMP%` on
    /// `C:`, the shape GitHub's Windows runners have.
    #[test]
    fn the_root_survives_a_re_freeze_as_the_universal_prefix() {
        let root = FrozenPath::root().live();
        assert_eq!(root.real_path(), Path::new("/"), "got {root:?}");
        let region = table(&[root], &[]);
        // A drive spelling is a path only on Windows.
        let mut paths = vec!["/etc/hosts"];
        if cfg!(windows) {
            paths.extend([r"C:\Users\someone\Temp\x", r"D:\a\repo\y"]);
        }
        for path in paths {
            assert!(permits(&region, path), "{path}");
        }
    }

    /// A floor entry that resolves through a non-Unicode name is dropped, not
    /// held as a sibling its lossy form names.
    #[cfg(unix)]
    #[test]
    fn a_non_unicode_floor_entry_is_dropped() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let odd = dir.path().join(std::ffi::OsStr::from_bytes(b"f\xff"));
        std::fs::create_dir(&odd).unwrap();
        let fine = dir.path().join("fine");
        std::fs::create_dir(&fine).unwrap();
        let kept = freeze_floor([odd, fine.clone()]);
        assert_eq!(kept, [FrozenPath::from_surface(fine)]);
    }

    fn layer(fs: FsPolicy) -> Capabilities {
        Capabilities {
            fs: Some(fs),
            ..Capabilities::default()
        }
    }

    fn running(places: &[&str]) -> Capabilities {
        Capabilities {
            exec: Some(
                (places.iter())
                    .map(|p| (ExecKey::Path(lit(p)), Evidence::ALLOW))
                    .collect::<ExecGrant>(),
            ),
            ..Capabilities::default()
        }
    }

    fn stack_of(layers: impl IntoIterator<Item = Capabilities>) -> GrantStack {
        let mut stack = GrantStack::default();
        layers.into_iter().for_each(|l| stack.push(l));
        stack
    }

    /// The region for `op` of one layer allowing `allow` for both ops.
    fn region_of(op: &FsOp, allow: &[FrozenPath], deny: &[FrozenPath]) -> Region {
        let stack = stack_of([layer(FsPolicy {
            read_prefixes: allow.to_vec(),
            write_prefixes: allow.to_vec(),
            deny_paths: deny.to_vec(),
        })]);
        region(&stack, op).expect("fs opined")
    }

    #[test]
    fn the_read_region_admits_the_floor_a_layer_never_named() {
        let read = region_of(&FsOp::Read, &[lit("/work")], &[]);
        for f in floor() {
            assert!(read.evidence(f.real_path()).permits(), "{f:?}");
        }
    }

    #[test]
    fn an_exclusion_refuses_a_floor_path() {
        let f = floor().first().expect("a host has a floor");
        let read = region_of(&FsOp::Read, &[lit("/work")], std::slice::from_ref(f));
        assert_eq!(read.evidence(f.real_path()), Evidence::BOTH);
        assert!(matches!(
            read.withheld(f.real_path()),
            Some(Withheld::Excluded { .. })
        ));
    }

    #[test]
    fn the_write_region_admits_no_floor_path() {
        let write = region_of(&FsOp::Write, &[lit("/work")], &[]);
        for f in floor() {
            assert!(!write.evidence(f.real_path()).permits(), "{f:?}");
        }
    }

    #[test]
    fn an_empty_read_admits_exactly_the_floor() {
        let read = region_of(&FsOp::Read, &[], &[]);
        assert!(
            floor()
                .iter()
                .all(|f| read.evidence(f.real_path()).permits())
        );
        assert_eq!(
            read.evidence(Path::new("/no/such/place")),
            Evidence::NEITHER
        );
    }

    /// Writing reads: a layer that writes a folder and does not list it for
    /// reading reads it.
    #[test]
    fn writing_a_folder_reads_it() {
        let stack = stack_of([layer(FsPolicy {
            write_prefixes: vec![lit("/work")],
            ..FsPolicy::default()
        })]);
        assert!(stack.read(Path::new("/work/f")).is_ok());
        assert!(stack.write(Path::new("/work/f")).is_ok());
        assert!(stack.read(Path::new("/other/f")).is_err());
    }

    /// Allowances are frozen as decoded; only exclusions are re-frozen.
    #[test]
    fn an_allowance_keeps_its_frozen_real_path() {
        let stack = stack_of([layer(FsPolicy {
            read_prefixes: vec![p("/no/such/surface", "/frozen/real")],
            ..FsPolicy::default()
        })]);
        let read = region(&stack, &FsOp::Read).unwrap();
        assert!(permits(&read, "/frozen/real/f"));
        assert!(!permits(&read, "/no/such/surface/f"));
    }

    /// An enclosing grant's run places are readable by a nested fs-only
    /// restriction; the nested grant's own are not made readable for those
    /// beneath it.
    #[test]
    fn pairing_looks_down_the_stack() {
        let reads_work = layer(FsPolicy {
            read_prefixes: vec![lit("/work")],
            ..FsPolicy::default()
        });
        let nested = stack_of([running(&["/opt/prog"]), reads_work]);
        assert!(nested.read(Path::new("/opt/prog")).is_ok());

        // Order: a layer that reads nothing and omits exec, beneath a layer
        // that runs `s` and omits fs, refuses `s`; the other way round permits.
        let reads_nothing = layer(FsPolicy::default());
        let beneath = stack_of([reads_nothing.clone(), running(&["/opt/prog"])]);
        assert!(beneath.read(Path::new("/opt/prog")).is_err());
        let above = stack_of([running(&["/opt/prog"]), reads_nothing]);
        assert!(above.read(Path::new("/opt/prog")).is_ok());
    }

    /// An fs exclusion of a run place refuses it, naming the exclusion.
    #[test]
    fn an_exclusion_of_a_run_place_refuses_running_it() {
        let stack = stack_of([
            running(&["/opt/prog"]),
            layer(FsPolicy {
                read_prefixes: vec![lit("/")],
                deny_paths: vec![lit("/opt/prog")],
                ..FsPolicy::default()
            }),
        ]);
        let why = stack.read(Path::new("/opt/prog"));
        assert!(matches!(why, Err(Withheld::Excluded { .. })), "{why:?}");
    }

    /// The read judgment's two calls agree with the region of run places.
    #[test]
    fn the_read_judgment_agrees_with_the_region_of_places() {
        let stack = stack_of([
            running(&["/opt/prog", "/opt/tools"]),
            layer(FsPolicy {
                read_prefixes: vec![lit("/work")],
                deny_paths: vec![lit("/opt/tools/bad")],
                ..FsPolicy::default()
            }),
        ]);
        let full = running_region(&stack).unwrap();
        for path in [
            "/opt/prog",
            "/opt/tools/ok",
            "/opt/tools/bad",
            "/work/f",
            "/etc/zz",
        ] {
            assert_eq!(
                stack.read(Path::new(path)).is_ok(),
                full.evidence(Path::new(path)).permits(),
                "{path}"
            );
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
