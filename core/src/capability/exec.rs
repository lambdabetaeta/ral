//! Per-layer and stack-level exec policy evaluation.
//!
//! A layer's exec map admits a command two ways: by literal key (bare
//! name or absolute path), or by a covering `allow_dirs`/`deny_dirs`
//! prefix.  Literal beats dir, deeper dir beats shallower, and a tie
//! resolves to deny.

use crate::path;
use crate::types::{ExecMap, ExecPolicy, GrantStack, Meet};
use std::collections::{BTreeMap, BTreeSet};

/// What an admitted command may run: any argv, or only these
/// first-argument subcommands.
pub(super) enum Admit {
    Any,
    Subcommands(BTreeSet<String>),
}

/// `Admit` is [`ExecPolicy`] with the `Deny` bottom removed, so this
/// fold can never reach it — hence the `unreachable!` below.
impl Meet for Admit {
    fn meet(self, other: Self) -> Self {
        Self::from_policy(self.into_policy().meet(other.into_policy()))
    }
}

impl Admit {
    fn into_policy(self) -> ExecPolicy {
        match self {
            Self::Any => ExecPolicy::Allow,
            Self::Subcommands(s) => ExecPolicy::Subcommands(s),
        }
    }

    fn from_policy(policy: ExecPolicy) -> Self {
        match policy {
            ExecPolicy::Allow => Self::Any,
            ExecPolicy::Subcommands(s) => Self::Subcommands(s),
            ExecPolicy::Deny => unreachable!("meet of two admitted verdicts is never Deny"),
        }
    }
}

/// One opining capability layer's vote on a candidate command.
pub(super) enum LayerExec {
    Denied,
    Allowed(Admit),
}

/// The two identity sets a command carries into the gate, consulted
/// asymmetrically.  `deny` is broad — the policy names plus the
/// basenames of the resolved and as-invoked forms — so a bare
/// `bash: deny` still vetoes an absolute `/bin/bash`.  `allow` is
/// exactly the policy names, so a planted `/tmp/evil/rg` cannot inherit
/// an outer grant's bare `rg: allow`.  Both are built in
/// `runtime::command::identity`.
#[derive(Clone, Copy)]
pub(super) struct ExecNames<'a> {
    pub(super) deny: &'a [&'a str],
    pub(super) allow: &'a [&'a str],
}

/// Folded verdict across the whole capability stack; `Unrestricted`
/// means no layer held an exec opinion at all.
pub(super) enum ExecVerdict {
    Unrestricted,
    Denied,
    Allowed(Admit),
}

/// Fold every opining layer: one denial denies, allowances intersect,
/// and a stack that opines but admits nothing denies.  Takes the grant
/// stack alone, not a `Context` — every other input is lexical, so the
/// verdict is pure.
pub(super) fn evaluate_exec(grants: &GrantStack, names: ExecNames) -> ExecVerdict {
    let mut admit: Option<Admit> = None;
    let mut saw = false;
    for exec in grants.exec() {
        saw = true;
        match layer_exec_verdict(exec, names) {
            LayerExec::Denied => return ExecVerdict::Denied,
            LayerExec::Allowed(a) => admit = admit.meet(Some(a)),
        }
    }
    if let Some(a) = admit {
        ExecVerdict::Allowed(a)
    } else if saw {
        ExecVerdict::Denied
    } else {
        ExecVerdict::Unrestricted
    }
}

/// Decide one layer.  A literal `Deny` on any broad name goes first
/// because it must beat a covering allow dir; then a literal admission
/// on the narrow names; then the deepest covering dir; else deny by
/// default.  Dirs match only absolute names, and the basenames the
/// broad set adds are bare, so the narrow set suffices there.
pub(super) fn layer_exec_verdict(exec: &ExecMap, names: ExecNames) -> LayerExec {
    if literal_vetoes(&exec.literals, names.deny) {
        return LayerExec::Denied;
    }
    if let Some(policy) = match_literal_keys(&exec.literals, names.allow) {
        return match policy {
            ExecPolicy::Deny => LayerExec::Denied,
            ExecPolicy::Allow => LayerExec::Allowed(Admit::Any),
            ExecPolicy::Subcommands(s) => LayerExec::Allowed(Admit::Subcommands(s)),
        };
    }
    match longest_dir_match(exec, names.allow) {
        Some(true) => LayerExec::Allowed(Admit::Any),
        Some(false) | None => LayerExec::Denied,
    }
}

fn literal_vetoes(literals: &BTreeMap<String, ExecPolicy>, deny_names: &[&str]) -> bool {
    deny_names
        .iter()
        .any(|n| matches!(lookup_literal(literals, n), Some(ExecPolicy::Deny)))
}

/// Whether the gate admits, over an explicit deny/allow name pair.
/// Lets `runtime::command::identity` and the capability lattice tests
/// drive the real verdict with hand-built sets.
#[cfg(test)]
pub(crate) fn admits_for_test(grants: &GrantStack, deny: &[&str], allow: &[&str]) -> bool {
    !matches!(
        evaluate_exec(grants, ExecNames { deny, allow }),
        ExecVerdict::Denied
    )
}

/// Bare names and absolute paths share one keyspace, so a layer listing
/// the same binary under both takes the meet of the two policies.
fn match_literal_keys(
    literals: &BTreeMap<String, ExecPolicy>,
    names: &[&str],
) -> Option<ExecPolicy> {
    let mut matched = names.iter().filter_map(|n| lookup_literal(literals, n));
    let first = matched.next()?;
    Some(matched.fold(first, ExecPolicy::meet))
}

/// Off Windows, lookup is exact.  Under Windows identity distinct keys can be
/// fold-equal (`GIT` and `git` are one name to the OS), so every fold-equal hit
/// is meet-folded: an exact `Allow` must not hide a `Deny` on another spelling.
fn lookup_literal(literals: &BTreeMap<String, ExecPolicy>, name: &str) -> Option<ExecPolicy> {
    lookup_literal_on(literals, name, cfg!(windows))
}

fn lookup_literal_on(
    literals: &BTreeMap<String, ExecPolicy>,
    name: &str,
    windows: bool,
) -> Option<ExecPolicy> {
    if !windows {
        return literals.get(name).cloned();
    }
    let mut matches = literals
        .iter()
        .filter(|(key, _)| names_match(key, name, true))
        .map(|(_, policy)| policy.clone());
    let first = matches.next()?;
    Some(matches.fold(first, ExecPolicy::meet))
}

/// The default PATHEXT list [`path::which`] falls back to.  `.bat` and
/// `.cmd` belong here even though `process::launch` refuses to spawn
/// them: that is a later gate on the image, not on the name.
const WINDOWS_EXEC_EXTENSIONS: &[&str] = &["com", "exe", "bat", "cmd"];

/// `git.EXE` → `git`; an unrecognised extension (`my.tool`) is left
/// alone.
fn strip_windows_extension(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, ext))
            if WINDOWS_EXEC_EXTENSIONS
                .iter()
                .any(|e| e.eq_ignore_ascii_case(ext)) =>
        {
            stem
        }
        _ => name,
    }
}

/// True iff `name` pins a specific executable extension rather than
/// naming a bare stem.
fn names_an_extension(name: &str) -> bool {
    strip_windows_extension(name).len() != name.len()
}

/// True iff exec-map key `literal` and command identity `candidate`
/// name the same executable: byte-exact off Windows; under Windows
/// identity, case folds and a candidate's PATHEXT extension is
/// transparent, so a bare `git` key matches `GIT.CMD`.  A key that
/// names an extension is a pin, not a stem — `git.exe: 'allow'` must
/// not admit a planted `git.com`, which default PATHEXT resolution
/// tries first.  `windows` is a parameter, not a `cfg!` read, so the
/// rule is testable off Windows; [`lookup_literal`] is the platform
/// gate.
fn names_match(literal: &str, candidate: &str, windows: bool) -> bool {
    if !windows {
        return literal == candidate;
    }
    if names_an_extension(literal) {
        return literal.eq_ignore_ascii_case(candidate);
    }
    literal.eq_ignore_ascii_case(strip_windows_extension(candidate))
}

/// The deepest directory prefix covering any absolute candidate, and
/// whether it allows.  "Deepest" by [`path::lex::identity_depth`] —
/// components of the alias-folded form, not characters of the raw
/// surface, so a firmlink spelling (`/tmp` vs `/private/tmp`) cannot
/// buy a shallow directory rank.  An allow and a deny of equal depth do
/// reach here — composition only strips a clash where
/// [`same_gate_dir`](crate::path::resolved::NormalizedPrefix::same_gate_dir)
/// holds — so the two loops break the tie in opposite directions: allow
/// displaces `best` on strictly greater depth, deny on greater-or-equal.
/// A gate's ambiguity must resolve to deny.
fn longest_dir_match(exec: &ExecMap, names: &[&str]) -> Option<bool> {
    let mut best: Option<(usize, bool)> = None;
    let mut consider = |dir: &str, allow: bool, wins_tie: bool| {
        let matches_any = names
            .iter()
            .any(|n| path::is_absolute(n) && path::path_within_str(n, dir));
        if !matches_any {
            return;
        }
        let depth = path::lex::identity_depth(dir, cfg!(windows));
        match best {
            Some((best_depth, _)) if best_depth > depth || (best_depth == depth && !wins_tie) => {}
            _ => best = Some((depth, allow)),
        }
    };
    for dir in &exec.allow_dirs {
        consider(dir.as_str(), true, false);
    }
    for dir in &exec.deny_dirs {
        consider(dir.as_str(), false, true);
    }
    best.map(|(_, allow)| allow)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `which.rs`'s `%PATHEXT%` fallback and the grant-key strip list are
    /// twin copies of one fact; they may only drift together.
    #[cfg(windows)]
    #[test]
    fn grant_key_extensions_agree_with_the_resolver_default_pathext() {
        let from_pathext: Vec<String> = crate::path::which::DEFAULT_PATHEXT
            .split(';')
            .map(|e| e.trim_start_matches('.').to_lowercase())
            .collect();
        let from_grant_keys: Vec<String> = WINDOWS_EXEC_EXTENSIONS
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(from_pathext, from_grant_keys);
    }

    #[test]
    fn names_match_off_windows_is_byte_exact() {
        assert!(names_match("git", "git", false));
        assert!(!names_match("git", "git.exe", false));
        assert!(!names_match("git", "Git", false));
    }

    /// No `cfg(windows)`: the Windows rule runs on every CI host.
    #[test]
    fn windows_names_match_ignores_case() {
        assert!(names_match("git", "Git", true));
        assert!(names_match("GIT", "git", true));
    }

    #[test]
    fn windows_names_match_strips_pathext() {
        assert!(names_match("git", "git.exe", true));
        assert!(names_match("git", "GIT.EXE", true));
        assert!(names_match("git", "git.cmd", true));
        assert!(names_match("git", "git.CMD", true));
        assert!(names_match("git", "git.com", true));
        assert!(names_match("git", "git.bat", true));
    }

    #[test]
    fn windows_names_match_rejects_unrelated_extension() {
        assert!(!names_match("git", "git.tool", true));
        assert!(!names_match("git", "gitx", true));
    }

    #[test]
    fn windows_names_match_still_requires_the_same_stem() {
        assert!(!names_match("git", "gitk.exe", true));
    }

    #[test]
    fn windows_names_match_extension_pin_is_exact() {
        assert!(names_match("git.exe", "git.exe", true));
        assert!(names_match("git.exe", "GIT.EXE", true));
        assert!(!names_match("git.exe", "git.com", true));
        assert!(!names_match("git.exe", "git.bat", true));
        assert!(!names_match("git.exe", "git", true));
    }

    #[test]
    fn strip_windows_extension_leaves_unknown_extensions_alone() {
        assert_eq!(strip_windows_extension("my.tool"), "my.tool");
        assert_eq!(strip_windows_extension("git"), "git");
        assert_eq!(strip_windows_extension("git.EXE"), "git");
    }

    /// An allow and a deny of equal depth, both covering the candidate,
    /// must resolve to deny — the gate's own half of the guarantee,
    /// independent of what composition already strips.  The fixture is
    /// spelled for the host: the gate weighs only candidates
    /// [`path::is_absolute`] admits, and a rooted path with no drive is
    /// not absolute to Windows.
    #[test]
    fn longest_dir_match_ties_resolve_to_deny() {
        let (dir, divergent, candidate) = if cfg!(windows) {
            (r"C:\x", r"C:\y", r"C:\x\bin")
        } else {
            ("/x", "/y", "/x/bin")
        };
        let exec = ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::from([path::NormalizedPrefix::from_surface(dir)]),
            deny_dirs: BTreeSet::from([path::NormalizedPrefix::for_test(
                dir,
                divergent,
                path::Namespace::Host,
            )]),
        };
        assert_eq!(longest_dir_match(&exec, &[candidate]), Some(false));
    }

    /// A deny on `/tmp/bin` and an allow on its firmlink alias
    /// `/private/tmp/bin` are one directory to the gate
    /// (`path_within` follows firmlinks) but distinct bytes.
    /// `identity_depth` ranks them equal, so the deny-wins tie-break
    /// closes the clash even when it reaches the gate uncomposed, as
    /// here.
    #[cfg(target_os = "macos")]
    #[test]
    fn longest_dir_match_firmlink_alias_does_not_outrank_deny() {
        let exec = ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::from([path::NormalizedPrefix::from_surface("/private/tmp/bin")]),
            deny_dirs: BTreeSet::from([path::NormalizedPrefix::from_surface("/tmp/bin")]),
        };
        assert_eq!(longest_dir_match(&exec, &["/tmp/bin/evil"]), Some(false));
    }

    /// `/tmp/a/b` nests three deep; `/private/tmp` is an alias of the
    /// one-deep `/tmp`, yet spells longer.  Counting characters would
    /// rank the alias above the real descendant — fail-open whichever
    /// side carries the deny.
    #[cfg(target_os = "macos")]
    #[test]
    fn longest_dir_match_depth_counts_components_not_characters() {
        let exec = ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::from([path::NormalizedPrefix::from_surface("/private/tmp")]),
            deny_dirs: BTreeSet::from([path::NormalizedPrefix::from_surface("/tmp/a/b")]),
        };
        assert_eq!(longest_dir_match(&exec, &["/tmp/a/b/x"]), Some(false));
    }

    #[test]
    fn lookup_literal_exact_match_always_hits() {
        let literals = BTreeMap::from([("git".to_string(), ExecPolicy::Allow)]);
        assert_eq!(lookup_literal(&literals, "git"), Some(ExecPolicy::Allow));
    }

    /// [`lookup_literal`] reads the real `cfg(windows)`, so the outcome
    /// is the host's; [`names_match`] is where the rule itself gets a
    /// fixed-outcome test.
    #[test]
    fn lookup_literal_case_mismatch_follows_real_platform() {
        let literals = BTreeMap::from([("git".to_string(), ExecPolicy::Allow)]);
        let hit = lookup_literal(&literals, "Git");
        assert_eq!(hit.is_some(), cfg!(windows));
    }

    /// Windows identity folds distinct map keys.  Both a mixed spelling and an
    /// exact `Allow` hit must still see the fold-equal `Deny`.
    #[test]
    fn lookup_literal_meets_windows_fold_equal_keys_deny_wins() {
        let literals = BTreeMap::from([
            ("GIT".to_string(), ExecPolicy::Allow),
            ("git".to_string(), ExecPolicy::Deny),
        ]);
        assert_eq!(
            lookup_literal_on(&literals, "Git.exe", true),
            Some(ExecPolicy::Deny)
        );
        assert_eq!(
            lookup_literal_on(&literals, "GIT", true),
            Some(ExecPolicy::Deny)
        );
    }
}
