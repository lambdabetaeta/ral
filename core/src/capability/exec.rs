//! Exec authority as rules about programs.
//!
//! Decode leaves each layer an authored [`ExecGrant`];
//! [`ExecRules::compile`] turns it into rules over host files, bundled tools,
//! directories and vetoed names, and [`ExecRules::verdict`] is the one
//! function that judges a [`Program`].  A stack's authority is the pointwise
//! meet of its layers' tables, compiled afresh on every question.

use crate::path::{RealPath, SearchCwd};
use crate::types::{ExecGrant, ExecRule, GrantStack, Meet, Verdict, meet_insert};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// What a grant judges, and what the launcher runs.
#[derive(Clone, Debug)]
pub(crate) enum Program {
    /// A bundled tool, run as ral itself.
    Tool(String),
    /// A host file: `path` is what the launcher runs, `real` is `realpath(path)`.
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

/// What a grant judges: a borrowed view of a [`Program`].
#[derive(Clone, Copy)]
pub(crate) enum Subject<'a> {
    Tool(&'a str),
    File(&'a RealPath),
}

/// How specific a rule is.  The derived `Ord` is precedence: the greater rank
/// decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    Dir(usize),
    Carrier,
    Exact,
    Veto,
}

/// One rule, before meeting the others.
pub(crate) enum Rule {
    File(RealPath, Verdict),
    Tool(String, Verdict),
    /// `true` admits.
    Dir(RealPath, bool),
    Veto(String),
}

/// One grant's exec authority, as a function from programs to verdicts.
#[derive(Clone, Debug, Default)]
pub(crate) struct ExecRules {
    files: BTreeMap<RealPath, Verdict>,
    tools: BTreeMap<String, Verdict>,
    dirs: BTreeMap<RealPath, bool>,
    /// The [`name_key`] of every bare deny.
    vetoes: BTreeSet<String>,
}

impl Extend<Rule> for ExecRules {
    fn extend<I: IntoIterator<Item = Rule>>(&mut self, rules: I) {
        for rule in rules {
            match rule {
                Rule::File(real, v) => meet_insert(&mut self.files, real, v),
                Rule::Tool(name, v) => meet_insert(&mut self.tools, name, v),
                Rule::Dir(real, v) => meet_insert(&mut self.dirs, real, v),
                Rule::Veto(name) => {
                    self.vetoes.insert(name_key(&name).into_owned());
                }
            }
        }
    }
}

impl FromIterator<Rule> for ExecRules {
    fn from_iter<I: IntoIterator<Item = Rule>>(rules: I) -> Self {
        let mut table = Self::default();
        table.extend(rules);
        table
    }
}

impl ExecRules {
    /// A bare key is the file the host `PATH` finds — the process's own, so a
    /// scoped `PATH` cannot redirect it — and the bundled tool of that name,
    /// and for a deny a veto on the name everywhere.  Path and dir keys are
    /// their frozen forms, never re-read from disk.
    pub(crate) fn compile(grant: &ExecGrant) -> Self {
        let host_path = std::env::var("PATH").ok();
        let names = grant.names.iter().flat_map(|(name, v)| {
            let tool =
                crate::uutils::is_uutils_tool(name).then(|| Rule::Tool(name.clone(), v.clone()));
            let file = crate::path::locate(name, host_path.as_deref(), SearchCwd::nowhere())
                .and_then(|hit| RealPath::of(&hit).ok())
                .map(|real| Rule::File(real, v.clone()));
            let veto = v.is_denied().then(|| Rule::Veto(name.clone()));
            [tool, file, veto].into_iter().flatten()
        });
        let paths = grant
            .paths
            .iter()
            .map(|(path, v)| Rule::File(RealPath::frozen(path), v.clone()));
        let dirs = grant
            .dirs
            .iter()
            .map(|(dir, allow)| Rule::Dir(RealPath::frozen(dir), *allow));
        names.chain(paths).chain(dirs).collect()
    }

    pub(crate) fn verdict(&self, subject: Subject<'_>) -> Verdict {
        most_specific(self.matching(subject)).unwrap_or(Verdict::Deny)
    }

    /// Every rule that speaks to `subject`, ranked.
    fn matching<'s>(&'s self, subject: Subject<'s>) -> impl Iterator<Item = (Rank, Verdict)> + 's {
        let (name, exact, real) = match subject {
            Subject::Tool(name) => (Cow::Borrowed(name), self.tools.get(name), None),
            Subject::File(real) => (real.name(), self.files.get(real), Some(real)),
        };
        let veto = self.vetoes.contains(name_key(&name).as_ref());
        let dirs = real.into_iter().flat_map(move |real| self.covering(real));
        veto.then_some((Rank::Veto, Verdict::Deny))
            .into_iter()
            .chain(exact.map(|v| (Rank::Exact, v.clone())))
            .chain(dirs)
    }

    fn covering<'s>(&'s self, real: &'s RealPath) -> impl Iterator<Item = (Rank, Verdict)> + 's {
        self.dirs
            .iter()
            .filter(move |(dir, _)| real.within(dir))
            .map(|(dir, allow)| (Rank::Dir(dir.depth()), Verdict::from(*allow)))
    }

    /// What the dirs alone say of `real` — a dir covers itself.
    fn dir_verdict(&self, real: &RealPath) -> Verdict {
        most_specific(self.covering(real)).unwrap_or(Verdict::Deny)
    }

    /// What the kernel is to make of `real`: the table's rules, and a carrier
    /// as an allow ranked under an exact rule.
    fn kernel_verdict(&self, real: &RealPath, carriers: &BTreeSet<RealPath>) -> Verdict {
        let carrier = carriers
            .contains(real)
            .then_some((Rank::Carrier, Verdict::Allow));
        most_specific(self.matching(Subject::File(real)).chain(carrier)).unwrap_or(Verdict::Deny)
    }

    /// The kernel's rules, in ascending [`Rank`] so last-match-wins is
    /// [`most_specific`]; within a rank denies follow allows, as equal ranks
    /// meet.  Each file is emitted once, at its final verdict, so an
    /// allow-only renderer (Landlock) may keep the allows as they stand.  The
    /// kernel cannot see argv, so `Only` is an allow; tools are not rendered,
    /// the kernel seeing ral's own binary for them.
    pub(crate) fn kernel(&self, carriers: &BTreeSet<RealPath>) -> Vec<ExecRule> {
        let dirs = self.dirs.iter().map(|(d, &allow)| {
            let dir = ExecRule::Dir {
                path: d.to_string(),
                allow,
            };
            (Rank::Dir(d.depth()), dir)
        });
        let files: BTreeSet<&RealPath> = self.files.keys().chain(carriers).collect();
        let files = files.into_iter().map(|f| {
            let allow = !self.kernel_verdict(f, carriers).is_denied();
            let file = ExecRule::File {
                path: f.to_string(),
                allow,
            };
            (Rank::Exact, file)
        });
        let vetoes = (self.vetoes.iter()).map(|n| (Rank::Veto, ExecRule::Veto(n.clone())));
        let mut ranked: Vec<_> = dirs.chain(files).chain(vetoes).collect();
        ranked.sort_by_key(|(rank, rule)| {
            let allows = matches!(
                rule,
                ExecRule::Dir { allow: true, .. } | ExecRule::File { allow: true, .. }
            );
            (*rank, !allows)
        });
        ranked.into_iter().map(|(_, rule)| rule).collect()
    }

    /// The files a rule admits.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn allowed_files(&self) -> impl Iterator<Item = &RealPath> {
        self.files
            .keys()
            .filter(|real| !self.verdict(Subject::File(real)).is_denied())
    }
}

/// The greatest rank decides; equal ranks meet.
fn most_specific(candidates: impl IntoIterator<Item = (Rank, Verdict)>) -> Option<Verdict> {
    candidates
        .into_iter()
        .fold(None::<(Rank, Verdict)>, |best, (rank, v)| match best {
            Some((held, b)) if held > rank => Some((held, b)),
            Some((held, b)) if held == rank => Some((held, b.meet(v))),
            _ => Some((rank, v)),
        })
        .map(|(_, v)| v)
}

/// The meet of two finite-support functions: the union of their supports,
/// each point met.
fn pointwise<'k, K: Ord + Clone + 'k, V>(
    keys: impl Iterator<Item = &'k K>,
    at: impl Fn(&K) -> V,
) -> BTreeMap<K, V> {
    keys.map(|k| (k.clone(), at(k))).collect()
}

/// `verdict(a ∧ b, p) == verdict(a, p) ∧ verdict(b, p)` for every program `p`.
impl Meet for ExecRules {
    fn meet(self, other: Self) -> Self {
        let (a, b) = (&self, &other);
        let at = |s: Subject<'_>| a.verdict(s).meet(b.verdict(s));
        Self {
            files: pointwise(a.files.keys().chain(b.files.keys()), |k| {
                at(Subject::File(k))
            }),
            tools: pointwise(a.tools.keys().chain(b.tools.keys()), |k| {
                at(Subject::Tool(k))
            }),
            dirs: pointwise(a.dirs.keys().chain(b.dirs.keys()), |k| {
                !a.dir_verdict(k).meet(b.dir_verdict(k)).is_denied()
            }),
            vetoes: &a.vetoes | &b.vetoes,
        }
    }
}

/// The stack's exec authority; `None` when no layer holds an exec opinion.
pub(crate) fn rules(grants: &GrantStack) -> Option<ExecRules> {
    grants.exec().map(ExecRules::compile).reduce(Meet::meet)
}

/// The extensions Windows runs a bare name through.  `.bat` and `.cmd` belong
/// here even though `process::launch` refuses to spawn them: that is a later
/// refusal of the image, not of the name.
pub(crate) const WINDOWS_EXEC_EXTENSIONS: &[&str] = &["com", "exe", "bat", "cmd"];

/// A command name as the host identifies it.
fn name_key(name: &str) -> Cow<'_, str> {
    name_key_on(name, cfg!(windows))
}

/// Off Windows the name itself; on Windows ASCII lower-case, a trailing
/// executable extension stripped.  `windows` is a parameter so the Windows
/// rule is tested on every host.
fn name_key_on(name: &str, windows: bool) -> Cow<'_, str> {
    if !windows {
        return Cow::Borrowed(name);
    }
    let lower = name.to_ascii_lowercase();
    match lower.rsplit_once('.') {
        Some((stem, ext)) if WINDOWS_EXEC_EXTENSIONS.contains(&ext) => Cow::Owned(stem.to_string()),
        _ => Cow::Owned(lower),
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs scaffolding: a tempdir file and a link to it"
)]
mod tests {
    use super::*;
    use crate::path::{Namespace, NormalizedPrefix};

    /// `which.rs`'s `%PATHEXT%` fallback and the name-key strip list are twin
    /// copies of one fact; they may only drift together.
    #[cfg(windows)]
    #[test]
    fn name_key_extensions_agree_with_the_resolver_default_pathext() {
        let from_pathext: Vec<String> = crate::path::which::DEFAULT_PATHEXT
            .split(';')
            .map(|e| e.trim_start_matches('.').to_lowercase())
            .collect();
        let from_name_key: Vec<String> = WINDOWS_EXEC_EXTENSIONS
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(from_pathext, from_name_key);
    }

    #[test]
    fn name_key_off_windows_is_the_name() {
        assert_eq!(name_key_on("Git.exe", false), "Git.exe");
    }

    #[test]
    fn windows_name_key_folds_case_and_strips_an_executable_extension() {
        for name in [
            "git", "GIT", "git.exe", "Git.EXE", "git.cmd", "git.com", "git.bat",
        ] {
            assert_eq!(name_key_on(name, true), "git", "{name}");
        }
        assert_eq!(name_key_on("git.tool", true), "git.tool");
        assert_eq!(name_key_on("gitk.exe", true), "gitk");
    }

    #[test]
    fn rank_is_precedence() {
        assert!(Rank::Dir(0) < Rank::Dir(9));
        assert!(Rank::Dir(9) < Rank::Carrier);
        assert!(Rank::Carrier < Rank::Exact);
        assert!(Rank::Exact < Rank::Veto);
    }

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

    fn file(p: &str, v: Verdict) -> Rule {
        Rule::File(real(p), v)
    }

    fn dir(p: &str, allow: bool) -> Rule {
        Rule::Dir(real(p), allow)
    }

    fn of_file(rules: &ExecRules, p: &str) -> Verdict {
        rules.verdict(Subject::File(&real(p)))
    }

    #[test]
    fn a_veto_beats_a_file_allow() {
        let rules: ExecRules = [file("/x/bash", Verdict::Allow), Rule::Veto("bash".into())]
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
        let allowed: ExecRules = std::iter::once(Rule::Tool("ls".into(), Verdict::Allow)).collect();
        assert_eq!(allowed.verdict(Subject::Tool("ls")), Verdict::Allow);
        let covered: ExecRules = [dir("/", true), file("/bin/ls", Verdict::Allow)]
            .into_iter()
            .collect();
        assert_eq!(covered.verdict(Subject::Tool("ls")), Verdict::Deny);
        let vetoed: ExecRules = [
            Rule::Tool("rm".into(), Verdict::Allow),
            Rule::Veto("rm".into()),
        ]
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

    /// A tiny xorshift: deterministic and dependency-free.
    struct Rng(usize);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }

        fn pick<T: Clone>(&mut self, items: &[T]) -> T {
            items[self.below(items.len())].clone()
        }
    }

    const DIRS: [&str; 4] = ["/a", "/a/b", "/a/b/c", "/z"];
    const FILES: [&str; 7] = [
        "/a/x",
        "/a/b/x",
        "/a/b/c/x",
        "/a/b/c/d/x",
        "/z/x",
        "/q/x",
        "/a/b/sh",
    ];
    const TOOLS: [&str; 2] = ["ls", "rm"];
    const NAMES: [&str; 3] = ["x", "sh", "rm"];

    fn verdicts() -> [Verdict; 4] {
        let only = |subs: &[&str]| Verdict::Only(subs.iter().map(ToString::to_string).collect());
        [
            Verdict::Allow,
            Verdict::Deny,
            only(&["a"]),
            only(&["a", "b"]),
        ]
    }

    /// A table over the universe, each rule present one time in three.
    fn table(rng: &mut Rng) -> ExecRules {
        let mut rules = Vec::new();
        for f in FILES {
            if rng.below(3) == 0 {
                rules.push(file(f, rng.pick(&verdicts())));
            }
        }
        for t in TOOLS {
            if rng.below(3) == 0 {
                rules.push(Rule::Tool(t.into(), rng.pick(&verdicts())));
            }
        }
        for d in DIRS {
            if rng.below(3) == 0 {
                rules.push(dir(d, rng.pick(&[true, false])));
            }
        }
        for n in NAMES {
            if rng.below(6) == 0 {
                rules.push(Rule::Veto(n.into()));
            }
        }
        rules.into_iter().collect()
    }

    #[test]
    fn the_meet_of_two_tables_judges_as_the_meet_of_their_verdicts() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let files: Vec<RealPath> = FILES.iter().chain(&DIRS).map(|p| real(p)).collect();
        for _ in 0..2000 {
            let (a, b) = (table(&mut rng), table(&mut rng));
            let met = a.clone().meet(b.clone());
            let subjects = files
                .iter()
                .map(Subject::File)
                .chain(TOOLS.iter().map(|t| Subject::Tool(t)));
            for s in subjects {
                assert_eq!(
                    met.verdict(s),
                    a.verdict(s).meet(b.verdict(s)),
                    "a = {a:?}\nb = {b:?}"
                );
            }
        }
    }

    /// The kernel's last-match-wins over `rules`, deny by default.
    fn last_match(rules: &[ExecRule], real: &RealPath) -> bool {
        rules
            .iter()
            .rev()
            .find_map(|rule| match rule {
                ExecRule::Dir { path, allow } => {
                    real.within(&RealPath::assumed(path)).then_some(*allow)
                }
                ExecRule::File { path, allow } => {
                    (*real == RealPath::assumed(path)).then_some(*allow)
                }
                ExecRule::Veto(name) => (name_key(&real.name()) == name.as_str()).then_some(false),
            })
            .unwrap_or(false)
    }

    #[test]
    fn the_kernel_rules_judge_as_the_table_and_its_carriers() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let files: Vec<RealPath> = FILES.iter().chain(&DIRS).map(|p| real(p)).collect();
        for _ in 0..2000 {
            let rules = table(&mut rng);
            let carriers: BTreeSet<RealPath> = files
                .iter()
                .filter(|_| rng.below(4) == 0)
                .cloned()
                .collect();
            let kernel = rules.kernel(&carriers);
            for f in &files {
                let expected = if rules.vetoes.contains(name_key(&f.name()).as_ref()) {
                    false
                } else if rules.files.contains_key(f) {
                    !rules.verdict(Subject::File(f)).is_denied()
                } else {
                    carriers.contains(f) || !rules.dir_verdict(f).is_denied()
                };
                assert_eq!(
                    last_match(&kernel, f),
                    expected,
                    "{f}\nrules = {rules:?}\ncarriers = {carriers:?}\nkernel = {kernel:?}"
                );
            }
        }
    }

    fn kernel_file(kernel: &[ExecRule], f: &RealPath) -> Vec<bool> {
        let want = f.to_string();
        kernel
            .iter()
            .filter_map(|rule| match rule {
                ExecRule::File { path, allow } if *path == want => Some(*allow),
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
    fn a_carrier_whose_name_is_vetoed_is_not_allowed() {
        let rules: ExecRules = std::iter::once(Rule::Veto("c".into())).collect();
        let kernel = rules.kernel(&carriers(&["/x/c"]));
        assert_eq!(kernel_file(&kernel, &real("/x/c")), [false]);
    }

    #[test]
    fn an_exact_allow_whose_name_is_vetoed_is_denied_and_not_listed() {
        let rules: ExecRules = [file("/x/bash", Verdict::Allow), Rule::Veto("bash".into())]
            .into_iter()
            .collect();
        assert_eq!(
            kernel_file(&rules.kernel(&BTreeSet::new()), &real("/x/bash")),
            [false]
        );
        assert_eq!(rules.allowed_files().count(), 0);
    }

    #[test]
    fn each_file_is_emitted_once() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let files: Vec<RealPath> = FILES.iter().map(|p| real(p)).collect();
        for _ in 0..500 {
            let rules = table(&mut rng);
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
        let grant = ExecGrant {
            paths: BTreeMap::from([
                (NormalizedPrefix::from_surface(&link), Verdict::Deny),
                (NormalizedPrefix::from_surface(&target), Verdict::Allow),
            ]),
            ..ExecGrant::default()
        };
        let rules = ExecRules::compile(&grant);
        assert_eq!(
            rules.files.into_values().collect::<Vec<_>>(),
            [Verdict::Deny]
        );
    }

    /// A table of one dir key `/l` frozen as resolving to `/x`.
    fn linked_dir(allow: bool, extra_allow: Option<&str>) -> ExecRules {
        let linked = NormalizedPrefix::for_test(&host("/l"), &host("/x"), Namespace::Host);
        let mut dirs = BTreeMap::from([(linked, allow)]);
        if let Some(extra) = extra_allow {
            dirs.insert(NormalizedPrefix::from_surface(host(extra)), true);
        }
        ExecRules::compile(&ExecGrant {
            dirs,
            ..ExecGrant::default()
        })
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
        let rules: ExecRules = [
            Rule::Dir(RealPath::assumed("/private/tmp/bin"), true),
            Rule::Dir(RealPath::assumed("/tmp/bin"), false),
        ]
        .into_iter()
        .collect();
        assert_eq!(of_file(&rules, "/tmp/bin/evil"), Verdict::Deny);
    }

    /// `/private/tmp` is an alias of the one-deep `/tmp` yet spells longer:
    /// depth counts components, or the alias would outrank `/tmp/a/b`.
    #[cfg(target_os = "macos")]
    #[test]
    fn depth_counts_components_not_characters() {
        let rules: ExecRules = [
            Rule::Dir(RealPath::assumed("/private/tmp"), true),
            Rule::Dir(RealPath::assumed("/tmp/a/b"), false),
        ]
        .into_iter()
        .collect();
        assert_eq!(of_file(&rules, "/tmp/a/b/x"), Verdict::Deny);
    }
}
