//! Exec authority as a [`Table`] of evidence about programs, and the judgment
//! that mints [`Admitted`].
//!
//! Decode leaves each layer an authored [`ExecGrant`];
//! [`ExecRules::compile`] turns it into evidence over host files, bundled
//! tools, directories and vetoed names, which judges a [`Program`] as any
//! table judges a subject.  A stack's authority is the meet of its layers'
//! tables, compiled afresh on every question.  [`GrantStack::admit`] is the
//! one judgment; the guard words its refusals and the sandbox renders the
//! table it judged with.

use super::fs::FsScope;
use super::table::{Scope, Table, Withheld};
use super::{Evidence, ExecGrant, ExecKey, GrantStack, Join, Meet};
use crate::path::{FrozenPath, Polarity, RealPath, command_name_key};
use crate::syntax::quote::quote_word;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::LazyLock;

/// A program the stack admitted with these arguments, and the table that
/// admitted it.  Only [`GrantStack::admit`] makes one, so nothing launches
/// unjudged.
#[derive(Debug)]
#[must_use]
pub(crate) struct Admitted {
    program: Program,
    args: Vec<String>,
    rules: Option<ExecRules>,
}

impl Admitted {
    pub(crate) fn program(&self) -> &Program {
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
    Run(Withheld<ExecScope>),
    /// Allowed to run, but what it reads is not readable (law 7).
    Read(Withheld<FsScope>),
}

/// A call the stack refused.
#[derive(Debug)]
pub(crate) struct Refused {
    pub program: Program,
    pub args: Vec<String>,
    pub why: ExecDenial,
}

/// ral's own binary, the file a bundled tool runs and so reads.
pub(crate) fn ral_binary() -> Option<&'static RealPath> {
    static OWN: LazyLock<Option<RealPath>> =
        LazyLock::new(|| RealPath::of_path(&std::env::current_exe().ok()?).ok());
    OWN.as_ref()
}

impl GrantStack {
    /// Judge a program and its argv against the stack's exec rules, then what
    /// running it reads.
    pub(crate) fn admit(
        &self,
        program: Program,
        args: Vec<String>,
    ) -> Result<Admitted, Box<Refused>> {
        let rules = rules(self);
        match self.judge(rules.as_ref(), &program) {
            None => Ok(Admitted {
                program,
                args,
                rules,
            }),
            Some(why) => Err(Box::new(Refused { program, args, why })),
        }
    }

    /// Head-only admission, before any argv is known.
    pub(crate) fn admits(&self, program: &Program) -> bool {
        self.refusal(program).is_none()
    }

    /// Why a head is refused, for a refusal raised before [`GrantStack::admit`].
    pub(crate) fn refusal(&self, program: &Program) -> Option<ExecDenial> {
        self.judge(rules(self).as_ref(), program)
    }

    /// `↓run = ↓exec ∧ ↓read` (law 7): a supplied exec table must support the
    /// program, then what running it reads must be readable.  A bundled tool
    /// runs ral's image, which only an fs exclusion withholds.
    fn judge(&self, rules: Option<&ExecRules>, program: &Program) -> Option<ExecDenial> {
        if let Some(why) = rules.and_then(|r| r.withheld(program.subject())) {
            return Some(ExecDenial::Run(why));
        }
        let read = match program {
            Program::File { real, .. } => self.read(real.as_path()),
            Program::Tool(_) => {
                ral_binary().map_or(Ok(()), |own| self.read_not_excluded(own.as_path()))
            }
        };
        read.err().map(ExecDenial::Read)
    }
}

/// What a grant judges, and what the launcher runs.
#[derive(Clone, Debug)]
pub(crate) enum Program {
    /// A bundled tool, run as ral itself.
    Tool(String),
    /// A host file: `real` is `realpath(path)`, judged and run; `path` is the
    /// spelling, which the program sees as `argv[0]`. A `#!` script sees
    /// `real` as its script name: the kernel hands the interpreter the exec
    /// path.
    File { path: PathBuf, real: RealPath },
}

impl Program {
    pub(crate) fn subject(&self) -> Subject<'_> {
        match self {
            Self::Tool(name) => Subject::Tool(name),
            Self::File { real, .. } => Subject::File(real),
        }
    }
}

/// A file by its real path, a bundled tool by name.
impl std::fmt::Display for Program {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tool(name) => write!(f, "bundled {name}"),
            Self::File { real, .. } => real.fmt(f),
        }
    }
}

/// What a grant judges: a borrowed view of a [`Program`], or what a rule is
/// itself judged at.
#[derive(Clone, Copy)]
pub(crate) enum Subject<'a> {
    Any,
    Tool(&'a str),
    File(&'a RealPath),
    Under(&'a RealPath),
}

/// Where an exec rule applies.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ExecScope {
    /// Every program: compiled from [`ExecGrant::every`], never negative.
    Any,
    Dir(RealPath),
    /// `named` is the bare name the file was found under: provenance, never
    /// consulted by judgement.  Entries differing only in `named` hold the
    /// same subjects, each holding the other's `own()`, so the meet's nesting
    /// obligation holds.
    File {
        path: RealPath,
        named: Option<String>,
    },
    Tool(String),
    /// The [`command_name_key`] of a bare exclusion: a veto on the name
    /// everywhere, never positive.
    Name(String),
}

impl ExecScope {
    pub(crate) fn file(path: RealPath) -> Self {
        Self::File { path, named: None }
    }
}

/// As a grant spells it: a directory under its `dir` constructor, a file as
/// its `absolute` place, a tool or name under `name`.
impl std::fmt::Display for ExecScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Any => f.write_str("every program"),
            Self::Dir(path) => write!(f, "`dir `absolute {}", quote_word(&path.to_string())),
            Self::File { named: Some(n), .. } => write!(f, "`name {}", quote_word(n)),
            Self::File { path, .. } => write!(f, "`absolute {}", quote_word(&path.to_string())),
            Self::Tool(name) | Self::Name(name) => write!(f, "`name {}", quote_word(name)),
        }
    }
}

impl Scope for ExecScope {
    type Subject<'a> = Subject<'a>;

    /// A tool has no place on disk, and only a dir speaks to `Under`.  A
    /// name's key is folded already, so it holds every spelling under either
    /// polarity.
    fn holds<P: Polarity>(&self, subject: Subject<'_>) -> bool {
        match (self, subject) {
            (Self::Any, _) => true,
            (Self::Dir(dir), Subject::File(path) | Subject::Under(path)) => path.within::<P>(dir),
            (Self::File { path: file, .. }, Subject::File(path)) => {
                path.within::<P>(file) && file.within::<P>(path)
            }
            (Self::Tool(tool), Subject::Tool(name)) => tool == name,
            (Self::Name(key), Subject::Tool(name)) => *key == command_name_key(name),
            (Self::Name(key), Subject::File(path)) => path
                .name()
                .is_some_and(|name| *key == command_name_key(name)),
            _ => false,
        }
    }

    fn own(&self) -> Subject<'_> {
        match self {
            Self::Any => Subject::Any,
            Self::Dir(dir) => Subject::Under(dir),
            Self::File { path, .. } => Subject::File(path),
            Self::Tool(name) | Self::Name(name) => Subject::Tool(name),
        }
    }

    fn stores(&self, e: Evidence) -> bool {
        match self {
            Self::Name(_) => !e.positive,
            Self::Any => !e.negative,
            _ => true,
        }
    }
}

/// Exec authority, as a function from programs to evidence.
pub(crate) type ExecRules = Table<ExecScope>;

impl ExecRules {
    /// A bare key is the file the host `PATH` finds, resolved afresh as every
    /// run resolves it (the process's own `PATH`, so a scoped one cannot
    /// redirect it), and the bundled tool of that name, and for an exclusion
    /// a veto on the name everywhere.  Every key carries its evidence to every
    /// scope it compiles to, except the veto, which carries only the negative
    /// bit.  A path or dir key splits its polarities: the allowance keeps the
    /// real path frozen at decode, the exclusion is re-resolved from its
    /// surface now.  `every` is [`ExecScope::Any`].
    pub(crate) fn compile(grant: &ExecGrant) -> Self {
        let host_path = std::env::var("PATH").ok();
        let every = grant.every().then_some((ExecScope::Any, Evidence::ALLOW));
        let keyed = grant.iter().flat_map(|(key, &evidence)| match key {
            ExecKey::Name(name) => {
                let tool =
                    crate::uutils::is_uutils_tool(name).then(|| ExecScope::Tool(name.clone()));
                let file = crate::path::locate(name, host_path.as_deref(), None)
                    .and_then(|hit| RealPath::of_path(&hit).ok())
                    .map(|path| ExecScope::File {
                        path,
                        named: Some(name.clone()),
                    });
                [tool, file]
                    .into_iter()
                    .flatten()
                    .map(|scope| (scope, evidence))
                    .chain(
                        evidence
                            .negative
                            .then(|| (ExecScope::Name(command_name_key(name)), Evidence::DENY)),
                    )
                    .collect()
            }
            ExecKey::Path(path) => frozen_and_live(path, evidence, ExecScope::file),
            ExecKey::Dir(dir) => frozen_and_live(dir, evidence, ExecScope::Dir),
        });
        every.into_iter().chain(keyed).collect()
    }

    /// What the kernel is handed: this table beside `support`, the files its
    /// launch needs beyond the policy (carriers, the linker), joined as
    /// allows.  Bundled tools are not rendered, the kernel seeing ral's own
    /// binary for them.  Support yields to every exclusion, which the caller
    /// has already applied to it.
    pub(crate) fn for_kernel(&self, support: &BTreeSet<RealPath>) -> Self {
        let rules: Self = (self.rules())
            .filter(|(scope, _)| !matches!(scope, ExecScope::Tool(_)))
            .map(|(scope, e)| (scope.clone(), *e))
            .collect();
        let support = support
            .iter()
            .map(|c| (ExecScope::file(c.clone()), Evidence::ALLOW));
        rules.join(support.collect())
    }

    /// The run places: the real paths of the positive
    /// files and directories, which running reads.
    pub(crate) fn places(&self) -> impl Iterator<Item = &RealPath> {
        self.positives().filter_map(|scope| match scope {
            ExecScope::Dir(path) | ExecScope::File { path, .. } => Some(path),
            _ => None,
        })
    }

    /// The files a rule admits.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn allowed_files(&self) -> impl Iterator<Item = &RealPath> {
        self.live().filter_map(|scope| match scope {
            ExecScope::File { path, .. } => Some(path),
            _ => None,
        })
    }

    /// The directories a rule admits.
    #[cfg(target_os = "linux")]
    pub(crate) fn allowed_dirs(&self) -> impl Iterator<Item = &RealPath> {
        self.live().filter_map(|scope| match scope {
            ExecScope::Dir(dir) => Some(dir),
            _ => None,
        })
    }
}

/// Rule 6 on a path key: the positive bit at the real path frozen at decode,
/// the negative bit at the one its surface reaches now.
fn frozen_and_live(
    path: &FrozenPath,
    evidence: Evidence,
    scope: fn(RealPath) -> ExecScope,
) -> Vec<(ExecScope, Evidence)> {
    let allow = (evidence.positive).then(|| (scope(path.real().clone()), Evidence::ALLOW));
    let deny = (evidence.negative).then(|| (scope(path.live_real()), Evidence::DENY));
    allow.into_iter().chain(deny).collect()
}

/// The stack's exec authority; `None` when no layer holds an exec opinion.
pub(crate) fn rules(grants: &GrantStack) -> Option<ExecRules> {
    grants.exec().map(ExecRules::compile).reduce(Meet::meet)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs scaffolding: a tempdir file and a link to it"
)]
pub(super) mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::capability::Axis;
    use crate::path::{Allow, Deny, test_absolute};
    #[cfg(unix)]
    use std::path::Path;

    fn real(p: &str) -> RealPath {
        RealPath::assumed(test_absolute(p))
    }

    pub(crate) const ALLOW: Evidence = Evidence::ALLOW;
    pub(crate) const DENY: Evidence = Evidence::DENY;
    const BOTH: Evidence = Evidence::BOTH;

    fn file(p: &str, e: Evidence) -> (ExecScope, Evidence) {
        (ExecScope::file(real(p)), e)
    }

    fn dir(p: &str, e: Evidence) -> (ExecScope, Evidence) {
        (ExecScope::Dir(real(p)), e)
    }

    fn tool(name: &str, e: Evidence) -> (ExecScope, Evidence) {
        (ExecScope::Tool(name.into()), e)
    }

    fn veto(name: &str) -> (ExecScope, Evidence) {
        (ExecScope::Name(command_name_key(name)), DENY)
    }

    fn any() -> (ExecScope, Evidence) {
        (ExecScope::Any, ALLOW)
    }

    fn of_file(rules: &ExecRules, p: &str) -> Evidence {
        rules.evidence(Subject::File(&real(p)))
    }

    #[test]
    fn a_veto_beats_a_file_allow() {
        let rules: ExecRules = [file("/x/bash", ALLOW), veto("bash")].into_iter().collect();
        assert_eq!(of_file(&rules, "/x/bash"), BOTH);
    }

    /// Neither an exact file nor a deeper directory reopens an exclusion.
    #[test]
    fn an_exclusion_beats_a_file_and_a_deeper_dir() {
        let rules: ExecRules = [file("/x/tool", ALLOW), dir("/x", DENY)]
            .into_iter()
            .collect();
        assert_eq!(of_file(&rules, "/x/tool"), BOTH);
        assert_eq!(of_file(&rules, "/x/other"), DENY);
        let rules: ExecRules = [dir("/x", DENY), dir("/x/sub", ALLOW)]
            .into_iter()
            .collect();
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
        let allowed: ExecRules = std::iter::once(tool("ls", ALLOW)).collect();
        assert!(allowed.evidence(Subject::Tool("ls")).permits());
        let covered: ExecRules = [dir("/", ALLOW), file("/bin/ls", ALLOW)]
            .into_iter()
            .collect();
        assert_eq!(covered.evidence(Subject::Tool("ls")), Evidence::NEITHER);
        let vetoed: ExecRules = [tool("rm", ALLOW), veto("rm")].into_iter().collect();
        assert_eq!(vetoed.evidence(Subject::Tool("rm")), BOTH);
    }

    /// `Any` supports every program, a tool and a file alike, less what is
    /// excluded.
    #[test]
    fn any_supports_everything_less_the_exclusions() {
        let rules: ExecRules = [any(), dir("/x", DENY), veto("rm")].into_iter().collect();
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
        let a: ExecRules = [dir("/a", ALLOW), dir("/a/b", DENY)].into_iter().collect();
        let b: ExecRules = std::iter::once(dir("/a/b/c", ALLOW)).collect();
        assert!(!of_file(&a.meet(b), "/a/b/c/x").permits());
    }

    /// a key one layer never mentioned is not written down as an
    /// exclusion, which would hold every spelling of it.
    #[test]
    fn the_meet_writes_no_default_as_an_exclusion() {
        let a: ExecRules = std::iter::once(dir("/a/B", ALLOW)).collect();
        let b: ExecRules = [dir("/a/B", ALLOW), dir("/a/b", ALLOW)]
            .into_iter()
            .collect();
        let met = a.meet(b);
        assert!(of_file(&met, "/a/B/x").permits());
        assert_eq!(of_file(&met, "/a/b/x"), Evidence::NEITHER);
        let a: ExecRules = std::iter::once(file("/x/T", ALLOW)).collect();
        let b: ExecRules = [file("/x/T", ALLOW), file("/x/t", ALLOW)]
            .into_iter()
            .collect();
        let met = a.meet(b);
        assert!(of_file(&met, "/x/T").permits());
        assert_eq!(of_file(&met, "/x/t"), Evidence::NEITHER);
    }

    /// A directory exclusion keeps its `dir` marker in a refusal, distinct
    /// from a file exclusion at the same place.
    #[test]
    fn a_directory_exclusion_reads_apart_from_a_file_exclusion() {
        let at = |scope: fn(RealPath) -> ExecScope| scope(RealPath::assumed("/tools")).to_string();
        assert_eq!(at(ExecScope::Dir), "`dir `absolute /tools");
        assert_eq!(at(ExecScope::file), "`absolute /tools");
        assert_eq!(ExecScope::Name("bash".into()).to_string(), "`name bash");
    }

    /// A file found under a bare name is cited as the author wrote it.
    #[test]
    fn a_file_found_by_name_is_cited_by_name() {
        let path = RealPath::assumed("/usr/bin/dash");
        let named = ExecScope::File {
            path: path.clone(),
            named: Some("sh".into()),
        };
        assert_eq!(named.to_string(), "`name sh");
        assert_eq!(ExecScope::file(path).to_string(), "`absolute /usr/bin/dash");
    }

    /// an exclusion holds every spelling of its name, an allow its own.
    #[test]
    fn an_exclusion_holds_every_spelling_and_an_allow_its_own() {
        let (nfc, nfd) = ("/x/caf\u{e9}", "/x/cafe\u{301}");
        let deny = |scope: &ExecScope, p: &str| scope.holds::<Deny>(Subject::File(&real(p)));
        let allow = |scope: &ExecScope, p: &str| scope.holds::<Allow>(Subject::File(&real(p)));
        let (evil, tool) = (
            ExecScope::Dir(real("/x/Evil")),
            ExecScope::file(real("/x/Tool")),
        );
        let (cafe_dir, cafe) = (ExecScope::Dir(real(nfc)), ExecScope::file(real(nfc)));
        assert!(deny(&evil, "/x/evil/t"));
        assert!(!deny(&evil, "/x/evilx/t"));
        assert!(allow(&evil, "/x/Evil/t"));
        assert!(deny(&tool, "/x/tool"));
        assert!(allow(&tool, "/x/Tool"));
        assert!(deny(&cafe_dir, &format!("{nfd}/t")));
        assert!(!allow(&cafe_dir, &format!("{nfd}/t")));
        assert!(deny(&cafe, nfd));
        assert!(!allow(&cafe, nfd));
        assert!(!allow(&evil, "/x/evil/t"));
        assert!(!allow(&tool, "/x/tool"));
    }

    /// a folded match is an exclusion as a stored one is: ties are Both,
    /// and neither a deeper rule nor an exact one reopens it.
    #[test]
    fn a_folded_match_excludes_as_a_stored_one() {
        let tools: ExecRules = [dir("/x/Tools", ALLOW), dir("/x/tools/evil", DENY)]
            .into_iter()
            .collect();
        assert!(!of_file(&tools, "/x/Tools/evil/t").permits());
        assert!(of_file(&tools, "/x/Tools/good/t").permits());
        assert!(!of_file(&tools, "/x/tools/evil/t").permits());

        let tie: ExecRules = [dir("/x/b", ALLOW), dir("/x/B", DENY)]
            .into_iter()
            .collect();
        assert_eq!(of_file(&tie, "/x/b/t"), BOTH);

        let exact: ExecRules = [dir("/x/B", DENY), file("/x/b/tool", ALLOW)]
            .into_iter()
            .collect();
        assert!(!of_file(&exact, "/x/b/tool").permits());

        let files: ExecRules = [file("/x/tool", ALLOW), file("/x/Tool", DENY)]
            .into_iter()
            .collect();
        assert_eq!(of_file(&files, "/x/tool"), BOTH);

        let a: ExecRules = [dir("/a", ALLOW), dir("/a/B", DENY)].into_iter().collect();
        let b: ExecRules = [dir("/a/b", ALLOW), dir("/a/c", ALLOW)]
            .into_iter()
            .collect();
        let met = a.meet(b);
        assert!(!of_file(&met, "/a/b/x").permits());
        assert!(of_file(&met, "/a/c/x").permits());
    }

    /// A respelling is named only when no exclusion holds the program as
    /// stored.
    #[test]
    fn a_respelling_is_named_only_when_no_exclusion_holds_the_stored_name() {
        let program = real("/x/evil/t");
        let folded: ExecRules = [dir("/", ALLOW), dir("/x/Evil", DENY)]
            .into_iter()
            .collect();
        assert_eq!(
            folded.withheld(Subject::File(&program)),
            Some(Withheld::Excluded {
                scope: ExecScope::Dir(real("/x/Evil")),
                respelled: true
            })
        );
        let both: ExecRules = [dir("/", ALLOW), dir("/x", DENY), dir("/x/Evil", DENY)]
            .into_iter()
            .collect();
        assert_eq!(
            both.withheld(Subject::File(&program)),
            Some(Withheld::Excluded {
                scope: ExecScope::Dir(real("/x")),
                respelled: false
            })
        );
    }

    /// The refusal cites an exclusion that applies, and never one that does
    /// not.
    #[test]
    fn the_witness_is_an_exclusion_that_holds_the_program() {
        let rules: ExecRules = [dir("/X", DENY), file("/x/TOOL", DENY), dir("/y", DENY)]
            .into_iter()
            .collect();
        let Some(Withheld::Excluded { scope, .. }) =
            rules.withheld(Subject::File(&real("/x/tool")))
        else {
            panic!("an exclusion holds /x/tool");
        };
        assert!(scope.holds::<Deny>(Subject::File(&real("/x/tool"))));
    }

    /// a veto holds every spelling of the name.
    #[test]
    fn a_veto_holds_every_spelling_of_the_name() {
        let rules: ExecRules = [dir("/x", ALLOW), veto("sh")].into_iter().collect();
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

    /// Every path the universe names.
    pub(crate) fn paths() -> Vec<RealPath> {
        (FILES.iter().chain(&DIRS).chain(&PROBES))
            .map(|p| real(p))
            .collect()
    }

    /// Each path as a file and as a directory, each tool, and every program.
    pub(crate) fn subjects(paths: &[RealPath]) -> impl Iterator<Item = Subject<'_>> {
        (paths.iter())
            .flat_map(|p| [Subject::File(p), Subject::Under(p)])
            .chain(TOOLS.map(Subject::Tool))
            .chain([Subject::Any])
    }

    /// A table over the universe, each rule present one time in three; no
    /// `Name` entry is positive and no `Any` entry negative.
    pub(crate) fn random(rng: &mut Rng) -> ExecRules {
        let mut rules = Vec::new();
        for f in FILES {
            if rng.below(3) == 0 {
                rules.push(file(f, rng.pick(&evidences())));
            }
        }
        for t in TOOLS {
            if rng.below(3) == 0 {
                rules.push(tool(t, rng.pick(&evidences())));
            }
        }
        for d in DIRS {
            if rng.below(3) == 0 {
                rules.push(dir(d, rng.pick(&evidences())));
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
                .cloned()
                .collect();
            let kernel = rules.for_kernel(&support);
            assert!(!kernel.rules().any(|(s, _)| matches!(s, ExecScope::Tool(_))));
            let mut with = rules.clone();
            with.extend((support.iter()).map(|c| (ExecScope::file(c.clone()), Evidence::ALLOW)));
            for p in &paths {
                for s in [Subject::File(p), Subject::Under(p)] {
                    assert_eq!(
                        kernel.evidence(s),
                        with.evidence(s),
                        "{p}\nrules = {rules:?}\nsupport = {support:?}\nkernel = {kernel:?}"
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
        let rules: ExecRules = [file("/x/bash", ALLOW), veto("bash")].into_iter().collect();
        assert_eq!(rules.live().count(), 0);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert_eq!(rules.allowed_files().count(), 0);
    }

    /// The run places are the positive files and directories.
    #[test]
    fn the_places_are_the_positive_files_and_dirs() {
        let rules: ExecRules = [
            file("/a/x", ALLOW),
            dir("/a/b", BOTH),
            file("/a/y", DENY),
            tool("ls", ALLOW),
            any(),
            veto("sh"),
        ]
        .into_iter()
        .collect();
        let places: Vec<_> = rules.places().collect();
        assert_eq!(places, [&real("/a/b"), &real("/a/x")]);
    }

    /// A key with both bits compiles to both entries, at paths that may differ;
    /// `every` is `Any`.
    #[cfg(unix)]
    #[test]
    fn compile_carries_evidence_to_every_scope_and_sets_any() {
        let grant: ExecGrant =
            std::iter::once((ExecKey::Dir(FrozenPath::for_test("/d", "/d")), BOTH)).collect();
        let compiled = ExecRules::compile(&grant);
        assert_eq!(compiled.evidence(Subject::Under(&real("/d"))), BOTH);
        let every = std::iter::once((ExecKey::Name("zzz-no-such".into()), ALLOW))
            .collect::<ExecGrant>()
            .join_top()
            .unwrap();
        let compiled = ExecRules::compile(&every);
        assert!(
            compiled
                .rules()
                .any(|(s, e)| *s == ExecScope::Any && *e == ALLOW)
        );
    }

    /// One file, two path keys: an exclusion on the link `a` and an allowance on
    /// its target `b` compile to one rule, and it holds both.
    #[cfg(unix)]
    #[test]
    fn two_path_keys_naming_one_file_are_both() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("b");
        std::fs::write(&target, "").unwrap();
        let link = tmp.path().join("a");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let grant: ExecGrant = [
            (
                ExecKey::Path(FrozenPath::from_surface(crate::path::utf8(&link).unwrap())),
                DENY,
            ),
            (
                ExecKey::Path(FrozenPath::from_surface(
                    crate::path::utf8(&target).unwrap(),
                )),
                ALLOW,
            ),
        ]
        .into_iter()
        .collect();
        let table = ExecRules::compile(&grant);
        assert_eq!(table.rules().map(|(_, e)| *e).collect::<Vec<_>>(), [BOTH]);
    }

    /// A table of one dir key `/l` frozen as resolving to `/x`.
    fn linked_dir(e: Evidence) -> ExecRules {
        let linked = FrozenPath::for_test(&test_absolute("/l"), &test_absolute("/x"));
        ExecRules::compile(&std::iter::once((ExecKey::Dir(linked), e)).collect())
    }

    /// An allowed dir that is a symlink covers where it was frozen to.
    #[test]
    fn a_symlinked_allow_dir_covers_its_frozen_target() {
        assert!(of_file(&linked_dir(ALLOW), "/x/tool").permits());
    }

    /// A deeper allow inside a symlinked exclusion's target reopens nothing.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_exclusion_covers_its_target_whatever_sits_beneath() {
        let tmp = tempfile::tempdir().unwrap();
        let (target, link) = (tmp.path().join("x"), tmp.path().join("l"));
        std::fs::create_dir_all(target.join("sub")).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let grant: ExecGrant = [
            (
                ExecKey::Dir(FrozenPath::from_surface(crate::path::utf8(&link).unwrap())),
                DENY,
            ),
            (
                ExecKey::Dir(FrozenPath::from_surface(
                    crate::path::utf8(&target.join("sub")).unwrap(),
                )),
                ALLOW,
            ),
        ]
        .into_iter()
        .collect();
        let rules = ExecRules::compile(&grant);
        let at = |p: &Path| RealPath::of_path(p).unwrap();
        assert!(
            !rules
                .evidence(Subject::File(&at(&target.join("sub"))))
                .permits()
        );
    }

    /// Rule 6: an exclusion of `/work/blocked` follows the name after it
    /// becomes a link to an admitted directory, and an allowance does not.
    #[cfg(unix)]
    #[test]
    fn an_exclusion_follows_its_name_and_an_allowance_keeps_its_real_path() {
        let tmp = tempfile::tempdir().unwrap();
        let (blocked, tools) = (tmp.path().join("blocked"), tmp.path().join("tools"));
        std::fs::create_dir_all(&blocked).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(tools.join("prog"), "").unwrap();
        let (excluded, allowed) = (
            FrozenPath::from_surface(crate::path::utf8(&blocked).unwrap()),
            FrozenPath::from_surface(crate::path::utf8(&blocked).unwrap()),
        );
        std::fs::remove_dir(&blocked).unwrap();
        std::os::unix::fs::symlink(&tools, &blocked).unwrap();
        let prog = RealPath::of_path(&tools.join("prog")).unwrap();
        let denied = ExecRules::compile(&std::iter::once((ExecKey::Dir(excluded), DENY)).collect());
        assert!(denied.evidence(Subject::File(&prog)).negative);
        let granted =
            ExecRules::compile(&std::iter::once((ExecKey::Dir(allowed), ALLOW)).collect());
        assert_eq!(granted.evidence(Subject::File(&prog)), Evidence::NEITHER);
    }

    /// `/tmp/bin` and `/private/tmp/bin` are one directory to the guard but
    /// distinct bytes: the exclusion holds the alias.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_firmlink_alias_is_excluded_with_its_directory() {
        let rules: ExecRules = [dir("/private/tmp/bin", ALLOW), dir("/tmp/bin", DENY)]
            .into_iter()
            .collect();
        assert!(!of_file(&rules, "/tmp/bin/evil").permits());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_alias_directory_does_not_reopen_what_a_deeper_exclusion_holds() {
        let rules: ExecRules = [dir("/private/tmp", ALLOW), dir("/tmp/a/b", DENY)]
            .into_iter()
            .collect();
        assert!(!of_file(&rules, "/tmp/a/b/x").permits());
    }
}
