//! Exec authority as a [`Table`] of evidence about programs, and the judgment
//! that mints [`Admitted`].
//!
//! A layer's exec is its [`ExecRules`], resolved where the layer is decoded:
//! allowances over host files, bundled tools and directories, and exclusions
//! of files, directories and names, which judge a [`Program`] as any table
//! judges a subject.  A stack's authority is the meet of its layers' tables.
//! [`GrantStack::admit`] is the one judgment; the guard words its refusals
//! and the sandbox renders the table it judged with.

use super::fs::{Floor, FsAllow, FsDeny};
use super::lattice::{Extent, GrantPath};
use super::table::{Allows, Denies, Table, Withheld};
use super::{ExecGrant, ExecKey, ExecSummary, GrantStack, Join, Meet};
use crate::guard::freeze::PolicyError;
use crate::path::{
    self, Authority, Depth, Excluded, FrozenPath, Judged, Located, MatchedBy, Reading, RealPath,
    Trace, Unjudged, Walked, command_name_key,
};
use crate::syntax::quote::quote_word;
use crate::types::EnvVars;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// A program the stack admitted with these arguments, and the table that
/// admitted it.  Only [`GrantStack::admit`] makes one, so nothing launches
/// unjudged: a file is held as vouched for reading, which running it does.
#[derive(Debug)]
#[must_use]
pub(crate) struct Admitted {
    program: Program<Judged<Reading>>,
    args: Vec<String>,
    rules: Option<ExecRules>,
}

impl Admitted {
    pub(crate) fn program(&self) -> &Program<Judged<Reading>> {
        &self.program
    }

    pub(crate) fn args(&self) -> &[String] {
        &self.args
    }

    /// The stack's exec table, `None` where no layer restricts exec.
    pub(crate) fn rules(&self) -> Option<&ExecRules> {
        self.rules.as_ref()
    }
}

/// Why a stack refused a call.
#[derive(Debug)]
pub(crate) enum ExecDenial {
    Run(Withheld<ExecDeny>),
    /// Allowed to run, but what it reads is not readable (law 7).
    Read(Withheld<FsDeny>),
}

/// A call the stack refused.
#[derive(Debug)]
pub(crate) struct Refused {
    pub program: Program,
    pub args: Vec<String>,
    pub why: ExecDenial,
}

/// ral's own binary, the file a bundled tool runs and so reads: what the boot
/// pin reached, `None` while ral is unpinned.
pub(crate) fn ral_binary() -> Option<&'static Trace> {
    crate::sandbox::reexec::own_binary()
}

impl GrantStack {
    /// Judge a program and its argv against the stack's exec rules, then what
    /// running it reads, which vouches for the file held.
    pub(crate) fn admit(
        &self,
        floor: &Floor,
        program: Program,
        args: Vec<String>,
    ) -> Result<Admitted, Box<Refused>> {
        let rules = rules(self);
        match self.judge_run(floor, rules.as_ref(), program) {
            Ok(program) => Ok(Admitted {
                program,
                args,
                rules,
            }),
            Err(denied) => {
                let (program, why) = *denied;
                Err(Box::new(Refused { program, args, why }))
            }
        }
    }

    /// Head-only admission, before any argv is known.
    pub(crate) fn admits(&self, floor: &Floor, program: &Program) -> bool {
        self.refusal(floor, program).is_none()
    }

    /// Why a head is refused, for a refusal raised before [`GrantStack::admit`].
    pub(crate) fn refusal(&self, floor: &Floor, program: &Program) -> Option<ExecDenial> {
        if let Some(why) = rules(self).and_then(|r| r.withheld(program.subject())) {
            return Some(ExecDenial::Run(why));
        }
        let read = match program {
            Program::File { located, .. } => self.read(floor, located.trace()),
            Program::Tool(_) => self.tool_read(floor),
        };
        read.err().map(ExecDenial::Read)
    }

    /// `↓run = ↓exec ∧ ↓read` (law 7): a supplied exec table must support the
    /// program, then what running it reads must be readable, and a file is
    /// vouched for by that read judgment.  A refusal hands the program back.
    fn judge_run(
        &self,
        floor: &Floor,
        rules: Option<&ExecRules>,
        program: Program,
    ) -> Result<Program<Judged<Reading>>, Box<(Program, ExecDenial)>> {
        if let Some(why) = rules.and_then(|r| r.withheld(program.subject())) {
            return Err(Box::new((program, ExecDenial::Run(why))));
        }
        match program {
            Program::Tool(name) => match self.tool_read(floor) {
                Ok(()) => Ok(Program::Tool(name)),
                Err(why) => Err(Box::new((Program::Tool(name), ExecDenial::Read(why)))),
            },
            Program::File { arg0, located } => match self.read_region(floor).judge_file(*located) {
                Ok(located) => Ok(Program::File {
                    arg0,
                    located: Box::new(located),
                }),
                Err(denied) => {
                    let (located, why) = *denied;
                    let program = Program::File {
                        arg0,
                        located: Box::new(located),
                    };
                    Err(Box::new((program, ExecDenial::Read(why))))
                }
            },
        }
    }

    /// A bundled tool runs ral's image, which only an fs exclusion withholds.
    fn tool_read(&self, floor: &Floor) -> Result<(), Withheld<FsDeny>> {
        ral_binary().map_or(Ok(()), |own| self.read_not_excluded(floor, own))
    }
}

/// What a grant judges, and what the launcher runs.
pub(crate) enum Program<A: Authority = Unjudged> {
    /// A bundled tool, run as ral itself.
    Tool(String),
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
    /// A file a launch needs beyond the policy, which no author wrote.
    pub(crate) fn support(real: &RealPath) -> Self {
        Self::File {
            written: FrozenPath::of_real(real),
            real: real.clone(),
            found_as: None,
        }
    }

    /// The key as a grant spells it, `None` for what holds every program.
    fn spelled(&self) -> Option<String> {
        match self {
            Self::Any => None,
            Self::Dir(dir) => Some(dir_key(&dir.written)),
            Self::File {
                found_as: Some(name),
                ..
            }
            | Self::Tool(name) => Some(name_key(name)),
            Self::File { written, .. } => Some(file_key(written)),
        }
    }
}

/// What an exec exclusion refuses.
///
/// `Name` comes first: the derived order is the order a refusal cites, and a
/// denied bare name is cited as written before the file it was found as.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum ExecDeny {
    /// The [`command_name_key`] of a bare exclusion: a veto on the name
    /// everywhere.
    Name(String),
    Dir(GrantPath<Excluded>),
    File(GrantPath<Excluded>),
}

impl ExecDeny {
    fn rehold(&mut self) {
        match self {
            Self::Dir(path) | Self::File(path) => path.real.rehold(),
            Self::Name(_) => {}
        }
    }
}

fn dir_key(written: &FrozenPath) -> String {
    format!("`dir `absolute {}", quote_word(&written.shown()))
}

fn file_key(written: &FrozenPath) -> String {
    format!("`absolute {}", quote_word(&written.shown()))
}

fn name_key(name: &str) -> String {
    format!("`name {}", quote_word(name))
}

/// As a grant spells it: a directory under its `dir` constructor, a file as
/// its `absolute` place, a name under `name`.
impl std::fmt::Display for ExecDeny {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&match self {
            Self::Dir(dir) => dir_key(&dir.written),
            Self::File(file) => file_key(&file.written),
            Self::Name(name) => name_key(name),
        })
    }
}

impl Allows for ExecAllow {
    type Subject<'a> = Subject<'a>;

    /// A tool has no place on disk, and only a program or a directory
    /// speaks to a directory allowance.
    fn contains(&self, subject: Subject<'_>) -> bool {
        match (self, subject) {
            (Self::Any, _) => true,
            (Self::Dir(dir), Subject::Program(at) | Subject::Dir(at)) => dir.real.contains(at),
            (Self::File { real, .. }, Subject::Program(at)) => at
                .position()
                .is_some_and(|at| real.contains(&at) && at.contains(real)),
            (Self::Tool(tool), Subject::Tool(name)) => tool == name,
            _ => false,
        }
    }

    fn own(&self) -> Subject<'_> {
        match self {
            Self::Any => Subject::Any,
            Self::Dir(GrantPath {
                real: Extent::Under(position),
                ..
            }) => Subject::Dir(path::Subject::Position(position)),
            Self::Dir(GrantPath {
                real: Extent::Everywhere,
                ..
            }) => Subject::Dir(path::Subject::Everywhere),
            Self::File { real, .. } => Subject::Program(path::Subject::Position(real)),
            Self::Tool(name) => Subject::Tool(name),
        }
    }
}

impl Denies<ExecAllow> for ExecDeny {
    /// A tool has no place on disk, and only a directory exclusion speaks to
    /// a directory.  A name's key is folded already, so it holds every
    /// spelling as stored.
    fn matches(&self, subject: Subject<'_>) -> Option<MatchedBy> {
        let vetoes =
            |key: &str, name: &str| (key == command_name_key(name)).then_some(MatchedBy::Name);
        match (self, subject) {
            (Self::Dir(dir), Subject::Program(at) | Subject::Dir(at)) => {
                dir.real.matches(at, Depth::Within)
            }
            (Self::File(file), Subject::Program(at)) => file.real.matches(at, Depth::Exact),
            (Self::Name(key), Subject::Tool(name)) => vetoes(key, name),
            (Self::Name(key), Subject::Program(at)) => {
                let at = at.position()?;
                vetoes(key, at.name()?)
            }
            _ => None,
        }
    }
}

/// Exec authority, as a function from programs to evidence.
pub(crate) type ExecRules = Table<ExecAllow, ExecDeny>;

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

/// How a walked key becomes a rule, by the polarity that walked it: an
/// allowance keeps the position, an exclusion the holds.
pub(crate) trait Keyed: Walked {
    fn file(path: GrantPath<Self>) -> Rule;
    fn dir(path: GrantPath<Self>) -> Rule;
    /// A bare name: the tool bundled under it and the file found on the
    /// `PATH`, and for an exclusion the veto on the name everywhere.
    fn name(name: &str, bundled: bool, found: Option<GrantPath<Self>>) -> Vec<Rule>;
}

impl Keyed for Trace {
    fn file(path: GrantPath<Self>) -> Rule {
        Rule::Allow(ExecAllow::File {
            real: path.real.real().clone(),
            written: path.written,
            found_as: None,
        })
    }

    fn dir(path: GrantPath<Self>) -> Rule {
        Rule::Allow(ExecAllow::Dir(path.extent()))
    }

    fn name(name: &str, bundled: bool, found: Option<GrantPath<Self>>) -> Vec<Rule> {
        let tool = bundled.then(|| Rule::Allow(ExecAllow::Tool(name.into())));
        let file = found.map(|path| {
            Rule::Allow(ExecAllow::File {
                real: path.real.real().clone(),
                written: path.written,
                found_as: Some(name.into()),
            })
        });
        tool.into_iter().chain(file).collect()
    }
}

impl Keyed for Excluded {
    fn file(path: GrantPath<Self>) -> Rule {
        Rule::Deny(Box::new(ExecDeny::File(path)))
    }

    fn dir(path: GrantPath<Self>) -> Rule {
        Rule::Deny(Box::new(ExecDeny::Dir(path)))
    }

    fn name(name: &str, _: bool, found: Option<GrantPath<Self>>) -> Vec<Rule> {
        let file = found.map(|path| Rule::Deny(Box::new(ExecDeny::File(path))));
        let veto = Rule::Deny(Box::new(ExecDeny::Name(command_name_key(name))));
        file.into_iter().chain([veto]).collect()
    }
}

/// The rules a bare `name` makes on `path`: the bundled tool, where this
/// platform bundles it (`unix_available` says whether it counts the Unix-only
/// ones), and the file the `PATH` finds, walked as its polarity walks; a name
/// found nowhere makes no allowance and keeps its veto.
///
/// # Errors
/// A found file whose walk fails, named.
pub(crate) fn name_rules<W: Keyed>(
    name: &str,
    path: Option<&str>,
    unix_available: bool,
) -> Result<Vec<Rule>, PolicyError> {
    let found = path::locate(name, path, None)
        .and_then(|hit| FrozenPath::freeze(&hit).ok())
        .map(GrantPath::<W>::walk)
        .transpose()?;
    Ok(W::name(
        name,
        crate::uutils::bundled(name, unix_available),
        found,
    ))
}

impl ExecRules {
    /// The rules of an authored grant: each key walked as its polarity walks,
    /// a bare name on the `PATH` of `env`.  `every` is [`ExecAllow::Any`].
    ///
    /// # Errors
    /// A spelling that cannot be resolved, named; a root exclusion.
    pub(crate) fn of_grant(grant: &ExecGrant, env: &EnvVars) -> Result<Self, PolicyError> {
        let path = env.get_or_host("PATH");
        let mut rules = Vec::new();
        if grant.every() {
            rules.push(Rule::Allow(ExecAllow::Any));
        }
        for (key, evidence) in grant.iter() {
            if evidence.positive {
                rules.extend(key_rules::<Trace>(key, path.as_deref())?);
            }
            if evidence.negative {
                rules.extend(key_rules::<Excluded>(key, path.as_deref())?);
            }
        }
        Ok(rules.into_iter().collect())
    }

    /// What the kernel is handed: this table beside `support`, the files its
    /// launch needs beyond the policy (carriers, the linker), joined as
    /// allows.  Bundled tools are not rendered, the kernel seeing ral's own
    /// binary for them.  Support yields to every exclusion, which the caller
    /// has already applied to it.
    pub(crate) fn for_kernel(&self, support: &BTreeSet<RealPath>) -> Self {
        let rules = Self::new(
            (self.allows())
                .filter(|scope| !matches!(scope, ExecAllow::Tool(_)))
                .cloned(),
            self.denies().cloned(),
        );
        rules.join(Self::new(support.iter().map(ExecAllow::support), []))
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
                real: Extent::Under(place),
                ..
            })
            | ExecAllow::File { real: place, .. } => Some(FsAllow::RunPlace(place.clone())),
            _ => None,
        })
    }

    /// The files a rule admits.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn allowed_files(&self) -> impl Iterator<Item = &RealPath> {
        self.live().filter_map(|scope| match scope {
            ExecAllow::File { real, .. } => Some(real),
            _ => None,
        })
    }

    /// The directories a rule admits.
    #[cfg(target_os = "linux")]
    pub(crate) fn allowed_dirs(&self) -> impl Iterator<Item = RealPath> {
        self.live().filter_map(|scope| match scope {
            ExecAllow::Dir(dir) => Some(dir.real.real()),
            _ => None,
        })
    }

    /// Take hold again of every exclusion's objects.
    pub(super) fn rehold(&mut self) {
        self.rehold_with(ExecDeny::rehold);
    }

    /// What the table shows of itself: each key as a grant spells it.
    pub(super) fn summary(&self) -> ExecSummary {
        let sorted = |keys: BTreeSet<String>| keys.into_iter().collect();
        ExecSummary {
            every: self.allows().any(|a| matches!(a, ExecAllow::Any)),
            allows: sorted(self.allows().filter_map(ExecAllow::spelled).collect()),
            denies: sorted(self.denies().map(ToString::to_string).collect()),
        }
    }
}

/// The rules of one authored key at polarity `W`.
fn key_rules<W: Keyed>(key: &ExecKey, path: Option<&str>) -> Result<Vec<Rule>, PolicyError> {
    Ok(match key {
        ExecKey::Name(name) => name_rules::<W>(name, path, cfg!(unix))?,
        ExecKey::Path(file) => vec![W::file(GrantPath::walk(file.clone())?)],
        ExecKey::Dir(dir) if dir.is_root() && !W::EXCLUDES => {
            let everywhere = GrantPath::allowing(dir.clone())?;
            vec![Rule::Allow(ExecAllow::Dir(everywhere))]
        }
        ExecKey::Dir(dir) => vec![W::dir(GrantPath::walk(dir.clone())?)],
    })
}

impl super::Axis for ExecRules {
    /// A fragment that says anything, over the omitted `t`: every program,
    /// less its exclusions.
    fn join_top(self) -> Option<Self> {
        (self.allows().next().is_some() || self.denies().next().is_some())
            .then(|| self.join(Self::new([ExecAllow::Any], [])))
    }

    fn is_top(&self) -> bool {
        self.allows().any(|a| matches!(a, ExecAllow::Any)) && self.denies().next().is_none()
    }
}

/// The stack's exec authority; `None` when no layer holds an exec opinion.
pub(crate) fn rules(grants: &GrantStack) -> Option<ExecRules> {
    grants.exec().cloned().reduce(Meet::meet)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs scaffolding: a tempdir file and a link to it"
)]
pub(super) mod tests {
    use super::*;
    use crate::capability::Evidence;
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
        GrantPath::walk(written(p)).unwrap()
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
        ExecAllow::Dir(GrantPath::allowing(written(p)).unwrap())
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
        Rule::Deny(Box::new(ExecDeny::Name(command_name_key(name))))
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

    /// An exclusion is cited as its author wrote it, a directory under its
    /// `dir` marker, apart from a file exclusion at the same place.
    #[test]
    fn a_directory_exclusion_reads_apart_from_a_file_exclusion() {
        let tools = Tree::nowhere().at("/tools");
        let tools = quote_word(&tools);
        assert_eq!(
            ExecDeny::Dir(excluded("/tools")).to_string(),
            format!("`dir `absolute {tools}")
        );
        assert_eq!(
            ExecDeny::File(excluded("/tools")).to_string(),
            format!("`absolute {tools}")
        );
        assert_eq!(ExecDeny::Name("bash".into()).to_string(), "`name bash");
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
                format!("`name {}", quote_word("zz-found")),
                format!("`absolute {}", quote_word(&written.shown())),
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
            let kernel = rules.for_kernel(&support);
            assert!(kernel.allows().all(|s| !matches!(s, ExecAllow::Tool(_))));
            let with = rules
                .clone()
                .join(ExecRules::new(support.iter().map(ExecAllow::support), []));
            for t in &paths {
                let at = path::Subject::Position(t.real());
                for s in [
                    Subject::Program(path::Subject::Access(t)),
                    Subject::Program(at),
                    Subject::Dir(at),
                ] {
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
        let kernel = rules.for_kernel(&std::iter::once(real("/x/c")).collect());
        assert_eq!(of_file(&kernel, "/x/c"), BOTH);
    }

    #[test]
    fn an_exact_allow_whose_name_is_vetoed_is_not_live() {
        let rules: ExecRules = [allow_file("/x/bash"), veto("bash")].into_iter().collect();
        assert_eq!(rules.live().count(), 0);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert_eq!(rules.allowed_files().count(), 0);
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

    /// The rules a grant makes: each key walked, a key with both bits both
    /// kinds of rule, and `every` is `Any`.
    fn granting(keys: impl IntoIterator<Item = (ExecKey, Evidence)>) -> ExecRules {
        ExecRules::of_grant(&keys.into_iter().collect(), &EnvVars::new()).unwrap()
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

    /// A key with both bits resolves to both entries, at paths that may differ;
    /// `every` is `Any`.
    #[test]
    fn of_grant_carries_evidence_to_every_rule_and_sets_any() {
        let rules = granting([(ExecKey::Dir(written("/d")), BOTH)]);
        assert_eq!(
            rules.evidence(Subject::Dir(path::Subject::Position(&real("/d")))),
            BOTH
        );
        let every = std::iter::once((ExecKey::Name("zzz-no-such".into()), ALLOW))
            .collect::<ExecGrant>()
            .with_every();
        let rules = ExecRules::of_grant(&every, &EnvVars::new()).unwrap();
        assert!(rules.allows().any(|s| *s == ExecAllow::Any));
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
        let table = granting([
            (ExecKey::Path(spelled(&link)), DENY),
            (ExecKey::Path(spelled(&target)), ALLOW),
        ]);
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
        let rules = granting([(ExecKey::Dir(spelled(&link)), ALLOW)]);
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
        let rules = granting([
            (ExecKey::Dir(spelled(&link)), DENY),
            (ExecKey::Dir(spelled(&target.join("sub"))), ALLOW),
        ]);
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
        let key = |e| [(ExecKey::Dir(spelled(&blocked)), e)];

        let (denied, granted) = (granting(key(DENY)), granting(key(ALLOW)));
        std::fs::remove_dir(&blocked).unwrap();
        std::os::unix::fs::symlink(&tools, &blocked).unwrap();
        let name = LexicalPath::absolute(&blocked).unwrap();
        let name = walk_lenient(&name, LeafMode::NoFollow).unwrap();
        let name = Subject::Program(path::Subject::Access(&name));
        assert!(denied.evidence(name).negative);
        assert_eq!(denied.evidence(program(&prog)), Evidence::NEITHER);
        assert_eq!(granted.evidence(program(&prog)), Evidence::NEITHER);

        let (denied, granted) = (granting(key(DENY)), granting(key(ALLOW)));
        let Some(ExecDeny::Dir(held)) = denied.denies().next() else {
            panic!("a directory exclusion: {denied:?}");
        };
        assert_eq!(held.real.named().real().as_path(), blocked.as_path());
        assert_eq!(
            held.real.target().map(|t| t.real().as_path()),
            Some(tools.as_path())
        );
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
        let allowed = granting([(ExecKey::Dir(spelled(&dir)), ALLOW)]);
        let denying = granting([(ExecKey::Dir(spelled(&denied)), DENY)]);
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
        let allowed = granting([(ExecKey::Path(spelled(&bin.join("tool"))), ALLOW)]);
        std::fs::rename(bin.join("tool"), denied.join("tool")).unwrap();
        let denying = granting([(ExecKey::Dir(spelled(&denied)), DENY)]);
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
}
