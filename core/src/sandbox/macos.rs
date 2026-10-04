//! macOS sandbox using the Seatbelt (`sandbox_init`) API.
//!
//! Only the per-command re-exec child is confined: it carries
//! `--sandbox-projection`, enters the profile at startup through
//! `enter_current_process`, then execs the target — a host binary via
//! `--ral-sandbox-exec`, or a bundled tool in-process — which inherits the
//! confinement.  The parent ral process is never confined; authorising a
//! binary through `exec:` already shifts trust to that binary.
//!
//! Seatbelt has no per-address network rules, so `SandboxProjection::net` is
//! one allow/deny bit rather than an endpoint list.

use crate::path::{Rendered, render_paths, rendered_ancestors};
use crate::types::{ExecProjection, ExecRule, FsProjection, FsRules, SandboxProjection};
use std::ffi::{CStr, CString};
use std::fmt::{self, Write};
use std::os::raw::{c_char, c_int};

/// Apply `policy` to the current process.  Seatbelt entry cannot be undone.
pub(super) fn enter_current_process(policy: &SandboxProjection) -> Result<(), String> {
    let profile = build_profile(policy)?;
    apply_profile(&profile).map_err(|e| format!("ral: failed to enter sandbox: {e}"))
}

fn apply_profile(profile: &str) -> std::io::Result<()> {
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

pub(super) fn build_profile(policy: &SandboxProjection) -> Result<String, String> {
    let mut lines: Vec<String> = vec![BASE_PROFILE.to_string()];
    let rendered = policy.rendered()?;
    match &rendered.fs {
        FsProjection::Restricted(rules) => emit_fs_restricted(&mut lines, rules)?,
        FsProjection::Unrestricted => {
            // Pass fs through, so an exec-only grant can enter the sandbox
            // for exec gating without clamping the agent's cwd or HOME.
            lines.push("(allow file-read*)".to_string());
            lines.push("(allow file-write*)".to_string());
        }
    }

    // A veto under an fs nobody restricted is hollow unless the allow-set is
    // frozen — see `emit_exec_rules`.
    let freeze_admitted_set =
        matches!(rendered.fs, FsProjection::Unrestricted) && rendered.exec.carries_veto();
    emit_exec_rules(&mut lines, &rendered.exec, freeze_admitted_set)?;

    // After the broad allows: Seatbelt is last-match-wins.  `subpath` so a
    // denied directory covers everything under it; `file-link` (Seatbelt
    // has no `file-link*`) blocks `link(2)` against the source, closing the
    // hole where a second name elsewhere would let writes bypass the deny.
    for path in rendered
        .fs
        .rules()
        .map(|r| r.deny_paths.as_slice())
        .unwrap_or_default()
    {
        let escaped = escape_path(path);
        lines.push(format!("(deny file-read* (subpath \"{escaped}\"))"));
        lines.push(format!("(deny file-write* (subpath \"{escaped}\"))"));
        lines.push(format!("(deny file-link (subpath \"{escaped}\"))"));
    }
    // A pinned dir keeps its entries mutable — only its own name-in-parent is
    // frozen — so `literal`, never `subpath`, which would also block
    // unlinking every entry inside it.  Unconditional on existence, like the
    // deny paths above: an ancestor absent now can be created later under the
    // write prefix that covers it.
    for dir in rendered
        .fs
        .rules()
        .map(|r| r.pinned_dirs.as_slice())
        .unwrap_or_default()
    {
        lines.push(format!(
            "(deny file-write-unlink (literal \"{}\"))",
            escape_path(dir)
        ));
    }
    if policy.net {
        lines.push(NET_PROFILE.to_string());
    }

    Ok(lines.join("\n"))
}

/// Emit the allow rules and ancestor-metadata carve-outs for a restricted fs
/// projection.  The denies the caller layers after them are its own to emit —
/// Seatbelt is last-match-wins, so they must follow every allow in the
/// profile, not just these.
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
/// loader base and ral's own binary first, then renders each rule in order,
/// one [`Sbpl`] form each: Seatbelt is last-match-wins, the order the rules
/// already carry.  Exec denies deny no reads: those are fs's.
/// `freeze_admitted_set` denies writes under everything admitted, without
/// which `(allow file-write*)` makes every veto hollow.  Rule by rule:
/// `docs/ral-wiki/internals/seatbelt-profile.md`.
///
/// `Err` when the platform base's or the self-exec path's own name-class
/// expansion is not valid UTF-8, which [`render_paths`] refuses.
fn emit_exec_rules(
    lines: &mut Vec<String>,
    exec: &ExecProjection<Rendered>,
    freeze_admitted_set: bool,
) -> Result<(), String> {
    let ExecProjection::Restricted(rules) = exec else {
        lines.push("(allow process-exec)".to_string());
        return Ok(());
    };
    // Toolchains (`gcc → cc1 → as → ld`) arrive through the grant, `system:`
    // among them; only the loader base is ambient.
    let system_dirs = existing_system_paths(|k| k == SystemAccess::Exec)?;
    // Bundled tools re-exec this binary (`--ral-bundled-tool`; the per-tool
    // check is `vet`'s).  Rendered like the rest: execve presents `/tmp/x` as
    // `/private/tmp/x`.
    let self_exec = render_paths(super::reexec::self_exec_path_string().as_slice())?;
    let mut clauses = String::new();
    for path in &self_exec {
        let _ = write!(clauses, "\n  (literal \"{}\")", escape_path(path));
    }
    for dir in &system_dirs {
        let _ = write!(clauses, "\n  (subpath \"{}\")", escape_path(dir));
    }
    // An operand-less `(allow file-read* process-exec)` is an unconditional
    // allow under SBPL, so an empty base must emit nothing.
    if !clauses.is_empty() {
        lines.push(format!("(allow file-read* process-exec{clauses})"));
    }
    lines.extend(rules.iter().map(|rule| Sbpl(rule).to_string()));
    let files: Vec<&Rendered> = self_exec
        .iter()
        .chain(rules.iter().filter_map(|rule| match rule {
            ExecRule::File { path, allow: true } => Some(path),
            _ => None,
        }))
        .collect();
    emit_ancestor_metadata(lines, files.iter().copied());
    if freeze_admitted_set {
        let dirs = system_dirs
            .iter()
            .chain(rules.iter().filter_map(|rule| match rule {
                ExecRule::Dir { path, allow: true } => Some(path),
                _ => None,
            }));
        for dir in dirs {
            lines.push(format!(
                "(deny file-write* (subpath \"{}\"))",
                escape_path(dir)
            ));
        }
        for path in files {
            lines.push(format!(
                "(deny file-write* (literal \"{}\"))",
                escape_path(path)
            ));
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
    use super::{build_profile, withheld_doors};
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
            path: path.into(),
            allow,
        }
    }

    fn file(path: &str, allow: bool) -> ExecRule {
        ExecRule::File {
            path: path.into(),
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

    /// Seatbelt is last-match-wins, and the projection's order is precedence.
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
            !build_profile(&open).unwrap().contains("(deny file-write*"),
            "a veto-free grant had its admitted set frozen"
        );
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
        super::super::reexec::register_sandbox_self();
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
    /// a spelling execve never presents and its bundled-tool re-exec dies.
    ///
    /// `register_sandbox_self` pins this test binary, which is the only handle
    /// on what the profile will name; where that path touches no firmlink and
    /// no symlink its class is a singleton and the loop degenerates to "the
    /// literal is present".  The compile-time guarantee — `escape_path` takes
    /// only [`Rendered`] — is what holds on such a host.
    #[test]
    fn mac_profile_expands_firmlinks_in_self_exec_literal() {
        super::super::reexec::register_sandbox_self();
        let self_exec = super::super::reexec::self_exec_path_string()
            .expect("registration pins this test binary");
        let profile = build_profile(&restricted(FsProjection::default(), Vec::new())).unwrap();
        for form in crate::path::render_paths(&[self_exec]).unwrap() {
            assert!(
                profile.contains(&format!("(literal \"{}\")", form.as_str())),
                "self-exec admit for {} missing:\n{profile}",
                form.as_str()
            );
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
        // /tmp firmlinks to /private/tmp, so both spellings must appear, and
        // each of the three denies must follow its covering allow.
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
            let allow_idx = profile
                .find(&format!("(allow file-write* (subpath \"{form}\"))"))
                .unwrap_or_else(|| panic!("write allow for {form} missing"));
            for op in ["file-read*", "file-write*", "file-link"] {
                let deny_idx = profile
                    .find(&format!("(deny {op} (subpath \"{form}/.exarch.toml\"))"))
                    .unwrap_or_else(|| panic!("{op} deny for {form}/.exarch.toml missing"));
                assert!(
                    allow_idx < deny_idx,
                    "{op} deny must follow allow for {form}"
                );
            }
        }
    }

    /// The write prefix root and the intermediate `.ssh` directory both get
    /// pinned against rename/unlink, each in both firmlink spellings, after
    /// the write allow that covers them — the fix for the `mv /repo/.ssh
    /// /repo/x` and `mv /repo /scratch/r` escapes.
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
            let allow_idx = profile
                .find(&format!("(allow file-write* (subpath \"{form}\"))"))
                .unwrap_or_else(|| panic!("write allow for {form} missing"));
            for dir in [form.to_string(), format!("{form}/.ssh")] {
                let pin_idx = profile
                    .find(&format!("(deny file-write-unlink (literal \"{dir}\"))"))
                    .unwrap_or_else(|| panic!("pin for {dir} missing:\n{profile}"));
                assert!(
                    allow_idx < pin_idx,
                    "pin for {dir} must follow the write allow"
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
