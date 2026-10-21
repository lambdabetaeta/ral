//! The fs dimension: a rules table per op, as [`super::exec`] holds the exec
//! dimension.
//!
//! Both readers of fs authority consume it: the in-process guard
//! (`crate::guard`) judges an access by the table, the OS projection
//! (`crate::sandbox::SandboxProjection`) renders its live allows and its
//! denies.  Agreement
//! between guard and sandbox profile is then structural — one fold per op,
//! two consumers — rather than a property two independent folds have to be
//! tested into.
//!
//! A restricted read region is the layers' meet joined with the [`Floor`]:
//! the platform's fixed read-only system paths any program needs to start.
//! No layer's `read` narrows it; only a `deny` removes a floor path.  Writing
//! reads, and running reads: each layer reads its own write prefixes and the
//! run places of the exec meet over it and the layers beneath it.
//!
//! A layer is resolved where it is decoded: an allowance holds the position
//! its spelling walked to, an exclusion the objects its walk held.  The guard
//! folds the layers afresh on every check; the projection folds once, at
//! spawn, because that is when the OS profile is written.

use super::exec::ExecRules;
use super::lattice::{Extent, GrantPath};
use super::table::{Allows, Denies, Table, Withheld};
use super::{FsPolicy, GrantLayer, GrantStack, Join, Meet};
use crate::guard::freeze::PolicyError;
use crate::guard::toolchains;
use crate::path::{
    Depth, Excluded, FrozenPath, LeafMode, MatchedBy, RealPath, Subject, Trace, walk_lenient,
};
use serde::{Deserialize, Serialize};
use strum::IntoStaticStr;

/// Which fs table a check consults: the read or the write one.
#[derive(IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum FsOp {
    Read,
    Write,
}

/// An allowance of an fs table: a prefix as the author wrote it and resolved,
/// or a place exec runs.  Both are positions, so a place reads as a prefix
/// whether exec runs it as a file or a directory, and containment is
/// transitive across every pair.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum FsAllow {
    Prefix(GrantPath<Extent>),
    RunPlace(RealPath),
}

/// An exclusion of an fs table: a spelling and the objects its walk held.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FsDeny(pub(super) GrantPath<Excluded>);

impl std::fmt::Display for FsDeny {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.written.shown())
    }
}

/// Fs evidence over one op: an exclusion beating every allowance.  Only the
/// read table holds a place.
pub(crate) type FsRules = Table<FsAllow, FsDeny>;

/// A layer's fs opinion: what it reads and what it writes, each against every
/// exclusion.  The read table holds the read and the write allowances, the
/// write table the write allowances, so nothing can say "read `/work`, do not
/// write it" and survive a meet with a layer that only reads it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsLayer {
    read: FsRules,
    write: FsRules,
}

impl FsLayer {
    /// The two tables `read`, `write` and `deny` make.
    pub(crate) fn new(
        read: impl IntoIterator<Item = GrantPath<Extent>>,
        write: impl IntoIterator<Item = GrantPath<Extent>>,
        deny: impl IntoIterator<Item = GrantPath<Excluded>>,
    ) -> Self {
        let write: Vec<_> = write.into_iter().map(FsAllow::Prefix).collect();
        let denies: Vec<_> = deny.into_iter().map(FsDeny).collect();
        let reads = read.into_iter().map(FsAllow::Prefix).chain(write.clone());
        Self {
            read: Table::new(reads, denies.clone()),
            write: Table::new(write, denies),
        }
    }

    /// The layer an authored policy makes: each spelling walked as its
    /// polarity walks.
    ///
    /// # Errors
    /// A spelling that cannot be resolved, named; a root exclusion.
    pub(crate) fn of_policy(policy: FsPolicy) -> Result<Self, PolicyError> {
        let FsPolicy {
            read_prefixes,
            write_prefixes,
            deny_paths,
        } = policy;
        let allowing = |paths: Vec<FrozenPath>| {
            (paths.into_iter())
                .map(GrantPath::allowing)
                .collect::<Result<Vec<_>, _>>()
        };
        Ok(Self::new(
            allowing(read_prefixes)?,
            allowing(write_prefixes)?,
            (deny_paths.into_iter())
                .map(GrantPath::walk)
                .collect::<Result<Vec<_>, _>>()?,
        ))
    }

    /// Take hold again of every exclusion's objects.
    pub(super) fn rehold(&mut self) {
        self.read.rehold_with(FsDeny::rehold);
        self.write.rehold_with(FsDeny::rehold);
    }

    pub(crate) fn table(&self, op: &FsOp) -> &FsRules {
        match op {
            FsOp::Read => &self.read,
            FsOp::Write => &self.write,
        }
    }

    /// What the layer shows of itself: each table's spellings as written.
    pub(super) fn summary(&self) -> super::FsSummary {
        let written = |table: &FsRules| {
            table
                .allows()
                .filter_map(|a| match a {
                    FsAllow::Prefix(prefix) => Some(prefix.written.shown()),
                    FsAllow::RunPlace(_) => None,
                })
                .collect()
        };
        super::FsSummary {
            read: written(&self.read),
            write: written(&self.write),
            deny: self.read.denies().map(ToString::to_string).collect(),
        }
    }
}

impl Join for FsLayer {
    fn join(self, other: Self) -> Self {
        Self {
            read: self.read.join(other.read),
            write: self.write.join(other.write),
        }
    }
}

impl super::Axis for FsLayer {
    /// A fragment of exclusions, over the omitted `t`: everywhere, less them.
    fn join_top(self) -> Option<Self> {
        self.write.denies().next()?;
        let everywhere = || FsRules::new([FsAllow::Prefix(GrantPath::everywhere())], []);
        Some(Self {
            read: self.read.join(everywhere()),
            write: self.write.join(everywhere()),
        })
    }

    /// Writing reads, so writing everywhere reads everywhere too; everywhere
    /// is what a spelling that walked to the root also is.
    fn is_top(&self) -> bool {
        self.write.denies().next().is_none()
            && (self.write.allows())
                .any(|a| matches!(a, FsAllow::Prefix(p) if p.real == Extent::Everywhere))
    }
}

impl FsAllow {
    /// The position this allowance holds from, `/` for everywhere.
    #[cfg(any(target_os = "linux", windows, test, feature = "test-util"))]
    pub(crate) fn real(&self) -> RealPath {
        match self {
            Self::Prefix(prefix) => prefix.real.real(),
            Self::RunPlace(place) => place.clone(),
        }
    }
}

impl FsDeny {
    /// The spelling this exclusion was written as.
    pub(crate) fn written(&self) -> &FrozenPath {
        &self.0.written
    }

    fn rehold(&mut self) {
        self.0.real.rehold();
    }
}

/// `Everywhere` contains every subject, which only an allowance that holds
/// everywhere does; a position contains what it is the prefix of, and never
/// everywhere.
impl Allows for FsAllow {
    type Subject<'a> = Subject<'a>;

    fn contains(&self, subject: Subject<'_>) -> bool {
        match self {
            Self::Prefix(prefix) => prefix.real.contains(subject),
            Self::RunPlace(place) => subject.beneath(place),
        }
    }

    fn own(&self) -> Subject<'_> {
        match self {
            Self::Prefix(GrantPath {
                real: Extent::Under(position),
                ..
            })
            | Self::RunPlace(position) => Subject::Position(position),
            Self::Prefix(GrantPath {
                real: Extent::Everywhere,
                ..
            }) => Subject::Everywhere,
        }
    }
}

impl Denies<FsAllow> for FsDeny {
    fn matches(&self, subject: Subject<'_>) -> Option<MatchedBy> {
        self.0.real.matches(subject, Depth::Within)
    }
}

/// Rosetta's runtime, the loader analogue: always readable.
#[cfg(target_os = "macos")]
const ROSETTA: &str = "/Library/Apple/usr";

/// The read-only system paths admitted under every restricted `read`.
///
/// Each is resolved once, where the [`Runtime`](crate::types::Runtime) is
/// built: the host's own, not the grant's, so no stack holds it and no wire
/// carries it.  The system-wide prefixes of [`toolchains::FLOOR`] outside the
/// process's home are among them; paths absent on this host are dropped.
#[derive(Clone, Debug, Default)]
pub struct Floor(Vec<FsAllow>);

impl Floor {
    /// The floor of this host, its spellings walked.
    ///
    /// # Errors
    /// A floor spelling whose walk fails, named: a host that cannot resolve
    /// its own floor cannot run anything.
    pub(crate) fn of_host() -> Result<Self, String> {
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
        #[allow(
            clippy::disallowed_methods,
            reason = "the floor has no FreezeCtx: its home is the process's, as `home` places default to"
        )]
        let home = crate::host::home();
        let mut floor = Self::resolve(listed.map(std::path::PathBuf::from))?;
        let prefixes = toolchains::floor(home.as_deref())?;
        floor.0.extend(
            prefixes
                .into_iter()
                .map(|dir| FsAllow::Prefix(dir.extent())),
        );
        Ok(floor)
    }

    /// The floor of `spellings`: each walked; one that is not absolute, or
    /// whose name is not Unicode (no grant can hold it), or that is absent
    /// here, is dropped.
    fn resolve(spellings: impl IntoIterator<Item = std::path::PathBuf>) -> Result<Self, String> {
        let mut floor = Vec::new();
        for spelling in spellings {
            let Some(written) = crate::path::is_absolute(&spelling)
                .then(|| FrozenPath::freeze(&spelling).ok())
                .flatten()
            else {
                continue;
            };
            let trace = walk_lenient(&written.lexical(), LeafMode::Follow).map_err(|e| {
                format!(
                    "ral cannot resolve the read floor's '{}': {e}",
                    written.shown()
                )
            })?;
            if trace.kind().is_some() {
                floor.push(FsAllow::Prefix(GrantPath::of(written, &trace)));
            }
        }
        Ok(Self(floor))
    }

    pub(crate) fn allows(&self) -> impl Iterator<Item = &FsAllow> {
        self.0.iter()
    }
}

/// `acc` met with `next`, the first `next` standing alone.
fn meet_onto<T: Meet>(acc: &mut Option<T>, next: T) {
    *acc = Some(match acc.take() {
        Some(held) => held.meet(next),
        None => next,
    });
}

/// The stack's table for `op`: each opining layer's allowances for `op` and
/// its exclusions, met; for `Read`, joined with the [`Floor`], which only an
/// exclusion removes.  `None` exactly when no layer held an `fs` opinion, so
/// the guard is unrestricted and the projection needs no fs rules; a layer
/// that opined and admitted nothing withholds support beyond the floor.
pub(crate) fn region(grants: &GrantStack, op: &FsOp, floor: &Floor) -> Option<FsRules> {
    fold(grants, op, false, floor)
}

/// The read table where each layer also reads the run places of the exec meet
/// over it and the layers beneath it: no layer makes anything readable for
/// those beneath it.
pub(crate) fn running_region(grants: &GrantStack, floor: &Floor) -> Option<FsRules> {
    fold(grants, &FsOp::Read, true, floor)
}

fn fold(grants: &GrantStack, op: &FsOp, places: bool, floor: &Floor) -> Option<FsRules> {
    let layers: Vec<&GrantLayer> = grants.layers().collect();
    let last = layers.iter().rposition(|l| l.fs().is_some())?;
    let mut run: Option<ExecRules> = None;
    let mut met: Option<FsRules> = None;
    for layer in &layers[..=last] {
        if places && let Some(exec) = layer.exec() {
            meet_onto(&mut run, exec.clone());
        }
        let Some(fs) = layer.fs() else { continue };
        let placed = run.iter().flat_map(ExecRules::places);
        meet_onto(&mut met, fs.table(op).clone().join(Table::new(placed, [])));
    }
    let met = met?;
    Some(match op {
        FsOp::Read => met.join(Table::new(floor.allows().cloned(), [])),
        FsOp::Write => met,
    })
}

impl GrantStack {
    /// The read judgment: may the access `trace` be read?  Two folds: a permit
    /// or an exclusion of the first, which holds no run places, is final with
    /// the same witness; only a missing allowance asks again with the run
    /// places, which add positive evidence alone.
    pub(crate) fn read(&self, floor: &Floor, trace: &Trace) -> Result<(), Withheld<FsDeny>> {
        let access = Subject::Access(trace);
        let Some(region) = region(self, &FsOp::Read, floor) else {
            return Ok(());
        };
        match region.withheld(access) {
            None => Ok(()),
            Some(Withheld::MissingAllowance) => {
                let placed =
                    running_region(self, floor).expect("a region held an fs opinion before");
                placed.withheld(access).map_or(Ok(()), Err)
            }
            Some(excluded) => Err(excluded),
        }
    }

    /// The write judgment: may the access `trace` be written?
    pub(crate) fn write(&self, floor: &Floor, trace: &Trace) -> Result<(), Withheld<FsDeny>> {
        let region = region(self, &FsOp::Write, floor);
        region
            .and_then(|r| r.withheld(Subject::Access(trace)))
            .map_or(Ok(()), Err)
    }

    /// Whether only an exclusion withholds the read of `trace`, which is all a
    /// bundled tool needs of ral's binary.
    pub(crate) fn read_not_excluded(
        &self,
        floor: &Floor,
        trace: &Trace,
    ) -> Result<(), Withheld<FsDeny>> {
        let withheld =
            region(self, &FsOp::Read, floor).and_then(|r| r.withheld(Subject::Access(trace)));
        match withheld {
            Some(excluded @ Withheld::Excluded { .. }) => Err(excluded),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] a tempdir tree with real links; fixture paths already in normal form"
)]
mod tests {
    use super::*;
    use crate::capability::Evidence;
    use crate::capability::{Capabilities, ExecDenial, ExecGrant, ExecKey, Program};
    use crate::path::{test_absolute, utf8};
    use crate::test_helper::{core_runtime, layer, stack, trace};
    use std::path::{Path, PathBuf};

    /// A spelling taken as written, on this host.
    fn lit(s: &str) -> FrozenPath {
        FrozenPath::from_surface(&test_absolute(s))
    }

    /// A Windows spelling taken exactly as written: the `cfg(windows)` tests
    /// below judge the host's own spellings, which no [`test_absolute`]
    /// mapping may rewrite.
    #[cfg(windows)]
    fn native(s: &str) -> FrozenPath {
        FrozenPath::from_surface(s)
    }

    /// The access to `path`, a POSIX-style fixture path, walked as it is: an
    /// absent path is its own lexical position.
    fn at(path: &str) -> Trace {
        trace(test_absolute(path))
    }

    fn table(allows: &[FrozenPath], denies: &[FrozenPath]) -> FsRules {
        FsRules::new(
            (allows.iter().cloned()).map(|p| FsAllow::Prefix(GrantPath::allowing(p).unwrap())),
            (denies.iter().cloned()).map(|p| FsDeny(GrantPath::walk(p).unwrap())),
        )
    }

    fn live(region: &FsRules) -> Vec<FsAllow> {
        region.live().cloned().collect()
    }

    fn prefixes(paths: &[&str]) -> Vec<FsAllow> {
        (paths.iter())
            .map(|p| FsAllow::Prefix(GrantPath::allowing(lit(p)).unwrap()))
            .collect()
    }

    fn permits(region: &FsRules, access: &Trace) -> bool {
        region.evidence(Subject::Access(access)).permits()
    }

    /// A tempdir and the real path of its root.
    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = RealPath::of_path(tmp.path())
            .unwrap()
            .as_path()
            .to_path_buf();
        (tmp, root)
    }

    /// The directory `name` of `root`, made.
    fn made(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `link`, a directory link to `target`.
    fn link_dir(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(target, link).unwrap();
    }

    fn spelled(path: &Path) -> FrozenPath {
        FrozenPath::from_surface(utf8(path).unwrap())
    }

    #[test]
    fn the_meet_keeps_the_deeper_prefix_of_each_overlapping_pair() {
        let met = table(&[lit("/a"), lit("/b")], &[]).meet(table(&[lit("/a/x"), lit("/c")], &[]));
        assert_eq!(live(&met), prefixes(&["/a/x"]));
    }

    /// Security regression.  A grant that lexically nests under a shallower
    /// ceiling but resolves outside it through a symlink must not survive
    /// the meet: the survivor would reach the OS sandbox, where `bwrap
    /// --bind` follows the source symlink and Seatbelt matches lexically, so
    /// a spawned child could read the link's target.
    #[test]
    fn a_symlinked_grant_cannot_escape_a_shallower_ceiling() {
        let (_tmp, root) = tmp();
        let ceiling = made(&root, "a");
        let outside = made(&root, "x");
        let link = ceiling.join("link");
        link_dir(&outside, &link);
        let met = table(&[spelled(&ceiling)], &[]).meet(table(&[spelled(&link)], &[]));
        assert!(live(&met).is_empty(), "{met:?}");
    }

    /// Positive control: the meet narrows, it does not blanket-deny.
    #[test]
    fn legitimate_nesting_survives_the_meet() {
        let met = table(&[lit("/a")], &[]).meet(table(&[lit("/a/sub")], &[]));
        assert_eq!(live(&met), prefixes(&["/a/sub"]));
    }

    /// the exclusion relation folds a spelling the allowance relation
    /// keeps apart, and a refusal by the fold names the exclusion.
    #[test]
    fn an_exclusion_holds_every_spelling_and_an_allowance_only_its_own() {
        let access = at("/d/secrets/f");
        let region = table(&[lit("/")], &[lit("/d/Secrets")]);
        assert!(!permits(&region, &access));
        assert!(permits(&region, &at("/d/other")));
        assert_eq!(
            region.withheld(Subject::Access(&access)),
            Some(Withheld::Excluded {
                scope: Box::new(FsDeny(GrantPath::walk(lit("/d/Secrets")).unwrap())),
                matched_by: MatchedBy::FoldedName
            })
        );
        assert!(!permits(&table(&[lit("/d/Secrets")], &[]), &access));
    }

    /// the projection drops an allowance an exclusion holds in another
    /// spelling, and keeps one a narrower exclusion leaves standing.
    #[test]
    fn the_live_allows_are_those_no_exclusion_holds() {
        let region = table(
            &[lit("/d/secrets/f"), lit("/d/other"), lit("/e")],
            &[lit("/d/Secrets"), lit("/e/f")],
        );
        assert_eq!(live(&region), prefixes(&["/d/other", "/e"]));
    }

    /// A prefix is judged by where it resolves, whatever it spells.
    #[test]
    fn a_prefix_holds_by_its_resolved_form_not_its_surface() {
        let (_tmp, root) = tmp();
        let here = made(&root, "a");
        let elsewhere = made(&root, "elsewhere");
        let (leaves_here, leads_here) = (root.join("away"), root.join("link"));
        link_dir(&elsewhere, &leaves_here);
        link_dir(&here, &leads_here);
        let access = trace(here.join("file"));
        assert!(!permits(&table(&[spelled(&leaves_here)], &[]), &access));
        assert!(permits(&table(&[spelled(&leads_here)], &[]), &access));
    }

    /// The refusal cites an exclusion that holds the path, never one that does
    /// not, and the same one every time.
    #[test]
    fn the_witness_is_an_exclusion_that_holds_the_path() {
        let region = table(&[lit("/")], &[lit("/D"), lit("/D/Secrets"), lit("/E")]);
        let path = at("/d/secrets/f");
        let Some(Withheld::Excluded { scope, .. }) = region.withheld(Subject::Access(&path)) else {
            panic!("an exclusion holds the path");
        };
        assert_eq!(
            scope.matches(Subject::Access(&path)),
            Some(MatchedBy::FoldedName)
        );
        assert_eq!(
            region.withheld(Subject::Access(&path)),
            region.withheld(Subject::Access(&path))
        );
    }

    /// The empty table is the fail-closed meet: it supports nothing, and is
    /// not an exclusion.
    #[test]
    fn the_empty_table_supports_nothing() {
        let region = FsRules::default();
        let access = at("/a");
        assert_eq!(region.evidence(Subject::Access(&access)), Evidence::NEITHER);
        assert_eq!(
            region.withheld(Subject::Access(&access)),
            Some(Withheld::MissingAllowance)
        );
    }

    /// Regression: the ceiling holds every path without shrinking to a
    /// drive.  A resolver anchors a driveless path to a cwd, so the
    /// universal root once came back as the root of whichever drive the
    /// process ran from: a session launched from `D:` lost a `%TEMP%` on
    /// `C:`, the shape GitHub's Windows runners have.
    #[test]
    fn the_root_is_the_universal_prefix() {
        let root = GrantPath::<Extent>::allowing(FrozenPath::root()).unwrap();
        assert_eq!(root.real, Extent::Everywhere);
        let region = table(&[FrozenPath::root()], &[]);
        // A drive spelling is a path only on Windows.
        let mut paths = vec!["/etc/hosts"];
        if cfg!(windows) {
            paths.extend([r"C:\Users\someone\Temp\x", r"D:\a\repo\y"]);
        }
        for path in paths {
            let position = RealPath::lenient(Path::new(path));
            assert!(
                region.evidence(Subject::Position(&position)).permits(),
                "{path}"
            );
        }
    }

    /// A floor entry that resolves through a non-Unicode name is dropped, not
    /// held as a sibling its lossy form names.  APFS refuses such a name.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_non_unicode_floor_entry_is_dropped() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let odd = dir.path().join(std::ffi::OsStr::from_bytes(b"f\xff"));
        std::fs::create_dir(&odd).unwrap();
        let fine = dir.path().join("fine");
        std::fs::create_dir(&fine).unwrap();
        let floor = Floor::resolve([odd, fine.clone()]).unwrap();
        assert_eq!(
            floor.allows().collect::<Vec<_>>(),
            [&FsAllow::Prefix(
                GrantPath::allowing(spelled(&fine)).unwrap()
            )]
        );
    }

    fn fs_layer(fs: FsPolicy) -> GrantLayer {
        layer(Capabilities {
            fs: Some(fs),
            ..Capabilities::default()
        })
    }

    fn running(places: &[&str]) -> GrantLayer {
        layer(Capabilities {
            exec: Some(
                (places.iter())
                    .map(|p| (ExecKey::Path(lit(p)), Evidence::ALLOW))
                    .collect::<ExecGrant>(),
            ),
            ..Capabilities::default()
        })
    }

    /// The region for `op` of one layer allowing `allow` for both ops.
    fn region_of(op: &FsOp, allow: &[FrozenPath], deny: &[FrozenPath], floor: &Floor) -> FsRules {
        let stack = stack([fs_layer(FsPolicy {
            read_prefixes: allow.to_vec(),
            write_prefixes: allow.to_vec(),
            deny_paths: deny.to_vec(),
        })]);
        region(&stack, op, floor).expect("fs opined")
    }

    #[test]
    fn the_read_region_admits_the_floor_a_layer_never_named() {
        let runtime = core_runtime();
        let read = region_of(&FsOp::Read, &[lit("/work")], &[], runtime.floor());
        for f in runtime.floor().allows() {
            assert!(read.evidence(f.own()).permits(), "{f:?}");
        }
    }

    #[test]
    fn an_exclusion_refuses_a_floor_path() {
        let runtime = core_runtime();
        let f = runtime.floor().allows().next().expect("a host has a floor");
        let FsAllow::Prefix(prefix) = f else {
            panic!("the floor holds prefixes: {f:?}");
        };
        let read = region_of(
            &FsOp::Read,
            &[lit("/work")],
            std::slice::from_ref(&prefix.written),
            runtime.floor(),
        );
        assert_eq!(read.evidence(f.own()), Evidence::BOTH);
        assert!(matches!(
            read.withheld(f.own()),
            Some(Withheld::Excluded { .. })
        ));
    }

    #[test]
    fn the_write_region_admits_no_floor_path() {
        let runtime = core_runtime();
        let write = region_of(&FsOp::Write, &[lit("/work")], &[], runtime.floor());
        for f in runtime.floor().allows() {
            assert!(!write.evidence(f.own()).permits(), "{f:?}");
        }
    }

    #[test]
    fn an_empty_read_admits_exactly_the_floor() {
        let runtime = core_runtime();
        let read = region_of(&FsOp::Read, &[], &[], runtime.floor());
        assert!((runtime.floor().allows()).all(|f| read.evidence(f.own()).permits()));
        assert_eq!(
            read.evidence(Subject::Access(&at("/no/such/place"))),
            Evidence::NEITHER
        );
    }

    /// Writing reads: a layer that writes a folder and does not list it for
    /// reading reads it.
    #[test]
    fn writing_a_folder_reads_it() {
        let stack = stack([fs_layer(FsPolicy {
            write_prefixes: vec![lit("/work")],
            ..FsPolicy::default()
        })]);
        let floor = Floor::default();
        assert!(stack.read(&floor, &at("/work/f")).is_ok());
        assert!(stack.write(&floor, &at("/work/f")).is_ok());
        assert!(stack.read(&floor, &at("/other/f")).is_err());
    }

    /// Reading a folder is not writing it; writing one reads it.
    #[test]
    fn a_reading_layer_does_not_write_and_a_writing_layer_reads() {
        let (_tmp, root) = tmp();
        let work = made(&root, "work");
        let (floor, file) = (Floor::default(), trace(work.join("f")));
        let reads = stack([fs_layer(FsPolicy {
            read_prefixes: vec![spelled(&work)],
            ..FsPolicy::default()
        })]);
        assert!(reads.read(&floor, &file).is_ok());
        assert!(matches!(
            reads.write(&floor, &file),
            Err(Withheld::MissingAllowance)
        ));
        let writes = stack([fs_layer(FsPolicy {
            write_prefixes: vec![spelled(&work)],
            ..FsPolicy::default()
        })]);
        assert!(writes.read(&floor, &file).is_ok());
        assert!(writes.write(&floor, &file).is_ok());
    }

    /// Everywhere less an exclusion, met with a folder, is the folder less the
    /// exclusion.
    #[test]
    fn the_meet_of_everywhere_less_a_deny_with_a_folder_is_the_folder_less_the_deny() {
        let (_tmp, root) = tmp();
        let (work, secret) = (made(&root, "work"), made(&root, "secret"));
        std::fs::write(work.join(".env"), "").unwrap();
        let everywhere = table(&[FrozenPath::root()], &[spelled(&work.join(".env"))]);
        let met = everywhere.meet(table(&[spelled(&work)], &[]));
        assert!(permits(&met, &trace(work.join("f"))));
        assert!(!permits(&met, &trace(secret.join("f"))));
        assert!(!permits(&met, &trace(work.join(".env"))));
    }

    #[test]
    fn the_meet_of_everywhere_with_nothing_admits_nothing() {
        let (_tmp, root) = tmp();
        let met = table(&[FrozenPath::root()], &[]).meet(table(&[], &[]));
        assert!(!permits(&met, &trace(root.join("f"))));
    }

    /// The floor is an argument of the judgment: with it a read admits the
    /// floor and what the layer names; without it, only the latter.
    #[test]
    fn read_with_and_without_the_floor_differ_exactly_on_the_floor() {
        let (_tmp, root) = tmp();
        let (work, system, other) = (
            made(&root, "work"),
            made(&root, "system"),
            made(&root, "other"),
        );
        let floor = Floor::resolve([system.clone()]).unwrap();
        let reads = stack([fs_layer(FsPolicy {
            read_prefixes: vec![spelled(&work)],
            ..FsPolicy::default()
        })]);
        for (path, named, floored) in [
            (work.join("f"), true, false),
            (system.join("lib"), false, true),
            (other.join("f"), false, false),
        ] {
            let access = trace(path);
            assert_eq!(reads.read(&Floor::default(), &access).is_ok(), named);
            assert_eq!(reads.read(&floor, &access).is_ok(), named || floored);
        }
    }

    /// Law 7: a layer that reads nothing runs a program under the floor, and
    /// is refused for reading it where the floor is absent.
    #[test]
    fn a_program_under_the_floor_alone_is_readable_to_run() {
        let (_tmp, root) = tmp();
        let system = made(&root, "system");
        std::fs::write(system.join("prog"), "").unwrap();
        let floor = Floor::resolve([system.clone()]).unwrap();
        let program = Program::File {
            trace: trace(system.join("prog")),
            path: system.join("prog"),
        };
        let reads_nothing = stack([fs_layer(FsPolicy::default())]);
        assert!(reads_nothing.admits(&floor, &program));
        assert!(matches!(
            reads_nothing.refusal(&Floor::default(), &program),
            Some(ExecDenial::Read(_))
        ));
    }

    /// A host that cannot walk its own floor cannot make a runtime, and says
    /// which spelling: here the home, which the toolchain rows are judged by.
    #[cfg(unix)]
    #[test]
    fn a_runtime_whose_floor_cannot_be_walked_refuses_construction_naming_it() {
        let (_tmp, root) = tmp();
        let (a, b) = (root.join("a"), root.join("b"));
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        let home = a.join("home");
        let built = crate::test_env::with_var("HOME", utf8(&home).ok(), || {
            crate::HostSurface::default().runtime(crate::terminal::TerminalState::default())
        });
        let Err(why) = built else {
            panic!("a floor through a link cycle cannot be walked");
        };
        assert!(why.contains(utf8(&home).unwrap()), "{why}");
    }

    /// An allowance is held where it was decoded: the link its spelling went
    /// through is retargeted, and the allowance stays on the old target.
    #[test]
    fn an_allowance_keeps_its_frozen_real_path() {
        let (_tmp, root) = tmp();
        let real = made(&root, "real");
        let other = made(&root, "other");
        let surface = root.join("surface");
        link_dir(&real, &surface);
        let stack = stack([fs_layer(FsPolicy {
            read_prefixes: vec![spelled(&surface)],
            ..FsPolicy::default()
        })]);
        #[cfg(unix)]
        std::fs::remove_file(&surface).unwrap();
        #[cfg(windows)]
        std::fs::remove_dir(&surface).unwrap();
        link_dir(&other, &surface);
        let read = region(&stack, &FsOp::Read, &Floor::default()).unwrap();
        assert!(permits(&read, &trace(real.join("f"))));
        assert!(!permits(&read, &trace(surface.join("f"))));
    }

    /// An enclosing grant's run places are readable by a nested fs-only
    /// restriction; the nested grant's own are not made readable for those
    /// beneath it.
    #[test]
    fn pairing_looks_down_the_stack() {
        let floor = Floor::default();
        let reads_work = fs_layer(FsPolicy {
            read_prefixes: vec![lit("/work")],
            ..FsPolicy::default()
        });
        let nested = stack([running(&["/x/prog"]), reads_work]);
        assert!(nested.read(&floor, &at("/x/prog")).is_ok());

        // Order: a layer that reads nothing and omits exec, beneath a layer
        // that runs `s` and omits fs, refuses `s`; the other way round permits.
        let reads_nothing = fs_layer(FsPolicy::default());
        let beneath = stack([reads_nothing.clone(), running(&["/x/prog"])]);
        assert!(beneath.read(&floor, &at("/x/prog")).is_err());
        let above = stack([running(&["/x/prog"]), reads_nothing]);
        assert!(above.read(&floor, &at("/x/prog")).is_ok());
    }

    /// An fs exclusion of a run place refuses it, naming the exclusion.
    #[test]
    fn an_exclusion_of_a_run_place_refuses_running_it() {
        let stack = stack([
            running(&["/x/prog"]),
            fs_layer(FsPolicy {
                read_prefixes: vec![lit("/")],
                deny_paths: vec![lit("/x/prog")],
                ..FsPolicy::default()
            }),
        ]);
        let why = stack.read(&Floor::default(), &at("/x/prog"));
        assert!(matches!(why, Err(Withheld::Excluded { .. })), "{why:?}");
    }

    /// The read judgment's two calls agree with the region of run places.
    #[test]
    fn the_read_judgment_agrees_with_the_region_of_places() {
        let floor = Floor::default();
        let stack = stack([
            running(&["/x/prog", "/x/tools"]),
            fs_layer(FsPolicy {
                read_prefixes: vec![lit("/work")],
                deny_paths: vec![lit("/x/tools/bad")],
                ..FsPolicy::default()
            }),
        ]);
        let full = running_region(&stack, &floor).unwrap();
        for path in ["/x/prog", "/x/tools/ok", "/x/tools/bad", "/work/f", "/y/zz"] {
            let access = at(path);
            assert_eq!(
                stack.read(&floor, &access).is_ok(),
                permits(&full, &access),
                "{path}"
            );
        }
    }

    /// The meet holds a nested allow through the `\\?\`-verbatim spelling
    /// `std::fs::canonicalize` returns.  Gated because the head fold of
    /// `path_within` fires only under a real `cfg!(windows)` build; the
    /// spellings are the host's own, so no fixture maps them.
    #[cfg(windows)]
    #[test]
    fn the_meet_admits_a_windows_verbatim_prefix_variant() {
        let sub = native(r"\\?\C:\work\sub");
        let met = table(&[native(r"C:\work")], &[]).meet(table(std::slice::from_ref(&sub), &[]));
        assert_eq!(
            live(&met),
            [FsAllow::Prefix(GrantPath::allowing(sub).unwrap())]
        );
    }

    /// An allow holds a name as stored: the meet folds the separator and the
    /// drive letter's case, as `windows_stored_components` does, and nothing
    /// else, so `c:/work/sub` lies within `C:\work` while `C:\WORK\sub` does
    /// not.  Gated like the verbatim test above.
    #[cfg(windows)]
    #[test]
    fn the_meet_folds_a_windows_separator_and_drive_letter_but_not_other_case() {
        let outer = || table(&[native(r"C:\work")], &[]);

        let sub = native("c:/work/sub");
        let met = outer().meet(table(std::slice::from_ref(&sub), &[]));
        assert_eq!(
            live(&met),
            [FsAllow::Prefix(GrantPath::allowing(sub).unwrap())]
        );

        let met = outer().meet(table(&[native(r"C:\WORK\sub")], &[]));
        assert_eq!(live(&met), Vec::<FsAllow>::new());
    }
}
