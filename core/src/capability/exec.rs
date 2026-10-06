//! Exec authority as a [`Table`] of rules about programs.
//!
//! Decode leaves each layer an authored [`ExecGrant`];
//! [`ExecRules::compile`] turns it into rules over host files, bundled tools,
//! directories and vetoed names, which judge a [`Program`] as any table
//! judges a subject.  A stack's authority is the meet of its layers' tables,
//! compiled afresh on every question.

use super::table::{Scope, Table};
use crate::path::{Polarity, RealPath, SearchCwd, command_name_key};
use crate::types::{ExecGrant, ExecKey, ExecRule, GrantStack, Meet, Verdict};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// What a grant judges, and what the launcher runs.
#[derive(Clone, Debug)]
pub(crate) enum Program {
    /// A bundled tool, run as ral itself.
    Tool(String),
    /// A host file: `real` is `realpath(path)`, judged and run; `path` is the
    /// spelling, which the program sees as `argv[0]`.
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

/// What a grant judges: a borrowed view of a [`Program`], or the directory a
/// dir rule is itself judged at.
#[derive(Clone, Copy)]
pub(crate) enum Subject<'a> {
    Tool(&'a str),
    File(&'a RealPath),
    Under(&'a RealPath),
}

/// Where an exec rule applies.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ExecScope {
    Dir(RealPath),
    /// A file the kernel must admit for an admitted one to start; only
    /// [`ExecRules::kernel`] writes one.
    Carrier(RealPath),
    File(RealPath),
    Tool(String),
    /// The [`command_name_key`] of a bare deny: a veto on the name everywhere.
    Name(String),
}

/// How specific a rule is.  The derived `Ord` is precedence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Rank {
    Dir(usize),
    Carrier,
    Exact,
    Name,
}

/// The path or the name.
impl std::fmt::Display for ExecScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dir(path) | Self::Carrier(path) | Self::File(path) => path.fmt(f),
            Self::Tool(name) | Self::Name(name) => name.fmt(f),
        }
    }
}

impl Scope for ExecScope {
    type Subject<'a> = Subject<'a>;
    type Rank = Rank;

    fn rank(&self, _: &Verdict) -> Rank {
        match self {
            Self::Dir(dir) => Rank::Dir(dir.depth()),
            Self::Carrier(_) => Rank::Carrier,
            Self::File(_) | Self::Tool(_) => Rank::Exact,
            Self::Name(_) => Rank::Name,
        }
    }

    /// A tool has no place on disk, and only a dir speaks to `Under`.  A
    /// name's key is folded already, so it holds every spelling under either
    /// polarity.
    fn holds<P: Polarity>(&self, subject: Subject<'_>) -> bool {
        match (self, subject) {
            (Self::Dir(dir), Subject::File(path) | Subject::Under(path)) => path.within::<P>(dir),
            (Self::File(file) | Self::Carrier(file), Subject::File(path)) => {
                path.within::<P>(file) && file.within::<P>(path)
            }
            (Self::Tool(tool), Subject::Tool(name)) => tool == name,
            (Self::Name(key), Subject::Tool(name)) => *key == command_name_key(name),
            (Self::Name(key), Subject::File(path)) => *key == command_name_key(&path.name()),
            _ => false,
        }
    }

    fn own(&self) -> Subject<'_> {
        match self {
            Self::Dir(dir) => Subject::Under(dir),
            Self::File(file) | Self::Carrier(file) => Subject::File(file),
            Self::Tool(name) | Self::Name(name) => Subject::Tool(name),
        }
    }
}

/// Exec authority, as a function from programs to verdicts.
pub(crate) type ExecRules = Table<ExecScope>;

impl ExecRules {
    /// A bare key is the file the host `PATH` finds — the process's own, so a
    /// scoped `PATH` cannot redirect it — and the bundled tool of that name,
    /// and for a deny a veto on the name everywhere.  Path and dir keys are
    /// their frozen forms, never re-read from disk.
    pub(crate) fn compile(grant: &ExecGrant) -> Self {
        let host_path = std::env::var("PATH").ok();
        (grant.0.iter())
            .flat_map(|(key, v)| {
                let (scopes, v): (Vec<ExecScope>, Verdict) = match key {
                    ExecKey::Name(name) => {
                        let tool = crate::uutils::is_uutils_tool(name)
                            .then(|| ExecScope::Tool(name.clone()));
                        let file =
                            crate::path::locate(name, host_path.as_deref(), SearchCwd::nowhere())
                                .and_then(|hit| RealPath::of(&hit).ok())
                                .map(ExecScope::File);
                        let veto = v
                            .is_denied()
                            .then(|| ExecScope::Name(command_name_key(name)));
                        (
                            [tool, file, veto].into_iter().flatten().collect(),
                            v.clone(),
                        )
                    }
                    ExecKey::Path(path) => {
                        (vec![ExecScope::File(RealPath::frozen(path))], v.clone())
                    }
                    // A dir admits or denies; `Only` needs a named command.
                    ExecKey::Dir(dir) => (
                        vec![ExecScope::Dir(RealPath::frozen(dir))],
                        Verdict::from(!v.is_denied()),
                    ),
                };
                scopes.into_iter().map(move |scope| (scope, v.clone()))
            })
            .collect()
    }

    /// The kernel's rules, in ascending [`Rank`] so last-match-wins is
    /// highest-rank-wins; within a rank denies follow allows, as equal ranks
    /// meet.  Each file is emitted once, at its verdict with `carriers`
    /// allowed beneath an exact rule, so an allow-only renderer (Landlock)
    /// may keep the allows as they stand.  The kernel cannot see argv, so
    /// `Only` is an allow; tools are not rendered, the kernel seeing ral's
    /// own binary for them.
    pub(crate) fn kernel(&self, carriers: &BTreeSet<RealPath>) -> Vec<ExecRule> {
        let mut with = self.clone();
        with.extend(
            carriers
                .iter()
                .map(|c| (ExecScope::Carrier(c.clone()), Verdict::Allow)),
        );
        let files: BTreeSet<&RealPath> = (with.rules())
            .filter_map(|(scope, _)| match scope {
                ExecScope::File(file) | ExecScope::Carrier(file) => Some(file),
                _ => None,
            })
            .collect();
        let files = files.into_iter().map(|file| {
            let allow = !with.verdict(Subject::File(file)).is_denied();
            let path = file.clone();
            (Rank::Exact, ExecRule::File { path, allow })
        });
        let dirs_and_vetoes = (with.rules()).filter_map(|(scope, v)| {
            let rule = match scope {
                ExecScope::Dir(path) => ExecRule::Dir {
                    path: path.clone(),
                    allow: !v.is_denied(),
                },
                ExecScope::Name(name) => ExecRule::Veto(name.clone()),
                _ => return None,
            };
            Some((scope.rank(v), rule))
        });
        let mut ranked: Vec<_> = dirs_and_vetoes.chain(files).collect();
        ranked.sort_by_key(|(rank, rule)| {
            let allows = matches!(
                rule,
                ExecRule::Dir { allow: true, .. } | ExecRule::File { allow: true, .. }
            );
            (*rank, !allows)
        });
        ranked.into_iter().map(|(_, rule)| rule).collect()
    }

    /// The kernel's list read back as a table: each file at its final
    /// verdict, so the carriers are already folded in; a dir at its verdict;
    /// a veto a `Name` deny.  [`Table::verdict`] over it is the kernel's
    /// last-match-wins, which `the_kernel_rules_judge_as_the_table_and_its_carriers`
    /// asserts.
    #[cfg(target_os = "linux")]
    pub(crate) fn from_kernel(rules: &[ExecRule]) -> Self {
        (rules.iter())
            .map(|rule| match rule {
                ExecRule::Dir { path, allow } => {
                    (ExecScope::Dir(path.clone()), Verdict::from(*allow))
                }
                ExecRule::File { path, allow } => {
                    (ExecScope::File(path.clone()), Verdict::from(*allow))
                }
                ExecRule::Veto(name) => (ExecScope::Name(name.clone()), Verdict::Deny),
            })
            .collect()
    }

    /// The files a rule admits.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn allowed_files(&self) -> impl Iterator<Item = &RealPath> {
        self.live().filter_map(|scope| match scope {
            ExecScope::File(file) => Some(file),
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
    use crate::capability::table::speaks;
    use crate::path::{Allow, Deny, FrozenPath};
    use std::collections::BTreeMap;

    /// `p` as an absolute path on the host, which for Windows needs a drive.
    fn host(p: &str) -> String {
        if cfg!(windows) {
            format!("C:{}", p.replace('/', r"\"))
        } else {
            p.to_string()
        }
    }

    fn real(p: &str) -> RealPath {
        RealPath::assumed(host(p))
    }

    fn file(p: &str, v: Verdict) -> (ExecScope, Verdict) {
        (ExecScope::File(real(p)), v)
    }

    fn dir(p: &str, allow: bool) -> (ExecScope, Verdict) {
        (ExecScope::Dir(real(p)), Verdict::from(allow))
    }

    fn tool(name: &str, v: Verdict) -> (ExecScope, Verdict) {
        (ExecScope::Tool(name.into()), v)
    }

    fn veto(name: &str) -> (ExecScope, Verdict) {
        (ExecScope::Name(command_name_key(name)), Verdict::Deny)
    }

    fn of_file(rules: &ExecRules, p: &str) -> Verdict {
        rules.verdict(Subject::File(&real(p)))
    }

    #[test]
    fn a_veto_beats_a_file_allow() {
        let rules: ExecRules = [file("/x/bash", Verdict::Allow), veto("bash")]
            .into_iter()
            .collect();
        assert_eq!(of_file(&rules, "/x/bash"), Verdict::Deny);
    }

    #[test]
    fn a_file_rule_beats_a_covering_deny_dir() {
        let rules: ExecRules = [file("/x/tool", Verdict::Allow), dir("/x", false)]
            .into_iter()
            .collect();
        assert_eq!(of_file(&rules, "/x/tool"), Verdict::Allow);
        assert_eq!(of_file(&rules, "/x/other"), Verdict::Deny);
    }

    #[test]
    fn the_deepest_dir_wins() {
        let rules: ExecRules = [dir("/x", false), dir("/x/sub", true)]
            .into_iter()
            .collect();
        assert_eq!(of_file(&rules, "/x/sub/tool"), Verdict::Allow);
        assert_eq!(of_file(&rules, "/x/tool"), Verdict::Deny);
    }

    #[test]
    fn no_rule_denies() {
        assert_eq!(of_file(&ExecRules::default(), "/x/tool"), Verdict::Deny);
        assert_eq!(
            ExecRules::default().verdict(Subject::Tool("ls")),
            Verdict::Deny
        );
    }

    /// A tool has no place on disk, so no dir covers it.
    #[test]
    fn a_tool_is_judged_by_tools_and_vetoes_only() {
        let allowed: ExecRules = std::iter::once(tool("ls", Verdict::Allow)).collect();
        assert_eq!(allowed.verdict(Subject::Tool("ls")), Verdict::Allow);
        let covered: ExecRules = [dir("/", true), file("/bin/ls", Verdict::Allow)]
            .into_iter()
            .collect();
        assert_eq!(covered.verdict(Subject::Tool("ls")), Verdict::Deny);
        let vetoed: ExecRules = [tool("rm", Verdict::Allow), veto("rm")]
            .into_iter()
            .collect();
        assert_eq!(vetoed.verdict(Subject::Tool("rm")), Verdict::Deny);
    }

    /// A deeper allow in one layer cannot lift a deny another layer places
    /// above it.
    #[test]
    fn a_stacked_deeper_allow_does_not_lift_a_deny() {
        let a: ExecRules = [dir("/a", true), dir("/a/b", false)].into_iter().collect();
        let b: ExecRules = std::iter::once(dir("/a/b/c", true)).collect();
        assert_eq!(of_file(&a.meet(b), "/a/b/c/x"), Verdict::Deny);
    }

    /// U1: a key one layer never mentioned is not written down as a deny,
    /// which would hold every spelling of it.
    #[test]
    fn the_meet_writes_no_default_as_a_deny() {
        let a: ExecRules = std::iter::once(dir("/a/B", true)).collect();
        let b: ExecRules = [dir("/a/B", true), dir("/a/b", true)].into_iter().collect();
        let met = a.meet(b);
        assert_eq!(of_file(&met, "/a/B/x"), Verdict::Allow);
        if cfg!(not(windows)) {
            assert_eq!(of_file(&met, "/a/b/x"), Verdict::Deny);
        }
        let a: ExecRules = std::iter::once(file("/x/T", Verdict::Allow)).collect();
        let b: ExecRules = [file("/x/T", Verdict::Allow), file("/x/t", Verdict::Allow)]
            .into_iter()
            .collect();
        let met = a.meet(b);
        assert_eq!(of_file(&met, "/x/T"), Verdict::Allow);
        if cfg!(not(windows)) {
            assert_eq!(of_file(&met, "/x/t"), Verdict::Deny);
        }
    }

    /// U2: a deny holds every spelling of its name, an allow its own.
    #[test]
    fn a_deny_holds_every_spelling_and_an_allow_its_own() {
        let (nfc, nfd) = ("/x/caf\u{e9}", "/x/cafe\u{301}");
        let deny = |scope: &ExecScope, p: &str| scope.holds::<Deny>(Subject::File(&real(p)));
        let allow = |scope: &ExecScope, p: &str| scope.holds::<Allow>(Subject::File(&real(p)));
        let (evil, tool) = (
            ExecScope::Dir(real("/x/Evil")),
            ExecScope::File(real("/x/Tool")),
        );
        let (cafe_dir, cafe) = (ExecScope::Dir(real(nfc)), ExecScope::File(real(nfc)));
        assert!(deny(&evil, "/x/evil/t"));
        assert!(!deny(&evil, "/x/evilx/t"));
        assert!(allow(&evil, "/x/Evil/t"));
        assert!(deny(&tool, "/x/tool"));
        assert!(allow(&tool, "/x/Tool"));
        assert!(deny(&cafe_dir, &format!("{nfd}/t")));
        assert!(!allow(&cafe_dir, &format!("{nfd}/t")));
        assert!(deny(&cafe, nfd));
        assert!(!allow(&cafe, nfd));
        if cfg!(not(windows)) {
            assert!(!allow(&evil, "/x/evil/t"));
            assert!(!allow(&tool, "/x/tool"));
        }
    }

    /// U3: a folded match ranks as a stored one; ties meet, and the deeper
    /// rule or an exact one still wins.
    #[test]
    fn a_folded_match_ranks_as_a_stored_one() {
        let tools: ExecRules = [dir("/x/Tools", true), dir("/x/tools/evil", false)]
            .into_iter()
            .collect();
        assert_eq!(of_file(&tools, "/x/Tools/evil/t"), Verdict::Deny);
        assert_eq!(of_file(&tools, "/x/Tools/good/t"), Verdict::Allow);
        assert_eq!(of_file(&tools, "/x/tools/evil/t"), Verdict::Deny);

        let tie: ExecRules = [dir("/x/b", true), dir("/x/B", false)]
            .into_iter()
            .collect();
        assert_eq!(of_file(&tie, "/x/b/t"), Verdict::Deny);

        let exact: ExecRules = [dir("/x/B", false), file("/x/b/tool", Verdict::Allow)]
            .into_iter()
            .collect();
        assert_eq!(of_file(&exact, "/x/b/tool"), Verdict::Allow);
        assert_eq!(of_file(&exact, "/x/b/other"), Verdict::Deny);

        let deeper: ExecRules = [dir("/x/B", false), dir("/x/b/sub", true)]
            .into_iter()
            .collect();
        assert_eq!(of_file(&deeper, "/x/b/sub/t"), Verdict::Allow);

        let files: ExecRules = [
            file("/x/tool", Verdict::Allow),
            file("/x/Tool", Verdict::Deny),
        ]
        .into_iter()
        .collect();
        assert_eq!(of_file(&files, "/x/tool"), Verdict::Deny);

        let a: ExecRules = [dir("/a", true), dir("/a/B", false)].into_iter().collect();
        let b: ExecRules = [dir("/a/b", true), dir("/a/c", true)].into_iter().collect();
        let met = a.meet(b);
        assert_eq!(of_file(&met, "/a/b/x"), Verdict::Deny);
        assert_eq!(of_file(&met, "/a/c/x"), Verdict::Allow);
    }

    /// A respelling is named only when no deny holds the program as stored,
    /// though the deepest deny holds it only by a fold.
    #[test]
    fn a_respelling_is_named_only_when_no_deny_holds_the_stored_name() {
        let program = real("/x/evil/t");
        let folded: ExecRules = [dir("/", true), dir("/x/Evil", false)]
            .into_iter()
            .collect();
        if cfg!(not(windows)) {
            assert_eq!(
                folded.respelled(Subject::File(&program)),
                Some(&ExecScope::Dir(real("/x/Evil")))
            );
        }
        let both: ExecRules = [dir("/", true), dir("/x", false), dir("/x/Evil", false)]
            .into_iter()
            .collect();
        assert_eq!(of_file(&both, "/x/evil/t"), Verdict::Deny);
        assert_eq!(both.respelled(Subject::File(&program)), None);
    }

    /// The refusal cites the most specific deny that holds the program only
    /// by a fold.
    #[test]
    fn the_most_specific_folded_deny_is_named() {
        let rules: ExecRules = [dir("/X", false), file("/x/TOOL", Verdict::Deny)]
            .into_iter()
            .collect();
        if cfg!(not(windows)) {
            assert_eq!(
                rules.respelled(Subject::File(&real("/x/tool"))),
                Some(&ExecScope::File(real("/x/TOOL")))
            );
        }
    }

    /// U7: a veto holds every spelling of the name.
    #[test]
    fn a_veto_holds_every_spelling_of_the_name() {
        let rules: ExecRules = [dir("/x", true), veto("sh")].into_iter().collect();
        assert_eq!(of_file(&rules, "/x/SH"), Verdict::Deny);
        assert_eq!(of_file(&rules, "/x/\u{17f}h"), Verdict::Deny);
        assert_eq!(of_file(&rules, "/x/shx"), Verdict::Allow);
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

        fn pick<T: Clone>(&mut self, items: &[T]) -> T {
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

    fn verdicts() -> [Verdict; 4] {
        let only = |subs: &[&str]| Verdict::Only(subs.iter().map(ToString::to_string).collect());
        [
            Verdict::Allow,
            Verdict::Deny,
            only(&["a"]),
            only(&["a", "b"]),
        ]
    }

    /// Every path the universe names.
    pub(crate) fn paths() -> Vec<RealPath> {
        (FILES.iter().chain(&DIRS).chain(&PROBES))
            .map(|p| real(p))
            .collect()
    }

    /// Each path as a file and as a directory, and each tool.
    pub(crate) fn subjects(paths: &[RealPath]) -> impl Iterator<Item = Subject<'_>> {
        (paths.iter())
            .flat_map(|p| [Subject::File(p), Subject::Under(p)])
            .chain(TOOLS.map(Subject::Tool))
    }

    /// A table over the universe, each rule present one time in three.
    pub(crate) fn random(rng: &mut Rng) -> ExecRules {
        let mut rules = Vec::new();
        for f in FILES {
            if rng.below(3) == 0 {
                rules.push(file(f, rng.pick(&verdicts())));
            }
        }
        for t in TOOLS {
            if rng.below(3) == 0 {
                rules.push(tool(t, rng.pick(&verdicts())));
            }
        }
        for d in DIRS {
            if rng.below(3) == 0 {
                rules.push(dir(d, rng.pick(&[true, false])));
            }
        }
        for n in NAMES {
            if rng.below(6) == 0 {
                rules.push(veto(n));
            }
        }
        rules.into_iter().collect()
    }

    /// The kernel's last-match-wins over `kernel`, deny by default.
    fn last_match(kernel: &[ExecRule], real: &RealPath) -> bool {
        let program = Subject::File(real);
        (kernel.iter().rev())
            .find_map(|rule| {
                let (scope, allow) = match rule {
                    ExecRule::Dir { path, allow } => (ExecScope::Dir(path.clone()), *allow),
                    ExecRule::File { path, allow } => (ExecScope::File(path.clone()), *allow),
                    ExecRule::Veto(name) => (ExecScope::Name(name.clone()), false),
                };
                speaks(&scope, &Verdict::from(allow), program).then_some(allow)
            })
            .unwrap_or(false)
    }

    #[test]
    fn the_kernel_rules_judge_as_the_table_and_its_carriers() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let paths = paths();
        for _ in 0..2000 {
            let rules = random(&mut rng);
            let carriers: BTreeSet<RealPath> = (paths.iter())
                .filter(|_| rng.below(4) == 0)
                .cloned()
                .collect();
            let kernel = rules.kernel(&carriers);
            let mut with = rules.clone();
            with.extend(
                carriers
                    .iter()
                    .map(|c| (ExecScope::Carrier(c.clone()), Verdict::Allow)),
            );
            for f in &paths {
                assert_eq!(
                    last_match(&kernel, f),
                    !with.verdict(Subject::File(f)).is_denied(),
                    "{f}\nrules = {rules:?}\ncarriers = {carriers:?}\nkernel = {kernel:?}"
                );
            }
            #[cfg(target_os = "linux")]
            {
                let read_back = ExecRules::from_kernel(&kernel);
                for p in &paths {
                    for (s, kind) in [(Subject::File(p), "file"), (Subject::Under(p), "dir")] {
                        assert_eq!(
                            read_back.verdict(s).is_denied(),
                            with.verdict(s).is_denied(),
                            "{p} as a {kind}\nrules = {rules:?}\ncarriers = {carriers:?}\n\
                             kernel = {kernel:?}"
                        );
                    }
                }
            }
        }
    }

    fn kernel_file(kernel: &[ExecRule], f: &RealPath) -> Vec<bool> {
        kernel
            .iter()
            .filter_map(|rule| match rule {
                ExecRule::File { path, allow } if path == f => Some(*allow),
                _ => None,
            })
            .collect()
    }

    fn carriers(ps: &[&str]) -> BTreeSet<RealPath> {
        ps.iter().map(|p| real(p)).collect()
    }

    #[test]
    fn a_carrier_with_an_exact_deny_is_denied_to_the_kernel() {
        let rules: ExecRules = std::iter::once(file("/x/c", Verdict::Deny)).collect();
        let kernel = rules.kernel(&carriers(&["/x/c"]));
        assert_eq!(kernel_file(&kernel, &real("/x/c")), [false]);
    }

    #[test]
    fn a_carrier_outranks_a_covering_deny_dir() {
        let rules: ExecRules = std::iter::once(dir("/x", false)).collect();
        let kernel = rules.kernel(&carriers(&["/x/c"]));
        assert_eq!(kernel_file(&kernel, &real("/x/c")), [true]);
    }

    #[test]
    fn a_carrier_whose_name_is_vetoed_is_not_allowed() {
        let rules: ExecRules = std::iter::once(veto("c")).collect();
        let kernel = rules.kernel(&carriers(&["/x/c"]));
        assert_eq!(kernel_file(&kernel, &real("/x/c")), [false]);
    }

    #[test]
    fn an_exact_allow_whose_name_is_vetoed_is_denied_and_not_listed() {
        let rules: ExecRules = [file("/x/bash", Verdict::Allow), veto("bash")]
            .into_iter()
            .collect();
        assert_eq!(
            kernel_file(&rules.kernel(&BTreeSet::new()), &real("/x/bash")),
            [false]
        );
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert_eq!(rules.allowed_files().count(), 0);
    }

    #[test]
    fn each_file_is_emitted_once() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let files: Vec<RealPath> = FILES.iter().map(|p| real(p)).collect();
        for _ in 0..500 {
            let rules = random(&mut rng);
            let carriers: BTreeSet<RealPath> = files
                .iter()
                .filter(|_| rng.below(2) == 0)
                .cloned()
                .collect();
            let kernel = rules.kernel(&carriers);
            for f in &files {
                let n = kernel_file(&kernel, f).len();
                assert!(n <= 1, "{f} appears {n} times in {kernel:?}");
            }
        }
    }

    /// One file, two path keys: a deny on the link `a` and an allow on its
    /// target `b` compile to one rule, and it denies.
    #[cfg(unix)]
    #[test]
    fn two_path_keys_naming_one_file_meet() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("b");
        std::fs::write(&target, "").unwrap();
        let link = tmp.path().join("a");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let grant = ExecGrant(BTreeMap::from([
            (
                ExecKey::Path(FrozenPath::from_surface(&link)),
                Verdict::Deny,
            ),
            (
                ExecKey::Path(FrozenPath::from_surface(&target)),
                Verdict::Allow,
            ),
        ]));
        let table = ExecRules::compile(&grant);
        assert_eq!(
            table.rules().map(|(_, v)| v).collect::<Vec<_>>(),
            [&Verdict::Deny]
        );
    }

    /// A table of one dir key `/l` frozen as resolving to `/x`.
    fn linked_dir(allow: bool, extra_allow: Option<&str>) -> ExecRules {
        let linked = FrozenPath::for_test(&host("/l"), &host("/x"));
        let extra = extra_allow.map(|e| (FrozenPath::from_surface(host(e)), Verdict::Allow));
        let dirs = std::iter::once((linked, Verdict::from(allow))).chain(extra);
        ExecRules::compile(&ExecGrant(
            dirs.map(|(dir, v)| (ExecKey::Dir(dir), v)).collect(),
        ))
    }

    /// A dir that is a symlink covers where it points, allow or deny alike.
    #[test]
    fn a_symlinked_dir_covers_its_target() {
        assert_eq!(of_file(&linked_dir(false, None), "/x/tool"), Verdict::Deny);
        assert_eq!(of_file(&linked_dir(true, None), "/x/tool"), Verdict::Allow);
    }

    /// Matched through its target, a symlinked dir ranks at the target's
    /// depth, so a deeper allow inside the target still wins a deny.
    #[test]
    fn a_symlinked_deny_dir_ranks_at_its_target_depth() {
        let inner = linked_dir(false, Some("/x/sub"));
        assert_eq!(of_file(&inner, "/x/sub/tool"), Verdict::Allow);
        let level = linked_dir(false, Some("/x"));
        assert_eq!(of_file(&level, "/x/tool"), Verdict::Deny);
    }

    /// `/tmp/bin` and `/private/tmp/bin` are one directory to the guard but
    /// distinct bytes: equal rank, so the deny meets the allow.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_firmlink_alias_does_not_outrank_a_deny() {
        let rules: ExecRules = [dir("/private/tmp/bin", true), dir("/tmp/bin", false)]
            .into_iter()
            .collect();
        assert_eq!(of_file(&rules, "/tmp/bin/evil"), Verdict::Deny);
    }

    /// `/private/tmp` is an alias of the one-deep `/tmp` yet spells longer:
    /// depth counts components, or the alias would outrank `/tmp/a/b`.
    #[cfg(target_os = "macos")]
    #[test]
    fn depth_counts_components_not_characters() {
        let rules: ExecRules = [dir("/private/tmp", true), dir("/tmp/a/b", false)]
            .into_iter()
            .collect();
        assert_eq!(of_file(&rules, "/tmp/a/b/x"), Verdict::Deny);
    }
}
