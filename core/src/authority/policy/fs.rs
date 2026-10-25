//! The fs dimension: a rules table per op, as [`super::exec`] holds the exec
//! dimension.
//!
//! Both readers of fs authority consume it: the in-process guard
//! (`crate::authority::guard`) judges an access by the table, the OS projection
//! (`crate::authority::sandbox::SandboxProjection`) renders its live allows and its
//! denies.  Agreement
//! between guard and sandbox profile is then structural — one fold per op,
//! two consumers — rather than a property two independent folds have to be
//! tested into.
//!
//! A restricted read region is the layers' meet joined with the [`Baseline`]:
//! the platform's fixed read-only system paths any program needs to start.
//! No layer's `read` narrows it; only a `deny` removes a baseline path.  Writing
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
use super::{GrantLayer, GrantStack, Join, Meet};
use crate::path::{
    Depth, Dir, DirEntry, Excluded, Judged, LeafMode, LexicalPath, Located, MatchedBy, ObjectId,
    Op, Reading, RealPath, Subject, Trace, Writing, walk, walk_dir,
};
use serde::{Deserialize, Serialize};
use std::io;
use std::marker::PhantomData;
use strum::IntoStaticStr;

/// Which fs table a check consults: the read or the write one.
#[derive(IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
pub enum FsOp {
    Read,
    Write,
}

/// The grant stack's say-so for the op `O`: what turns an unjudged handle into
/// a judged one.  Its field is private to this module, so [`GrantStack::judge`],
/// [`ReadRegion`] and [`Ungoverned`] alone make one (the last with a read side
/// and a write side, which only [`crate::own`] calls), and these are the
/// constructions to audit.
pub(crate) struct Verdict<O: Op>(PhantomData<O>);

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
    /// The position this allowance holds from; `None` for everywhere.
    pub(crate) fn real(&self) -> Option<&RealPath> {
        match self {
            Self::Prefix(prefix) => prefix.real.real(),
            Self::RunPlace(place) => Some(place),
        }
    }
}

impl FsDeny {
    /// The exclusion as the walk made it: its spelling and the objects it held.
    pub(crate) fn grant(&self) -> &GrantPath<Excluded> {
        &self.0
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
                real: Extent::Under { position, .. },
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

/// Host system reads and boot-image protection, outside the grant stack.
///
/// The image a bundled tool reads and pinned objects no write may alter.
/// Constructed once per runtime; never carried by a grant or over the wire.
/// Grant construction resolves the system reads; `authority.rs` attaches sandbox boot pins.
#[derive(Clone, Debug, Default)]
pub struct Baseline {
    reads: Vec<FsAllow>,
    image: Option<&'static Trace>,
    protected: Vec<(ObjectId, &'static str)>,
}

impl FromIterator<FsAllow> for Baseline {
    fn from_iter<I: IntoIterator<Item = FsAllow>>(allows: I) -> Self {
        Self {
            reads: allows.into_iter().collect(),
            ..Self::default()
        }
    }
}

impl Baseline {
    pub(crate) fn with_images(
        mut self,
        image: Option<&'static Trace>,
        protected: Vec<(ObjectId, &'static str)>,
    ) -> Self {
        self.image = image;
        self.protected = protected;
        self
    }

    pub(crate) fn image(&self) -> Option<&Trace> {
        self.image
    }

    fn protected(&self, id: ObjectId) -> Option<&'static str> {
        self.protected
            .iter()
            .find_map(|&(held, name)| (held == id).then_some(name))
    }

    pub(crate) fn allows(&self) -> impl Iterator<Item = &FsAllow> {
        self.reads.iter()
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
/// its exclusions, met; for `Read`, joined with the [`Baseline`], which only an
/// exclusion removes.  `None` exactly when no layer held an `fs` opinion, so
/// the guard is unrestricted and the projection needs no fs rules; a layer
/// that opined and admitted nothing withholds support beyond the baseline.
pub(crate) fn region(grants: &GrantStack, op: &FsOp, baseline: &Baseline) -> Option<FsRules> {
    fold(grants, op, baseline).map(|f| f.plain)
}

/// The read table where each layer also reads the run places of the exec meet
/// over it and the layers beneath it: no layer makes anything readable for
/// those beneath it.
pub(crate) fn running_region(grants: &GrantStack, baseline: &Baseline) -> Option<FsRules> {
    fold(grants, &FsOp::Read, baseline).map(|f| f.placed)
}

/// Both tables for `op` in one pass over the layers.
fn fold(grants: &GrantStack, op: &FsOp, baseline: &Baseline) -> Option<Folded> {
    let layers: Vec<&GrantLayer> = grants.layers().collect();
    let last = layers.iter().rposition(|l| l.fs().is_some())?;
    let mut run: Option<ExecRules> = None;
    let (mut plain, mut placed): (Option<FsRules>, Option<FsRules>) = (None, None);
    for layer in &layers[..=last] {
        if let Some(exec) = layer.exec() {
            meet_onto(&mut run, exec.clone());
        }
        let Some(fs) = layer.fs() else { continue };
        let table = fs.table(op).clone();
        let places = run.iter().flat_map(ExecRules::places);
        meet_onto(&mut placed, table.clone().join(Table::new(places, [])));
        meet_onto(&mut plain, table);
    }
    let with_system_reads = |met: FsRules| match op {
        FsOp::Read => met.join(Table::new(baseline.allows().cloned(), [])),
        FsOp::Write => met,
    };
    Some(Folded {
        plain: with_system_reads(plain?),
        placed: with_system_reads(placed?),
    })
}

/// An object a judgment refused, handed back with what withheld it.
pub(crate) type Returned = Box<(Located, Withheld<FsDeny>)>;

/// The read and running regions folded once, for a walk that judges many
/// objects; `None` where no layer held an fs opinion.
pub struct ReadRegion(Option<Folded>);

/// A table and the same with each layer's run places joined.  A permit or an
/// exclusion of `plain`, which holds no run places, is final with the same
/// witness; only a missing allowance asks `placed`, whose run places add
/// positive evidence alone.
struct Folded {
    plain: FsRules,
    placed: FsRules,
}

/// The user's own authority: no grant governs these reads and writes.
///
/// The second minting point beside [`GrantStack::judge`]; clippy bans its
/// methods except at documented typed sites. Its read side is public; its
/// write side is crate-private, and [`crate::own`] is its one caller.
pub struct Ungoverned;

impl Ungoverned {
    pub fn read(l: Located) -> Located<Judged<Reading>> {
        l.judged(Verdict(PhantomData))
    }

    pub fn read_dir(d: Dir) -> Dir<Judged<Reading>> {
        d.judged(Verdict(PhantomData))
    }

    pub fn region() -> ReadRegion {
        ReadRegion(None)
    }

    /// The write side of the user's own authority: the program's own
    /// directories, the files the user writes at its places, and the one new
    /// file a user names. [`crate::own`] is the one caller, with a typed site
    /// documenting why no grant governs it.
    pub(crate) fn write(l: Located) -> Located<Judged<Writing>> {
        l.judged(Verdict(PhantomData))
    }

    /// [`write`](Self::write), for a directory.
    pub(crate) fn write_dir(d: Dir) -> Dir<Judged<Writing>> {
        d.judged(Verdict(PhantomData))
    }
}

impl ReadRegion {
    fn withheld(&self, s: Subject<'_>) -> Result<(), Withheld<FsDeny>> {
        let Some(Folded { plain, placed }) = &self.0 else {
            return Ok(());
        };
        match plain.withheld(s) {
            None => Ok(()),
            Some(Withheld::MissingAllowance) => placed.withheld(s).map_or(Ok(()), Err),
            Some(excluded) => Err(excluded),
        }
    }

    /// Judge the directory `d` for reading.
    ///
    /// # Errors
    /// What withholds it.
    pub(crate) fn judge(&self, d: Dir) -> Result<Dir<Judged<Reading>>, Withheld<FsDeny>> {
        self.withheld(Subject::Access(d.trace()))?;
        Ok(d.judged(Verdict(PhantomData)))
    }

    /// Judge the object `l` for reading.
    ///
    /// # Errors
    /// What withholds it, with `l` handed back.
    pub(crate) fn judge_file(&self, l: Located) -> Result<Located<Judged<Reading>>, Returned> {
        match self.withheld(Subject::Access(l.trace())) {
            Ok(()) => Ok(l.judged(Verdict(PhantomData))),
            Err(why) => Err(Box::new((l, why))),
        }
    }

    /// A listing entry, judged by what the listing saw of it; one it did not
    /// see by its position alone.  Takes the entry, not a position, so no
    /// caller can judge an object it holds by position alone.
    pub fn admits(&self, e: &DirEntry<'_>) -> bool {
        match e.listed() {
            Some(listed) => self.withheld(Subject::Listed(&listed)),
            None => self.withheld(Subject::Position(&e.real())),
        }
        .is_ok()
    }
}

/// Why a judgment refused.  `Guarded` is the pinned-binary veto: a write onto
/// ral's own image or bwrap's, refused before the stack is consulted and
/// under an unrestricted stack too; `Withheld` is the stack's.
#[derive(Debug)]
pub enum Refused {
    Guarded(&'static str),
    Withheld(Withheld<FsDeny>),
}

/// Why a [`GrantStack::locate`] gave no handle: the walk failed, or the stack
/// refused what it found.
#[derive(Debug)]
pub enum LocateError {
    Io(io::Error),
    Refused(Refused),
}

impl GrantStack {
    /// The read judgment: may the access `trace` be read?
    pub(crate) fn read(&self, baseline: &Baseline, trace: &Trace) -> Result<(), Withheld<FsDeny>> {
        self.read_region(baseline).withheld(Subject::Access(trace))
    }

    /// The read and running regions, folded now.
    pub fn read_region(&self, baseline: &Baseline) -> ReadRegion {
        ReadRegion(fold(self, &FsOp::Read, baseline))
    }

    /// The decision on `op` for the access `trace`: under `Write`, first the
    /// pinned-binary veto, before the stack, since an empty stack is exactly
    /// the case it must catch; then the fold for `op`.  The exclusions it
    /// decides against are the ones their walks held; the sandbox's
    /// projection folds the same tables once, at spawn.
    ///
    /// # Errors
    /// [`Refused`], the veto's or the stack's.
    pub(crate) fn refusal_fs(
        &self,
        baseline: &Baseline,
        op: &FsOp,
        trace: &Trace,
    ) -> Result<(), Refused> {
        let withheld = match op {
            FsOp::Write => {
                if let Some(pinned) = trace.leaf_id().and_then(|id| baseline.protected(id)) {
                    return Err(Refused::Guarded(pinned));
                }
                self.write(baseline, trace)
            }
            FsOp::Read => self.read(baseline, trace),
        };
        withheld.map_err(Refused::Withheld)
    }

    /// [`refusal_fs`](Self::refusal_fs) as a plain bool, for a caller with
    /// nothing to audit through or a side read it may forgo.
    pub fn admits_fs(&self, baseline: &Baseline, op: &FsOp, trace: &Trace) -> bool {
        self.refusal_fs(baseline, op, trace).is_ok()
    }

    /// Judge `l` for `O`: [`refusal_fs`](Self::refusal_fs), then, admitted,
    /// the handle vouched for.  The one place evidence becomes authority, with
    /// [`ReadRegion`] and [`Ungoverned`].
    ///
    /// # Errors
    /// [`Refused`], the veto's or the stack's.
    pub fn judge<O: Op>(
        &self,
        baseline: &Baseline,
        l: Located,
    ) -> Result<Located<Judged<O>>, Refused> {
        self.refusal_fs(baseline, &O::FS, l.trace())?;
        Ok(l.judged(Verdict(PhantomData)))
    }

    /// [`judge`](Self::judge) for a directory.
    ///
    /// # Errors
    /// As [`judge`](Self::judge).
    pub fn judge_dir<O: Op>(&self, baseline: &Baseline, d: Dir) -> Result<Dir<Judged<O>>, Refused> {
        self.refusal_fs(baseline, &O::FS, d.trace())?;
        Ok(d.judged(Verdict(PhantomData)))
    }

    /// A caller with no `Shell`: the walk, then [`judge`](Self::judge).
    ///
    /// # Errors
    /// [`LocateError`], keeping the walk's failure apart from the judgment's.
    pub fn locate<O: Op>(
        &self,
        baseline: &Baseline,
        rp: &LexicalPath,
    ) -> Result<Located<Judged<O>>, LocateError> {
        let located = walk(rp, LeafMode::Follow).map_err(LocateError::Io)?;
        self.judge(baseline, located).map_err(LocateError::Refused)
    }

    /// [`locate`](Self::locate) for a directory entered.
    ///
    /// # Errors
    /// As [`locate`](Self::locate).
    pub fn locate_dir<O: Op>(
        &self,
        baseline: &Baseline,
        rp: &LexicalPath,
    ) -> Result<Dir<Judged<O>>, LocateError> {
        let dir = walk_dir(rp).map_err(LocateError::Io)?;
        self.judge_dir(baseline, dir).map_err(LocateError::Refused)
    }

    /// The write judgment: may the access `trace` be written?
    pub(crate) fn write(&self, baseline: &Baseline, trace: &Trace) -> Result<(), Withheld<FsDeny>> {
        let region = region(self, &FsOp::Write, baseline);
        region
            .and_then(|r| r.withheld(Subject::Access(trace)))
            .map_or(Ok(()), Err)
    }

    /// Whether only an exclusion withholds the read of `trace`, which is all a
    /// bundled tool needs of ral's binary.
    pub(crate) fn read_not_excluded(
        &self,
        baseline: &Baseline,
        trace: &Trace,
    ) -> Result<(), Withheld<FsDeny>> {
        let withheld =
            region(self, &FsOp::Read, baseline).and_then(|r| r.withheld(Subject::Access(trace)));
        match withheld {
            Some(excluded @ Withheld::Excluded { .. }) => Err(excluded),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::authority::grant::freeze::Home;
    use crate::authority::grant::host::{resolve_reads, windows_listed};
    use crate::authority::policy::{Capabilities, ExecDenial, ExecGrant, ExecKey, Program};
    use crate::authority::policy::{Evidence, FsPolicy};
    use crate::path::FrozenPath;
    use crate::path::utf8;
    use crate::test_helper::{Tree, core_runtime, layer, located, real, stack, trace};
    use std::path::{Path, PathBuf};

    /// A `/`-rooted fixture name as a spelling on this host: a name below a
    /// place nothing occupies, `/` itself the universal prefix.
    fn lit(s: &str) -> FrozenPath {
        match s {
            "/" => FrozenPath::root(),
            _ => FrozenPath::from_surface(&Tree::nowhere().at(s)),
        }
    }

    /// A Windows spelling taken exactly as written: the `cfg(windows)` tests
    /// below judge the host's own spellings, which no [`lit`] mapping may
    /// rewrite.
    #[cfg(windows)]
    fn native(s: &str) -> FrozenPath {
        FrozenPath::from_surface(s)
    }

    /// The access to `path`, a `/`-rooted fixture name, walked as it is: an
    /// absent path is its own lexical position.
    fn at(path: &str) -> Trace {
        trace(Tree::nowhere().path(path))
    }

    fn table(allows: &[FrozenPath], denies: &[FrozenPath]) -> FsRules {
        FsRules::new(
            (allows.iter().cloned()).map(|p| FsAllow::Prefix(GrantPath::<Trace>::walk(p).unwrap())),
            (denies.iter().cloned()).map(|p| FsDeny(GrantPath::<Excluded>::walk(p).unwrap())),
        )
    }

    fn live(region: &FsRules) -> Vec<FsAllow> {
        region.live().cloned().collect()
    }

    fn prefixes(paths: &[&str]) -> Vec<FsAllow> {
        (paths.iter())
            .map(|p| FsAllow::Prefix(GrantPath::<Trace>::walk(lit(p)).unwrap()))
            .collect()
    }

    fn permits(region: &FsRules, access: &Trace) -> bool {
        region.evidence(Subject::Access(access)).permits()
    }

    /// A tempdir and the real path of its root.
    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = real(tmp.path()).as_path().to_path_buf();
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
                scope: Box::new(FsDeny(
                    GrantPath::<Excluded>::walk(lit("/d/Secrets")).unwrap()
                )),
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
        let root = GrantPath::<Trace>::walk(FrozenPath::root()).unwrap();
        assert_eq!(root.real, Extent::Everywhere);
        let region = table(&[FrozenPath::root()], &[]);
        // A drive spelling is a path only on Windows, and `D:` may not exist.
        let mut positions = vec![at("/etc/hosts").real().clone()];
        if cfg!(windows) {
            positions.extend(
                [r"C:\Users\someone\Temp\x", r"D:\a\repo\y"].map(RealPath::unwalked_for_test),
            );
        }
        for position in positions {
            assert!(
                region.evidence(Subject::Position(&position)).permits(),
                "{position}"
            );
        }
    }

    /// A baseline entry that resolves through a non-Unicode name is dropped, not
    /// held as a sibling its lossy form names.  APFS refuses such a name.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_non_unicode_system_reads_entry_is_dropped() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let odd = dir.path().join(std::ffi::OsStr::from_bytes(b"f\xff"));
        std::fs::create_dir(&odd).unwrap();
        let fine = dir.path().join("fine");
        std::fs::create_dir(&fine).unwrap();
        let baseline = resolve_reads([odd, fine.clone()], None)
            .unwrap()
            .into_iter()
            .collect::<Baseline>();
        assert_eq!(
            baseline.allows().collect::<Vec<_>>(),
            [&FsAllow::Prefix(
                GrantPath::<Trace>::walk(spelled(&fine)).unwrap()
            )]
        );
    }

    /// `baseline` holds exactly the prefixes at `dirs`, each an existing
    /// directory, in order.
    fn assert_holds(baseline: &Baseline, dirs: &[&Path]) {
        let want: Vec<_> = (dirs.iter())
            .map(|d| FsAllow::Prefix(GrantPath::<Trace>::walk(spelled(d)).unwrap()))
            .collect();
        assert_eq!(baseline.allows().cloned().collect::<Vec<_>>(), want);
    }

    /// The root of this host's filesystem, as a baseline spelling.
    fn host_root() -> PathBuf {
        PathBuf::from(if cfg!(windows) { r"C:\" } else { "/" })
    }

    /// A listed directory that is a filesystem root is no baseline and no error,
    /// as a toolchain row's is not: the root would hold the whole filesystem.
    #[test]
    fn a_listed_root_is_dropped_and_an_ordinary_directory_kept() {
        let (_tmp, root) = tmp();
        let ordinary = made(&root, "ordinary");
        let baseline = resolve_reads([host_root(), ordinary.clone()], None)
            .unwrap()
            .into_iter()
            .collect::<Baseline>();
        assert_holds(&baseline, &[&ordinary]);
    }

    /// The home, what lies inside it and what it lies inside are the user's
    /// to grant or the world: a listed directory so placed is dropped, and
    /// one beside the home is kept.
    #[test]
    fn a_listed_directory_in_the_home_or_around_it_is_dropped() {
        let (_tmp, root) = tmp();
        let home = made(&root, "h");
        let (inside, beside) = (made(&home, "inside"), made(&root, "beside"));
        let held = Home::of(utf8(&home).unwrap()).unwrap().real;
        assert!(held.is_some(), "an absolute home lies somewhere");
        let listed = [root, home, inside, beside.clone()];
        let baseline = resolve_reads(listed, held.as_ref())
            .unwrap()
            .into_iter()
            .collect::<Baseline>();
        assert_holds(&baseline, &[&beside]);
    }

    /// An environment that names a root as one of Windows' system directories
    /// puts no root on the baseline; the variable that is unset lists nothing.
    #[test]
    fn a_windows_system_variable_naming_a_root_is_dropped() {
        let (_tmp, root) = tmp();
        let programs = made(&root, "programs");
        let (drive, programs_at) = (
            utf8(&host_root()).unwrap().to_owned(),
            utf8(&programs).unwrap(),
        );
        let pairs = [
            ("SystemRoot", drive.as_str()),
            ("ProgramFiles", programs_at),
        ];
        let listed = windows_listed(vars(&pairs));
        assert_eq!(listed, [drive.as_str(), programs_at, ""]);
        let baseline = resolve_reads(listed.map(PathBuf::from), None)
            .unwrap()
            .into_iter()
            .collect::<Baseline>();
        assert_holds(&baseline, &[&programs]);
    }

    /// The same on Windows, through the baseline of a whole environment: no
    /// drive root named by a system variable stays, whatever the rows add.
    #[cfg(windows)]
    #[test]
    fn a_drive_root_in_a_windows_system_variable_is_no_system_reads() {
        let drive = r"C:\";
        let pairs = [
            ("SystemRoot", drive),
            ("ProgramFiles", drive),
            ("ProgramFiles(x86)", drive),
        ];
        let baseline = Baseline::of_env(vars(&pairs), None).unwrap();
        for allow in baseline.allows() {
            let FsAllow::Prefix(dir) = allow else {
                panic!("the baseline holds prefixes: {allow:?}");
            };
            let below_a_root = dir.real.real().is_some_and(|p| !p.is_root());
            assert!(below_a_root, "{dir:?}");
        }
    }

    /// An environment as the fixtures give it.
    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| (pairs.iter()).find_map(|&(var, v)| (var == k).then(|| v.to_owned()))
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
    fn region_of(
        op: &FsOp,
        allow: &[FrozenPath],
        deny: &[FrozenPath],
        baseline: &Baseline,
    ) -> FsRules {
        let stack = stack([fs_layer(FsPolicy {
            read_prefixes: allow.to_vec(),
            write_prefixes: allow.to_vec(),
            deny_paths: deny.to_vec(),
        })]);
        region(&stack, op, baseline).expect("fs opined")
    }

    #[test]
    fn the_read_region_admits_the_system_reads_a_layer_never_named() {
        let runtime = core_runtime();
        let read = region_of(&FsOp::Read, &[lit("/work")], &[], runtime.baseline());
        for f in runtime.baseline().allows() {
            assert!(read.evidence(f.own()).permits(), "{f:?}");
        }
    }

    #[test]
    fn an_exclusion_refuses_a_system_reads_path() {
        let runtime = core_runtime();
        let f = runtime
            .baseline()
            .allows()
            .next()
            .expect("a host has a baseline");
        let FsAllow::Prefix(prefix) = f else {
            panic!("the baseline holds prefixes: {f:?}");
        };
        let read = region_of(
            &FsOp::Read,
            &[lit("/work")],
            std::slice::from_ref(&prefix.written),
            runtime.baseline(),
        );
        assert_eq!(read.evidence(f.own()), Evidence::BOTH);
        assert!(matches!(
            read.withheld(f.own()),
            Some(Withheld::Excluded { .. })
        ));
    }

    #[test]
    fn the_write_region_admits_no_system_reads_path() {
        let runtime = core_runtime();
        let write = region_of(&FsOp::Write, &[lit("/work")], &[], runtime.baseline());
        for f in runtime.baseline().allows() {
            assert!(!write.evidence(f.own()).permits(), "{f:?}");
        }
    }

    #[test]
    fn an_empty_read_admits_exactly_the_system_reads() {
        let runtime = core_runtime();
        let read = region_of(&FsOp::Read, &[], &[], runtime.baseline());
        assert!((runtime.baseline().allows()).all(|f| read.evidence(f.own()).permits()));
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
        let baseline = Baseline::default();
        assert!(stack.read(&baseline, &at("/work/f")).is_ok());
        assert!(stack.write(&baseline, &at("/work/f")).is_ok());
        assert!(stack.read(&baseline, &at("/other/f")).is_err());
    }

    /// Reading a folder is not writing it; writing one reads it.
    #[test]
    fn a_reading_layer_does_not_write_and_a_writing_layer_reads() {
        let (_tmp, root) = tmp();
        let work = made(&root, "work");
        let (baseline, file) = (Baseline::default(), trace(work.join("f")));
        let reads = stack([fs_layer(FsPolicy {
            read_prefixes: vec![spelled(&work)],
            ..FsPolicy::default()
        })]);
        assert!(reads.read(&baseline, &file).is_ok());
        assert!(matches!(
            reads.write(&baseline, &file),
            Err(Withheld::MissingAllowance)
        ));
        let writes = stack([fs_layer(FsPolicy {
            write_prefixes: vec![spelled(&work)],
            ..FsPolicy::default()
        })]);
        assert!(writes.read(&baseline, &file).is_ok());
        assert!(writes.write(&baseline, &file).is_ok());
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

    /// The baseline is an argument of the judgment: with it a read admits the
    /// baseline and what the layer names; without it, only the latter.
    #[test]
    fn read_with_and_without_the_system_reads_differ_exactly_on_the_system_reads() {
        let (_tmp, root) = tmp();
        let (work, system, other) = (
            made(&root, "work"),
            made(&root, "system"),
            made(&root, "other"),
        );
        let baseline: Baseline = resolve_reads([system.clone()], None)
            .unwrap()
            .into_iter()
            .collect();
        let reads = stack([fs_layer(FsPolicy {
            read_prefixes: vec![spelled(&work)],
            ..FsPolicy::default()
        })]);
        for (path, named, system_read) in [
            (work.join("f"), true, false),
            (system.join("lib"), false, true),
            (other.join("f"), false, false),
        ] {
            let access = trace(path);
            assert_eq!(reads.read(&Baseline::default(), &access).is_ok(), named);
            assert_eq!(reads.read(&baseline, &access).is_ok(), named || system_read);
        }
    }

    /// Law 7: a layer that reads nothing runs a program under the baseline, and
    /// is refused for reading it where the baseline is absent.
    #[test]
    fn a_program_under_the_system_reads_alone_is_readable_to_run() {
        let (_tmp, root) = tmp();
        let system = made(&root, "system");
        std::fs::write(system.join("prog"), "").unwrap();
        let baseline: Baseline = resolve_reads([system.clone()], None)
            .unwrap()
            .into_iter()
            .collect();
        let program = Program::File {
            located: Box::new(located(system.join("prog"))),
            arg0: system.join("prog"),
        };
        let reads_nothing = stack([fs_layer(FsPolicy::default())]);
        assert!(reads_nothing.admits(&baseline, &program));
        assert!(matches!(
            reads_nothing.refusal(&Baseline::default(), &program),
            Some(ExecDenial::Read(_))
        ));
    }

    /// A host that cannot walk its own baseline cannot make a runtime, and says
    /// which spelling: here the home, which the toolchain rows are judged by.
    #[cfg(unix)]
    #[test]
    fn a_system_reads_that_cannot_be_walked_refuses_construction_naming_it() {
        let (_tmp, root) = tmp();
        let (a, b) = (root.join("a"), root.join("b"));
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        let home = a.join("home");
        let Err(why) = Baseline::of_home(utf8(&home).ok()) else {
            panic!("a baseline through a link cycle cannot be walked");
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
        let read = region(&stack, &FsOp::Read, &Baseline::default()).unwrap();
        assert!(permits(&read, &trace(real.join("f"))));
        assert!(!permits(&read, &trace(surface.join("f"))));
    }

    /// An enclosing grant's run places are readable by a nested fs-only
    /// restriction; the nested grant's own are not made readable for those
    /// beneath it.
    #[test]
    fn pairing_looks_down_the_stack() {
        let baseline = Baseline::default();
        let reads_work = fs_layer(FsPolicy {
            read_prefixes: vec![lit("/work")],
            ..FsPolicy::default()
        });
        let nested = stack([running(&["/x/prog"]), reads_work]);
        assert!(nested.read(&baseline, &at("/x/prog")).is_ok());

        // Order: a layer that reads nothing and omits exec, beneath a layer
        // that runs `s` and omits fs, refuses `s`; the other way round permits.
        let reads_nothing = fs_layer(FsPolicy::default());
        let beneath = stack([reads_nothing.clone(), running(&["/x/prog"])]);
        assert!(beneath.read(&baseline, &at("/x/prog")).is_err());
        let above = stack([running(&["/x/prog"]), reads_nothing]);
        assert!(above.read(&baseline, &at("/x/prog")).is_ok());
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
        let why = stack.read(&Baseline::default(), &at("/x/prog"));
        assert!(matches!(why, Err(Withheld::Excluded { .. })), "{why:?}");
    }

    /// The read judgment's two calls agree with the region of run places.
    #[test]
    fn the_read_judgment_agrees_with_the_region_of_places() {
        let baseline = Baseline::default();
        let stack = stack([
            running(&["/x/prog", "/x/tools"]),
            fs_layer(FsPolicy {
                read_prefixes: vec![lit("/work")],
                deny_paths: vec![lit("/x/tools/bad")],
                ..FsPolicy::default()
            }),
        ]);
        let full = running_region(&stack, &baseline).unwrap();
        for path in ["/x/prog", "/x/tools/ok", "/x/tools/bad", "/work/f", "/y/zz"] {
            let access = at(path);
            assert_eq!(
                stack.read(&baseline, &access).is_ok(),
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
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let sub = native(&format!(r"\\?\{}\sub", utf8(&work).unwrap()));
        let met = table(&[spelled(&work)], &[]).meet(table(std::slice::from_ref(&sub), &[]));
        assert_eq!(
            live(&met),
            [FsAllow::Prefix(GrantPath::<Trace>::walk(sub).unwrap())]
        );
    }

    /// A present directory's spellings normalise equally, whatever their
    /// separator, drive letter's case or, on a case-insensitive volume, name
    /// case; absent components keep the case written and so differ.  Gated
    /// like the verbatim test above.
    #[cfg(windows)]
    #[test]
    fn the_meet_folds_a_windows_spelling_of_a_present_directory_and_keeps_an_absent_name_s_case() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let outer = || table(&[spelled(&work)], &[]);
        let s = utf8(&work).unwrap();
        // `c:/…/work/sub`: separator and drive case fold.
        let slashed = format!(
            "{}{}/sub",
            s[..1].to_ascii_lowercase(),
            s[1..].replace('\\', "/")
        );
        let sub = native(&slashed);
        let met = outer().meet(table(std::slice::from_ref(&sub), &[]));
        assert_eq!(
            live(&met),
            [FsAllow::Prefix(GrantPath::<Trace>::walk(sub).unwrap())]
        );
        // `…\WORK\sub`: a present directory is held under its stored name, so
        // the spelling's case is the volume's business and the allow survives.
        let upper = native(&format!("{}\\WORK\\sub", utf8(tmp.path()).unwrap()));
        let met = outer().meet(table(std::slice::from_ref(&upper), &[]));
        assert_eq!(
            live(&met),
            [FsAllow::Prefix(GrantPath::<Trace>::walk(upper).unwrap())]
        );
        // Absent components keep the case written: `ABSENT` is not `absent`.
        let absent = || table(&[spelled(&tmp.path().join("absent"))], &[]);
        let other = native(&format!("{}\\ABSENT\\sub", utf8(tmp.path()).unwrap()));
        assert_eq!(
            live(&absent().meet(table(&[other], &[]))),
            Vec::<FsAllow>::new()
        );
    }

    /// A stack that reads under `root` but for the `denied` names of it.
    fn reading_all_but(root: &Path, denied: &[&str]) -> GrantStack {
        stack([layer(Capabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec![spelled(root)],
                deny_paths: denied
                    .iter()
                    .map(|name| spelled(&root.join(name)))
                    .collect(),
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        })])
    }

    fn held_dir(path: &Path) -> Dir {
        let spelling = LexicalPath::absolute(path).expect("a temp path is absolute");
        crate::path::walk_dir(&spelling).unwrap()
    }

    #[test]
    fn a_read_region_refuses_a_denied_directory() {
        let (_tmp, root) = tmp();
        let (open, secret) = (made(&root, "open"), made(&root, "secret"));
        let region = reading_all_but(&root, &["secret"]).read_region(&Baseline::default());
        assert!(region.judge(held_dir(&open)).is_ok());
        assert!(matches!(
            region.judge(held_dir(&secret)),
            Err(Withheld::Excluded { .. })
        ));
    }

    /// A listing is judged by what it saw of each entry; an entry it could not
    /// state, gone since the names were read, has only its position.
    #[test]
    fn a_listing_is_judged_by_what_it_saw_and_an_entry_gone_by_its_position() {
        let (_tmp, root) = tmp();
        for name in ["ok", "secret", "gone", "secret-gone"] {
            std::fs::write(root.join(name), b"x").unwrap();
        }
        let region =
            reading_all_but(&root, &["secret", "secret-gone"]).read_region(&Baseline::default());
        let Ok(dir) = region.judge(held_dir(&root)) else {
            panic!("the root is readable");
        };
        let listing = dir.read_dir().unwrap();
        std::fs::remove_file(root.join("gone")).unwrap();
        std::fs::remove_file(root.join("secret-gone")).unwrap();
        let judged: Vec<_> = listing
            .map(|entry| {
                let name = entry.name().as_os_str().to_str().unwrap().to_owned();
                (name, entry.listed().is_some(), region.admits(&entry))
            })
            .collect();
        let expected = [
            ("gone", false, true),
            ("ok", true, true),
            ("secret", true, false),
            ("secret-gone", false, false),
        ];
        assert_eq!(
            judged,
            expected.map(|(name, seen, admitted)| (name.to_owned(), seen, admitted))
        );
    }
}
