//! Per-layer and stack-level exec policy evaluation.
//!
//! Two internal types encode the per-layer and whole-stack verdicts;
//! `evaluate_exec` folds the stack, `layer_exec_verdict` decides one
//! layer.  Within a layer the unified exec map admits commands two
//! ways: by literal key match (bare name or absolute path), or by
//! subpath-prefix match (a key ending in `/` covering anything under
//! it).  Literal beats subpath; deeper subpath beats shallower.

use crate::path;
use crate::types::{ExecMap, ExecPolicy, GrantStack, Meet};
use std::collections::{BTreeMap, BTreeSet};

/// What an admitted command may run: any arguments, or only a fixed
/// set of first-argument subcommands.
pub(super) enum Admit {
    Any,
    Subcommands(BTreeSet<String>),
}

/// `Admit` is [`ExecPolicy`] with the `Deny` bottom removed: a `Deny`
/// can never reach an admitted verdict, so only the two authority
/// points that survive admission remain.  Meet delegates to
/// [`ExecPolicy::meet`] over those points, never producing `Deny`.
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
    /// Layer has exec restrictions and the command matches none.
    Denied,
    /// Layer admits the command with this allowance.
    Allowed(Admit),
}

/// The two identity sets a command carries into the exec gate.
///
/// A command has two identities — the bare name and the resolved
/// absolute path — and the basename of either form.  These widen the
/// VETO surface but must not widen the ADMISSION surface: a planted
/// `/tmp/evil/rg` invoked by absolute path must not inherit the bare
/// `rg` admission of an outer grant.  So the gate carries both sets and
/// consults them asymmetrically — deny-broad, allow-narrow.
///
/// * `deny` — broad: `policy_names` ∪ basenames of the resolved and
///   as-invoked forms.  Any hit on a literal `Deny`, or any absolute
///   `deny` name landing in a `Deny` dir, vetoes.
/// * `allow` — narrow: exactly `policy_names`.  Only a hit here on a
///   literal `Allow`/`Subcommands`, or an absolute `allow` name landing
///   in an `Allow` dir, admits.
#[derive(Clone, Copy)]
pub(super) struct ExecNames<'a> {
    pub(super) deny: &'a [&'a str],
    pub(super) allow: &'a [&'a str],
}

/// Folded verdict across the whole capability stack.
pub(super) enum ExecVerdict {
    /// No layer has any exec opinion.
    Unrestricted,
    /// At least one layer denies; the call is rejected.
    Denied,
    /// Every opining layer allowed; effective allowance is the
    /// intersection of those layers' allowances.
    Allowed(Admit),
}

/// Walk the stack and combine per-layer verdicts.
///
/// Any layer that denies → command denied.  Allowed opinions
/// intersect.  If the stack declared exec policy but no layer admitted
/// the command, deny; only a stack with no exec policy at all is
/// unrestricted.
///
/// Takes the grant stack alone: exec admission is a question about the
/// policy, not the dynamic context it happens to be evaluated from — the
/// only other inputs, `path::is_absolute` and `path_within_str`, are
/// lexical. This makes the exec verdict pure outright.
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

/// Decide a single layer's verdict on a command.
///
/// The exec map admits or denies commands two ways: a literal key
/// match (bare name or absolute path) and a directory-prefix match.
/// The two identity sets are consulted asymmetrically — deny-broad,
/// allow-narrow (see [`ExecNames`]) — so a basename can close a veto
/// hole without widening admission.  Match order:
///
///   1. A literal `Deny` on any BROAD name is the strongest veto: it
///      fires even when a covering directory would admit the path.
///   2. A literal `Allow`/`Subcommands` on the NARROW names wins next.
///      Multiple narrow literal hits are meet-folded, so a bare
///      `git: Allow` paired with `/usr/bin/git: Deny` still yields
///      `Deny` (the deny side already caught it in step 1; the fold
///      confirms it).
///   3. Otherwise the deepest matching directory prefix wins.  A dir
///      `Deny` propagates as `LayerExec::Denied`; a dir `Allow` yields
///      `LayerExec::Allowed(Allow)`.  Deeper prefix beats shallower, so
///      `/usr/bin/sudo: Deny` excludes a hole inside `/usr/bin: Allow`.
///   4. Neither form fires: strict deny — the deny-by-default that
///      every opining layer carries.
///
/// Dirs match only absolute names; the basenames the broad set adds are
/// bare (no slash) and never absolute, so dir matching sees the same
/// candidates from `allow` and `deny` and needs only the narrow set.
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

/// True iff any broad identity hits a literal `Deny`.  A literal `Deny`
/// is the strongest veto and beats a covering allow dir, so this is
/// consulted before any admission path.
fn literal_vetoes(literals: &BTreeMap<String, ExecPolicy>, deny_names: &[&str]) -> bool {
    deny_names
        .iter()
        .any(|n| matches!(lookup_literal(literals, n), Some(ExecPolicy::Deny)))
}

/// Run the stack-level exec verdict over an explicit deny/allow name
/// pair, returning whether the command is admitted (not denied).  Lets
/// a test feed the narrow set as both sets — reproducing the pre-fix
/// gate, which had no broad veto identity — against the fixed gate.
#[cfg(all(test, unix))]
pub(crate) fn admits_for_test(grants: &GrantStack, deny: &[&str], allow: &[&str]) -> bool {
    !matches!(
        evaluate_exec(grants, ExecNames { deny, allow }),
        ExecVerdict::Denied
    )
}

/// Look up every candidate name as a literal key (bare names and
/// absolute paths both live in the same keyspace).  Multiple hits are
/// meet-folded so a layer that lists the same binary under both a
/// bare name and its resolved path takes the intersection of the two
/// policies.
fn match_literal_keys(
    literals: &BTreeMap<String, ExecPolicy>,
    names: &[&str],
) -> Option<ExecPolicy> {
    let mut matched = names.iter().filter_map(|n| lookup_literal(literals, n));
    let first = matched.next()?;
    Some(matched.fold(first, ExecPolicy::meet))
}

/// Look up `name` among `literals`' keys: an exact match first (the
/// only comparison off Windows, and the common case everywhere), and
/// — under Windows path semantics — a case- and PATHEXT-insensitive
/// scan when the exact lookup misses.  A profile's bare `git` must
/// still admit a resolved `C:\...\GIT.EXE`: PATHEXT resolution picks
/// the extension and Windows command lookup ignores case, so the
/// policy author shouldn't have to spell either out — see
/// [`names_match`].
///
/// Under Windows path identity, distinct keys can be fold-equal (`GIT`
/// and `git` are one name to the OS).  A `BTreeMap` keeps both as
/// separate entries, so every fold-equal match is meet-folded rather
/// than taking whichever the map's iteration order finds first — a
/// literal `Deny` on one spelling must veto even when another spelling
/// says `Allow`, regardless of key insertion order.
fn lookup_literal(literals: &BTreeMap<String, ExecPolicy>, name: &str) -> Option<ExecPolicy> {
    if let Some(policy) = literals.get(name) {
        return Some(policy.clone());
    }
    if cfg!(windows) {
        let mut matches = literals
            .iter()
            .filter(|(key, _)| names_match(key, name, true))
            .map(|(_, policy)| policy.clone());
        let first = matches.next()?;
        return Some(matches.fold(first, ExecPolicy::meet));
    }
    None
}

/// Windows executable-name extensions PATHEXT resolution may append.
/// Mirrors the default `path::which` falls back to (`.COM;.EXE;.BAT;
/// .CMD`) when `%PATHEXT%` is unset.  `.bat`/`.cmd` candidates are
/// refused later, at the spawn boundary (`process::launch`) — that is
/// a separate, later gate on whether a resolved path may be launched
/// at all, not on whether a *name* matches the exec policy, so both
/// extensions still belong in this comparison.
const WINDOWS_EXEC_EXTENSIONS: &[&str] = &["com", "exe", "bat", "cmd"];

/// Strip a trailing extension recognised by [`WINDOWS_EXEC_EXTENSIONS`],
/// case-insensitively.  `git.EXE` → `git`; a name with no extension, or
/// with an extension that isn't in the set (`my.tool`), is returned
/// unchanged.
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

/// True iff `name` itself ends in a recognised executable extension —
/// i.e. the author pinned a specific extension rather than writing a
/// bare stem.
fn names_an_extension(name: &str) -> bool {
    strip_windows_extension(name).len() != name.len()
}

/// True iff `literal` (an exec-map key) and `candidate` (one of a
/// command's identity strings) name the same executable.
///
/// Off Windows this is a byte-exact comparison — the existing rule.
/// Under Windows path semantics, command resolution is case-
/// insensitive and PATHEXT makes a *candidate's* trailing extension
/// transparent, so an unextended `literal` (`git`) matches any
/// PATHEXT-resolved candidate (`git.exe`, `GIT.CMD`, …) stem- and
/// case-insensitively.  But a `literal` that itself names an extension
/// (`git.exe`) is a pin, not a stem: the author asked for exactly that
/// resolved form, so only case is folded, not the extension — a
/// planted `git.com` (which default PATHEXT resolution tries first)
/// must not slip through a `git.exe: 'allow'` pin. `windows` is a
/// parameter rather than a `cfg(windows)` read inside this function so
/// the Windows rule has a name and a unit test that runs on every host
/// — the real platform gate lives at the one call site,
/// [`lookup_literal`].
fn names_match(literal: &str, candidate: &str, windows: bool) -> bool {
    if !windows {
        return literal == candidate;
    }
    if names_an_extension(literal) {
        return literal.eq_ignore_ascii_case(candidate);
    }
    literal.eq_ignore_ascii_case(strip_windows_extension(candidate))
}

/// Find the deepest directory prefix that covers any absolute
/// candidate and return whether it was an allow or a deny.  "Deepest"
/// by [`path::lex::identity_depth`] — components of the alias-folded
/// form, not characters of the raw surface, so a firmlink alias
/// (`/tmp` vs `/private/tmp`) can't buy a shallower directory extra
/// rank by virtue of a longer spelling.  Returns `None` if no
/// directory matches, `Some(true)` for the deepest match being an
/// allow, `Some(false)` for a deny.  An allow and a deny of *equal*
/// depth can both reach here — `Meet`/`Join` only strip a clash where
/// [`same_gate_dir`](crate::path::resolved::NormalizedPrefix::same_gate_dir)
/// holds, and an allow one level short of a deny's directory, or in a
/// different namespace, is no clash at all — so the two loops below
/// break that tie in opposite directions on purpose: allow displaces
/// `best` only on strictly greater depth, deny displaces it on
/// greater-or-equal, so a same-depth deny always ends up the one left
/// standing. A gate's ambiguity must resolve to deny.
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

    #[test]
    fn names_match_off_windows_is_byte_exact() {
        assert!(names_match("git", "git", false));
        assert!(!names_match("git", "git.exe", false));
        assert!(!names_match("git", "Git", false));
    }

    /// The Windows rule is exercised directly — no `cfg(windows)` on
    /// this test — so it runs on every CI host, matching the plan's
    /// requirement that Windows admission logic have a unit test
    /// reachable from Unix CI.
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

    /// M1 regression: a literal that itself names an extension is a
    /// pin, not a stem — it must not admit a different PATHEXT
    /// candidate for the same stem.  Without this, a profile pinning
    /// `git.exe: 'allow'` would also admit a planted `git.com`, which
    /// default PATHEXT resolution tries first.
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

    /// The tie-break half of the authority-leak fix: an allow and a
    /// deny of equal depth both covering the candidate must resolve to
    /// deny.  This is the shape `ExecMap::join` leaves behind when a
    /// base veto's surface is re-granted with a divergent `resolved` —
    /// `Meet`/`Join` no longer let that pair share a set slot, but this
    /// pins the gate's own half of the fix directly, independent of
    /// composition.
    #[test]
    fn longest_dir_match_ties_resolve_to_deny() {
        let exec = ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::from([path::NormalizedPrefix::from_surface("/x")]),
            deny_dirs: BTreeSet::from([path::NormalizedPrefix::for_test(
                "/x",
                "/y",
                path::Namespace::Host,
            )]),
        };
        assert_eq!(longest_dir_match(&exec, &["/x/bin"]), Some(false));
    }

    /// The alias-clash half of the sibling hole: a deny on `/tmp/bin`
    /// and an allow on its firmlink alias `/private/tmp/bin` name the
    /// same directory to the gate (`path_within` follows firmlinks),
    /// but byte-compare distinct, longer surfaces outrank shorter ones
    /// under the old character-count depth metric — so the allow used
    /// to outrank the deny outright, no tie-break needed. Fixed by
    /// `same_gate_dir` catching the clash at composition and
    /// `identity_depth` ranking both at the same depth so the deny-wins
    /// tie-break (above) closes it even if a clash reached the gate
    /// directly, as here.
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

    /// The depth-metric half of the sibling hole, independent of the
    /// first: `/tmp/a/b` is a real 3-component path, `/private/tmp` is
    /// a firmlink alias of the 1-component `/tmp`, but the old
    /// character-count metric ranked the 12-character alias above the
    /// 8-character real descendant — fail-open regardless of which
    /// side is allow or deny. `identity_depth` counts alias-folded
    /// components, so `/tmp/a/b` (4, folded to
    /// `/private/tmp/a/b`) outranks `/private/tmp` (2) as it should.
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

    /// `lookup_literal` itself reads the real `cfg(windows)`, so a case
    /// mismatch only resolves on an actual Windows host — asserted
    /// against `cfg!(windows)` rather than a fixed platform so this
    /// test is honest on every CI host.  [`names_match`] above is where
    /// the Windows rule itself gets a fixed-outcome test.
    #[test]
    fn lookup_literal_case_mismatch_follows_real_platform() {
        let literals = BTreeMap::from([("git".to_string(), ExecPolicy::Allow)]);
        let hit = lookup_literal(&literals, "Git");
        assert_eq!(hit.is_some(), cfg!(windows));
    }

    /// M2 regression: `GIT` and `git` are fold-equal keys under Windows
    /// path identity.  A `BTreeMap` keeps them as two entries, so a
    /// naive first-match scan resolves the collision by iteration
    /// order — here `"GIT" < "git"` byte-wise, so the pre-fix `.find`
    /// would return `GIT`'s `Allow` and never see `git`'s `Deny`.  The
    /// fix meet-folds every fold-equal hit, so the veto always wins
    /// regardless of order.
    #[test]
    fn lookup_literal_meets_fold_equal_keys_deny_wins() {
        let literals = BTreeMap::from([
            ("GIT".to_string(), ExecPolicy::Allow),
            ("git".to_string(), ExecPolicy::Deny),
        ]);
        let hit = lookup_literal(&literals, "Git.exe");
        assert_eq!(hit.is_some(), cfg!(windows));
        if cfg!(windows) {
            assert_eq!(hit, Some(ExecPolicy::Deny));
        }
    }
}
