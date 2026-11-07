//! Exec authority as a [`Table`] of evidence about programs, and the judgment
//! that mints [`Admitted`].
//!
//! A layer's exec is its [`ExecRules`], resolved where the layer is decoded:
//! allowances over host files, bundled tools and directories, and exclusions
//! of files, directories and names, which judge a [`Program`] as any table
//! judges a subject.  A stack's authority is the meet of its layers' tables.
//! [`GrantStack::admit`] is the one judgment; the guard words its refusals
//! and the sandbox renders the same table as the kernel holds it
//! ([`KernelRules`]).

use super::fs::{Baseline, FsAllow, FsDeny};
use super::lattice::{Extent, GrantPath};
use super::table::{Allows, Denies, Table, Withheld};
use super::{GrantStack, Join, Meet};
use crate::authority::grant::value::{ExecTable, Target};
use crate::path::{
    self, Authority, Depth, Excluded, FrozenPath, Judged, Located, MatchedBy, Reading, RealPath,
    Trace, Unjudged, command_name_key,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// A program the stack admitted. Only [`GrantStack::admit`] makes one, so
/// nothing launches unjudged: a file is held as vouched for reading, which
/// running it does.
/// It launches under the stack that made it: each path admits and launches
/// within one machine step.
#[derive(Debug)]
#[must_use]
pub(crate) struct Admitted {
    program: Program<Judged<Reading>>,
}

impl Admitted {
    pub(crate) fn program(&self) -> &Program<Judged<Reading>> {
        &self.program
    }
}

/// Why a stack refused a call.
#[derive(Debug)]
pub(crate) enum ExecDenial {
    Run(Withheld<ExecDeny>),
    /// Allowed to run, but what it reads is not readable (law 7).
    Read(Withheld<FsDeny>),
}

impl GrantStack {
    /// `↓run = ↓exec ∧ ↓read` (law 7): the exec table, then what running
    /// reads, whose judgment of a file vouches for the handle.  A refused
    /// program is dropped, its handle with it.
    pub(crate) fn admit(
        &self,
        baseline: &Baseline,
        program: Program,
    ) -> Result<Admitted, ExecDenial> {
        run_withheld(rules(self).as_ref(), program.subject())?;
        let program = match program {
            Program::Tool(name) => {
                self.tool_read(baseline).map_err(ExecDenial::Read)?;
                Program::Tool(name)
            }
            Program::File { arg0, located } => {
                let located =
                    (self.read_region(baseline).judge(*located)).map_err(ExecDenial::Read)?;
                Program::File {
                    arg0,
                    located: Box::new(located),
                }
            }
        };
        Ok(Admitted { program })
    }

    /// [`admit`](Self::admit) of a name a walk reached no object at: the same
    /// two judgments at the name's transcript, minting nothing.
    pub(crate) fn admit_name(&self, baseline: &Baseline, trace: &Trace) -> Result<(), ExecDenial> {
        run_withheld(
            rules(self).as_ref(),
            Subject::Program(path::Subject::Access(trace)),
        )?;
        self.read(baseline, trace).map_err(ExecDenial::Read)
    }

    /// A bundled tool runs ral's image, which only an fs exclusion withholds.
    fn tool_read(&self, baseline: &Baseline) -> Result<(), Withheld<FsDeny>> {
        baseline
            .image()
            .map_or(Ok(()), |own| self.read_not_excluded(baseline, own))
    }
}

/// Why a supplied exec table withholds `subject`'s run, before what running
/// it reads is asked.
fn run_withheld(rules: Option<&ExecRules>, subject: Subject<'_>) -> Result<(), ExecDenial> {
    rules
        .and_then(|rules| rules.withheld(subject))
        .map_or(Ok(()), |why| Err(ExecDenial::Run(why)))
}

/// What a grant judges, and what the launcher runs.
pub(crate) enum Program<A: Authority = Unjudged> {
    /// A bundled tool, run as ral itself.
    Tool(&'static str),
    /// A host file, the one holder of the object the walk found, judged and
    /// run: `arg0` is the spelling, which the program sees as `argv[0]`.  A
    /// `#!` script sees its real path as its script name: the kernel hands the
    /// interpreter the exec path.
    File {
        arg0: PathBuf,
        located: Box<Located<A>>,
    },
}

impl<A: Authority> Program<A> {
    pub(crate) fn subject(&self) -> Subject<'_> {
        match self {
            Self::Tool(name) => Subject::Tool(name),
            Self::File { located, .. } => Subject::Program(path::Subject::Access(located.trace())),
        }
    }
}

impl<A: Authority> std::fmt::Debug for Program<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tool(name) => f.debug_tuple("Tool").field(name).finish(),
            Self::File { arg0, located } => f
                .debug_struct("File")
                .field("arg0", arg0)
                .field("trace", located.trace())
                .finish(),
        }
    }
}

/// A file by its real path, a bundled tool by name.
impl<A: Authority> std::fmt::Display for Program<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tool(name) => write!(f, "bundled {name}"),
            Self::File { located, .. } => located.real().fmt(f),
        }
    }
}

/// What a grant judges: a program, by where it is; a directory, by where it
/// is; a bundled tool, by name; or what holds every program.  Also what a
/// rule is itself judged at.
#[derive(Clone, Copy)]
pub(crate) enum Subject<'a> {
    Program(path::Subject<'a>),
    Dir(path::Subject<'a>),
    Tool(&'a str),
    Any,
}

/// What an exec allowance admits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum ExecAllow {
    /// Every program.
    Any,
    Dir(GrantPath<Extent>),
    /// One file, at one position, never everywhere.  `found_as` is the bare
    /// name it was found under: provenance, never consulted by judgement.
    /// Entries differing only in `found_as` hold the same subjects, each
    /// holding the other's `own()`, so the meet's nesting obligation holds.
    File {
        written: FrozenPath,
        real: RealPath,
        found_as: Option<String>,
    },
    Tool(String),
}

impl ExecAllow {
    /// This allowance as the kernel holds it; `None` for a bundled tool.
    fn kernel(&self) -> Option<KernelAllow> {
        match self {
            Self::Any => Some(KernelAllow::Any),
            Self::Dir(dir) => Some(KernelAllow::Dir(dir.real.clone())),
            Self::File { real, .. } => Some(KernelAllow::File(real.clone())),
            Self::Tool(_) => None,
        }
    }

    /// The key a grant writes, resolved; `None` for every program, which no
    /// grant writes.
    fn key(&self) -> Option<Key<'_>> {
        match self {
            Self::Any => None,
            Self::Dir(dir) => Some(Key::Dir(&dir.written)),
            Self::File {
                found_as: Some(name),
                ..
            }
            | Self::Tool(name) => Some(Key::Name(name)),
            Self::File { written, .. } => Some(Key::File(written)),
        }
    }
}

/// What an exec exclusion refuses.
///
/// `Name` comes first: the derived order is the order a refusal cites, and a
/// denied bare name is cited as written before the file it was found as.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum ExecDeny {
    Name(Veto),
    Dir(GrantPath<Excluded>),
    File(GrantPath<Excluded>),
}

/// A bare exclusion, a veto on the name everywhere: the name as written, and
/// the [`command_name_key`] it judges by.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct Veto {
    written: String,
    key: String,
}

impl Veto {
    pub(crate) fn of(name: &str) -> Self {
        Self {
            written: name.to_owned(),
            key: command_name_key(name),
        }
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }
}

impl ExecDeny {
    fn rehold(&mut self) {
        match self {
            Self::Dir(path) | Self::File(path) => path.real.rehold(),
            Self::Name(_) => {}
        }
    }

    /// The key a grant writes, resolved.
    fn key(&self) -> Key<'_> {
        match self {
            Self::Name(veto) => Key::Name(&veto.written),
            Self::Dir(dir) => Key::Dir(&dir.written),
            Self::File(file) => Key::File(&file.written),
        }
    }
}

/// As a grant writes it, resolved: `` `dir `absolute '/opt' ``.
impl std::fmt::Display for ExecDeny {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.key().target(), f)
    }
}

/// A rule's key resolved: a bare name, or the spelling a file or directory
/// froze to.  Ordered as a grant lists them, names first.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Key<'a> {
    Name(&'a str),
    File(&'a FrozenPath),
    Dir(&'a FrozenPath),
}

impl Key<'_> {
    fn target(&self) -> Target {
        match *self {
            Self::Name(name) => Target::Name(name.to_owned()),
            Self::File(written) => Target::File(written.into()),
            Self::Dir(written) => Target::Dir(written.into()),
        }
    }
}

/// `keys` as a grant lists them: a set, in order.
fn listed<'a>(keys: impl Iterator<Item = Key<'a>>) -> Vec<Target> {
    let keys: BTreeSet<_> = keys.collect();
    keys.iter().map(Key::target).collect()
}

impl Allows for ExecAllow {
    type Subject<'a> = Subject<'a>;

    /// A tool has no place on disk, and only a program or a directory
    /// speaks to a directory allowance.
    fn contains(&self, subject: Subject<'_>) -> bool {
        match (self, subject) {
            (Self::Any, _) => true,
            (Self::Dir(dir), Subject::Program(at) | Subject::Dir(at)) => dir.real.contains(at),
            (Self::File { real, .. }, Subject::Program(at)) => holds_file(real, at),
            (Self::Tool(tool), Subject::Tool(name)) => tool == name,
            _ => false,
        }
    }

    fn own(&self) -> Subject<'_> {
        match self {
            Self::Any => Subject::Any,
            Self::Dir(dir) => inside(&dir.real),
            Self::File { real, .. } => Subject::Program(path::Subject::Position(real)),
            Self::Tool(name) => Subject::Tool(name),
        }
    }
}

/// A file allowance holds a program at its own position, and nothing else.
fn holds_file(real: &RealPath, at: path::Subject<'_>) -> bool {
    at.position()
        .is_some_and(|at| real.contains(&at) && at.contains(real))
}

/// A directory allowance's own subject: its inside.
fn inside(dir: &Extent) -> Subject<'_> {
    Subject::Dir(match dir {
        Extent::Under { position, .. } => path::Subject::Position(position),
        Extent::Everywhere => path::Subject::Everywhere,
    })
}

/// An exec allowance as the kernel is handed it: no bundled tool, which the
/// kernel sees as ral's own binary.  It holds what its [`ExecAllow`] holds.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum KernelAllow {
    Any,
    Dir(Extent),
    File(RealPath),
}

impl Allows for KernelAllow {
    type Subject<'a> = Subject<'a>;

    fn contains(&self, subject: Subject<'_>) -> bool {
        match (self, subject) {
            (Self::Any, _) => true,
            (Self::Dir(dir), Subject::Program(at) | Subject::Dir(at)) => dir.contains(at),
            (Self::File(real), Subject::Program(at)) => holds_file(real, at),
            _ => false,
        }
    }

    fn own(&self) -> Subject<'_> {
        match self {
            Self::Any => Subject::Any,
            Self::Dir(dir) => inside(dir),
            Self::File(real) => Subject::Program(path::Subject::Position(real)),
        }
    }
}

impl ExecDeny {
    /// The one reading both tables give an exclusion.  A tool has no place
    /// on disk, and only a directory exclusion speaks to a directory.  A
    /// name's key is folded already, so it holds every spelling as stored.
    pub(crate) fn matches(&self, subject: Subject<'_>) -> Option<MatchedBy> {
        let vetoes =
            |key: &str, name: &str| (key == command_name_key(name)).then_some(MatchedBy::Name);
        match (self, subject) {
            (Self::Dir(dir), Subject::Program(at) | Subject::Dir(at)) => {
                dir.real.matches(at, Depth::Within)
            }
            (Self::File(file), Subject::Program(at)) => file.real.matches(at, Depth::Exact),
            (Self::Name(veto), Subject::Tool(name)) => vetoes(veto.key(), name),
            (Self::Name(veto), Subject::Program(at)) => {
                let at = at.position()?;
                vetoes(veto.key(), at.name()?)
            }
            _ => None,
        }
    }
}

impl Denies<ExecAllow> for ExecDeny {
    fn matches(&self, subject: Subject<'_>) -> Option<MatchedBy> {
        Self::matches(self, subject)
    }
}

impl Denies<KernelAllow> for ExecDeny {
    fn matches(&self, subject: Subject<'_>) -> Option<MatchedBy> {
        Self::matches(self, subject)
    }
}

/// Exec authority, as a function from programs to evidence.
pub(crate) type ExecRules = Table<ExecAllow, ExecDeny>;

/// Exec authority as the kernel holds it ([`ExecRules::for_kernel`]): the
/// kernel sees no tools and no argv, so this is its poorer view of the same
/// evidence.
pub(crate) type KernelRules = Table<KernelAllow, ExecDeny>;

/// A rule of either half.
#[derive(Clone, Debug)]
pub(crate) enum Rule {
    Allow(ExecAllow),
    Deny(Box<ExecDeny>),
}

impl FromIterator<Rule> for ExecRules {
    fn from_iter<I: IntoIterator<Item = Rule>>(rules: I) -> Self {
        let (mut allows, mut denies) = (Vec::new(), Vec::new());
        for rule in rules {
            match rule {
                Rule::Allow(allow) => allows.push(allow),
                Rule::Deny(deny) => denies.push(*deny),
            }
        }
        Self::new(allows, denies)
    }
}

impl ExecRules {
    /// This table as the kernel holds it: every exclusion, and every
    /// allowance but the bundled tools.
    pub(crate) fn for_kernel(&self) -> KernelRules {
        KernelRules::new(
            self.allows().filter_map(ExecAllow::kernel),
            self.denies().cloned(),
        )
    }

    /// The fs allowances running makes: each allowed file and directory is a
    /// place, and a directory that holds everywhere reads everywhere, as an
    /// authored `dir /` does.
    pub(crate) fn places(&self) -> impl Iterator<Item = FsAllow> + '_ {
        self.allows().filter_map(|scope| match scope {
            ExecAllow::Dir(
                dir @ GrantPath {
                    real: Extent::Everywhere,
                    ..
                },
            ) => Some(FsAllow::Prefix(dir.clone())),
            ExecAllow::Dir(GrantPath {
                real: Extent::Under {
                    position: place, ..
                },
                ..
            })
            | ExecAllow::File { real: place, .. } => Some(FsAllow::RunPlace(place.clone())),
            _ => None,
        })
    }

    /// Take hold again of every exclusion's objects.
    pub(super) fn rehold(&mut self) {
        self.rehold_with(ExecDeny::rehold);
    }

    /// The table a grant writes, resolved: every rule but every program,
    /// which only an extension makes.
    pub(super) fn written(&self) -> ExecTable {
        ExecTable {
            allow: listed(self.allows().filter_map(ExecAllow::key)),
            deny: listed(self.denies().map(ExecDeny::key)),
        }
    }

    /// Whether this table runs every program.
    pub(super) fn runs_every(&self) -> bool {
        self.allows().any(|a| matches!(a, ExecAllow::Any))
    }
}

impl super::Axis for ExecRules {
    /// A fragment that says anything, over the omitted `t`: every program,
    /// less its exclusions.
    fn join_top(self) -> Option<Self> {
        (self.allows().next().is_some() || self.denies().next().is_some())
            .then(|| self.join(Self::new([ExecAllow::Any], [])))
    }

    fn is_top(&self) -> bool {
        self.runs_every() && self.denies().next().is_none()
    }
}

impl KernelRules {
    /// This table beside `support`, the files a launch needs beyond the
    /// policy (carriers, the linker), joined as allows.  Support yields to
    /// every exclusion, which the caller has already applied to it.
    pub(crate) fn with_support(self, support: &BTreeSet<RealPath>) -> Self {
        self.join(Self::new(
            support.iter().cloned().map(KernelAllow::File),
            [],
        ))
    }

    /// The files a rule admits.
    #[cfg(unix)]
    pub(crate) fn allowed_files(&self) -> impl Iterator<Item = &RealPath> {
        self.live().filter_map(|scope| match scope {
            KernelAllow::File(real) => Some(real),
            _ => None,
        })
    }

    /// The directories a rule admits.
    #[cfg(target_os = "linux")]
    pub(crate) fn allowed_dirs(&self) -> impl Iterator<Item = RealPath> {
        self.live().filter_map(|scope| match scope {
            KernelAllow::Dir(dir) => Some(dir.position().into_owned()),
            _ => None,
        })
    }
}

/// The stack's exec authority; `None` when no layer holds an exec opinion.
pub(crate) fn rules(grants: &GrantStack) -> Option<ExecRules> {
    grants.exec().cloned().reduce(Meet::meet)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
pub(super) mod tests {
    use super::*;
    use crate::authority::grant::name_rules;
    use crate::authority::policy::{Capabilities, Evidence, ExecGrant, ExecKey};
    use crate::path::Trace;
    use crate::path::utf8;
    use crate::test_helper::{Tree, real as walked, trace};
    use std::path::Path;
    use std::sync::LazyLock;

    /// The access to `p`, a `/`-rooted fixture name, walked as it is: an
    /// absent path is its own lexical position.
    fn access(p: &str) -> Trace {
        trace(Tree::nowhere().path(p))
    }

    fn real(p: &str) -> RealPath {
        access(p).real().clone()
    }

    fn written(p: &str) -> FrozenPath {
        match p {
            "/" => FrozenPath::root(),
            _ => FrozenPath::from_surface(&Tree::nowhere().at(p)),
        }
    }

    fn excluded(p: &str) -> GrantPath<Excluded> {
        GrantPath::<Excluded>::walk(written(p)).unwrap()
    }

    pub(crate) const ALLOW: Evidence = Evidence::ALLOW;
    pub(crate) const DENY: Evidence = Evidence::DENY;
    const BOTH: Evidence = Evidence::BOTH;

    fn file_allowance(p: &str) -> ExecAllow {
        ExecAllow::File {
            written: written(p),
            real: real(p),
            found_as: None,
        }
    }

    fn dir_allowance(p: &str) -> ExecAllow {
        ExecAllow::Dir(GrantPath::<Trace>::walk(written(p)).unwrap())
    }

    fn allow_file(p: &str) -> Rule {
        Rule::Allow(file_allowance(p))
    }

    fn deny_file(p: &str) -> Rule {
        Rule::Deny(Box::new(ExecDeny::File(excluded(p))))
    }

    fn allow_dir(p: &str) -> Rule {
        Rule::Allow(dir_allowance(p))
    }

    fn deny_dir(p: &str) -> Rule {
        Rule::Deny(Box::new(ExecDeny::Dir(excluded(p))))
    }

    fn allow_tool(name: &str) -> Rule {
        Rule::Allow(ExecAllow::Tool(name.into()))
    }

    fn veto(name: &str) -> Rule {
        Rule::Deny(Box::new(ExecDeny::Name(Veto::of(name))))
    }

    fn any() -> Rule {
        Rule::Allow(ExecAllow::Any)
    }

    /// `dir /`, which holds every program and no tool.
    fn everywhere() -> Rule {
        Rule::Allow(ExecAllow::Dir(GrantPath::everywhere()))
    }

    /// The rules `e` says of a place, whose allowance and exclusion are given:
    /// an allowance, an exclusion, or both.
    fn say(place: &(ExecAllow, ExecDeny), e: Evidence) -> impl Iterator<Item = Rule> {
        (e.positive.then(|| Rule::Allow(place.0.clone())))
            .into_iter()
            .chain(e.negative.then(|| Rule::Deny(Box::new(place.1.clone()))))
    }

    fn program(at: &RealPath) -> Subject<'_> {
        Subject::Program(path::Subject::Position(at))
    }

    fn of_file(rules: &ExecRules, p: &str) -> Evidence {
        rules.evidence(program(&real(p)))
    }

    #[test]
    fn a_veto_beats_a_file_allow() {
        let rules: ExecRules = [allow_file("/x/bash"), veto("bash")].into_iter().collect();
        assert_eq!(of_file(&rules, "/x/bash"), BOTH);
    }

    /// Neither an exact file nor a deeper directory reopens an exclusion.
    #[test]
    fn an_exclusion_beats_a_file_and_a_deeper_dir() {
        let rules: ExecRules = [allow_file("/x/tool"), deny_dir("/x")]
            .into_iter()
            .collect();
        assert_eq!(of_file(&rules, "/x/tool"), BOTH);
        assert_eq!(of_file(&rules, "/x/other"), DENY);
        let rules: ExecRules = [deny_dir("/x"), allow_dir("/x/sub")].into_iter().collect();
        assert_eq!(of_file(&rules, "/x/sub/tool"), BOTH);
        assert_eq!(of_file(&rules, "/x/tool"), DENY);
    }

    #[test]
    fn no_rule_is_neither_and_withheld_as_a_missing_allowance() {
        let rules = ExecRules::default();
        assert_eq!(of_file(&rules, "/x/tool"), Evidence::NEITHER);
        assert_eq!(rules.evidence(Subject::Tool("ls")), Evidence::NEITHER);
        assert_eq!(
            rules.withheld(Subject::Tool("ls")),
            Some(Withheld::MissingAllowance)
        );
    }

    /// A tool has no place on disk, so no dir covers it.
    #[test]
    fn a_tool_is_judged_by_tools_and_vetoes_only() {
        let allowed: ExecRules = std::iter::once(allow_tool("ls")).collect();
        assert!(allowed.evidence(Subject::Tool("ls")).permits());
        let covered: ExecRules = [allow_dir("/"), allow_file("/x/bin/ls")]
            .into_iter()
            .collect();
        assert_eq!(covered.evidence(Subject::Tool("ls")), Evidence::NEITHER);
        let vetoed: ExecRules = [allow_tool("rm"), veto("rm")].into_iter().collect();
        assert_eq!(vetoed.evidence(Subject::Tool("rm")), BOTH);
    }

    /// `Any` supports every program, a tool and a file alike, less what is
    /// excluded.
    #[test]
    fn any_supports_everything_less_the_exclusions() {
        let rules: ExecRules = [any(), deny_dir("/x"), veto("rm")].into_iter().collect();
        assert!(rules.evidence(Subject::Tool("ls")).permits());
        assert!(of_file(&rules, "/y/t").permits());
        assert!(!of_file(&rules, "/x/t").permits());
        assert!(!rules.evidence(Subject::Tool("rm")).permits());
        assert_eq!(rules.places().count(), 0);
    }

    /// A deeper allow in one layer cannot lift an exclusion another layer
    /// places above it.
    #[test]
    fn a_stacked_deeper_allow_does_not_lift_an_exclusion() {
        let a: ExecRules = [allow_dir("/a"), deny_dir("/a/b")].into_iter().collect();
        let b: ExecRules = std::iter::once(allow_dir("/a/b/c")).collect();
        assert!(!of_file(&a.meet(b), "/a/b/c/x").permits());
    }

    /// a key one layer never mentioned is not written down as an
    /// exclusion, which would hold every spelling of it.
    #[test]
    fn the_meet_writes_no_default_as_an_exclusion() {
        let a: ExecRules = std::iter::once(allow_dir("/a/B")).collect();
        let b: ExecRules = [allow_dir("/a/B"), allow_dir("/a/b")].into_iter().collect();
        let met = a.meet(b);
        assert!(of_file(&met, "/a/B/x").permits());
        assert_eq!(of_file(&met, "/a/b/x"), Evidence::NEITHER);
        let a: ExecRules = std::iter::once(allow_file("/x/T")).collect();
        let b: ExecRules = [allow_file("/x/T"), allow_file("/x/t")]
            .into_iter()
            .collect();
        let met = a.meet(b);
        assert!(of_file(&met, "/x/T").permits());
        assert_eq!(of_file(&met, "/x/t"), Evidence::NEITHER);
    }

    /// An exclusion is cited as a grant writes it, resolved, and printed as
    /// ral prints a value: a directory under its `dir` marker, apart from a
    /// file exclusion at the same place.
    #[test]
    fn a_directory_exclusion_reads_apart_from_a_file_exclusion() {
        let tools = excluded("/tools");
        let at = tools.written.shown();
        assert_eq!(
            ExecDeny::Dir(tools.clone()).to_string(),
            format!("`dir `absolute '{at}'")
        );
        assert_eq!(
            ExecDeny::File(tools).to_string(),
            format!("`absolute '{at}'")
        );
        assert_eq!(ExecDeny::Name(Veto::of("Bash")).to_string(), "`name 'Bash'");
    }

    /// A file found under a bare name keeps the name it was found as, and the
    /// denial of a bare name holds the file found beside the veto on the name.
    #[test]
    fn a_file_found_by_name_is_cited_by_its_path_and_vetoed_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = walked(tmp.path()).as_path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let file = bin.join(format!("zz-found{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&file, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let search = Some(utf8(&bin).unwrap());
        let found = walked(&file);

        let allows: ExecRules = name_rules::<Trace>("zz-found", search, cfg!(unix))
            .unwrap()
            .into_iter()
            .collect();
        let [
            ExecAllow::File {
                written,
                real,
                found_as,
            },
        ] = allows.allows().collect::<Vec<_>>()[..]
        else {
            panic!("the name finds one file: {allows:?}");
        };
        assert_eq!((real, found_as.as_deref()), (&found, Some("zz-found")));

        let denies: ExecRules = name_rules::<Excluded>("zz-found", search, cfg!(unix))
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(
            denies.denies().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "`name 'zz-found'".to_owned(),
                format!("`absolute '{}'", written.shown()),
            ]
        );
    }

    /// an exclusion holds every spelling of its name, an allow its own.
    #[test]
    fn an_exclusion_holds_every_spelling_and_an_allow_its_own() {
        let (nfc, nfd) = ("/x/caf\u{e9}", "/x/cafe\u{301}");
        let deny = |scope: &ExecDeny, p: &str| scope.matches(program(&real(p)));
        let allow = |scope: &ExecAllow, p: &str| scope.contains(program(&real(p)));
        let (evil, tool) = (
            ExecDeny::Dir(excluded("/x/Evil")),
            ExecDeny::File(excluded("/x/Tool")),
        );
        let (cafe_dir, cafe) = (ExecDeny::Dir(excluded(nfc)), ExecDeny::File(excluded(nfc)));
        let (evil_dir, tool_file) = (dir_allowance("/x/Evil"), file_allowance("/x/Tool"));
        let (cafe_allow_dir, cafe_allow) = (dir_allowance(nfc), file_allowance(nfc));
        assert_eq!(deny(&evil, "/x/evil/t"), Some(MatchedBy::FoldedName));
        assert_eq!(deny(&evil, "/x/Evil/t"), Some(MatchedBy::Name));
        assert_eq!(deny(&evil, "/x/evilx/t"), None);
        assert_eq!(deny(&tool, "/x/tool"), Some(MatchedBy::FoldedName));
        assert_eq!(
            deny(&cafe_dir, &format!("{nfd}/t")),
            Some(MatchedBy::FoldedName)
        );
        assert_eq!(deny(&cafe, nfd), Some(MatchedBy::FoldedName));
        assert!(allow(&evil_dir, "/x/Evil/t"));
        assert!(allow(&tool_file, "/x/Tool"));
        assert!(!allow(&cafe_allow_dir, &format!("{nfd}/t")));
        assert!(!allow(&cafe_allow, nfd));
        assert!(!allow(&evil_dir, "/x/evil/t"));
        assert!(!allow(&tool_file, "/x/tool"));
    }

    /// a folded match is an exclusion as a stored one is: ties are Both,
    /// and neither a deeper rule nor an exact one reopens it.
    #[test]
    fn a_folded_match_excludes_as_a_stored_one() {
        let tools: ExecRules = [allow_dir("/x/Tools"), deny_dir("/x/tools/evil")]
            .into_iter()
            .collect();
        assert!(!of_file(&tools, "/x/Tools/evil/t").permits());
        assert!(of_file(&tools, "/x/Tools/good/t").permits());
        assert!(!of_file(&tools, "/x/tools/evil/t").permits());

        let tie: ExecRules = [allow_dir("/x/b"), deny_dir("/x/B")].into_iter().collect();
        assert_eq!(of_file(&tie, "/x/b/t"), BOTH);

        let exact: ExecRules = [deny_dir("/x/B"), allow_file("/x/b/tool")]
            .into_iter()
            .collect();
        assert!(!of_file(&exact, "/x/b/tool").permits());

        let files: ExecRules = [allow_file("/x/tool"), deny_file("/x/Tool")]
            .into_iter()
            .collect();
        assert_eq!(of_file(&files, "/x/tool"), BOTH);

        let a: ExecRules = [allow_dir("/a"), deny_dir("/a/B")].into_iter().collect();
        let b: ExecRules = [allow_dir("/a/b"), allow_dir("/a/c")].into_iter().collect();
        let met = a.meet(b);
        assert!(!of_file(&met, "/a/b/x").permits());
        assert!(of_file(&met, "/a/c/x").permits());
    }

    /// A folded name is cited only when no exclusion holds the program as
    /// stored.
    #[test]
    fn a_folded_name_is_cited_only_when_no_exclusion_holds_the_stored_name() {
        let program_at = real("/x/evil/t");
        let folded: ExecRules = [allow_dir("/"), deny_dir("/x/Evil")].into_iter().collect();
        assert_eq!(
            folded.withheld(program(&program_at)),
            Some(Withheld::Excluded {
                scope: Box::new(ExecDeny::Dir(excluded("/x/Evil"))),
                matched_by: MatchedBy::FoldedName
            })
        );
        let both: ExecRules = [allow_dir("/"), deny_dir("/x"), deny_dir("/x/Evil")]
            .into_iter()
            .collect();
        assert_eq!(
            both.withheld(program(&program_at)),
            Some(Withheld::Excluded {
                scope: Box::new(ExecDeny::Dir(excluded("/x"))),
                matched_by: MatchedBy::Name
            })
        );
    }

    /// The refusal cites an exclusion that applies, and never one that does
    /// not.
    #[test]
    fn the_witness_is_an_exclusion_that_holds_the_program() {
        let rules: ExecRules = [deny_dir("/X"), deny_file("/x/TOOL"), deny_dir("/y")]
            .into_iter()
            .collect();
        let Some(Withheld::Excluded { scope, .. }) = rules.withheld(program(&real("/x/tool")))
        else {
            panic!("an exclusion holds /x/tool");
        };
        assert!(scope.matches(program(&real("/x/tool"))).is_some());
    }

    /// a veto holds every spelling of the name.
    #[test]
    fn a_veto_holds_every_spelling_of_the_name() {
        let rules: ExecRules = [allow_dir("/x"), veto("sh")].into_iter().collect();
        assert!(!of_file(&rules, "/x/SH").permits());
        assert!(!of_file(&rules, "/x/\u{17f}h").permits());
        assert!(of_file(&rules, "/x/shx").permits());
    }

    /// A tiny xorshift: deterministic and dependency-free.
    pub(crate) struct Rng(pub(crate) usize);

    impl Rng {
        pub(crate) fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }

        pub(crate) fn pick<T: Clone>(&mut self, items: &[T]) -> T {
            items[self.below(items.len())].clone()
        }
    }

    /// Case pairs and an NFC/NFD pair, so the universe holds names one
    /// filesystem keeps apart and another merges.
    const DIRS: [&str; 7] = ["/a", "/a/b", "/a/b/c", "/z", "/A", "/a/B", "/a/b/C"];
    const FILES: [&str; 12] = [
        "/a/x",
        "/a/b/x",
        "/a/b/c/x",
        "/a/b/c/d/x",
        "/z/x",
        "/q/x",
        "/a/b/sh",
        "/a/X",
        "/a/B/x",
        "/a/b/c/X",
        "/a/caf\u{e9}",
        "/a/cafe\u{301}",
    ];
    const PROBES: [&str; 2] = ["/a/b/X", "/A/B/C/x"];
    const TOOLS: [&str; 2] = ["ls", "rm"];
    const NAMES: [&str; 4] = ["x", "sh", "rm", "X"];

    fn evidences() -> [Evidence; 3] {
        [ALLOW, DENY, BOTH]
    }

    /// The access to every path the universe names.
    pub(crate) fn paths() -> Vec<Trace> {
        (FILES.iter().chain(&DIRS).chain(&PROBES))
            .map(|p| access(p))
            .collect()
    }

    /// Each path as a program, by its access and by its position, and as a
    /// directory by its position; everywhere as a directory, each tool, and
    /// every program.
    pub(crate) fn subjects(paths: &[Trace]) -> impl Iterator<Item = Subject<'_>> {
        (paths.iter())
            .flat_map(|t| {
                let at = path::Subject::Position(t.real());
                [
                    Subject::Program(path::Subject::Access(t)),
                    Subject::Program(at),
                    Subject::Dir(at),
                ]
            })
            .chain([Subject::Dir(path::Subject::Everywhere)])
            .chain(TOOLS.map(Subject::Tool))
            .chain([Subject::Any])
    }

    /// The universe's files and directories as the rules that name them,
    /// walked once.
    struct World {
        files: Vec<(ExecAllow, ExecDeny)>,
        dirs: Vec<(ExecAllow, ExecDeny)>,
    }

    static WORLD: LazyLock<World> = LazyLock::new(|| World {
        files: (FILES.iter())
            .map(|f| (file_allowance(f), ExecDeny::File(excluded(f))))
            .collect(),
        dirs: (DIRS.iter())
            .map(|d| (dir_allowance(d), ExecDeny::Dir(excluded(d))))
            .collect(),
    });

    /// A table over the universe, each rule present one time in three.
    pub(crate) fn random(rng: &mut Rng) -> ExecRules {
        let mut rules = Vec::new();
        for place in &WORLD.files {
            if rng.below(3) == 0 {
                rules.extend(say(place, rng.pick(&evidences())));
            }
        }
        for t in TOOLS {
            if rng.below(3) == 0 {
                rules.push(allow_tool(t));
            }
        }
        for place in &WORLD.dirs {
            if rng.below(3) == 0 {
                rules.extend(say(place, rng.pick(&evidences())));
            }
        }
        for n in NAMES {
            if rng.below(6) == 0 {
                rules.push(veto(n));
            }
        }
        if rng.below(6) == 0 {
            rules.push(any());
        }
        if rng.below(6) == 0 {
            rules.push(everywhere());
        }
        rules.into_iter().collect()
    }

    /// The kernel's table is the policy joined with its support, as evidence
    /// at every subject, bundled tools aside.
    #[test]
    fn the_kernel_rules_are_the_table_joined_with_its_support() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let paths = paths();
        for _ in 0..2000 {
            let rules = random(&mut rng);
            let support: BTreeSet<RealPath> = (paths.iter())
                .filter(|_| rng.below(4) == 0)
                .map(|t| t.real().clone())
                .collect();
            let kernel = rules.for_kernel().with_support(&support);
            let supported = support.iter().map(|real| ExecAllow::File {
                written: FrozenPath::from_surface(utf8(real.as_path()).unwrap()),
                real: real.clone(),
                found_as: None,
            });
            let with = rules.clone().join(ExecRules::new(supported, []));
            let everywhere = [Subject::Any, Subject::Dir(path::Subject::Everywhere)];
            for t in &paths {
                let at = path::Subject::Position(t.real());
                for s in [
                    Subject::Program(path::Subject::Access(t)),
                    Subject::Program(at),
                    Subject::Dir(at),
                ]
                .into_iter()
                .chain(everywhere)
                {
                    assert_eq!(
                        kernel.evidence(s),
                        with.evidence(s),
                        "{}\nrules = {rules:?}\nsupport = {support:?}\nkernel = {kernel:?}",
                        t.real()
                    );
                }
            }
        }
    }

    #[test]
    fn support_whose_name_is_vetoed_is_both_to_the_kernel() {
        let rules: ExecRules = std::iter::once(veto("c")).collect();
        let kernel = rules
            .for_kernel()
            .with_support(&std::iter::once(real("/x/c")).collect());
        assert_eq!(kernel.evidence(program(&real("/x/c"))), BOTH);
    }

    #[test]
    fn an_exact_allow_whose_name_is_vetoed_is_not_live() {
        let rules: ExecRules = [allow_file("/x/bash"), veto("bash")].into_iter().collect();
        assert_eq!(rules.live().count(), 0);
        #[cfg(unix)]
        assert_eq!(rules.for_kernel().allowed_files().count(), 0);
    }

    /// The kernel's table drops the bundled tools, and only them.
    #[test]
    fn the_kernel_holds_every_allowance_but_the_tools() {
        let rules: ExecRules = [
            allow_tool("ls"),
            allow_file("/x/ls"),
            allow_dir("/d"),
            any(),
        ]
        .into_iter()
        .collect();
        let kernel: Vec<_> = rules.for_kernel().allows().cloned().collect();
        assert_eq!(
            kernel,
            [
                KernelAllow::Any,
                KernelAllow::Dir(GrantPath::<Trace>::walk(written("/d")).unwrap().real),
                KernelAllow::File(real("/x/ls")),
            ]
        );
    }

    /// The run places are the positive files and directories: a file and a
    /// directory are places, and `dir /` is the prefix at everywhere.
    #[test]
    fn the_places_are_the_positive_files_and_dirs() {
        let rules: ExecRules = [
            allow_file("/a/x"),
            allow_dir("/a/b"),
            deny_dir("/a/b"),
            deny_file("/a/y"),
            allow_tool("ls"),
            any(),
            veto("sh"),
            allow_dir("/"),
        ]
        .into_iter()
        .collect();
        let places: BTreeSet<_> = rules.places().collect();
        assert_eq!(
            places,
            BTreeSet::from([
                FsAllow::Prefix(GrantPath::everywhere()),
                FsAllow::RunPlace(real("/a/b")),
                FsAllow::RunPlace(real("/a/x")),
            ])
        );
    }

    fn exec(grant: ExecGrant) -> Capabilities {
        Capabilities {
            exec: Some(grant),
            ..Capabilities::default()
        }
    }

    /// The rules a grant makes: each key walked, a key in both sets both
    /// kinds of rule.
    fn granting(
        allow: impl IntoIterator<Item = ExecKey>,
        deny: impl IntoIterator<Item = ExecKey>,
    ) -> ExecRules {
        let layer = crate::test_helper::layer(exec(ExecGrant::of(allow, deny)));
        layer.exec().cloned().expect("an exec grant")
    }

    /// The directory `name` of `root`, made.
    fn made(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A tempdir and the real path of its root.
    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = walked(tmp.path()).as_path().to_path_buf();
        (tmp, root)
    }

    fn spelled(path: &Path) -> FrozenPath {
        FrozenPath::from_surface(utf8(path).unwrap())
    }

    fn at(path: &Path) -> RealPath {
        walked(path)
    }

    /// No grant spells every program: `Any` is what extending an omitted
    /// exec axis makes, never what a grant decodes to.
    #[test]
    fn a_grant_resolves_both_sets_and_spells_no_any() {
        let key = ExecKey::Dir(written("/d"));
        let rules = granting([key.clone()], [key]);
        assert_eq!(
            rules.evidence(Subject::Dir(path::Subject::Position(&real("/d")))),
            BOTH
        );
        assert!(!rules.allows().any(|s| *s == ExecAllow::Any), "{rules:?}");
    }

    /// One file, two path keys: an exclusion on the link `a` and an allowance on
    /// its target `b` are one allowance and one exclusion, and hold both.
    #[cfg(unix)]
    #[test]
    fn two_path_keys_naming_one_file_are_both() {
        let (_tmp, root) = tmp();
        let (target, link) = (root.join("b"), root.join("a"));
        std::fs::write(&target, "").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let table = granting(
            [ExecKey::Path(spelled(&target))],
            [ExecKey::Path(spelled(&link))],
        );
        assert_eq!(table.evidence(program(&at(&target))), BOTH);
        assert_eq!((table.allows().count(), table.denies().count()), (1, 1));
    }

    /// An allowed dir that is a symlink covers where it was frozen to.
    #[test]
    fn a_symlinked_allow_dir_covers_its_frozen_target() {
        let (_tmp, root) = tmp();
        let target = made(&root, "x");
        std::fs::write(target.join("tool"), "").unwrap();
        let link = root.join("l");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target, &link).unwrap();
        let rules = granting([ExecKey::Dir(spelled(&link))], []);
        assert!(rules.evidence(program(&at(&target.join("tool")))).permits());
    }

    /// A deeper allow inside a symlinked exclusion's target reopens nothing.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_exclusion_covers_its_target_whatever_sits_beneath() {
        let (_tmp, root) = tmp();
        let target = made(&root, "x");
        made(&target, "sub");
        let link = root.join("l");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let rules = granting(
            [ExecKey::Dir(spelled(&target.join("sub")))],
            [ExecKey::Dir(spelled(&link))],
        );
        assert!(!rules.evidence(program(&at(&target.join("sub")))).permits());
    }

    /// An exclusion is frozen where its name was walked, and holds what it
    /// held: a name that was a link holds the link's position and its target; a
    /// name that was a plain directory keeps that position when it later
    /// becomes a link, and the new target is judged on its own.  An allowance
    /// keeps its position as it was walked.
    #[cfg(unix)]
    #[test]
    fn an_exclusion_holds_what_it_was_walked_to_and_an_allowance_its_position() {
        use crate::path::{LeafMode, LexicalPath, walk_lenient};
        let (_tmp, root) = tmp();
        let blocked = made(&root, "blocked");
        let tools = made(&root, "tools");
        std::fs::write(tools.join("prog"), "").unwrap();
        let prog = at(&tools.join("prog"));
        let key = || ExecKey::Dir(spelled(&blocked));

        let (denied, granted) = (granting([], [key()]), granting([key()], []));
        std::fs::remove_dir(&blocked).unwrap();
        std::os::unix::fs::symlink(&tools, &blocked).unwrap();
        let name = LexicalPath::absolute(&blocked).unwrap();
        let name = walk_lenient(&name, LeafMode::NoFollow).unwrap();
        let name = Subject::Program(path::Subject::Access(&name));
        assert!(denied.evidence(name).negative);
        assert_eq!(denied.evidence(program(&prog)), Evidence::NEITHER);
        assert_eq!(granted.evidence(program(&prog)), Evidence::NEITHER);

        let (denied, granted) = (granting([], [key()]), granting([key()], []));
        let Some(ExecDeny::Dir(held)) = denied.denies().next() else {
            panic!("a directory exclusion: {denied:?}");
        };
        let holds: Vec<_> = (held.real.holds())
            .map(|hold| hold.trace().real().as_path())
            .collect();
        assert_eq!(holds, [blocked.as_path(), tools.as_path()]);
        assert!(denied.evidence(program(&prog)).negative);
        assert_eq!(granted.evidence(program(&prog)), ALLOW);
    }

    /// `live` reads positions: a directory allowance stays where its spelling
    /// walked when the directory is renamed beneath a later exclusion, and the
    /// object at its new place is the exclusion's to refuse.
    #[test]
    fn a_directory_allowance_stays_live_where_its_directory_was_renamed_under_a_deny() {
        let (_tmp, root) = tmp();
        let (dir, denied) = (made(&root, "dir"), made(&root, "denied"));
        let allowed = granting([ExecKey::Dir(spelled(&dir))], []);
        let denying = granting([], [ExecKey::Dir(spelled(&denied))]);
        std::fs::rename(&dir, denied.join("dir")).unwrap();
        let table = allowed.join(denying);
        assert_eq!(table.live().count(), 1, "{table:?}");
        let moved = trace(denied.join("dir").join("t"));
        assert_eq!(
            table.evidence(program(moved.real())),
            DENY,
            "the old position allows nothing the object's new one does"
        );
    }

    /// As above for a file allowance: it is the position, not the object that
    /// moved, and the exclusion that holds the new place leaves it live.
    #[test]
    fn a_file_allowance_stays_live_where_its_object_moved_and_was_denied_elsewhere() {
        let (_tmp, root) = tmp();
        let (bin, denied) = (made(&root, "bin"), made(&root, "denied"));
        std::fs::write(bin.join("tool"), "").unwrap();
        let allowed = granting([ExecKey::Path(spelled(&bin.join("tool")))], []);
        std::fs::rename(bin.join("tool"), denied.join("tool")).unwrap();
        let denying = granting([], [ExecKey::Dir(spelled(&denied))]);
        let table = allowed.join(denying);
        assert!(
            table.live().any(
                |a| matches!(a, ExecAllow::File { real, .. } if real.as_path() == bin.join("tool"))
            ),
            "{table:?}"
        );
        let moved = trace(denied.join("tool"));
        assert_eq!(table.evidence(program(moved.real())), DENY);
    }

    /// `/tmp/bin` and `/private/tmp/bin` are one directory to the guard but
    /// distinct bytes: the exclusion holds the alias.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_firmlink_alias_is_excluded_with_its_directory() {
        let rules: ExecRules = [allow_dir("/private/tmp/bin"), deny_dir("/tmp/bin")]
            .into_iter()
            .collect();
        assert!(!of_file(&rules, "/tmp/bin/evil").permits());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_alias_directory_does_not_reopen_what_a_deeper_exclusion_holds() {
        let rules: ExecRules = [allow_dir("/private/tmp"), deny_dir("/tmp/a/b")]
            .into_iter()
            .collect();
        assert!(!of_file(&rules, "/tmp/a/b/x").permits());
    }

    /// The root is a directory: as a file key it names no program, and an
    /// exclusion of it excludes every path.
    #[test]
    fn a_root_file_key_is_refused() {
        let key = || ExecKey::Path(FrozenPath::root());
        for (grant, wording) in [
            (ExecGrant::of([key()], []), "filesystem root"),
            (ExecGrant::of([], [key()]), "excluding the root"),
        ] {
            let why = crate::test_helper::decoded(exec(grant)).unwrap_err();
            assert!(why.describe().contains(wording), "{}", why.describe());
        }
    }

    /// A file key naming a directory is refused on every route, as a ral
    /// author's is: it names a binary that cannot exist.
    #[test]
    fn a_file_key_naming_a_directory_is_refused() {
        let (_tmp, root) = tmp();
        let dir = made(&root, "tools");
        for grant in [
            ExecGrant::of([ExecKey::Path(spelled(&dir))], []),
            ExecGrant::of([], [ExecKey::Path(spelled(&dir))]),
        ] {
            let why = crate::test_helper::decoded(exec(grant)).unwrap_err();
            assert!(
                why.describe().contains("is a directory"),
                "{}",
                why.describe()
            );
            assert!(
                why.hints().iter().any(|h| h.contains("`dir")),
                "a `dir` hint"
            );
        }
    }

    /// A bare name is a command name, never a path, whoever authored it.
    #[test]
    fn a_path_shaped_name_key_is_refused() {
        let grant = ExecGrant::of([ExecKey::Name("bin/tool".into())], []);
        let why = crate::test_helper::decoded(exec(grant)).unwrap_err();
        assert!(
            why.describe().contains("bare command name"),
            "{}",
            why.describe()
        );
    }

    /// A grant has no "here": a relative `PATH` entry is the environment's, so
    /// a name found only there makes no file rule and keeps its veto.
    #[test]
    fn a_name_on_a_relative_path_entry_makes_no_file_rule() {
        let allow = name_rules::<Trace>("tool", Some("bin"), cfg!(unix)).unwrap();
        assert!(
            !allow
                .iter()
                .any(|r| matches!(r, Rule::Allow(ExecAllow::File { .. }))),
            "{allow:?}"
        );
        let deny = name_rules::<Excluded>("tool", Some("bin"), cfg!(unix)).unwrap();
        assert!(
            matches!(&deny[..], [Rule::Deny(d)] if **d == ExecDeny::Name(Veto::of("tool"))),
            "{deny:?}"
        );
    }
}
