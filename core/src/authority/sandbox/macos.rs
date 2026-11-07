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
//! one veto over IP sockets rather than an endpoint list.

use super::{ExecProjection, FsProjection, RenderedLists, SandboxProjection};
use crate::authority::policy::{ExecDeny, Gpu, KernelAllow, Net};
use crate::path::{
    Excluded, LeafMode, LexicalPath, Object, Rendered, join_slash, render_excluded, render_real,
    rendered_ancestors, walk_lenient,
};
use std::collections::BTreeSet;
use std::ffi::{CStr, CString};
use std::fmt;
use std::os::raw::{c_char, c_int};
use std::sync::OnceLock;

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
        let already_profiled = std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        let message = if errorbuf.is_null() {
            "sandbox_init_with_parameters failed".to_string()
        } else {
            // SAFETY: Seatbelt set `errorbuf` to a NUL-terminated message it
            // owns, which is freed once, after the copy.
            unsafe {
                let message = CStr::from_ptr(errorbuf).to_string_lossy().into_owned();
                sandbox_free_error(errorbuf);
                message
            }
        };
        if already_profiled {
            return Err(std::io::Error::other(format!(
                "{ALREADY_PROFILED} ({message})"
            )));
        }
        return Err(std::io::Error::other(message));
    }
    Ok(())
}

/// Policy-independent SBPL preamble: `(version 1)`, `(deny default)`, and the
/// Apple-required carve-outs.  A sibling file so the rules read as SBPL rather
/// than `format!()` strings.
const BASE_PROFILE: &str = include_str!("macos-base.sbpl");

/// The online rules: IP sockets, the resolver's Unix socket, and the
/// resolver and trust-evaluation Mach services that ride the same veto.
const NET_PROFILE: &str = include_str!("macos-net.sbpl");

/// The one Unix socket `NET_PROFILE` admits; a denial naming it is `net`'s.
pub(super) const RESOLVER_SOCKET: &str = "/private/var/run/mDNSResponder";

/// The rules a frame that leaves the GPU appends: Metal's device, its
/// `IOSurface`s and its compiler.
const GPU_PROFILE: &str = include_str!("macos-gpu.sbpl");

/// What [`GPU_PROFILE`] opens, as Seatbelt logs a denial of it.
const GPU_NAMES: [&str; 3] = [
    "AGXDeviceUserClient",
    "IOSurfaceRootUserClient",
    "com.apple.MTLCompilerService",
];

/// Whether a denied `IOKit` class or Mach name is one only [`GPU_PROFILE`]
/// admits, for [`crate::authority::sandbox::diag`]'s hint.
pub(super) fn gpu_name(name: &str) -> bool {
    GPU_NAMES.iter().any(|gpu| name.starts_with(gpu))
}

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
    net: Option<Net>,
    gpu: Option<Gpu>,
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
            gpu,
        } = self;
        f.write_str(BASE_PROFILE)?;
        for line in [fs_allows, exec_rules, exec_ancestors, fs_denies, pins]
            .into_iter()
            .flatten()
        {
            write!(f, "\n{line}")?;
        }
        if net.is_none() {
            write!(f, "\n{NET_PROFILE}")?;
        }
        if gpu.is_none() {
            write!(f, "\n{GPU_PROFILE}")?;
        }
        Ok(())
    }
}

pub(super) fn build_profile(policy: &SandboxProjection) -> Result<String, String> {
    let mut profile = Profile {
        net: policy.net,
        gpu: policy.gpu,
        ..Profile::default()
    };
    let rendered = policy.rendered()?;
    match &rendered.fs {
        FsProjection::Restricted(lists) => emit_fs_restricted(&mut profile, lists),
        FsProjection::Unrestricted => {
            // Pass fs through, so an exec-only grant can enter the sandbox
            // for exec gating without clamping the agent's cwd or HOME.
            profile
                .fs_allows
                .extend(["(allow file-read*)", "(allow file-write*)"].map(String::from));
        }
    }

    if policy.gpu.is_none() {
        emit_gpu_caches(&mut profile.fs_allows, gpu_caches());
    }

    emit_exec_rules(&mut profile, &rendered.exec)?;

    // Ral's own file, rendered like the rest: execve presents `/tmp/x` as
    // `/private/tmp/x`.  Protected whatever the grant, never admitted by it.
    let own = render_real(super::reexec::own()?.trace().real())?;
    let pinned = rendered
        .fs
        .lists()
        .map(|l| l.pinned_dirs.as_slice())
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
/// twin; its surface's walk may stat directories and read links, never list
/// those directories.  The read region carries what exec runs and what a
/// launch needs beyond it, so exec's own clauses read nothing.
fn emit_fs_restricted(profile: &mut Profile, lists: &RenderedLists) {
    let objects = || lists.read.iter().chain(&lists.write);
    // Each write prefix is among `read` too.
    let mut readable: Vec<&Rendered> = objects().flat_map(Object::names).collect();
    readable.sort();
    readable.dedup();
    let walk: BTreeSet<&Rendered> = objects().flat_map(|o| &o.walk).collect();
    let stat: BTreeSet<Rendered> = rendered_ancestors(readable.iter().chain(&walk).copied())
        .into_iter()
        .chain(walk.iter().copied().cloned())
        .collect();
    let lines = &mut profile.fs_allows;
    emit_lookups(lines, &stat);
    for name in walk {
        // Holding a link needs data access to the link, never its target.
        lines.push(Clause::allow("file-read-data", Filter::Symlink(name)).to_string());
    }
    for path in &readable {
        lines.push(Clause::allow("file-read*", Filter::Subpath(path)).to_string());
    }
    for prefix in lists.write.iter().flat_map(Object::names) {
        lines.push(Clause::allow("file-write*", Filter::Subpath(prefix)).to_string());
        lines.extend(socket_clauses(true, prefix));
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
    for path in &lists.deny {
        profile.fs_denies.extend(
            ["file-read*", "file-write*", "file-link", "process-exec"]
                .map(|op| Clause::deny(op, Filter::Subpath(path)).to_string()),
        );
        profile.fs_denies.extend(socket_clauses(false, path));
    }
}

fn socket_clauses(allow: bool, path: &Rendered) -> [String; 3] {
    [
        ("network-outbound", Filter::Subpath(path)),
        ("network-bind", Filter::LocalUnixSubpath(path)),
        ("network-inbound", Filter::LocalUnixSubpath(path)),
    ]
    .map(|(ops, filter)| Clause { allow, ops, filter }.to_string())
}

/// Metal's caches, read and written, and the lookups down to them.  Among
/// the fs allows, so a grant's deny still wins.
fn emit_gpu_caches(lines: &mut Vec<String>, caches: &[Rendered]) {
    emit_lookups(lines, &rendered_ancestors(caches));
    for cache in caches {
        lines.push(Clause::allow("file-read*", Filter::Subpath(cache)).to_string());
        lines.push(Clause::allow("file-write*", Filter::Subpath(cache)).to_string());
    }
}

/// Seatbelt carries the exec allow-list into the kernel via the
/// `process-exec` clauses [`emit_exec_rules`] renders below.
pub(crate) const RENDERS_EXEC: bool = true;

/// A `process-exec` clause over each of `names`: a directory's by `subpath`,
/// a file's by `literal`.
fn exec_clauses<'a>(
    allow: bool,
    names: &'a [Rendered],
    filter: fn(&'a Rendered) -> Filter<'a>,
) -> Vec<String> {
    let clause = |name| {
        Clause {
            allow,
            ops: "process-exec",
            filter: filter(name),
        }
        .to_string()
    };
    names.iter().map(clause).collect()
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
    // An exclusion at each position it holds, its name's and its link's
    // target's, as the fs renderers render one.
    let excluded = |excluded: &Excluded| render_excluded(&excluded.positions());
    let positives = rules.allows().map(|scope| -> Result<_, String> {
        Ok(match scope {
            KernelAllow::Any => vec![ANY_EXEC.to_string()],
            KernelAllow::Dir(dir) => {
                exec_clauses(true, &render_real(&dir.position())?, Filter::Subpath)
            }
            KernelAllow::File(real) => exec_clauses(true, &render_real(real)?, Filter::Literal),
        })
    });
    let negatives = rules.denies().map(|scope| -> Result<_, String> {
        Ok(match scope {
            ExecDeny::Dir(dir) => exec_clauses(false, &excluded(&dir.real)?, Filter::Subpath),
            ExecDeny::File(file) => exec_clauses(false, &excluded(&file.real)?, Filter::Literal),
            ExecDeny::Name(veto) => {
                vec![Clause::deny("process-exec", Filter::Regex(veto.key())).to_string()]
            }
        })
    });
    for clauses in positives.chain(negatives) {
        profile.exec_rules.extend(clauses?);
    }
    let granted = rules
        .live()
        .filter_map(|scope| match scope {
            KernelAllow::Dir(dir) => Some(render_real(&dir.position())),
            KernelAllow::File(real) => Some(render_real(real)),
            KernelAllow::Any => None,
        })
        .collect::<Result<Vec<_>, _>>()?
        .concat();
    emit_lookups(&mut profile.exec_ancestors, &rendered_ancestors(&granted));
    Ok(())
}

/// Every program may start: the wildcard, and what `Any` renders as.
const ANY_EXEC: &str = "(allow process-exec)";

/// What an SBPL clause matches.  `Display` escapes, so no name is spliced into
/// a rule without it.
enum Filter<'a> {
    Subpath(&'a Rendered),
    LocalUnixSubpath(&'a Rendered),
    Literal(&'a Rendered),
    Symlink(&'a Rendered),
    /// A final path component, wherever the name resolves.
    Regex(&'a str),
    /// Beneath none of these; never empty, which SBPL cannot say.
    Outside(&'a [&'a Rendered]),
}

impl fmt::Display for Filter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Subpath(path) => write!(f, "(subpath {})", path_literal(path)),
            Self::LocalUnixSubpath(path) => {
                write!(f, "(local unix-socket {})", Self::Subpath(path))
            }
            Self::Literal(path) => write!(f, "(literal {})", path_literal(path)),
            Self::Symlink(path) => write!(
                f,
                "(require-all {} (vnode-type SYMLINK))",
                Self::Literal(path)
            ),
            Self::Regex(name) => write!(f, "(regex {})", veto_literal(name)),
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

/// Mach services the base profile withholds on purpose, each with its reason as
/// data: a comment cannot reach the agent holding the `EPERM`, and a deliberate
/// refusal read as an accident is chased instead of worked around.
const WITHHELD_SERVICES: [(&str, &str); 3] = [
    (
        "com.apple.SecurityServer",
        "securityd hands out keychain items, so no profile admits it; cargo's bundled \
             git needs it for TLS, so set `net.git-fetch-with-cli` and cargo will fetch \
             through the git binary instead",
    ),
    (
        "com.apple.coreservices.launchservicesd",
        "launchservicesd spawns `/usr/bin/open`'s target as a child of launchd, outside \
             this profile: an escape, and for a URL an egress channel under `` net: `offline ``",
    ),
    (
        "com.apple.pasteboard",
        "the pasteboard is a clipboard no grant mentions",
    ),
];

/// Metal's per-user caches, which its compiler writes on a JIT build.
const GPU_CACHES: [&str; 3] = [
    "com.apple.metal",
    "com.apple.metalfe",
    "com.apple.gpuarchiver",
];

/// [`GPU_CACHES`] under this user's cache directory, each in every spelling
/// the kernel may present, resolved once.  Empty where `confstr` names no
/// cache directory or a name will not walk or render.
fn gpu_caches() -> &'static [Rendered] {
    static CACHES: OnceLock<Vec<Rendered>> = OnceLock::new();
    CACHES.get_or_init(|| {
        let Some(dir) = crate::host::user_cache_dir() else {
            return Vec::new();
        };
        GPU_CACHES
            .iter()
            .filter_map(|name| {
                let at = LexicalPath::absolute(&join_slash(&dir, name))?;
                render_real(walk_lenient(&at, LeafMode::Follow).ok()?.real()).ok()
            })
            .flatten()
            .collect()
    })
}

/// The reason the base profile withholds `name`, for
/// [`crate::authority::sandbox::diag`]'s denial hint.  Matched by prefix: Seatbelt logs a
/// Mach name with the instance suffix the lookup carried
/// (`com.apple.pasteboard.1`, `com.apple.distributed_notifications@Uv3`).
pub(super) fn withheld_service(name: &str) -> Option<&'static str> {
    WITHHELD_SERVICES
        .iter()
        .find(|(service, _)| name.starts_with(service))
        .map(|(_, why)| *why)
}

/// Each of `names` statable, which is how every emitter opens the lookups to
/// what it grants: Seatbelt gates each one on the way to a granted name, and
/// metadata is enough, search being all a resolver — the kernel's, or ral's
/// walk — holds on an ancestor.
fn emit_lookups<'a>(lines: &mut Vec<String>, names: impl IntoIterator<Item = &'a Rendered>) {
    for name in names {
        lines.push(Clause::allow("file-read-metadata", Filter::Literal(name)).to_string());
    }
}

/// A rendered name as an SBPL string literal.  Taking [`Rendered`] and not
/// `&str` is what makes it impossible to splice a path into a rule without
/// first passing it through [`render_real`]: every emitter in this file
/// funnels here, so the only `&str` items left are the denied basenames, which
/// are final components rather than paths and go to [`veto_literal`].
fn path_literal(path: &Rendered) -> String {
    string_literal(path.as_str())
}

/// The `/name$` pattern of a veto, with the name's regex metacharacters
/// escaped so `c++` matches itself, as an SBPL string literal.
fn veto_literal(name: &str) -> String {
    let mut pattern = String::from("/");
    for ch in name.chars() {
        if r".^$*+?()[]{}|\/".contains(ch) {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('$');
    string_literal(&pattern)
}

/// `"…"`, inside which SBPL reads only `\` and `"` specially, so every other
/// byte (a newline too) stays inside the literal.  Never the raw `#"…"`: it
/// has no escape, and the first `"` ends it.
fn string_literal(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

unsafe extern "C" {
    fn sandbox_init_with_parameters(
        profile: *const c_char,
        flags: u64,
        parameters: *const *const c_char,
        errorbuf: *mut *mut c_char,
    ) -> c_int;
    fn sandbox_free_error(errorbuf: *mut c_char);
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::{
        BASE_PROFILE, Clause, Filter, GPU_CACHES, GPU_NAMES, GPU_PROFILE, NET_PROFILE, Profile,
        RESOLVER_SOCKET, WITHHELD_SERVICES, build_profile, gpu_caches,
    };
    use crate::authority::policy::{
        ExecAllow, ExecDeny, ExecRules, FsAllow, FsDeny, Gpu, GrantPath, Net, Rule,
    };
    use crate::authority::sandbox::{ExecProjection, FsLists, FsProjection, SandboxProjection};
    use crate::path::proper_ancestors;
    use crate::path::{Excluded, FrozenPath, Trace, render_real};
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
                GrantPath::<Trace>::walk(written).expect("a directory walks"),
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
        Rule::Deny(Box::new(ExecDeny::Name(
            crate::authority::policy::Veto::of(name),
        )))
    }

    fn restricted(fs: FsProjection, rules: Vec<Rule>) -> SandboxProjection {
        SandboxProjection {
            fs,
            exec: ExecProjection::Restricted(rules.into_iter().collect::<ExecRules>().for_kernel()),
            ..SandboxProjection::default()
        }
    }

    fn prefix(path: &str) -> FsAllow {
        FsAllow::Prefix(
            GrantPath::<Trace>::walk(FrozenPath::from_surface(path)).expect("a prefix walks"),
        )
    }

    fn frozen(paths: &[&str]) -> Vec<FsDeny> {
        (paths.iter())
            .map(|p| {
                FsDeny(
                    GrantPath::<Excluded>::walk(FrozenPath::from_surface(p))
                        .expect("an exclusion walks"),
                )
            })
            .collect()
    }

    /// A restricted fs writing under `prefixes` and nothing else.
    fn writing(prefixes: &[&str]) -> FsProjection {
        FsProjection::Restricted(FsLists {
            write: prefixes.iter().copied().map(prefix).collect(),
            ..FsLists::default()
        })
    }

    /// A restricted fs reading `reads` and nothing else.
    fn reading(reads: Vec<FsAllow>) -> FsProjection {
        FsProjection::Restricted(FsLists {
            read: reads,
            ..FsLists::default()
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
            net: None,
            gpu: Some(Gpu::Withheld),
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
            r#"(deny process-exec (regex "/z$"))"#,
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
        let deny = at(&profile, r#"(deny process-exec (regex "/git$"))"#);
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
        let forms: Vec<_> = crate::path::twins(place.as_path())
            .map(std::borrow::Cow::into_owned)
            .collect();
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

    /// The surface admits traversal, not access beneath a retargeted link.
    #[test]
    fn mac_profile_lets_a_lookup_traverse_the_surface_of_a_frozen_allowance() {
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
            [
                format!("(allow file-read-metadata (literal \"{link}\"))"),
                format!(
                    "(allow file-read-data (require-all (literal \"{link}\") (vnode-type SYMLINK)))"
                ),
            ],
            "{profile}"
        );
    }

    /// Every name a lookup of an allowance's spelling passes is traversable: a
    /// chain's middle link, which lies on neither the spelling nor the real
    /// path, and a directory a `..` in a target climbs out of.
    #[test]
    fn mac_profile_lets_a_lookup_traverse_every_name_a_chain_passes() {
        let tmp = tempfile::tempdir().expect("a temp dir");
        let root = crate::test_helper::real(tmp.path());
        let under = |name: &str| root.as_path().join(name);
        std::fs::create_dir(under("sub")).expect("a directory to climb out of");
        std::fs::create_dir(under("real")).expect("the target");
        std::os::unix::fs::symlink("sub/../real", under("middle")).expect("the middle link");
        std::os::unix::fs::symlink(under("middle"), under("link")).expect("the first link");
        let linked = prefix(crate::path::utf8(&under("link")).unwrap());
        let profile = build_profile(&SandboxProjection {
            fs: reading(vec![linked]),
            ..SandboxProjection::default()
        })
        .unwrap();
        let kernel = rules(&profile);
        for name in ["link", "middle", "sub"] {
            let name = crate::path::utf8(&under(name)).unwrap().to_owned();
            let named: Vec<&str> = kernel.lines().filter(|l| l.contains(&name)).collect();
            assert_eq!(
                named,
                [
                    format!("(allow file-read-metadata (literal \"{name}\"))"),
                    format!(
                        "(allow file-read-data (require-all (literal \"{name}\") (vnode-type SYMLINK)))"
                    ),
                ],
                "{profile}"
            );
        }
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
        let veto = at(&profile, r#"(deny process-exec (regex "/git$"))"#);
        at(&profile, r#"(deny process-exec (regex "/c\\+\\+$"))"#);
        assert!(allow < veto, "a veto must follow the allow it overrides");
        assert!(
            !profile.contains(r#"(deny file-read* (regex "/git$"))"#),
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
        render_real(own.trace().real()).unwrap()
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
        let restricted_fs = FsProjection::Restricted(FsLists {
            read: vec![prefix(&dir)],
            write: vec![prefix(&dir)],
            deny: frozen(&[&format!("{dir}/x/secret")]),
            ..FsLists::default()
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

    /// A write prefix is among the reads too, as the fold makes it, and is
    /// read by one clause, never one per list naming it.
    #[test]
    fn mac_profile_reads_a_written_prefix_once() {
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsLists {
                read: vec![prefix("/ral-test/w")],
                write: vec![prefix("/ral-test/w")],
                ..FsLists::default()
            }),
            ..SandboxProjection::default()
        };
        let profile = build_profile(&policy).unwrap();
        let read = "(allow file-read* (subpath \"/ral-test/w\"))";
        assert_eq!(profile.matches(read).count(), 1, "{read}:\n{profile}");
        let outside = "(deny process-exec (require-not (require-any (subpath \"/ral-test/w\"))))";
        assert!(profile.contains(outside), "{outside}:\n{profile}");
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

    /// Offline closes DNS only because *no* network rule reaches the
    /// profile: the resolver talks to mDNSResponder over a UNIX socket, which
    /// Seatbelt gates as `network-outbound`.  Admitting that form for any
    /// local socket would reopen hostname-label egress while offline still
    /// read as closed, so the denial is asserted over every rule the
    /// profile carries, not just the IP ones.
    #[test]
    fn mac_profile_denies_network_when_disabled() {
        let profile = build_profile(&SandboxProjection {
            net: Some(Net::Offline),
            ..SandboxProjection::default()
        })
        .unwrap();
        for rule in rules(&profile).lines() {
            assert!(
                !rule.contains("network"),
                "offline admitted a network rule: {rule}\n{profile}"
            );
        }
        let open = build_profile(&SandboxProjection {
            net: None,
            ..SandboxProjection::default()
        })
        .unwrap();
        assert!(
            rules(&open).contains("(allow network-outbound (remote ip))"),
            "online emitted no network rule, so the denial above proves nothing:\n{open}"
        );
    }

    #[test]
    fn mac_write_prefix_admits_unix_connections_binding_and_listening() {
        let policy = SandboxProjection {
            fs: writing(&["/ral-test/write"]),
            net: Some(Net::Offline),
            ..SandboxProjection::default()
        };
        let profile = build_profile(&policy).unwrap();
        for clause in [
            "(allow network-outbound (subpath \"/ral-test/write\"))",
            "(allow network-bind (local unix-socket (subpath \"/ral-test/write\")))",
            "(allow network-inbound (local unix-socket (subpath \"/ral-test/write\")))",
        ] {
            assert!(profile.contains(clause), "missing {clause}:\n{profile}");
        }
    }

    #[test]
    fn mac_read_only_and_unrestricted_fs_admit_no_unix_socket() {
        for fs in [
            reading(vec![prefix("/ral-test/read")]),
            FsProjection::Unrestricted,
        ] {
            let policy = SandboxProjection {
                fs,
                net: Some(Net::Offline),
                ..SandboxProjection::default()
            };
            let profile = rules(&build_profile(&policy).unwrap());
            for op in ["network-outbound", "network-bind", "network-inbound"] {
                assert!(!profile.contains(op), "{profile}");
            }
        }
    }

    #[test]
    fn mac_fs_deny_withholds_unix_sockets_after_write_allows() {
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsLists {
                write: vec![prefix("/ral-test/write")],
                deny: frozen(&["/ral-test/write/denied"]),
                ..FsLists::default()
            }),
            net: Some(Net::Offline),
            ..SandboxProjection::default()
        };
        let profile = build_profile(&policy).unwrap();
        for (allow, deny) in [
            (
                "(allow network-outbound (subpath \"/ral-test/write\"))",
                "(deny network-outbound (subpath \"/ral-test/write/denied\"))",
            ),
            (
                "(allow network-bind (local unix-socket (subpath \"/ral-test/write\")))",
                "(deny network-bind (local unix-socket (subpath \"/ral-test/write/denied\")))",
            ),
            (
                "(allow network-inbound (local unix-socket (subpath \"/ral-test/write\")))",
                "(deny network-inbound (local unix-socket (subpath \"/ral-test/write/denied\")))",
            ),
        ] {
            assert!(at(&profile, allow) < at(&profile, deny));
        }
    }

    /// `net` is the internet, not every local socket: a Unix socket reaches a
    /// daemon acting with the user's authority, ssh-agent's for one, so the
    /// only path-named socket admitted is the resolver's.
    #[test]
    fn mac_net_profile_admits_no_unix_socket_but_the_resolver() {
        let resolver = format!("(allow network-outbound (literal \"{RESOLVER_SOCKET}\"))");
        let net = rules(NET_PROFILE);
        assert!(net.contains(&resolver), "no resolver socket:\n{net}");
        for rule in net.lines().filter(|l| l.contains("network")) {
            assert!(
                rule == resolver || rule.contains("(remote ip)") || rule.contains("(local ip)"),
                "online admitted a non-IP socket: {rule}\n{net}"
            );
        }
    }

    /// A wholesale `(allow mach-lookup)` hands out launchservicesd, and with
    /// it `/usr/bin/open` — a spawn performed by launchd outside this profile,
    /// which is an escape and, for a URL, an egress channel.  The service list is
    /// therefore closed by name, and the resolver's services ride the `net` veto
    /// with the sockets.
    #[test]
    fn mac_profile_names_every_mach_service() {
        let closed = rules(
            &build_profile(&SandboxProjection {
                net: Some(Net::Offline),
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
            net: None,
            ..SandboxProjection::default()
        })
        .unwrap();
        assert!(
            rules(&open).contains("com.apple.dnssd.service"),
            "online emitted no resolver service:\n{open}"
        );
        // A service the hint explains must be one no projection opens, or the
        // reason is quoted about a service the child actually holds.
        for (service, _) in WITHHELD_SERVICES {
            assert!(
                !closed.contains(service) && !rules(&open).contains(service),
                "{service} carries a withheld-service reason yet some profile admits it"
            );
        }
    }

    /// The GPU's rules ride the `gpu` veto alone, and the names the hint quotes
    /// are the ones the profile opens.
    #[test]
    fn mac_profile_withholds_the_gpu_when_withheld() {
        let closed = rules(
            &build_profile(&SandboxProjection {
                gpu: Some(Gpu::Withheld),
                ..SandboxProjection::default()
            })
            .unwrap(),
        );
        assert!(!closed.contains("iokit-open"), "{closed}");
        let open = rules(&build_profile(&SandboxProjection::default()).unwrap());
        for name in GPU_NAMES {
            assert!(
                !closed.contains(name),
                "withheld, the profile admits {name}:\n{closed}"
            );
            assert!(
                open.contains(name),
                "left on, the profile omits {name}:\n{open}"
            );
            assert!(
                rules(GPU_PROFILE).contains(name),
                "{name} is not in macos-gpu.sbpl"
            );
        }
    }

    /// Left on, the GPU writes Metal's caches in every spelling, among the fs
    /// allows so a grant's deny of them still wins; withheld, it writes none.
    #[test]
    fn mac_profile_writes_metals_caches_only_where_the_gpu_is_left() {
        let caches = gpu_caches();
        assert!(!caches.is_empty(), "this host names a user cache directory");
        let open = build_profile(&SandboxProjection::default()).unwrap();
        let closed = build_profile(&SandboxProjection {
            gpu: Some(Gpu::Withheld),
            ..SandboxProjection::default()
        })
        .unwrap();
        for cache in caches {
            let write = Clause::allow("file-write*", Filter::Subpath(cache)).to_string();
            assert!(open.contains(&write), "{write}:\n{open}");
            assert!(!closed.contains(&write), "{write}:\n{closed}");
        }
        for name in GPU_CACHES {
            assert!(
                caches.iter().any(|c| c.as_str().ends_with(name)),
                "{name} unrendered: {caches:?}"
            );
        }
    }

    /// A grant's deny of a Metal cache follows the cache's allow, so it wins.
    #[test]
    fn mac_profile_denies_a_metal_cache_after_the_gpu_allows_it() {
        let metal = gpu_caches()
            .iter()
            .find(|c| c.as_str().ends_with("/com.apple.metal"))
            .expect("com.apple.metal renders");
        let profile = build_profile(&SandboxProjection {
            fs: FsProjection::Restricted(FsLists {
                deny: frozen(&[metal.as_str()]),
                ..FsLists::default()
            }),
            ..SandboxProjection::default()
        })
        .unwrap();
        let allow = Clause::allow("file-write*", Filter::Subpath(metal)).to_string();
        let deny = Clause::deny("file-write*", Filter::Subpath(metal)).to_string();
        assert!(at(&profile, &allow) < at(&profile, &deny), "{profile}");
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
                "(allow ipc-posix-shm-read-data (ipc-posix-name \"apple.shm.notification_center\") (ipc-posix-name \"com.apple.featureflags.shm\"))"
            )
        );
        assert_eq!(profile.matches("(allow ipc-posix-shm").count(), 2);
        assert_eq!(profile.matches("(allow ipc-posix-sem").count(), 1);
        assert!(profile.contains("(allow ipc-posix-sem (ipc-posix-name-prefix \"/mp-\"))"));
        assert!(profile.contains("(allow ipc-posix-shm (ipc-posix-name-prefix \"/psm_\"))"));
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
        // The baseline arrives as read prefixes, ancestor metadata with them.
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
    fn mac_profile_denies_current_fs_and_exec_positions() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        let secret = tmp.path().join("secret");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(&secret, "secret").unwrap();
        let policy = restricted(
            FsProjection::Restricted(FsLists {
                write: vec![prefix(work.to_str().unwrap())],
                deny: frozen(&[secret.to_str().unwrap()]),
                ..FsLists::default()
            }),
            vec![
                dir(work.to_str().unwrap(), true),
                file(secret.to_str().unwrap(), false),
            ],
        );
        let moved = work.join("x");
        std::fs::rename(&secret, &moved).unwrap();
        let position = crate::test_helper::real(&moved);
        let profile = build_profile(&policy).unwrap();
        for name in render_real(&position).unwrap() {
            for op in ["file-read*", "file-write*", "process-exec"] {
                assert!(
                    profile.contains(&format!("(deny {op} (subpath \"{}\"))", name.as_str()))
                        || profile
                            .contains(&format!("(deny {op} (literal \"{}\"))", name.as_str())),
                    "{profile}"
                );
            }
        }
    }

    #[test]
    fn mac_profile_emits_deny_rules_for_deny_paths() {
        // /tmp firmlinks to /private/tmp, so both spellings must appear.
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsLists {
                write: vec![prefix("/tmp/work")],
                deny: frozen(&["/tmp/work/.exarch.toml"]),
                ..FsLists::default()
            }),
            net: None,
            gpu: None,
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
            fs: FsProjection::Restricted(FsLists {
                write: vec![prefix("/tmp/work")],
                deny: frozen(&["/tmp/work/.ssh/id_rsa"]),
                ..FsLists::default()
            }),
            net: None,
            gpu: None,
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
    fn mac_profile_pins_the_resolved_ancestor_reached_through_an_alias() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("top/deep")).expect("nested target dir");
        std::os::unix::fs::symlink(root.join("top/deep"), root.join("alias"))
            .expect("a symlink aliasing the nested target");

        let write = root.to_str().expect("ascii temp path").to_string();
        let deny = format!("{write}/alias/secret");
        let policy = SandboxProjection {
            fs: FsProjection::Restricted(FsLists {
                write: vec![prefix(&write)],
                deny: frozen(&[&deny]),
                ..FsLists::default()
            }),
            net: None,
            gpu: None,
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

    /// One SBPL string literal off the front of `s`, read as the profile
    /// reader does: `\\` takes the next byte, `"` ends it.
    fn read_literal(s: &str) -> Option<(String, &str)> {
        let body = s.strip_prefix('"')?;
        let mut chars = body.char_indices();
        let mut out = String::new();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => return Some((out, &body[i + 1..])),
                '\\' => out.push(chars.next()?.1),
                c => out.push(c),
            }
        }
        None
    }

    const HOSTILE: &str = "q\"b\\s\nn\")(allow default)";

    #[test]
    fn a_hostile_path_stays_inside_its_literal() {
        let dir = tempfile::tempdir().expect("temp dir");
        let hostile = dir.path().join(HOSTILE);
        std::fs::write(&hostile, "").expect("a file with a hostile name");
        let names = render_real(trace(&hostile).real()).expect("renders");
        assert_ne!(names, [] as [super::Rendered; 0]);
        for name in &names {
            let line = Filter::Literal(name).to_string();
            let read = line.strip_prefix("(literal ").and_then(read_literal);
            assert_eq!(read, Some((name.as_str().to_string(), ")")), "{line:?}");
        }
    }

    #[test]
    fn a_hostile_veto_stays_inside_its_regex_literal() {
        let name = format!("{HOSTILE}a.*+?()[]{{}}|^$/e");
        let line = Filter::Regex(&name).to_string();
        let (pattern, rest) = line
            .strip_prefix("(regex ")
            .and_then(read_literal)
            .unwrap_or_else(|| panic!("no literal in {line:?}"));
        assert_eq!(rest, ")", "{line:?}");
        let body = pattern
            .strip_prefix('/')
            .and_then(|p| p.strip_suffix('$'))
            .unwrap_or_else(|| panic!("not a final-component pattern: {pattern:?}"));
        let mut chars = body.chars();
        let mut matched = String::new();
        while let Some(c) = chars.next() {
            if c == '\\' {
                matched.extend(chars.next());
            } else {
                assert!(
                    !r".^$*+?()[]{}|\/".contains(c),
                    "{c:?} unescaped in {pattern:?}"
                );
                matched.push(c);
            }
        }
        assert_eq!(matched, name);
    }

    /// The kernel's reader agrees with `read_literal`: a hostile veto compiles
    /// and denies its own name alone.
    #[test]
    fn the_kernel_reads_a_hostile_veto_as_its_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let hostile = dir.path().join(HOSTILE);
        let plain = dir.path().join("plain");
        for file in [&hostile, &plain] {
            std::fs::write(file, "x").expect("a file");
        }
        let profile = format!(
            "(version 1)(allow default)(deny file-read-data {})",
            Filter::Regex(HOSTILE)
        );
        let reads = |file: &std::path::Path| {
            std::process::Command::new("/usr/bin/sandbox-exec")
                .args(["-p", &profile, "/bin/cat"])
                .arg(file)
                .output()
                .expect("sandbox-exec runs")
                .status
                .success()
        };
        assert_eq!((reads(&hostile), reads(&plain)), (false, true), "{profile}");
    }
}
