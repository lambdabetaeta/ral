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

use crate::path::{Rendered, render_paths, render_real, rendered_ancestors};
use crate::types::{
    ExecProjection, ExecRule, FsProjection, FsRules, SandboxProjection, WriteReach,
};
use std::collections::BTreeSet;
use std::ffi::{CStr, CString};
use std::fmt::{self, Write};
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
    /// `process-exec` allows and denies, in the order the grant carries them.
    exec_rules: Vec<String>,
    /// The lookups the exec admits need through their ancestors.
    exec_ancestors: Vec<String>,
    /// Writes denied under the admitted set: each dir a restricted fs's write
    /// prefixes cover without naming, or, under an unrestricted fs, the whole
    /// set when a veto asks.
    exec_freeze: Vec<String>,
    /// What the fs grant carves out of its allows.
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
            exec_freeze,
            fs_denies,
            pins,
            net,
        } = self;
        f.write_str(BASE_PROFILE)?;
        for line in [
            fs_allows,
            exec_rules,
            exec_ancestors,
            exec_freeze,
            fs_denies,
            pins,
        ]
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
        FsProjection::Restricted(rules) => emit_fs_restricted(&mut profile.fs_allows, rules)?,
        FsProjection::Unrestricted => {
            // Pass fs through, so an exec-only grant can enter the sandbox
            // for exec gating without clamping the agent's cwd or HOME.
            profile
                .fs_allows
                .extend(["(allow file-read*)", "(allow file-write*)"].map(String::from));
        }
    }

    // Ral's own file, rendered like the rest: execve presents `/tmp/x` as
    // `/private/tmp/x`.
    let own = render_paths(&[super::reexec::own()?.exec_path().to_string_lossy()])?;
    emit_exec_rules(&mut profile, &rendered.exec, &own, &rendered.fs)?;

    // `subpath` so a denied directory covers everything under it; `file-link`
    // (Seatbelt has no `file-link*`) blocks `link(2)` against the source,
    // closing the hole where a second name elsewhere would let writes bypass
    // the deny.
    for path in rendered
        .fs
        .rules()
        .map(|r| r.deny_paths.as_slice())
        .unwrap_or_default()
    {
        let escaped = escape_path(path);
        profile.fs_denies.extend([
            format!("(deny file-read* (subpath \"{escaped}\"))"),
            format!("(deny file-write* (subpath \"{escaped}\"))"),
            format!("(deny file-link (subpath \"{escaped}\"))"),
        ]);
    }
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
            .map(|path| format!("(deny file-write* (literal \"{}\"))", escape_path(path))),
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
        pins.push(format!(
            "(deny file-write-unlink (literal \"{}\"))",
            escape_path(dir)
        ));
    }
}

/// Emit the allow rules and ancestor-metadata carve-outs for a restricted fs
/// projection.
///
/// `Err` when the system paths' own name-class expansion is not valid UTF-8,
/// which [`render_paths`] refuses rather than approximates.
fn emit_fs_restricted(lines: &mut Vec<String>, rules: &FsRules<Rendered>) -> Result<(), String> {
    // `Exec` implies read.
    let system_read_paths = existing_system_paths(|_| true)?;
    emit_ancestor_metadata(lines, &system_read_paths);
    emit_read_subpaths(lines, &system_read_paths);
    emit_ancestor_metadata(
        lines,
        rules.read_prefixes.iter().chain(&rules.write_prefixes),
    );
    emit_read_subpaths(lines, &rules.read_prefixes);
    emit_read_subpaths(lines, &rules.write_prefixes);
    for prefix in &rules.write_prefixes {
        lines.push(format!(
            "(allow file-write* (subpath \"{}\"))",
            escape_path(prefix)
        ));
    }
    Ok(())
}

/// Seatbelt carries the exec allow-list into the kernel via the
/// `process-exec` clause [`emit_exec_rules`] renders below.
pub(crate) const RENDERS_EXEC: bool = true;

/// Render the `process-exec` rules.  `Unrestricted` is a wildcard, so an
/// fs-only grant does not attenuate exec here.  `Restricted` admits the
/// loader base and `own`, ral's binary, first, then renders each rule in
/// order, one [`Sbpl`] form each: Seatbelt is last-match-wins, the order the
/// rules already carry.  Exec denies deny no reads: those are fs's.
/// Writes under an admitted dir are denied wherever the fs grant would
/// otherwise let a child author what the dir runs, and a veto is hollow
/// without it: for each dir a restricted `fs`'s write prefixes cover without
/// naming ([`WriteReach::Covered`]), and under an unrestricted `fs`, where
/// `(allow file-write*)` covers everything, for the whole admitted set when
/// a veto asks.  `own` is frozen by [`build_profile`] whatever the grant.
/// Rule by rule: `docs/ral-wiki/internals/seatbelt-profile.md`.
///
/// `Err` when the platform base's own name-class expansion is not valid
/// UTF-8, which [`render_paths`] refuses.
fn emit_exec_rules(
    profile: &mut Profile,
    exec: &ExecProjection,
    own: &[Rendered],
    fs: &FsProjection<Rendered>,
) -> Result<(), String> {
    let ExecProjection::Restricted(rules) = exec else {
        profile.exec_rules.push("(allow process-exec)".to_string());
        return Ok(());
    };
    let mut rendered = Vec::with_capacity(rules.len());
    for rule in rules {
        rendered.extend(rule.try_flat_map(render_real)?);
    }
    let rules = rendered;
    // Toolchains (`gcc → cc1 → as → ld`) arrive through the grant, `system:`
    // among them; only the loader base is ambient.
    let system_dirs = existing_system_paths(|k| k == SystemAccess::Exec)?;
    // A ral run in here starts its bundled tools and pipeline anchors by
    // re-executing this binary, so it is admitted without the grant naming it.
    let mut clauses = String::new();
    for path in own {
        let _ = write!(clauses, "\n  (literal \"{}\")", escape_path(path));
    }
    for dir in &system_dirs {
        let _ = write!(clauses, "\n  (subpath \"{}\")", escape_path(dir));
    }
    // An operand-less `(allow file-read* process-exec)` is an unconditional
    // allow under SBPL, so an empty base must emit nothing.
    if !clauses.is_empty() {
        profile
            .exec_rules
            .push(format!("(allow file-read* process-exec{clauses})"));
    }
    profile
        .exec_rules
        .extend(rules.iter().map(|rule| Sbpl(rule).to_string()));
    let granted_files: Vec<&Rendered> = rules
        .iter()
        .filter_map(|rule| match rule {
            ExecRule::File { path, allow: true } => Some(path),
            _ => None,
        })
        .filter(|path| !own.contains(path))
        .collect();
    let granted_dirs: Vec<&Rendered> = rules
        .iter()
        .filter_map(|rule| match rule {
            ExecRule::Dir { path, allow: true } => Some(path),
            _ => None,
        })
        .collect();
    emit_ancestor_metadata(
        &mut profile.exec_ancestors,
        own.iter()
            .chain(granted_files.iter().copied())
            .chain(granted_dirs.iter().copied()),
    );
    let deny_writes = |filter: &str, path: &Rendered| {
        format!("(deny file-write* ({filter} \"{}\"))", escape_path(path))
    };
    let admitted_dirs: Vec<&Rendered> = system_dirs.iter().chain(granted_dirs).collect();
    match fs {
        FsProjection::Unrestricted if exec.carries_veto() => {
            profile.exec_freeze.extend(
                admitted_dirs
                    .into_iter()
                    .map(|dir| deny_writes("subpath", dir)),
            );
            profile.exec_freeze.extend(
                granted_files
                    .into_iter()
                    .map(|file| deny_writes("literal", file)),
            );
        }
        FsProjection::Unrestricted => {}
        FsProjection::Restricted(writes) => {
            for &dir in &admitted_dirs {
                if writes.write_reach(dir, &admitted_dirs) == WriteReach::Covered {
                    profile.exec_freeze.push(deny_writes("subpath", dir));
                }
            }
        }
    }
    Ok(())
}

/// One kernel exec rule as one SBPL form.  An allow admits `file-read*`
/// beside `process-exec`, which Seatbelt needs to spawn; a deny is exec only.
struct Sbpl<'a>(&'a ExecRule<Rendered>);

impl fmt::Display for Sbpl<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (allow, filter, path) = match self.0 {
            ExecRule::Dir { path, allow } => (allow, "subpath", path),
            ExecRule::File { path, allow } => (allow, "literal", path),
            // Wherever the name resolves: a final-component regex.
            ExecRule::Veto(name) => {
                return write!(
                    f,
                    "(deny process-exec (regex #\"/{}$\"))",
                    escape_regex(name)
                );
            }
        };
        let op = if *allow {
            "allow file-read* process-exec"
        } else {
            "deny process-exec"
        };
        write!(f, "({op} ({filter} \"{}\"))", escape_path(path))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SystemAccess {
    Read,
    Exec, // implies read; emitted in the folded `file-read* process-exec` rule
}

/// Baseline system paths the runtime needs regardless of user grant.  User
/// temp and workspace paths are deliberately absent — they must arrive via
/// the active fs grant.
fn system_paths() -> &'static [(&'static str, SystemAccess)] {
    use SystemAccess::{Exec, Read};
    &[
        ("/bin", Read),
        ("/usr", Read),
        // Rosetta's runtime, the loader analogue: no grant should name it.
        ("/Library/Apple/usr", Exec),
        ("/Library/Developer/CommandLineTools", Read),
        ("/Applications/Xcode.app/Contents/Developer", Read),
        ("/opt/homebrew", Read),
        ("/lib", Read),
        ("/System", Read),
        ("/dev", Read),
        ("/private/var/db/dyld", Read),
        // Wholesale: tools read whatever they read (gitconfig, paths.d,
        // zshenv, nix.conf, …); nothing user-secret sits here unprotected.
        ("/private/etc", Read),
        // xcode-select state; denied, build drivers report a broken install.
        ("/private/var/select", Read),
        // resolv.conf's target and the mDNSResponder socket.
        ("/private/var/run", Read),
    ]
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
             this profile — an escape, and for a URL an egress channel under `net: false`",
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

/// The host-existing `system_paths` whose access `wanted` selects, rendered
/// to every firmlink spelling (`/private/etc` → `[/etc, /private/etc]`).
fn existing_system_paths(wanted: impl Fn(SystemAccess) -> bool) -> Result<Vec<Rendered>, String> {
    let existing: Vec<String> = system_paths()
        .iter()
        .filter(|(p, k)| wanted(*k) && crate::path::exists(p))
        .map(|(p, _)| (*p).to_string())
        .collect();
    render_paths(&existing)
}

fn emit_read_subpaths<'a>(lines: &mut Vec<String>, paths: impl IntoIterator<Item = &'a Rendered>) {
    for path in paths {
        lines.push(format!(
            "(allow file-read* (subpath \"{}\"))",
            escape_path(path)
        ));
    }
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
        lines.push(format!(
            "(allow file-read-metadata (literal \"{}\"))",
            escape_path(&ancestor)
        ));
    }
}

/// Quote a rendered name for an SBPL string literal.  Taking [`Rendered`] and
/// not `&str` is what makes it impossible to splice a path into a rule without
/// first passing it through [`render_paths`]: every emitter in this file
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
    use crate::path::proper_ancestors;
    use crate::types::{ExecProjection, ExecRule, FsProjection, FsRules, SandboxProjection};

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

    fn dir(path: &str, allow: bool) -> ExecRule {
        ExecRule::Dir {
            path: crate::path::RealPath::assumed(path),
            allow,
        }
    }

    fn file(path: &str, allow: bool) -> ExecRule {
        ExecRule::File {
            path: crate::path::RealPath::assumed(path),
            allow,
        }
    }

    fn restricted(fs: FsProjection, rules: Vec<ExecRule>) -> SandboxProjection {
        SandboxProjection {
            fs,
            exec: ExecProjection::Restricted(rules),
            ..SandboxProjection::default()
        }
    }

    /// A restricted fs writing under `prefixes` and nothing else.
    fn writing(prefixes: &[&str]) -> FsProjection {
        FsProjection::Restricted(FsRules {
            write_prefixes: prefixes.iter().copied().map(String::from).collect(),
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

    #[test]
    fn mac_profile_renders_each_rule_as_one_form() {
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
            "(allow file-read* process-exec (subpath \"/usr/bin\"))",
            "(allow file-read* process-exec (subpath \"/opt/homebrew/bin\"))",
            "(allow file-read* process-exec (literal \"/usr/bin/git\"))",
            "(allow file-read-metadata (literal \"/usr\"))",
            "(allow file-read-metadata (literal \"/usr/bin\"))",
        ] {
            at(&profile, form);
        }
        assert!(
            !profile.contains("(allow process-exec)\n"),
            "wildcard process-exec leaked into restricted profile"
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
            exec_freeze: section("exec-freeze"),
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
            "\n(fs-allows)\n(exec-rules)\n(exec-ancestors)\n(exec-freeze)\n(fs-denies)\n(pins)"
        );
    }

    /// The projection's order is the precedence of its exec rules.
    #[test]
    fn mac_profile_keeps_the_rules_in_order() {
        let policy = restricted(
            FsProjection::default(),
            vec![
                dir("/x", true),
                dir("/x/y", false),
                file("/x/y/z", true),
                ExecRule::Veto("z".into()),
            ],
        );
        let profile = build_profile(&policy).unwrap();
        let forms = [
            "(allow file-read* process-exec (subpath \"/x\"))",
            "(deny process-exec (subpath \"/x/y\"))",
            "(allow file-read* process-exec (literal \"/x/y/z\"))",
            r#"(deny process-exec (regex #"/z$"))"#,
        ];
        let indices: Vec<usize> = forms.iter().map(|form| at(&profile, form)).collect();
        assert!(indices.is_sorted(), "out of order:\n{profile}");
    }

    /// An exec veto under an unrestricted fs is otherwise hollow: the child
    /// drops a binary into an admitted directory and runs it from there under
    /// a name the veto never mentions.
    #[test]
    fn mac_profile_freezes_the_admitted_set_when_a_veto_meets_unrestricted_fs() {
        let vetoed = restricted(
            FsProjection::Unrestricted,
            vec![dir("/usr/bin", true), ExecRule::Veto("git".into())],
        );
        let profile = build_profile(&vetoed).unwrap();
        assert!(
            profile.contains("(deny file-write* (subpath \"/usr/bin\"))"),
            "/usr/bin stayed writable under a veto:\n{profile}"
        );
        assert!(
            !profile.contains("(deny file-write* (subpath \"/usr\"))"),
            "/usr is no part of the admitted set:\n{profile}"
        );
        // Freezing is the veto's price, so a grant that vetoes nothing pays it
        // not at all.
        let open = restricted(FsProjection::Unrestricted, vec![dir("/usr/bin", true)]);
        assert!(
            !build_profile(&open)
                .unwrap()
                .contains("(deny file-write* (subpath"),
            "a veto-free grant had its admitted set frozen"
        );
    }

    /// Under a restricted fs the write prefixes author what an admitted dir runs,
    /// so a dir they cover without naming is frozen — veto or none, and after
    /// the write allow it overrides.
    #[test]
    fn mac_profile_freezes_an_admitted_dir_a_shallower_write_prefix_covers() {
        let policy = restricted(
            writing(&["/ral-test/w"]),
            vec![dir("/ral-test/w/bin", true)],
        );
        let profile = build_profile(&policy).unwrap();
        let write = at(&profile, "(allow file-write* (subpath \"/ral-test/w\"))");
        let freeze = at(&profile, "(deny file-write* (subpath \"/ral-test/w/bin\"))");
        assert!(
            write < freeze,
            "the freeze must follow the write it overrides:\n{profile}"
        );
    }

    /// The grant that names the tree, whole or in part, keeps it writable: the
    /// control for the freeze above, which differs only in what the prefix is.
    #[test]
    fn mac_profile_leaves_a_dir_the_write_prefixes_name_writable() {
        for prefix in ["/ral-test/w/bin", "/ral-test/w/bin/out"] {
            let policy = restricted(writing(&[prefix]), vec![dir("/ral-test/w/bin", true)]);
            let profile = build_profile(&policy).unwrap();
            at(
                &profile,
                &format!("(allow file-write* (subpath \"{prefix}\"))"),
            );
            assert!(
                !profile.contains("(deny file-write* (subpath \"/ral-test/w/bin\"))"),
                "{prefix} names the admit and it was frozen:\n{profile}"
            );
        }
    }

    #[test]
    fn mac_profile_leaves_a_dir_no_write_prefix_reaches_alone() {
        let policy = restricted(
            writing(&["/ral-test/other"]),
            vec![dir("/ral-test/w/bin", true)],
        );
        let profile = build_profile(&policy).unwrap();
        assert!(
            !profile.contains("(deny file-write* (subpath"),
            "a dir no write prefix reaches was frozen:\n{profile}"
        );
    }

    /// The writes a grant names at or below an admit stand for every admit
    /// beneath it, and for none beside it: a `$PATH` entry inside the project
    /// stays writable, the sibling of a carve-out does not.
    #[test]
    fn mac_profile_trusts_a_nested_admit_and_freezes_a_sibling_the_carve_out_misses() {
        let nested = restricted(
            writing(&["/ral-test/proj"]),
            vec![
                dir("/ral-test/proj", true),
                dir("/ral-test/proj/node_modules/.bin", true),
            ],
        );
        let profile = build_profile(&nested).unwrap();
        assert!(
            !profile.contains("(deny file-write* (subpath"),
            "an admit nested in a named one was frozen:\n{profile}"
        );

        let sibling = restricted(
            writing(&["/ral-test/home", "/ral-test/home/.cargo/registry"]),
            vec![
                dir("/ral-test/home/.cargo", true),
                dir("/ral-test/home/.cargo/bin", true),
            ],
        );
        let profile = build_profile(&sibling).unwrap();
        at(
            &profile,
            "(deny file-write* (subpath \"/ral-test/home/.cargo/bin\"))",
        );
        assert!(
            !profile.contains("(deny file-write* (subpath \"/ral-test/home/.cargo\"))"),
            "the admit holding the carve-out was frozen:\n{profile}"
        );
    }

    /// `/tmp` and `/private/tmp` are one directory: the freeze names both
    /// spellings, as every other rule on a firmlinked path does.
    #[test]
    fn mac_profile_freezes_a_covered_dir_under_every_firmlink_spelling() {
        let policy = restricted(
            writing(&["/tmp/ral-test"]),
            vec![dir("/tmp/ral-test/bin", true)],
        );
        let profile = build_profile(&policy).unwrap();
        for form in ["/tmp/ral-test/bin", "/private/tmp/ral-test/bin"] {
            at(
                &profile,
                &format!("(deny file-write* (subpath \"{form}\"))"),
            );
        }
    }

    /// The kernel exec set is the rules and the loader base: `/usr`, the
    /// toolchains and Homebrew are readable but run only where the grant
    /// (`system:`) names them.
    #[test]
    fn mac_profile_admits_no_ambient_exec_beyond_the_loader_base() {
        let policy = restricted(
            FsProjection::Restricted(FsRules::default()),
            vec![dir("/usr/bin", true)],
        );
        let profile = build_profile(&policy).unwrap();
        let exec_allows: Vec<&str> = profile
            .split("\n(")
            .filter(|form| form.starts_with("allow file-read* process-exec"))
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
    }

    #[test]
    fn mac_profile_emits_only_system_base_when_restricted_to_empty() {
        super::super::reexec::pin_self();
        let profile = build_profile(&restricted(FsProjection::default(), Vec::new())).unwrap();
        // An empty rule list still admits the platform base — ral itself —
        // just as an empty fs grant still admits libc and dyld.
        assert!(profile.contains("(allow file-read* process-exec"));
        assert!(!profile.contains("(allow process-exec)\n"));
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
        let allow = at(
            &profile,
            "(allow file-read* process-exec (subpath \"/usr/bin\"))",
        );
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
            vec![
                dir("/usr/bin", true),
                ExecRule::Veto("git".into()),
                ExecRule::Veto("c++".into()),
            ],
        );
        let profile = build_profile(&policy).unwrap();
        let allow = at(
            &profile,
            "(allow file-read* process-exec (subpath \"/usr/bin\"))",
        );
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
                &format!("(allow file-read* process-exec (literal \"{form}\"))"),
            );
        }
        for form in ["/tmp/evil", "/private/tmp/evil"] {
            at(
                &profile,
                &format!("(deny process-exec (literal \"{form}\"))"),
            );
        }
    }

    /// The self-exec admit is the one exec path the projection never carries,
    /// so it must come through the same door as everything else and appear
    /// in every form, or a ral binary running from `/tmp/…` is admitted under
    /// a spelling execve never presents and the ral run inside it cannot
    /// re-exec itself.
    ///
    /// `pin_self` pins this test binary, which is the only handle on what the
    /// profile will name; where that path touches no firmlink and
    /// no symlink its class is a singleton and the loop degenerates to "the
    /// literal is present".  The compile-time guarantee — `escape_path` takes
    /// only [`Rendered`] — is what holds on such a host.
    #[test]
    fn mac_profile_expands_firmlinks_in_self_exec_literal() {
        let own = own_names();
        let profile = build_profile(&restricted(FsProjection::default(), Vec::new())).unwrap();
        for form in own {
            assert!(
                profile.contains(&format!("(literal \"{}\")", form.as_str())),
                "self-exec admit for {} missing:\n{profile}",
                form.as_str()
            );
        }
    }

    /// Ral's own spellings, as the profile will name them.
    fn own_names() -> Vec<crate::path::Rendered> {
        super::super::reexec::pin_self();
        let own = super::super::reexec::own().expect("pin_self pins this test binary");
        crate::path::render_paths(&[own.exec_path().to_string_lossy()]).unwrap()
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
            read_prefixes: vec![dir.clone()],
            write_prefixes: vec![dir.clone()],
            deny_paths: vec![format!("{dir}/x/secret")],
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

    /// A grant that names ral's own file under a freezing veto would have the
    /// admitted-set freeze and the lock both deny its writes.
    #[test]
    fn mac_profile_denies_ral_writes_once_when_the_grant_names_it() {
        let own = own_names();
        let policy = restricted(
            FsProjection::Unrestricted,
            vec![
                file(own[0].as_str(), true),
                dir("/usr/bin", true),
                ExecRule::Veto("git".into()),
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
        // System read paths are emitted explicitly only when fs is
        // Restricted; otherwise the wildcard `(allow file-read*)` covers them.
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsRules::default()),
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
                write_prefixes: vec!["/tmp/work".into()],
                deny_paths: vec!["/tmp/work/.exarch.toml".into()],
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
            for op in ["file-read*", "file-write*", "file-link"] {
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
                write_prefixes: vec!["/tmp/work".into()],
                deny_paths: vec!["/tmp/work/.ssh/id_rsa".into()],
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
            "pins must be literal, not subpath — subpath would also block \
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
                write_prefixes: vec![write],
                deny_paths: vec![deny],
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
