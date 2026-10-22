//! macOS sandbox using the Seatbelt (`sandbox_init`) API.
//!
//! Only the per-command re-exec child is confined: the launch compiles the
//! projection with [`build_profile`] and hands it over in the child's
//! warrant, which enters it through [`apply_profile`] before becoming the
//! target — a host binary, or a bundled tool in-process — which inherits the
//! confinement.  The parent ral process is never confined; authorising a
//! binary through `exec:` already shifts trust to that binary.
//!
//! Seatbelt has no per-address network rules, so `SandboxProjection::net` is
//! one allow/deny bit rather than an endpoint list.

use super::{ExecProjection, FsProjection, RenderedRules, SandboxProjection};
use crate::capability::{ExecAllow, ExecDeny};
use crate::path::{Object, RealPath, Rendered, Walked, render_real, rendered_ancestors};
use std::collections::BTreeSet;
use std::ffi::{CStr, CString};
use std::fmt;
use std::os::raw::{c_char, c_int};

/// Seatbelt profiles do not stack: a process already inside one gets EPERM
/// entering another, and a launch promising more than that profile is refused.
pub const ALREADY_PROFILED: &str = "this process is already inside a macOS sandbox profile and macOS \
     will not let it enter a second, so the launch is refused rather than run under that wider \
     profile; is ral itself running confined?";

/// Apply `profile` to the current process.  Seatbelt entry cannot be undone.
pub(super) fn apply_profile(profile: &str) -> std::io::Result<()> {
    fn cstr(s: &str, what: &str) -> std::io::Result<CString> {
        CString::new(s).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{what} contains NUL byte"),
            )
        })
    }
    let profile_cstr = cstr(profile, "sandbox profile")?;
    // No SBPL parameters: a lone null terminator.
    let parameter_ptrs: [*const c_char; 1] = [std::ptr::null()];

    let mut errorbuf: *mut c_char = std::ptr::null_mut();
    let rc = unsafe {
        sandbox_init_with_parameters(
            profile_cstr.as_ptr(),
            0,
            parameter_ptrs.as_ptr(),
            &raw mut errorbuf,
        )
    };
    if rc != 0 {
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
            return Err(std::io::Error::other(ALREADY_PROFILED));
        }
        let message = if errorbuf.is_null() {
            "sandbox_init_with_parameters failed".to_string()
        } else {
            unsafe { CStr::from_ptr(errorbuf) }
                .to_string_lossy()
                .into_owned()
        };
        return Err(std::io::Error::other(message));
    }
    Ok(())
}

/// Policy-independent SBPL preamble: `(version 1)`, `(deny default)`, and the
/// Apple-required carve-outs.  A sibling file so the rules read as SBPL rather
/// than `format!()` strings.
const BASE_PROFILE: &str = include_str!("macos-base.sbpl");

/// The `net: true` rules — the wholesale socket allow and the resolver and
/// trust-evaluation Mach doors that ride the same bit.
const NET_PROFILE: &str = include_str!("macos-net.sbpl");

/// The profile as sections, in the order Seatbelt reads them.  Last match
/// wins, so a section overrides only those above it: a deny reaches the allows
/// before it, never one after.
#[derive(Default)]
struct Profile {
    /// What the fs grant reads and writes, with the directory lookups it needs.
    fs_allows: Vec<String>,
    /// Every `process-exec` allow, then every deny.
    exec_rules: Vec<String>,
    /// The lookups the exec admits need through their ancestors.
    exec_ancestors: Vec<String>,
    /// What the fs grant carves out of its allows, and every program it does
    /// not read.
    fs_denies: Vec<String>,
    /// Directory names frozen in their parents, and ral's own file locked.
    pins: Vec<String>,
    net: bool,
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            fs_allows,
            exec_rules,
            exec_ancestors,
            fs_denies,
            pins,
            net,
        } = self;
        f.write_str(BASE_PROFILE)?;
        for line in [fs_allows, exec_rules, exec_ancestors, fs_denies, pins]
            .into_iter()
            .flatten()
        {
            write!(f, "\n{line}")?;
        }
        if *net {
            write!(f, "\n{NET_PROFILE}")?;
        }
        Ok(())
    }
}

pub(super) fn build_profile(policy: &SandboxProjection) -> Result<String, String> {
    let mut profile = Profile {
        net: policy.net,
        ..Profile::default()
    };
    let rendered = policy.rendered()?;
    match &rendered.fs {
        FsProjection::Restricted(rules) => emit_fs_restricted(&mut profile, rules),
        FsProjection::Unrestricted => {
            // Pass fs through, so an exec-only grant can enter the sandbox
            // for exec gating without clamping the agent's cwd or HOME.
            profile
                .fs_allows
                .extend(["(allow file-read*)", "(allow file-write*)"].map(String::from));
        }
    }

    emit_exec_rules(&mut profile, &rendered.exec)?;

    // Ral's own file, rendered like the rest: execve presents `/tmp/x` as
    // `/private/tmp/x`.  Protected whatever the grant, never admitted by it.
    let own = render_real(super::reexec::own()?.trace().real())?;
    let pinned = rendered
        .fs
        .rules()
        .map(|r| r.pinned_dirs.as_slice())
        .unwrap_or_default();
    // Ral's own file is unwritable and its path unrenameable, whatever the
    // grant: a swap between the parent's check and the re-exec would never
    // enter Seatbelt.
    let own_ancestors = rendered_ancestors(&own);
    emit_pins(&mut profile.pins, pinned.iter().chain(&own_ancestors));
    profile.pins.extend(
        own.iter()
            .map(|path| Clause::deny("file-write*", Filter::Literal(path)).to_string()),
    );

    Ok(profile.to_string())
}

/// Freeze each directory's name in its parent, its entries staying mutable:
/// `literal`, never `subpath`, which would also block unlinking every entry
/// inside it.  Unconditional on existence: a directory absent now can be
/// created later under the write prefix that covers it.  Deduped, since the
/// pinned dirs and ral's ancestors may overlap.
fn emit_pins<'a>(pins: &mut Vec<String>, dirs: impl IntoIterator<Item = &'a Rendered>) {
    let mut seen = BTreeSet::new();
    for dir in dirs.into_iter().filter(|dir| seen.insert(*dir)) {
        pins.push(Clause::deny("file-write-unlink", Filter::Literal(dir)).to_string());
    }
}

/// Emit a restricted fs projection: its allows, the lookups they need, and its
/// denies.  Every allowance renders as its frozen real path and its firmlink
/// twin; the surface it was granted by is only stat-able on the way.  The read
/// region carries what exec runs and what a launch needs beyond it, so exec's
/// own clauses read nothing.
fn emit_fs_restricted(profile: &mut Profile, rules: &RenderedRules) {
    let objects = || rules.read.iter().chain(&rules.write);
    let readable: Vec<&Rendered> = objects().flat_map(Object::names).collect();
    let walk: BTreeSet<&Rendered> = objects().flat_map(|o| &o.walk).collect();
    let stat: BTreeSet<Rendered> = rendered_ancestors(readable.iter().chain(&walk).copied())
        .into_iter()
        .chain(walk.into_iter().cloned())
        .collect();
    let lines = &mut profile.fs_allows;
    for name in &stat {
        lines.push(Clause::allow("file-read-metadata", Filter::Literal(name)).to_string());
    }
    for path in &readable {
        lines.push(Clause::allow("file-read*", Filter::Subpath(path)).to_string());
    }
    for prefix in rules.write.iter().flat_map(Object::names) {
        lines.push(Clause::allow("file-write*", Filter::Subpath(prefix)).to_string());
    }
    // Seatbelt starts a program it may not read, so running reads is said here.
    profile.fs_denies.push(match readable.as_slice() {
        [] => "(deny process-exec)".to_string(),
        read => Clause::deny("process-exec", Filter::Outside(read)).to_string(),
    });
    // `subpath` so a denied directory covers everything under it; `file-link`
    // (Seatbelt has no `file-link*`) blocks `link(2)` against the source,
    // closing the hole where a second name elsewhere would let writes bypass
    // the deny.
    for path in &rules.deny {
        profile.fs_denies.extend(
            ["file-read*", "file-write*", "file-link", "process-exec"]
                .map(|op| Clause::deny(op, Filter::Subpath(path)).to_string()),
        );
    }
}

/// Seatbelt carries the exec allow-list into the kernel via the
/// `process-exec` clauses [`emit_exec_rules`] renders below.
pub(crate) const RENDERS_EXEC: bool = true;

/// A `process-exec` clause.
fn exec_clause(allow: bool, filter: Filter) -> String {
    Clause {
        allow,
        ops: "process-exec",
        filter,
    }
    .to_string()
}

/// A clause for each name the kernel may present `path` by: the subtree of a
/// `dir`, a file's literal.
fn exec_clauses(allow: bool, path: &RealPath, dir: bool) -> Result<Vec<String>, String> {
    let clause = |name| {
        let filter = if dir {
            Filter::Subpath(name)
        } else {
            Filter::Literal(name)
        };
        exec_clause(allow, filter)
    };
    Ok(render_real(path)?.iter().map(clause).collect())
}

/// Render the `process-exec` rules.  `Unrestricted` is a wildcard.
/// `Restricted` admits every positive scope (`Any` as the wildcard), then
/// every negative one, so that an exclusion reaches everything above it:
/// Seatbelt is last-match-wins.
/// Order within a group is for stable output; depth grants no exception.  A
/// place both allowed and excluded emits both.  Support arrives among the
/// positives as file allows.  Exec clauses are `process-exec` alone: reads are
/// the fs section's.  Rule by rule:
/// `docs/ral-wiki/internals/seatbelt-profile.md`.
///
/// `Err` when a name's expansion is not valid UTF-8, which [`render_real`]
/// refuses.
fn emit_exec_rules(profile: &mut Profile, exec: &ExecProjection) -> Result<(), String> {
    let ExecProjection::Restricted(rules) = exec else {
        profile.exec_rules.push(ANY_EXEC.to_string());
        return Ok(());
    };
    let positives = rules.allows().map(|scope| match scope {
        ExecAllow::Any => Ok(vec![ANY_EXEC.to_string()]),
        ExecAllow::Dir(dir) => exec_clauses(true, &dir.real.real(), true),
        ExecAllow::File { real, .. } => exec_clauses(true, real, false),
        ExecAllow::Tool(_) => Ok(Vec::new()),
    });
    let negatives = rules.denies().map(|scope| match scope {
        ExecDeny::Dir(dir) => exec_clauses(false, dir.real.followed().real(), true),
        ExecDeny::File(file) => exec_clauses(false, file.real.followed().real(), false),
        ExecDeny::Name(name) => Ok(vec![exec_clause(false, Filter::Regex(name))]),
    });
    for clauses in positives.chain(negatives) {
        profile.exec_rules.extend(clauses?);
    }
    let granted = rules
        .live()
        .filter_map(|scope| match scope {
            ExecAllow::Dir(dir) => Some(render_real(&dir.real.real())),
            ExecAllow::File { real, .. } => Some(render_real(real)),
            _ => None,
        })
        .collect::<Result<Vec<_>, _>>()?
        .concat();
    emit_ancestor_metadata(&mut profile.exec_ancestors, &granted);
    Ok(())
}

/// Every program may start: the wildcard, and what `Any` renders as.
const ANY_EXEC: &str = "(allow process-exec)";

/// What an SBPL clause matches.  `Display` escapes, so no name is spliced into
/// a rule without it.
enum Filter<'a> {
    Subpath(&'a Rendered),
    Literal(&'a Rendered),
    /// A final path component, wherever the name resolves.
    Regex(&'a str),
    /// Beneath none of these; never empty, which SBPL cannot say.
    Outside(&'a [&'a Rendered]),
}

impl fmt::Display for Filter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Subpath(path) => write!(f, "(subpath \"{}\")", escape_path(path)),
            Self::Literal(path) => write!(f, "(literal \"{}\")", escape_path(path)),
            Self::Regex(name) => write!(f, "(regex #\"/{}$\")", escape_regex(name)),
            Self::Outside(paths) => {
                f.write_str("(require-not (require-any")?;
                for path in *paths {
                    write!(f, " {}", Self::Subpath(path))?;
                }
                f.write_str("))")
            }
        }
    }
}

/// One SBPL rule: `(allow ops filter)` or `(deny ops filter)`.
struct Clause<'a> {
    allow: bool,
    ops: &'static str,
    filter: Filter<'a>,
}

impl<'a> Clause<'a> {
    const fn allow(ops: &'static str, filter: Filter<'a>) -> Self {
        Self {
            allow: true,
            ops,
            filter,
        }
    }

    const fn deny(ops: &'static str, filter: Filter<'a>) -> Self {
        Self {
            allow: false,
            ops,
            filter,
        }
    }
}

impl fmt::Display for Clause<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let verb = if self.allow { "allow" } else { "deny" };
        write!(f, "({verb} {} {})", self.ops, self.filter)
    }
}

/// Mach doors the base profile withholds on purpose, each with its reason as
/// data: a comment cannot reach the agent holding the `EPERM`, and a deliberate
/// refusal read as an accident is chased instead of worked around.
fn withheld_doors() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "com.apple.SecurityServer",
            "securityd hands out keychain items, so no profile admits it; cargo's bundled \
             git needs it for TLS, so set `net.git-fetch-with-cli` and cargo will fetch \
             through the git binary instead",
        ),
        (
            "com.apple.coreservices.launchservicesd",
            "launchservicesd spawns `/usr/bin/open`'s target as a child of launchd, outside \
             this profile: an escape, and for a URL an egress channel under `net: false`",
        ),
        (
            "com.apple.pasteboard",
            "the pasteboard is a clipboard no grant mentions",
        ),
    ]
}

/// The reason the base profile withholds `name`, for
/// [`crate::sandbox::diag`]'s denial hint.  Matched by prefix: Seatbelt logs a
/// Mach name with the instance suffix the lookup carried
/// (`com.apple.pasteboard.1`, `com.apple.distributed_notifications@Uv3`).
pub(super) fn withheld_door(name: &str) -> Option<&'static str> {
    withheld_doors()
        .iter()
        .find(|(door, _)| name.starts_with(door))
        .map(|(_, why)| *why)
}

/// Seatbelt gates each directory lookup on the way to a granted name.
/// Metadata is enough: search is all a resolver — the kernel's, or ral's walk
/// — holds on an ancestor.  Over every rendered path handed in, system and
/// self-exec included, which the projection's `pinned_dirs` never saw.
fn emit_ancestor_metadata<'a>(
    lines: &mut Vec<String>,
    paths: impl IntoIterator<Item = &'a Rendered>,
) {
    for ancestor in rendered_ancestors(paths) {
        lines.push(Clause::allow("file-read-metadata", Filter::Literal(&ancestor)).to_string());
    }
}

/// Quote a rendered name for an SBPL string literal.  Taking [`Rendered`] and
/// not `&str` is what makes it impossible to splice a path into a rule without
/// first passing it through [`render_real`]: every emitter in this file
/// funnels here, so the only `&str` items left are the denied basenames, which
/// are final components rather than paths and go to [`escape_regex`].
fn escape_path(path: &Rendered) -> String {
    path.as_str().replace('\\', "\\\\").replace('"', "\\\"")
}

/// Escape a command name for an SBPL `(regex …)` pattern, so a name like
/// `c++` or `python3.11` matches itself instead of acting as a pattern.
fn escape_regex(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if r".^$*+?()[]{}|\/".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out.replace('"', "\\\"")
}

unsafe extern "C" {
    fn sandbox_init_with_parameters(
        profile: *const c_char,
        flags: u64,
        parameters: *const *const c_char,
        errorbuf: *mut *mut c_char,
    ) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::{BASE_PROFILE, NET_PROFILE, Profile, build_profile, withheld_doors};
    use crate::capability::{ExecAllow, ExecDeny, Extent, FsAllow, GrantPath, Rule};
    use crate::path::proper_ancestors;
    use crate::path::{Excluded, FrozenPath, command_name_key};
    use crate::sandbox::{ExecProjection, FsProjection, FsRules, SandboxProjection};
    use crate::test_helper::trace;

    /// The profile with its commentary dropped — what the kernel reads.  The
    /// `.sbpl` files argue at length for what they leave *out*, so a test
    /// asking whether a service is admitted must not read the argument as the
    /// admission.
    fn rules(profile: &str) -> String {
        profile
            .lines()
            .filter(|l| !l.trim_start().starts_with(";;"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn dir(path: &str, allow: bool) -> Rule {
        let written = FrozenPath::from_surface(path);
        if allow {
            Rule::Allow(ExecAllow::Dir(
                GrantPath::<Extent>::allowing(written).expect("a directory walks"),
            ))
        } else {
            Rule::Deny(Box::new(ExecDeny::Dir(
                GrantPath::<Excluded>::walk(written).expect("a directory walks"),
            )))
        }
    }

    fn file(path: &str, allow: bool) -> Rule {
        let written = FrozenPath::from_surface(path);
        if allow {
            Rule::Allow(ExecAllow::File {
                real: trace(path).real().clone(),
                written,
                found_as: None,
            })
        } else {
            Rule::Deny(Box::new(ExecDeny::File(
                GrantPath::<Excluded>::walk(written).expect("a file walks"),
            )))
        }
    }

    fn veto(name: &str) -> Rule {
        Rule::Deny(Box::new(ExecDeny::Name(command_name_key(name))))
    }

    fn restricted(fs: FsProjection, rules: Vec<Rule>) -> SandboxProjection {
        SandboxProjection {
            fs,
            exec: ExecProjection::Restricted(rules.into_iter().collect()),
            ..SandboxProjection::default()
        }
    }

    fn prefix(path: &str) -> FsAllow {
        FsAllow::Prefix(
            GrantPath::<Extent>::allowing(FrozenPath::from_surface(path)).expect("a prefix walks"),
        )
    }

    fn frozen(paths: &[&str]) -> Vec<GrantPath<Excluded>> {
        (paths.iter())
            .map(|p| GrantPath::walk(FrozenPath::from_surface(p)).expect("an exclusion walks"))
            .collect()
    }

    /// A restricted fs writing under `prefixes` and nothing else.
    fn writing(prefixes: &[&str]) -> FsProjection {
        FsProjection::Restricted(FsRules {
            write: prefixes.iter().copied().map(prefix).collect(),
            ..FsRules::default()
        })
    }

    /// A restricted fs reading `reads` and nothing else.
    fn reading(reads: Vec<FsAllow>) -> FsProjection {
        FsProjection::Restricted(FsRules {
            read: reads,
            ..FsRules::default()
        })
    }

    /// The index of `form` in `profile`, which must hold it.
    fn at(profile: &str, form: &str) -> usize {
        profile
            .find(form)
            .unwrap_or_else(|| panic!("missing {form}:\n{profile}"))
    }

    #[test]
    fn mac_shell_profile_allows_general_exec_when_unrestricted() {
        let profile = build_profile(&SandboxProjection::default()).unwrap();
        assert!(profile.contains("(allow process-exec)"));
        assert!(!profile.contains("(allow file-read* process-exec"));
    }

    /// Positive clauses are `process-exec` alone: the reads are the fs
    /// section's, so an exec admit reads nothing of its own.
    #[test]
    fn mac_profile_renders_each_rule_as_one_exec_only_form() {
        let policy = restricted(
            FsProjection::default(),
            vec![
                dir("/usr/bin", true),
                dir("/opt/homebrew/bin", true),
                file("/usr/bin/git", true),
            ],
        );
        let profile = build_profile(&policy).unwrap();
        for form in [
            "(allow process-exec (subpath \"/usr/bin\"))",
            "(allow process-exec (subpath \"/opt/homebrew/bin\"))",
            "(allow process-exec (literal \"/usr/bin/git\"))",
            "(allow file-read-metadata (literal \"/usr\"))",
            "(allow file-read-metadata (literal \"/usr/bin\"))",
        ] {
            at(&profile, form);
        }
        assert!(
            !profile.contains("(allow process-exec)\n"),
            "wildcard process-exec leaked into restricted profile"
        );
        assert!(
            !profile.contains("(allow file-read* (subpath \"/usr/bin\"))"),
            "an exec admit read what the fs grant did not:\n{profile}"
        );
    }

    /// A directory admitted from outside the fs grant is reached by lookup
    /// through its ancestors, which Seatbelt gates one by one.
    #[test]
    fn mac_profile_grants_a_directory_allow_its_ancestor_metadata() {
        let policy = restricted(
            FsProjection::default(),
            vec![dir("/opt/ral-test/tools/bin", true)],
        );
        let profile = build_profile(&policy).unwrap();
        for ancestor in ["/opt", "/opt/ral-test", "/opt/ral-test/tools"] {
            at(
                &profile,
                &format!("(allow file-read-metadata (literal \"{ancestor}\"))"),
            );
        }
    }

    /// Seatbelt is last-match-wins, so a section's place is its precedence: a
    /// deny reaches only the allows above it.
    #[test]
    fn mac_profile_writes_its_sections_in_precedence_order() {
        let section = |name: &str| vec![format!("({name})")];
        let profile = Profile {
            fs_allows: section("fs-allows"),
            exec_rules: section("exec-rules"),
            exec_ancestors: section("exec-ancestors"),
            fs_denies: section("fs-denies"),
            pins: section("pins"),
            net: true,
        }
        .to_string();
        let sections = profile
            .strip_prefix(BASE_PROFILE)
            .and_then(|rest| rest.strip_suffix(&format!("\n{NET_PROFILE}")))
            .expect("the base leads and the net rules trail");
        assert_eq!(
            sections,
            "\n(fs-allows)\n(exec-rules)\n(exec-ancestors)\n(fs-denies)\n(pins)"
        );
    }

    /// Every positive clause precedes every negative one, whatever the depth
    /// of the scopes: depth grants no exception.
    #[test]
    fn mac_profile_emits_positives_then_negatives() {
        let policy = restricted(
            FsProjection::default(),
            vec![
                dir("/x", true),
                dir("/x/y", false),
                file("/x/y/z", true),
                veto("z"),
            ],
        );
        let profile = build_profile(&policy).unwrap();
        let positives = [
            "(allow process-exec (subpath \"/x\"))",
            "(allow process-exec (literal \"/x/y/z\"))",
        ];
        let negatives = [
            "(deny process-exec (subpath \"/x/y\"))",
            r#"(deny process-exec (regex #"/z$"))"#,
        ];
        let last_positive = positives.iter().map(|f| at(&profile, f)).max().unwrap();
        let first_negative = negatives.iter().map(|f| at(&profile, f)).min().unwrap();
        assert!(last_positive < first_negative, "out of order:\n{profile}");
    }

    /// A place both allowed and excluded emits both clauses.
    #[test]
    fn mac_profile_emits_both_clauses_for_a_place_allowed_and_excluded() {
        let both = vec![file("/x/tool", true), file("/x/tool", false)];
        let profile = build_profile(&restricted(FsProjection::default(), both)).unwrap();
        let allow = at(&profile, "(allow process-exec (literal \"/x/tool\"))");
        let deny = at(&profile, "(deny process-exec (literal \"/x/tool\"))");
        assert!(allow < deny);
    }

    /// `Any` is the wildcard among the positives, and the exclusions still
    /// follow it.
    #[test]
    fn mac_profile_renders_any_as_unrestricted_exec_before_its_exclusions() {
        let policy = restricted(
            FsProjection::default(),
            vec![Rule::Allow(ExecAllow::Any), veto("git")],
        );
        let profile = build_profile(&policy).unwrap();
        let any = at(&profile, "(allow process-exec)\n");
        let deny = at(&profile, r#"(deny process-exec (regex #"/git$"))"#);
        assert!(any < deny);
    }

    /// Running reads: under a restricted fs, a program outside the read
    /// region starts under no exec rule, `every` included.
    #[test]
    fn mac_profile_runs_nothing_the_fs_does_not_read() {
        let any = || vec![Rule::Allow(ExecAllow::Any)];
        let profile =
            build_profile(&restricted(reading(vec![prefix("/ral-test/r")]), any())).unwrap();
        let wildcard = at(&profile, "(allow process-exec)\n");
        let bound = at(
            &profile,
            "(deny process-exec (require-not (require-any (subpath \"/ral-test/r\"))))",
        );
        assert!(wildcard < bound);
        let nothing = build_profile(&restricted(reading(Vec::new()), any())).unwrap();
        at(&nothing, "(deny process-exec)\n");
        let open = build_profile(&restricted(FsProjection::Unrestricted, any())).unwrap();
        assert!(!open.contains("(deny process-exec"), "{open}");
    }

    /// Nothing is frozen: a write prefix covering an admitted dir, or a file
    /// it names, leaves it writable, exclusion or not.
    #[test]
    fn mac_profile_freezes_nothing_a_write_prefix_covers() {
        let policy = restricted(
            writing(&["/ral-test/w"]),
            vec![
                dir("/ral-test/w/bin", true),
                file("/ral-test/w/tool", true),
                veto("git"),
            ],
        );
        let profile = build_profile(&policy).unwrap();
        assert!(
            !profile.contains("(deny file-write* (subpath")
                && !profile.contains("(deny file-write* (literal \"/ral-test"),
            "an admitted path was frozen:\n{profile}"
        );
        let open = restricted(
            FsProjection::Unrestricted,
            vec![dir("/usr/bin", true), veto("git")],
        );
        assert!(
            !build_profile(&open)
                .unwrap()
                .contains("(deny file-write* (subpath \"/usr/bin\"))"),
            "a veto froze the admitted set"
        );
    }

    /// The read region renders as its frozen real path, run places included,
    /// whether or not exec is restricted.
    #[test]
    fn mac_profile_reads_a_place_as_its_frozen_real_path_and_twin() {
        let tmp = tempfile::tempdir().expect("a temp dir");
        let place = crate::test_helper::real(tmp.path());
        let forms = crate::path::canon::with_firmlink_twins(vec![place.as_path().to_path_buf()]);
        let profile = build_profile(&SandboxProjection {
            fs: reading(vec![FsAllow::RunPlace(place)]),
            ..SandboxProjection::default()
        })
        .unwrap();
        for form in forms {
            let form = crate::path::utf8(&form).unwrap();
            at(
                &profile,
                &format!("(allow file-read* (subpath \"{form}\"))"),
            );
        }
    }

    /// An allowance granted through a symlink reads as its frozen real path
    /// alone: the surface it was written by is only stat-able, so a lookup
    /// may pass the link and reach nothing beneath it.
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "[test] test fs scaffolding: a tempdir tree and a symlink"
    )]
    fn mac_profile_lets_a_lookup_stat_the_surface_of_a_frozen_allowance() {
        let tmp = tempfile::tempdir().expect("a temp dir");
        let root = crate::test_helper::real(tmp.path());
        let (real, link) = (root.as_path().join("real"), root.as_path().join("link"));
        std::fs::create_dir(&real).expect("the target");
        std::os::unix::fs::symlink(&real, &link).expect("a link to the target");
        let (real, link) = (
            crate::path::utf8(&real).unwrap(),
            crate::path::utf8(&link).unwrap(),
        );
        let linked = prefix(link);
        let profile = build_profile(&SandboxProjection {
            fs: reading(vec![linked]),
            ..SandboxProjection::default()
        })
        .unwrap();
        at(
            &profile,
            &format!("(allow file-read* (subpath \"{real}\"))"),
        );
        let kernel = rules(&profile);
        let surface: Vec<&str> = kernel.lines().filter(|l| l.contains(link)).collect();
        assert_eq!(
            surface,
            [format!("(allow file-read-metadata (literal \"{link}\"))")],
            "{profile}"
        );
    }

    /// The kernel exec set is the rules: `/usr`, the toolchains and Homebrew
    /// are readable but run only where the grant (`system:`) names them, and
    /// ral's own binary only where it names that.
    #[test]
    fn mac_profile_admits_no_ambient_exec() {
        let policy = restricted(reading(vec![prefix("/usr")]), vec![dir("/usr/bin", true)]);
        let profile = build_profile(&policy).unwrap();
        let exec_allows: Vec<&str> = profile
            .split("\n(")
            .filter(|form| form.starts_with("allow process-exec"))
            .collect();
        assert!(
            exec_allows
                .iter()
                .any(|form| form.contains("(subpath \"/usr/bin\")"))
        );
        for dir in [
            "/bin",
            "/usr",
            "/opt/homebrew",
            "/Library/Developer/CommandLineTools",
            "/Applications/Xcode.app/Contents/Developer",
        ] {
            assert!(
                !exec_allows
                    .iter()
                    .any(|form| form.contains(&format!("(subpath \"{dir}\")"))),
                "{dir} is exec-admitted without a grant:\n{profile}"
            );
        }
        assert!(profile.contains("(allow file-read* (subpath \"/usr\"))"));
        let own = own_names();
        assert!(
            !own.iter().any(|o| profile.contains(&format!(
                "(allow process-exec (literal \"{}\"))",
                o.as_str()
            ))),
            "ral's own binary was admitted without the grant naming it:\n{profile}"
        );
    }

    /// An empty exec table admits no program, ral and Rosetta's runtime
    /// included: an Intel program needs `process-exec` on itself alone.
    #[test]
    fn mac_profile_admits_nothing_when_restricted_to_empty() {
        super::super::reexec::pin_self();
        let profile = build_profile(&restricted(FsProjection::default(), Vec::new())).unwrap();
        assert!(!profile.contains("(allow process-exec"));
    }

    /// Exec denies follow the allow they carve, and deny no reads: reads are
    /// the fs grant's to refuse.
    #[test]
    fn mac_profile_exec_denies_follow_their_allow_and_deny_no_reads() {
        let policy = restricted(
            FsProjection::default(),
            vec![
                dir("/usr/bin", true),
                dir("/usr/bin/sensitive", false),
                file("/usr/bin/git", false),
            ],
        );
        let profile = build_profile(&policy).unwrap();
        let allow = at(&profile, "(allow process-exec (subpath \"/usr/bin\"))");
        let deny_dir = at(
            &profile,
            "(deny process-exec (subpath \"/usr/bin/sensitive\"))",
        );
        let deny_git = at(&profile, "(deny process-exec (literal \"/usr/bin/git\"))");
        assert!(allow < deny_dir && allow < deny_git);
        assert!(
            !profile.contains("(deny file-read*"),
            "an exec deny carved reads:\n{profile}"
        );
    }

    /// A veto renders as a `/name$` regex, so the name is exec-denied
    /// wherever it resolves, with metacharacters escaped so `c++` matches
    /// itself.
    #[test]
    fn mac_profile_emits_a_veto_as_a_final_component_regex() {
        let policy = restricted(
            FsProjection::default(),
            vec![dir("/usr/bin", true), veto("git"), veto("c++")],
        );
        let profile = build_profile(&policy).unwrap();
        let allow = at(&profile, "(allow process-exec (subpath \"/usr/bin\"))");
        let veto = at(&profile, r#"(deny process-exec (regex #"/git$"))"#);
        at(&profile, r#"(deny process-exec (regex #"/c\+\+$"))"#);
        assert!(allow < veto, "a veto must follow the allow it overrides");
        assert!(
            !profile.contains(r#"(deny file-read* (regex #"/git$"))"#),
            "a veto must not carve reads:\n{profile}"
        );
    }

    /// `/tmp` firmlinks to `/private/tmp`, and execve names the canonical
    /// spelling: a literal exec rule left raw would rule on a path the kernel
    /// never presents, so admit and deny alike expand like the dir rules do.
    #[test]
    fn mac_profile_expands_firmlinks_in_literal_exec_rules() {
        let policy = restricted(
            FsProjection::default(),
            vec![file("/tmp/tool", true), file("/tmp/evil", false)],
        );
        let profile = build_profile(&policy).unwrap();
        for form in ["/tmp/tool", "/private/tmp/tool"] {
            at(
                &profile,
                &format!("(allow process-exec (literal \"{form}\"))"),
            );
        }
        for form in ["/tmp/evil", "/private/tmp/evil"] {
            at(
                &profile,
                &format!("(deny process-exec (literal \"{form}\"))"),
            );
        }
    }

    /// Ral's binary is an ordinary file: admitted where the policy names it,
    /// under every spelling execve presents, and nowhere else.
    #[test]
    fn mac_profile_admits_ral_only_where_the_policy_names_it() {
        let own = own_names();
        let named = restricted(FsProjection::default(), vec![file(own[0].as_str(), true)]);
        let profile = build_profile(&named).unwrap();
        for form in &own {
            assert!(
                profile.contains(&format!(
                    "(allow process-exec (literal \"{}\"))",
                    form.as_str()
                )),
                "ral's spelling {} missing:\n{profile}",
                form.as_str()
            );
        }
    }

    /// Ral's own spellings, as the profile will name them.
    fn own_names() -> Vec<crate::path::Rendered> {
        super::super::reexec::pin_self();
        let own = super::super::reexec::own().expect("pin_self pins this test binary");
        crate::path::render_real(own.trace().real()).unwrap()
    }

    /// A swap of ral's binary between the parent's check and the re-exec
    /// would run a program that never enters Seatbelt, so the file and the
    /// names above it are frozen in every profile.  The write prefix is ral's
    /// own directory, so the pinned dir the deny below it needs is also one of
    /// ral's ancestors: the two emitters' overlap must leave one line, not two.
    #[test]
    fn mac_profile_locks_ral_and_the_names_above_it() {
        let own = own_names();
        let ancestors = crate::path::rendered_ancestors(&own);
        let dir = ancestors
            .iter()
            .max_by_key(|a| a.as_str().len())
            .expect("ral lies below the root")
            .as_str()
            .to_string();
        let restricted_fs = FsProjection::Restricted(FsRules {
            read: vec![prefix(&dir)],
            write: vec![prefix(&dir)],
            deny: frozen(&[&format!("{dir}/x/secret")]),
            ..FsRules::default()
        });
        for fs in [FsProjection::Unrestricted, restricted_fs] {
            let profile = build_profile(&SandboxProjection {
                fs,
                ..SandboxProjection::default()
            })
            .unwrap();
            let locks = own
                .iter()
                .map(|p| format!("(deny file-write* (literal \"{}\"))", p.as_str()))
                .chain(
                    ancestors
                        .iter()
                        .map(|a| format!("(deny file-write-unlink (literal \"{}\"))", a.as_str())),
                );
            for lock in locks {
                at(&profile, &lock);
                assert_eq!(
                    profile.matches(&lock).count(),
                    1,
                    "{lock} repeated:\n{profile}"
                );
            }
        }
    }

    /// A grant that names ral's own file leaves the lock its one write deny.
    #[test]
    fn mac_profile_denies_ral_writes_once_when_the_grant_names_it() {
        let own = own_names();
        let policy = restricted(
            FsProjection::Unrestricted,
            vec![
                file(own[0].as_str(), true),
                dir("/usr/bin", true),
                veto("git"),
            ],
        );
        let profile = build_profile(&policy).unwrap();
        for path in &own {
            let deny = format!("(deny file-write* (literal \"{}\"))", path.as_str());
            assert_eq!(profile.matches(&deny).count(), 1, "{deny}:\n{profile}");
        }
    }

    /// `net: false` closes DNS only because *no* network rule reaches the
    /// profile: the resolver talks to mDNSResponder over a UNIX socket, which
    /// Seatbelt gates as `network-outbound`.  Admitting that form for any
    /// local socket would reopen hostname-label egress while `net: false`
    /// still read as closed, so the denial is asserted over every rule the
    /// profile carries, not just the wholesale `(allow network*)`.
    #[test]
    fn mac_profile_denies_network_when_disabled() {
        let profile = build_profile(&SandboxProjection {
            net: false,
            ..SandboxProjection::default()
        })
        .unwrap();
        for rule in rules(&profile).lines() {
            assert!(
                !rule.contains("network"),
                "net: false admitted a network rule: {rule}\n{profile}"
            );
        }
        let open = build_profile(&SandboxProjection {
            net: true,
            ..SandboxProjection::default()
        })
        .unwrap();
        assert!(
            open.contains("(allow network*)"),
            "net: true emitted no network rule, so the denial above proves nothing:\n{open}"
        );
    }

    /// A wholesale `(allow mach-lookup)` hands out launchservicesd, and with
    /// it `/usr/bin/open` — a spawn performed by launchd outside this profile,
    /// which is an escape and, for a URL, an egress channel.  The door list is
    /// therefore closed by name, and the resolver's doors ride the `net` bit
    /// with the sockets.
    #[test]
    fn mac_profile_names_every_mach_service() {
        let closed = rules(
            &build_profile(&SandboxProjection {
                net: false,
                ..SandboxProjection::default()
            })
            .unwrap(),
        );
        assert!(
            !closed.lines().any(|l| l.trim() == "(allow mach-lookup)"),
            "wholesale mach-lookup:\n{closed}"
        );
        for name in ["launchservicesd", "com.apple.lsd", "pasteboard", "dnssd"] {
            assert!(
                !closed.contains(name),
                "the base profile admits {name}:\n{closed}"
            );
        }
        let open = build_profile(&SandboxProjection {
            net: true,
            ..SandboxProjection::default()
        })
        .unwrap();
        assert!(
            rules(&open).contains("com.apple.dnssd.service"),
            "net: true emitted no resolver door:\n{open}"
        );
        // A door the hint explains must be one no projection opens, or the
        // reason is quoted about a door the child actually holds.
        for (door, _) in withheld_doors() {
            assert!(
                !closed.contains(door) && !rules(&open).contains(door),
                "{door} carries a withheld-door reason yet some profile admits it"
            );
        }
    }

    #[test]
    fn mac_profile_allows_common_dev_writes() {
        let profile = build_profile(&SandboxProjection::default()).unwrap();
        for path in ["/dev/null", "/dev/zero", "/dev/dtracehelper", "/dev/tty"] {
            assert!(
                profile.contains(&format!("(allow file-write* (literal \"{path}\"))")),
                "missing write allowance for {path}"
            );
        }
    }

    #[test]
    fn mac_profile_leaves_tty_ioctl_available_for_tui_children() {
        let profile = build_profile(&SandboxProjection::default()).unwrap();
        assert!(profile.contains("(allow file-ioctl)"));
        assert!(
            !profile.contains("(deny file-ioctl (literal \"/dev/tty\"))"),
            "sandboxed full-screen TUI children need termios/window-size ioctls"
        );
    }

    #[test]
    fn mac_profile_names_notification_center_as_posix_shm() {
        let profile = build_profile(&SandboxProjection::default()).unwrap();
        assert!(
            profile.contains(
                "(allow ipc-posix-shm (ipc-posix-name \"apple.shm.notification_center\"))"
            )
        );
        assert!(
            !profile.contains("(global-name \"apple.shm.notification_center\")"),
            "notification_center is a POSIX shared-memory name, not a Mach service"
        );
    }

    #[test]
    fn mac_profile_grants_toolchain_ancestor_metadata() {
        let ancestors = proper_ancestors(["/Library/Developer/CommandLineTools/usr/bin/ld"]);
        assert!(ancestors.contains(&"/Library".to_string()));
        assert!(ancestors.contains(&"/Library/Developer".to_string()));
        assert!(ancestors.contains(&"/Library/Developer/CommandLineTools/usr/bin".to_string()));
        assert!(!ancestors.contains(&"/".to_string()));
    }

    #[test]
    fn mac_profile_allows_command_line_tools_lookup_when_installed() {
        if !crate::path::exists("/Library/Developer/CommandLineTools") {
            return;
        }
        // The floor arrives as read prefixes, ancestor metadata with them.
        let policy = SandboxProjection {
            fs: reading(vec![prefix("/Library/Developer/CommandLineTools")]),
            ..SandboxProjection::default()
        };
        let profile = build_profile(&policy).unwrap();
        assert!(
            profile
                .contains("(allow file-read* (subpath \"/Library/Developer/CommandLineTools\"))")
        );
        assert!(profile.contains("(allow file-read-metadata (literal \"/Library\"))"));
        assert!(profile.contains("(allow file-read-metadata (literal \"/Library/Developer\"))"));
    }

    #[test]
    fn mac_profile_does_not_grant_tmp_as_system_read_path() {
        let profile = build_profile(&SandboxProjection::default()).unwrap();
        assert!(!profile.contains("(allow file-read* (subpath \"/tmp\"))"));
        assert!(!profile.contains("(allow file-read* (subpath \"/private/tmp\"))"));
    }

    #[test]
    fn mac_profile_emits_deny_rules_for_deny_paths() {
        // /tmp firmlinks to /private/tmp, so both spellings must appear.
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                write: vec![prefix("/tmp/work")],
                deny: frozen(&["/tmp/work/.exarch.toml"]),
                ..FsRules::default()
            }),
            net: true,
            exec: ExecProjection::default(),
        };
        let profile = build_profile(&policy).unwrap();
        for form in ["/tmp/work", "/private/tmp/work"] {
            at(
                &profile,
                &format!("(allow file-write* (subpath \"{form}\"))"),
            );
            for op in ["file-read*", "file-write*", "file-link", "process-exec"] {
                at(
                    &profile,
                    &format!("(deny {op} (subpath \"{form}/.exarch.toml\"))"),
                );
            }
        }
    }

    /// The write prefix root and the intermediate `.ssh` directory both get
    /// pinned against rename/unlink, each in both firmlink spellings — the fix
    /// for the `mv /repo/.ssh /repo/x` and `mv /repo /scratch/r` escapes.
    #[test]
    fn mac_profile_emits_pin_rules_for_deny_ancestors() {
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                write: vec![prefix("/tmp/work")],
                deny: frozen(&["/tmp/work/.ssh/id_rsa"]),
                ..FsRules::default()
            }),
            net: true,
            exec: ExecProjection::default(),
        };
        let profile = build_profile(&policy).unwrap();
        for form in ["/tmp/work", "/private/tmp/work"] {
            for dir in [form.to_string(), format!("{form}/.ssh")] {
                at(
                    &profile,
                    &format!("(deny file-write-unlink (literal \"{dir}\"))"),
                );
            }
        }
        assert!(
            !profile.contains("(deny file-write-unlink (subpath"),
            "pins must be literal, not subpath: subpath would also block \
             unlinking every entry inside the pinned directory"
        );
    }

    /// A deny reached only through a symlinked ancestor must still pin the
    /// *resolved* chain: `alias → top/deep` inside the write prefix, denying
    /// `alias/secret`.  Rendering the deny before deriving ancestors is what
    /// surfaces `top` here — the surface chain alone (`alias`) never mentions
    /// it, so `mv top elsewhere` would otherwise move the denied bytes to a
    /// name no deny rule covers.
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "[test] test fs scaffolding: a tempdir tree and a symlink for the alias pin"
    )]
    fn mac_profile_pins_the_resolved_ancestor_reached_through_an_alias() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("top/deep")).expect("nested target dir");
        std::os::unix::fs::symlink(root.join("top/deep"), root.join("alias"))
            .expect("a symlink aliasing the nested target");

        let write = root.to_str().expect("ascii temp path").to_string();
        let deny = format!("{write}/alias/secret");
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsRules {
                write: vec![prefix(&write)],
                deny: frozen(&[&deny]),
                ..FsRules::default()
            }),
            net: true,
            exec: ExecProjection::default(),
        };
        let profile = build_profile(&policy).unwrap();

        let resolved_top = std::fs::canonicalize(root)
            .expect("the temp dir resolves")
            .join("top");
        let pin = format!(
            "(deny file-write-unlink (literal \"{}\"))",
            resolved_top.to_str().expect("ascii temp path")
        );
        assert!(
            profile.contains(&pin),
            "the ancestor reached only through the alias must be pinned:\n{profile}"
        );
    }
}
