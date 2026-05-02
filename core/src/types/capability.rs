//! Capability layer: typed authority pushed onto the dynamic stack.
//!
//! `Capabilities` is one frame of the stack — a bundle of per-effect
//! `*Policy` values plus an `audit` flag.  Frames are composed by
//! [`Capabilities::meet`], which makes the type a meet-semilattice with
//! [`Capabilities::root`] as top and [`Capabilities::deny_all`] as
//! bottom for positive authority.
//!
//! ## Fields
//!
//! - [`ExecPolicy`]: per-command exec rule (`Allow`, `Subcommands`, or `Deny`).
//! - [`FsPolicy`]: read/write prefixes and explicit `deny_paths` for
//!   single files.
//! - [`EditorPolicy`] / [`ShellPolicy`]: bit flags gating REPL-side builtins.
//! - `net`: tristate (None=inherit, Some(false)=deny, Some(true)=allow).
//! - `audit`: orthogonal flag — propagated upward by `meet` (logical OR),
//!   not part of the lattice.
//!
//! ## `SandboxProjection`
//!
//! [`SandboxProjection`] is the meet-folded effective fs+net policy used
//! to render the OS sandbox profile and ferry policy across the IPC
//! boundary.  Produced by `capability::EffectiveGrant`; consumed only by
//! sandbox backends.

use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

use super::{Shell, unique_strings};

// ── Lattice traits ────────────────────────────────────────────────────────
//
// `Meet` and `Join` name the two semilattice operations the capability
// system runs over.  They live here, alongside the types they're
// implemented on, so the algebraic structure is one file.  Lifting
// impls for `Option<T>` and `bool` come with the traits — `None` as
// identity is a capability convention (no opinion on a field), not a
// universal one, so the impls aren't general enough to belong in a
// stand-alone module.

/// Greatest lower bound under the type's partial order.  Combining
/// two `Meet` values produces the most-authority element below both.
///
/// Required laws (verified by `lattice_tests` per type):
///
/// * `a.meet(b) == b.meet(a)` — commutative.
/// * `(a.meet(b)).meet(c) == a.meet(b.meet(c))` — associative.
/// * `a.meet(a) == a` — idempotent.
pub trait Meet {
    fn meet(self, other: Self) -> Self;
}

/// Least upper bound under the type's partial order — dual of [`Meet`].
/// Used at load time to widen a base ceiling with an extension before
/// any attenuation runs (`base.join(extension)` adds authority).
pub trait Join {
    fn join(self, other: Self) -> Self;
}

/// `None` is the meet identity: a layer with no opinion on a field
/// contributes nothing, so the other side's value survives unchanged.
impl<T: Meet> Meet for Option<T> {
    fn meet(self, other: Self) -> Self {
        match (self, other) {
            (None, x) | (x, None) => x,
            (Some(a), Some(b)) => Some(a.meet(b)),
        }
    }
}

/// `None` is also the join identity, by the same reasoning: nothing
/// to widen with, so the present side survives.
impl<T: Join> Join for Option<T> {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (None, x) | (x, None) => x,
            (Some(a), Some(b)) => Some(a.join(b)),
        }
    }
}

/// Meet on bool is `&&`: both sides must hold.  Used for `net`,
/// `editor.{read,write,tui}`, `shell.chdir`.
impl Meet for bool {
    fn meet(self, other: Self) -> Self {
        self && other
    }
}

/// Join on bool is `||`: either side widens.
impl Join for bool {
    fn join(self, other: Self) -> Self {
        self || other
    }
}

/// Exec policy value for a single command in a `grant` exec map.
///
/// Forms a three-point lattice with `Allow` at top, `Subcommands(_)`
/// in the middle (more elements = more authority), and `Deny` at
/// bottom.  An explicit `Deny` is a sticky veto: it survives meet
/// against absence in another layer's map (so a base ceiling can
/// pin a command name out without restrict files having to repeat
/// it) and beats subpath admission elsewhere in the same map.
///
/// A key in the exec map may be a bare command name (`git`), an
/// absolute literal path (`/usr/bin/git`), or — when the key ends
/// with `/` — an absolute subpath prefix (`/usr/bin/`) that admits
/// or denies any binary resolving inside that directory.  Subpath
/// keys carry `Allow` or `Deny`; `Subcommands` on a subpath key is
/// rejected at validation time.  Longest-prefix wins among subpath
/// keys, and literal keys beat subpaths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecPolicy {
    /// Allow the command with any arguments.
    Allow,
    /// Allow only when the first argument is in this list.
    Subcommands(Vec<String>),
    /// Reject the command outright, even if `exec_dirs` would admit
    /// the resolved path.  Lattice bottom.
    Deny,
}

/// Filesystem access policy within a `grant` block.
///
/// `deny_paths` carves out subtrees that no read, write, link, or
/// rename may touch, even when a covering `read_prefix` or
/// `write_prefix` would otherwise admit them.  Treated as subpath
/// matches: a single file path denies just that file, a directory
/// path denies everything under it.
///
/// Two motivating cases:
///   - the active `.exarch.toml` capability profile, untouchable
///     inside an otherwise-writable cwd so the agent cannot widen
///     its own grant;
///   - credential subdirs of broadly-readable config roots
///     (`xdg:config/gh`, `xdg:config/op`, …) — `xdg:config` is
///     wholesale read so tools find their config, but the deny
///     overlay keeps OAuth tokens out of reach.
///
/// `deny_unknown_fields`: see [`EditorPolicy`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FsPolicy {
    #[serde(default)]
    pub read_prefixes: Vec<String>,
    #[serde(default)]
    pub write_prefixes: Vec<String>,
    #[serde(default)]
    pub deny_paths: Vec<String>,
}

/// OS-renderable view of the meet-folded fs policy.
///
/// `Unrestricted` is the lattice top: no layer attenuated fs, so the
/// OS profile passes fs through (broad `(allow file-read*)` /
/// `(allow file-write*)` on macOS; whole-tree `--dev-bind / /` on
/// Linux).  `Restricted` carries the closed set: the
/// [`FsPolicy::read_prefixes`], [`FsPolicy::write_prefixes`] and
/// [`FsPolicy::deny_paths`] survive into platform-specific rules.
///
/// Empty `Restricted(FsPolicy::default())` is "deny everything fs":
/// the user explicitly granted no read or write prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "policy", rename_all = "snake_case")]
pub enum FsProjection {
    Unrestricted,
    Restricted(FsPolicy),
}

impl Default for FsProjection {
    fn default() -> Self {
        Self::Unrestricted
    }
}

impl FsProjection {
    /// The policy when restricted, or `None` for the unrestricted top.
    /// Renderers that only care about the policy bytes (Linux's
    /// `make_command_with_policy`, the bind/check spec helpers) match
    /// on this; the macOS profile builder branches on the variant
    /// directly so it can emit different SBPL shapes.
    pub fn as_policy(&self) -> Option<&FsPolicy> {
        match self {
            Self::Unrestricted => None,
            Self::Restricted(p) => Some(p),
        }
    }
}

/// OS-renderable view of the meet-folded exec policy.
///
/// `Unrestricted` is the lattice top: no layer attenuated exec, so
/// the OS profile leaves `process-exec` wide open and the in-ral
/// gate is the only check.  `Restricted` carries the closed set the
/// OS profile may admit (`allow_paths` resolved literals and
/// `allow_dirs` subpath roots) plus the explicit `deny_dirs` carved
/// out of those admits with longest-prefix-wins semantics.  Anything
/// outside admits is denied at the OS layer too, closing the
/// `sh -c "PATH=…; cmd"` route by which a sandboxed child re-execs
/// binaries the in-ral gate never sees.
///
/// Empty `Restricted { allow_paths: [], allow_dirs: [], deny_dirs: [] }`
/// means a layer opted in to exec restriction and admitted nothing —
/// the OS profile emits no `(allow process-exec …)` rule and the
/// deny-default kills any spawn from inside the grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecProjection {
    Unrestricted,
    Restricted {
        allow_paths: Vec<String>,
        allow_dirs: Vec<String>,
        #[serde(default)]
        deny_dirs: Vec<String>,
    },
}

impl Default for ExecProjection {
    fn default() -> Self {
        Self::Unrestricted
    }
}

/// The OS-renderable projection of the effective capability grant.
///
/// Produced by `capability::EffectiveGrant::sandbox_projection` after
/// meet-folding the dynamic stack; consumed by the platform sandbox
/// backends (`sandbox::linux`, `sandbox::macos`) and ferried across
/// the IPC boundary in the internal `--sandbox-projection` flag.
///
/// This is distinct from `Capabilities` (one stack frame, possibly
/// extending authority) — a `SandboxProjection` is the reduced
/// fs+net+exec shape no further composition can widen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxProjection {
    #[serde(default)]
    pub fs: FsProjection,
    /// Final network verdict after reducing the capability stack.
    pub net: bool,
    #[serde(default)]
    pub exec: ExecProjection,
}

impl Default for SandboxProjection {
    fn default() -> Self {
        Self {
            fs: FsProjection::default(),
            net: true,
            exec: ExecProjection::default(),
        }
    }
}

/// Lexical view of the projection: prefixes as written, for the
/// Seatbelt / bwrap profile renderer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxBindSpec {
    pub read_prefixes: Vec<String>,
    pub write_prefixes: Vec<String>,
    pub deny_paths: Vec<String>,
}

/// Shell-resolved view of the projection: every prefix passed
/// through [`Dynamic::resolver`]'s lenient pipeline, for in-ral
/// path checks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxCheckSpec {
    pub read_prefixes: Vec<String>,
    pub write_prefixes: Vec<String>,
    pub deny_paths: Vec<String>,
}

impl SandboxProjection {
    /// Lexical-form bind spec for the OS profile renderer.  Returns an
    /// empty spec when fs is `Unrestricted` — the renderer should not
    /// emit per-prefix rules in that case (it emits broad allows).
    pub fn bind_spec(&self) -> SandboxBindSpec {
        let Some(fs) = self.fs.as_policy() else {
            return SandboxBindSpec::default();
        };
        SandboxBindSpec {
            read_prefixes: unique_strings(fs.read_prefixes.iter().cloned()),
            write_prefixes: unique_strings(fs.write_prefixes.iter().cloned()),
            deny_paths: unique_strings(fs.deny_paths.iter().cloned()),
        }
    }

    /// Resolver-form check spec for in-ral fs decisions.  Same shape as
    /// `bind_spec` and same `Unrestricted` → empty rule.
    pub fn check_spec(&self, shell: &Shell) -> SandboxCheckSpec {
        let Some(fs) = self.fs.as_policy() else {
            return SandboxCheckSpec::default();
        };
        let resolve = |prefixes: &[String]| -> Vec<String> {
            unique_strings(prefixes.iter().map(|p| {
                shell.dynamic.resolver().check(p).to_string_lossy().into_owned()
            }))
        };
        SandboxCheckSpec {
            read_prefixes: resolve(&fs.read_prefixes),
            write_prefixes: resolve(&fs.write_prefixes),
            deny_paths: resolve(&fs.deny_paths),
        }
    }
}

/// Editor policy for `grant` blocks.
///
/// `deny_unknown_fields` is structural: a stray top-level key that
/// accidentally lands inside `[editor]` due to TOML's table-attachment
/// rule (every key after a header belongs to that header until the
/// next one) errors instead of being silently dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditorPolicy {
    pub read: bool,
    pub write: bool,
    pub tui: bool,
}

/// Shell policy — controls what shell operations a plugin handler may perform.
///
/// `deny_unknown_fields`: see [`EditorPolicy`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellPolicy {
    pub chdir: bool,
}

/// Syntactic capability bundle as parsed from TOML, JSON, or
/// constructed by the runtime `grant` builtin.  Path lists may
/// hold `~` and `xdg:` sigils; lattice composition (meet/join)
/// runs here, before freezing.  The only path from `RawCapabilities`
/// to [`Capabilities`] is [`RawCapabilities::freeze`], which
/// resolves every sigil against `HOME` and produces a runtime
/// object the dynamic stack will accept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawCapabilities {
    /// Unified exec map.  Keys are either bare command names, absolute
    /// literal paths, or absolute subpath prefixes (trailing `/`).
    /// See [`ExecPolicy`] for the lattice.  Subpath keys may carry
    /// sigils (`xdg:bin/`, `~/.cargo/bin/`, `cwd:/`) which are
    /// resolved by [`Self::freeze`].
    #[serde(default)]
    pub exec: Option<BTreeMap<String, ExecPolicy>>,
    #[serde(default)]
    pub fs: Option<FsPolicy>,
    #[serde(default)]
    pub net: Option<bool>,
    #[serde(default)]
    pub audit: bool,
    #[serde(default)]
    pub editor: Option<EditorPolicy>,
    #[serde(default)]
    pub shell: Option<ShellPolicy>,
}

/// One frozen layer in the dynamic capabilities stack — a bundle
/// of policies plus an `audit` flag.  Constructed only by
/// [`RawCapabilities::freeze`] (or the trivial path-free
/// [`Capabilities::root`] / [`Capabilities::deny_all`] /
/// [`Capabilities::default`]); production code thus arranges
/// that every `Capabilities` on the dynamic stack has had its
/// sigils resolved.
///
/// `Deserialize` is implemented for the IPC mirror in
/// `sandbox/ipc/wire.rs`, where the wire is a trusted boundary
/// between cooperating ral processes (the parent has already
/// frozen).  Untrusted external input (TOML profiles, JSON
/// manifests) must go through [`RawCapabilities`] — the freeze
/// step is the only path that resolves sigils and applies the
/// XDG-escapes-HOME guard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Unified exec map.  See [`RawCapabilities::exec`].
    #[serde(default)]
    pub exec: Option<BTreeMap<String, ExecPolicy>>,
    #[serde(default)]
    pub fs: Option<FsPolicy>,
    #[serde(default)]
    pub net: Option<bool>,
    #[serde(default)]
    pub audit: bool,
    #[serde(default)]
    pub editor: Option<EditorPolicy>,
    #[serde(default)]
    pub shell: Option<ShellPolicy>,
}

impl Capabilities {
    /// Ambient authority — the root of every capabilities stack.
    /// All fields `None`: no attenuation.  Trivially frozen
    /// (no paths).
    pub fn root() -> Self {
        Self::default()
    }

    /// Deny every effect capability.  Used as the base for
    /// explicit grants: callers opt capabilities back in by
    /// replacing individual fields.  Trivially frozen.
    pub fn deny_all() -> Self {
        Self {
            exec: Some(BTreeMap::new()),
            fs: Some(FsPolicy::default()),
            net: Some(false),
            editor: Some(EditorPolicy::default()),
            shell: Some(ShellPolicy::default()),
            audit: false,
        }
    }

    /// True for a real attenuation context, false for the ambient
    /// root.
    pub fn is_restrictive(&self) -> bool {
        self.exec.is_some()
            || self.fs.is_some()
            || self.net.is_some()
            || self.editor.is_some()
            || self.shell.is_some()
    }
}

impl RawCapabilities {
    /// Deny every effect capability — see
    /// [`Capabilities::deny_all`].
    pub fn deny_all() -> Self {
        Self {
            exec: Some(BTreeMap::new()),
            fs: Some(FsPolicy::default()),
            net: Some(false),
            editor: Some(EditorPolicy::default()),
            shell: Some(ShellPolicy::default()),
            audit: false,
        }
    }

    /// True for a real attenuation context, false for ambient root.
    pub fn is_restrictive(&self) -> bool {
        self.exec.is_some()
            || self.fs.is_some()
            || self.net.is_some()
            || self.editor.is_some()
            || self.shell.is_some()
    }

    /// Reject unknown `xdg:` tokens in any path list, and reject any
    /// subpath-style exec key (`/path/to/dir/`) carrying a non-`Allow`
    /// policy.  Pure syntactic check; cheap, no env or filesystem
    /// access.  Production loaders should call [`Self::freeze`], which
    /// subsumes this check and resolves the tokens to concrete paths
    /// under `home`.
    pub fn validate_paths(&self) -> Result<(), String> {
        use crate::path::sigil::validate_xdg_tokens;
        if let Some(exec) = &self.exec {
            // Sigil-bearing keys (xdg:, ~, cwd:, tempdir:) are the only
            // exec-map entries that need xdg validation; literal names
            // like `git` carry no sigils.  Easiest: validate the whole
            // key set — `validate_xdg_tokens` no-ops on non-sigil keys.
            let keys: Vec<String> = exec.keys().cloned().collect();
            validate_xdg_tokens(&keys)?;
            for (key, policy) in exec {
                if is_subpath_key(key) && matches!(policy, ExecPolicy::Subcommands(_)) {
                    return Err(format!(
                        "exec subpath key '{key}' may carry 'Allow' or 'Deny'; \
                         'Subcommands' is name-shaped and requires a literal key"
                    ));
                }
            }
        }
        if let Some(fs) = &self.fs {
            validate_xdg_tokens(&fs.read_prefixes)?;
            validate_xdg_tokens(&fs.write_prefixes)?;
            validate_xdg_tokens(&fs.deny_paths)?;
        }
        Ok(())
    }

    /// Resolve every `~` / `xdg:` / `cwd:` / `tempdir:` sigil in
    /// this policy's path lists and produce the frozen runtime
    /// [`Capabilities`].  This is the only constructor for
    /// non-trivial `Capabilities`: the type system enforces that
    /// no sigil-bearing strings reach the dynamic stack or the
    /// OS sandbox profile.
    ///
    /// XDG values that escape `ctx.home` are rejected as a defence
    /// in depth — an attacker-controlled `XDG_*_HOME=/etc` would
    /// otherwise widen `xdg:data` to grant `/etc` read.  Tilde
    /// paths expand against `ctx.home`, `cwd:` against `ctx.cwd`,
    /// and `tempdir:` against `std::env::temp_dir()`.
    pub fn freeze(mut self, ctx: &crate::path::sigil::FreezeCtx<'_>) -> Result<Capabilities, String> {
        use crate::path::sigil::freeze_path_list;
        if let Some(exec) = self.exec.as_mut() {
            *exec = freeze_exec_map(std::mem::take(exec), ctx)?;
        }
        if let Some(fs) = self.fs.as_mut() {
            freeze_path_list(&mut fs.read_prefixes, ctx)?;
            freeze_path_list(&mut fs.write_prefixes, ctx)?;
            freeze_path_list(&mut fs.deny_paths, ctx)?;
        }
        Ok(Capabilities {
            exec: self.exec,
            fs: self.fs,
            net: self.net,
            audit: self.audit,
            editor: self.editor,
            shell: self.shell,
        })
    }

    /// Lattice meet — the most-authority capability below both
    /// `self` and `other`.  `RawCapabilities::default()` is top,
    /// `deny_all()` is bottom; `meet` is commutative,
    /// associative, idempotent.
    ///
    /// Each `Option<_>` field treats `None` as ⊤, so
    /// `meet(None, x) = x`.  Inner fields intersect (exec maps,
    /// fs prefixes), AND (net, editor, shell), and union
    /// (`fs.deny_paths` — more denies = less authority).
    /// `audit` is not part of the lattice: it propagates upward
    /// (logical OR).
    pub fn meet(self, other: Self) -> Self {
        // Per-field meets via the lattice trait (Option<T>: Meet does
        // the None-as-identity lift; bool, FsPolicy, EditorPolicy,
        // ShellPolicy each impl Meet directly).  The exec map needs
        // shape-aware partitioning for subpath keys, so it goes through
        // the local `meet_exec` helper rather than the trait.
        Self {
            exec:   match (self.exec, other.exec) {
                (None, x) | (x, None) => x,
                (Some(a), Some(b)) => Some(meet_exec(a, b)),
            },
            fs:     self.fs.meet(other.fs),
            net:    self.net.meet(other.net),
            editor: self.editor.meet(other.editor),
            shell:  self.shell.meet(other.shell),
            audit:  self.audit || other.audit,
        }
    }

    /// Lattice join — the least-authority capability above both
    /// `self` and `other`.  Used to widen a base ceiling with an
    /// extension TOML before any attenuation runs.  Commutative,
    /// associative, idempotent.
    ///
    /// `None` on a field acts as the join identity.  Inner
    /// fields union (exec maps, fs prefixes), OR (net, editor,
    /// shell), and intersect (`fs.deny_paths` — fewer denies =
    /// more authority).
    pub fn join(self, other: Self) -> Self {
        // Per-field joins via the lattice trait — symmetric to `meet`.
        Self {
            exec:   match (self.exec, other.exec) {
                (None, x) | (x, None) => x,
                (Some(a), Some(b)) => Some(join_exec(a, b)),
            },
            fs:     self.fs.join(other.fs),
            net:    self.net.join(other.net),
            editor: self.editor.join(other.editor),
            shell:  self.shell.join(other.shell),
            audit:  self.audit || other.audit,
        }
    }
}

/// True if `key` is a subpath-style exec map key — an absolute path
/// ending in `/` (or a sigil that resolves to such a path).  Bare
/// command names and absolute literal paths return false.
pub fn is_subpath_key(key: &str) -> bool {
    key.ends_with('/')
}

/// Freeze sigils in exec map keys.  Both subpath keys (`xdg:bin/`,
/// `~/.cargo/bin/`, `cwd:/`) and literal-path keys may carry sigils;
/// the trailing `/` survives expansion.  Bare command names
/// (`git`, `kubectl`) are passed through unchanged — they're not paths.
fn freeze_exec_map(
    map: BTreeMap<String, ExecPolicy>,
    ctx: &crate::path::sigil::FreezeCtx<'_>,
) -> Result<BTreeMap<String, ExecPolicy>, String> {
    use crate::path::sigil::{freeze_path_list, looks_like_path_or_sigil};
    let mut out = BTreeMap::new();
    for (key, policy) in map {
        if looks_like_path_or_sigil(&key) {
            let trailing_slash = key.ends_with('/');
            let mut singleton = vec![key.trim_end_matches('/').to_string()];
            freeze_path_list(&mut singleton, ctx)?;
            let mut frozen = singleton.into_iter().next().unwrap();
            if trailing_slash && !frozen.ends_with('/') {
                frozen.push('/');
            }
            out.insert(frozen, policy);
        } else {
            out.insert(key, policy);
        }
    }
    Ok(out)
}

// ── Lattice impls ─────────────────────────────────────────────────────────
//
// One impl per lattice type, both Meet and Join.  Map-level meets/joins
// (over the unified exec map) live below as free fns because they need
// the partition-by-shape that the per-element trait can't see.

impl Meet for ExecPolicy {
    fn meet(self, other: Self) -> Self {
        match (self, other) {
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Allow, Self::Allow) => Self::Allow,
            (Self::Allow, Self::Subcommands(s)) | (Self::Subcommands(s), Self::Allow) => {
                Self::Subcommands(unique_strings(s))
            }
            (Self::Subcommands(s1), Self::Subcommands(s2)) => {
                Self::Subcommands(unique_strings(s1.into_iter().filter(|x| s2.contains(x))))
            }
        }
    }
}

impl Join for ExecPolicy {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Allow, _) | (_, Self::Allow) => Self::Allow,
            (Self::Deny, p) | (p, Self::Deny) => p,
            (Self::Subcommands(s1), Self::Subcommands(s2)) => {
                Self::Subcommands(unique_strings(s1.into_iter().chain(s2)))
            }
        }
    }
}

impl Meet for FsPolicy {
    fn meet(self, other: Self) -> Self {
        Self {
            read_prefixes: intersect_prefix_strings(&self.read_prefixes, &other.read_prefixes),
            write_prefixes: intersect_prefix_strings(&self.write_prefixes, &other.write_prefixes),
            deny_paths: unique_strings(self.deny_paths.into_iter().chain(other.deny_paths)),
        }
    }
}

impl Join for FsPolicy {
    fn join(self, other: Self) -> Self {
        Self {
            read_prefixes: union_prefix_strings(self.read_prefixes, other.read_prefixes),
            write_prefixes: union_prefix_strings(self.write_prefixes, other.write_prefixes),
            deny_paths: self.deny_paths.into_iter().filter(|p| other.deny_paths.contains(p)).collect(),
        }
    }
}

impl Meet for EditorPolicy {
    fn meet(self, other: Self) -> Self {
        Self {
            read: self.read.meet(other.read),
            write: self.write.meet(other.write),
            tui: self.tui.meet(other.tui),
        }
    }
}

impl Join for EditorPolicy {
    fn join(self, other: Self) -> Self {
        Self {
            read: self.read.join(other.read),
            write: self.write.join(other.write),
            tui: self.tui.join(other.tui),
        }
    }
}

impl Meet for ShellPolicy {
    fn meet(self, other: Self) -> Self {
        Self { chdir: self.chdir.meet(other.chdir) }
    }
}

impl Join for ShellPolicy {
    fn join(self, other: Self) -> Self {
        Self { chdir: self.chdir.join(other.chdir) }
    }
}

/// Meet two unified exec maps.  Literal halves (bare names, absolute
/// literal paths) follow per-key meet — Allow keys must be on both
/// sides, `Deny` is sticky.  Subpath halves follow prefix meet —
/// deeper prefix wins on overlap.
///
/// Not a `Meet for BTreeMap<…>` blanket impl because the "absent key"
/// rule (Deny propagation) is exec-specific; other map uses of meet
/// would want different behaviour.
fn meet_exec(
    a: BTreeMap<String, ExecPolicy>,
    b: BTreeMap<String, ExecPolicy>,
) -> BTreeMap<String, ExecPolicy> {
    let (a_lit, a_sub) = partition_exec(a);
    let (b_lit, b_sub) = partition_exec(b);
    let mut out = meet_literal_exec(a_lit, b_lit);
    let a_paths: Vec<String> = a_sub.keys().cloned().collect();
    let b_paths: Vec<String> = b_sub.keys().cloned().collect();
    for path in intersect_prefix_strings(&a_paths, &b_paths) {
        out.insert(path, ExecPolicy::Allow);
    }
    out
}

/// Join over the unified exec map — symmetric counterpart to `meet_exec`.
fn join_exec(
    a: BTreeMap<String, ExecPolicy>,
    b: BTreeMap<String, ExecPolicy>,
) -> BTreeMap<String, ExecPolicy> {
    let (a_lit, a_sub) = partition_exec(a);
    let (b_lit, b_sub) = partition_exec(b);
    let mut out = join_literal_exec(a_lit, b_lit);
    let a_paths: Vec<String> = a_sub.keys().cloned().collect();
    let b_paths: Vec<String> = b_sub.keys().cloned().collect();
    for path in union_prefix_strings(a_paths, b_paths) {
        out.insert(path, ExecPolicy::Allow);
    }
    out
}

fn partition_exec(
    map: BTreeMap<String, ExecPolicy>,
) -> (BTreeMap<String, ExecPolicy>, BTreeMap<String, ExecPolicy>) {
    let mut literal = BTreeMap::new();
    let mut subpath = BTreeMap::new();
    for (k, v) in map {
        if is_subpath_key(&k) {
            subpath.insert(k, v);
        } else {
            literal.insert(k, v);
        }
    }
    (literal, subpath)
}

/// Per-name meet over the literal half of an exec map.  Allow-sided
/// keys must appear on both sides (uses `ExecPolicy::meet`); `Deny`
/// propagates from either side even when absent on the other.
///
/// Exposed crate-wide so projection-time reduction (which has already
/// partitioned subpath keys) doesn't have to re-implement it.
pub(crate) fn meet_literal_exec(
    a: BTreeMap<String, ExecPolicy>,
    b: BTreeMap<String, ExecPolicy>,
) -> BTreeMap<String, ExecPolicy> {
    let mut out = BTreeMap::new();
    for (name, pa) in &a {
        match b.get(name) {
            Some(pb) => { out.insert(name.clone(), pa.clone().meet(pb.clone())); }
            None if matches!(pa, ExecPolicy::Deny) => { out.insert(name.clone(), ExecPolicy::Deny); }
            None => {}
        }
    }
    for (name, pb) in &b {
        if a.contains_key(name) { continue; }
        if matches!(pb, ExecPolicy::Deny) {
            out.insert(name.clone(), ExecPolicy::Deny);
        }
    }
    out
}

fn join_literal_exec(
    a: BTreeMap<String, ExecPolicy>,
    b: BTreeMap<String, ExecPolicy>,
) -> BTreeMap<String, ExecPolicy> {
    let mut out = BTreeMap::new();
    for (name, pa) in &a {
        match b.get(name) {
            Some(pb) => { out.insert(name.clone(), pa.clone().join(pb.clone())); }
            None if !matches!(pa, ExecPolicy::Deny) => { out.insert(name.clone(), pa.clone()); }
            None => {}
        }
    }
    for (name, pb) in &b {
        if a.contains_key(name) { continue; }
        if !matches!(pb, ExecPolicy::Deny) {
            out.insert(name.clone(), pb.clone());
        }
    }
    out
}

/// Prefix-set intersection: keep the deeper prefix from each
/// overlapping pair.  Lexical-only (no symlink resolution); the
/// runtime fold in `capability::prefix::intersect_grant_paths` does
/// symlink-aware overlap when reducing the dynamic stack at
/// sandbox-render time.
fn intersect_prefix_strings(a: &[String], b: &[String]) -> Vec<String> {
    fn within(p: &str, q: &str) -> bool {
        crate::path::path_within(std::path::Path::new(p), std::path::Path::new(q))
    }
    let mut out: Vec<String> = Vec::new();
    for p in a {
        if b.iter().any(|q| within(p, q)) { out.push(p.clone()); }
    }
    for q in b {
        if a.iter().any(|p| within(q, p)) { out.push(q.clone()); }
    }
    unique_strings(out)
}

fn union_prefix_strings(a: Vec<String>, b: Vec<String>) -> Vec<String> {
    unique_strings(a.into_iter().chain(b))
}

// ── Lattice tests ─────────────────────────────────────────────────────────
#[cfg(test)]
mod lattice_tests {
    use super::*;

    // ── Generic lifts: Option<T> and bool ────────────────────────────────

    #[test]
    fn option_meet_treats_none_as_top() {
        assert_eq!(Some(true).meet(None), Some(true));
        assert_eq!(None::<bool>.meet(Some(false)), Some(false));
        assert_eq!(None::<bool>.meet(None), None);
    }

    #[test]
    fn option_join_treats_none_as_identity() {
        assert_eq!(Some(false).join(None), Some(false));
        assert_eq!(None::<bool>.join(Some(true)), Some(true));
    }

    #[test]
    fn bool_meet_is_and() {
        assert!(!true.meet(false));
        assert!(true.meet(true));
        assert!(!false.meet(false));
    }

    #[test]
    fn bool_join_is_or() {
        assert!(true.join(false));
        assert!(true.join(true));
        assert!(!false.join(false));
    }

    // ── RawCapabilities lattice properties ───────────────────────────────

    fn witness_a() -> RawCapabilities {
        RawCapabilities {
            exec: Some(BTreeMap::from([
                ("cargo".into(), ExecPolicy::Allow),
                ("git".into(), ExecPolicy::Subcommands(vec!["log".into(), "status".into()])),
                ("/usr/bin/".into(), ExecPolicy::Allow),
            ])),
            fs: Some(FsPolicy {
                read_prefixes: vec!["/tmp".into()],
                write_prefixes: vec!["/tmp".into()],
                deny_paths: vec!["/tmp/secret".into()],
            }),
            net: Some(true),
            audit: false,
            editor: Some(EditorPolicy { read: true, write: true, tui: false }),
            shell: Some(ShellPolicy { chdir: true }),
        }
    }

    fn witness_b() -> RawCapabilities {
        RawCapabilities {
            exec: Some(BTreeMap::from([
                ("cargo".into(), ExecPolicy::Subcommands(vec!["build".into()])),
                ("ls".into(), ExecPolicy::Allow),
                ("/usr/bin/".into(), ExecPolicy::Allow),
                ("/usr/local/bin/".into(), ExecPolicy::Allow),
            ])),
            fs: Some(FsPolicy {
                read_prefixes: vec!["/tmp/work".into()],
                write_prefixes: vec!["/tmp/work".into()],
                deny_paths: vec!["/tmp/work/.exarch.toml".into()],
            }),
            net: Some(false),
            audit: false,
            editor: Some(EditorPolicy { read: true, write: false, tui: true }),
            shell: Some(ShellPolicy { chdir: false }),
        }
    }

    fn witness_c() -> RawCapabilities {
        RawCapabilities {
            exec: Some(BTreeMap::from([("cargo".into(), ExecPolicy::Allow)])),
            fs: Some(FsPolicy {
                read_prefixes: vec!["/tmp".into()],
                write_prefixes: Vec::new(),
                deny_paths: Vec::new(),
            }),
            net: None,
            audit: false,
            editor: None,
            shell: None,
        }
    }

    #[test]
    fn meet_commutative() {
        let a = witness_a();
        let b = witness_b();
        assert_eq!(a.clone().meet(b.clone()), b.meet(a));
    }

    #[test]
    fn meet_associative() {
        let a = witness_a();
        let b = witness_b();
        let c = witness_c();
        assert_eq!(
            a.clone().meet(b.clone().meet(c.clone())),
            a.meet(b).meet(c),
        );
    }

    #[test]
    fn meet_idempotent() {
        let a = witness_a();
        assert_eq!(a.clone().meet(a.clone()), a);
    }

    #[test]
    fn meet_top_is_identity() {
        let a = witness_a();
        assert_eq!(a.clone().meet(RawCapabilities::default()), a.clone());
        assert_eq!(RawCapabilities::default().meet(a.clone()), a);
    }

    #[test]
    fn meet_bottom_zeroes_authority() {
        let a = witness_a();
        let m = a.meet(RawCapabilities::deny_all());
        assert!(m.exec.expect("exec retained").is_empty());
        let fs = m.fs.expect("fs retained");
        assert!(fs.read_prefixes.is_empty());
        assert!(fs.write_prefixes.is_empty());
        assert_eq!(m.net, Some(false));
        let ed = m.editor.expect("editor retained");
        assert!(!ed.read && !ed.write && !ed.tui);
        assert!(!m.shell.expect("shell retained").chdir);
    }

    #[test]
    fn meet_exec_intersects_and_meets_policies() {
        let m = witness_a().meet(witness_b());
        let exec = m.exec.unwrap();
        // Literal half: cargo is shared (Subcommands meet), git/ls are
        // one-sided so drop.  Subpath half: /usr/bin/ is shared,
        // /usr/local/bin/ is one-sided so drops.
        assert!(exec.contains_key("cargo"));
        match exec.get("cargo").unwrap() {
            ExecPolicy::Subcommands(s) => assert_eq!(s, &vec!["build".to_string()]),
            other => panic!("unexpected: {other:?}"),
        }
        assert!(!exec.contains_key("git"));
        assert!(!exec.contains_key("ls"));
        assert!(exec.contains_key("/usr/bin/"));
        assert!(!exec.contains_key("/usr/local/bin/"));
    }

    /// `Deny` is sticky downward: a base ceiling that vetos `bash`
    /// must keep that veto after meet with a restrict file that
    /// does not name `bash` at all.  Without this the `reasonable`
    /// base would lose its shell deny the moment any `[exec]`-bearing
    /// restrict came in.
    #[test]
    fn meet_exec_preserves_one_sided_deny() {
        let base = RawCapabilities {
            exec: Some(BTreeMap::from([
                ("ls".into(),   ExecPolicy::Allow),
                ("bash".into(), ExecPolicy::Deny),
            ])),
            ..Default::default()
        };
        let restrict = RawCapabilities {
            exec: Some(BTreeMap::from([("ls".into(), ExecPolicy::Allow)])),
            ..Default::default()
        };
        let m = base.meet(restrict);
        let exec = m.exec.unwrap();
        assert_eq!(exec.get("ls"),   Some(&ExecPolicy::Allow));
        assert_eq!(exec.get("bash"), Some(&ExecPolicy::Deny));
    }

    /// `Deny` only survives join when both sides agree.  An
    /// extend-base that re-grants `bash` must be able to lift the
    /// ceiling's veto.
    #[test]
    fn join_exec_drops_one_sided_deny() {
        let base = RawCapabilities {
            exec: Some(BTreeMap::from([("bash".into(), ExecPolicy::Deny)])),
            ..Default::default()
        };
        let extend = RawCapabilities {
            exec: Some(BTreeMap::from([("bash".into(), ExecPolicy::Allow)])),
            ..Default::default()
        };
        let j = base.join(extend);
        assert_eq!(j.exec.unwrap().get("bash"), Some(&ExecPolicy::Allow));
    }

    /// IPC roundtrip: a frozen `Capabilities` survives a JSON
    /// trip through the wire format unchanged.  Untrusted external
    /// input would round-trip as `RawCapabilities`; this is the
    /// trusted-IPC path where the parent has already frozen.
    #[test]
    fn ipc_roundtrip_preserves_frozen_capabilities() {
        let c = witness_a().freeze(&test_ctx("/h")).expect("freeze");
        let json = serde_json::to_string(&c).unwrap();
        let back: Capabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn meet_fs_unions_denies_and_intersects_prefixes() {
        let m = witness_a().meet(witness_b());
        let fs = m.fs.unwrap();
        assert!(fs.read_prefixes.iter().any(|p| p == "/tmp/work"));
        assert!(fs.deny_paths.iter().any(|p| p == "/tmp/secret"));
        assert!(fs.deny_paths.iter().any(|p| p == "/tmp/work/.exarch.toml"));
    }

    #[test]
    fn join_commutative() {
        let a = witness_a();
        let b = witness_b();
        assert_eq!(a.clone().join(b.clone()), b.join(a));
    }

    #[test]
    fn join_associative() {
        let a = witness_a();
        let b = witness_b();
        let c = witness_c();
        assert_eq!(
            a.clone().join(b.clone().join(c.clone())),
            a.join(b).join(c),
        );
    }

    #[test]
    fn join_idempotent() {
        let a = witness_a();
        assert_eq!(a.clone().join(a.clone()), a);
    }

    #[test]
    fn join_none_is_identity() {
        let a = witness_a();
        assert_eq!(a.clone().join(RawCapabilities::default()), a.clone());
        assert_eq!(RawCapabilities::default().join(a.clone()), a);
    }

    #[test]
    fn join_exec_widens_policies_and_unions_names() {
        let m = witness_a().join(witness_b());
        let exec = m.exec.unwrap();
        assert_eq!(exec.get("cargo"), Some(&ExecPolicy::Allow));
        assert_eq!(exec.get("ls"), Some(&ExecPolicy::Allow));
        match exec.get("git").unwrap() {
            ExecPolicy::Subcommands(s) => {
                assert!(s.contains(&"log".into()) && s.contains(&"status".into()));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn join_fs_unions_prefixes_and_intersects_denies() {
        let m = witness_a().join(witness_b());
        let fs = m.fs.unwrap();
        assert!(fs.read_prefixes.iter().any(|p| p == "/tmp"));
        assert!(fs.read_prefixes.iter().any(|p| p == "/tmp/work"));
        assert!(fs.deny_paths.is_empty());
    }

    /// `validate_paths` accepts known `xdg:` tokens (with and
    /// without a sub-path), tilde and absolute paths, and lets
    /// non-`xdg:` strings pass without inspection.
    #[test]
    fn validate_paths_accepts_known_tokens() {
        let caps = RawCapabilities {
            exec: Some(BTreeMap::from([
                ("xdg:bin/".into(), ExecPolicy::Allow),
                ("/usr/bin/".into(), ExecPolicy::Allow),
            ])),
            fs: Some(FsPolicy {
                read_prefixes: vec![
                    "xdg:config".into(),
                    "xdg:data/agda".into(),
                    "~/.cache".into(),
                    "/etc".into(),
                ],
                write_prefixes: vec!["xdg:cache".into()],
                deny_paths: vec!["xdg:config/secret".into()],
            }),
            ..Default::default()
        };
        caps.validate_paths().expect("known tokens should validate");
    }

    /// A typo in the `xdg:` namespace is caught at the boundary
    /// instead of silently passing through to match nothing at
    /// runtime.  Mirrors the `deny_unknown_fields` ethos.
    #[test]
    fn validate_paths_rejects_typo() {
        let caps = RawCapabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec!["xdg:cofnig".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = caps.validate_paths().unwrap_err();
        assert!(err.contains("xdg:cofnig"), "got {err}");
        assert!(err.contains("config"), "should list known kinds: {err}");
    }

    /// `freeze` rewrites every sigil into a concrete absolute
    /// path: after the call the resulting `Capabilities` carries
    /// no sigils, so subsequent matching is decoupled from any
    /// later env mutation.
    #[test]
    fn freeze_rewrites_sigils_to_concrete_paths() {
        let raw = RawCapabilities {
            exec: Some(BTreeMap::from([
                ("xdg:bin/".into(), ExecPolicy::Allow),
                ("/usr/bin/".into(), ExecPolicy::Allow),
            ])),
            fs: Some(FsPolicy {
                read_prefixes: vec!["~/notes".into(), "/etc".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let prev = std::env::var_os("XDG_BIN_HOME");
        // Safety: tests in this module mutate process env serially.
        // Restore afterwards.
        unsafe { std::env::remove_var("XDG_BIN_HOME") };
        let caps = raw.freeze(&test_ctx("/h")).expect("known sigils freeze");
        if let Some(v) = prev {
            unsafe { std::env::set_var("XDG_BIN_HOME", v) };
        }
        // Subpath keys preserve the trailing `/` after sigil expansion,
        // so the in-ral matcher still recognises them as subpaths.
        let exec = caps.exec.unwrap();
        assert!(exec.contains_key("/h/.local/bin/"));
        assert!(exec.contains_key("/usr/bin/"));
        let reads = caps.fs.unwrap().read_prefixes;
        assert_eq!(reads[0], "/h/notes");
        assert_eq!(reads[1], "/etc");
    }

    /// Defence in depth: a caller who sets `XDG_DATA_HOME=/etc`
    /// must not be able to widen a policy that names `xdg:data`.
    /// `freeze` rejects the resolution at the boundary with a
    /// message naming the offending env var so the operator can
    /// diagnose it.
    #[test]
    fn freeze_rejects_xdg_var_outside_home() {
        let raw = RawCapabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec!["xdg:data".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let prev = std::env::var_os("XDG_DATA_HOME");
        unsafe { std::env::set_var("XDG_DATA_HOME", "/etc") };
        let err = raw.freeze(&test_ctx("/h")).unwrap_err();
        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
        assert!(err.contains("XDG_DATA_HOME"), "should name the env var: {err}");
        assert!(err.contains("/etc"), "should show the bad value: {err}");
        assert!(err.contains("HOME"), "should mention HOME: {err}");
    }

    /// Empty `home` is a configuration error, not a silent allow.
    /// The check produces a question-shaped message — per the
    /// `ral` style, we prefer prompting over guessing.
    #[test]
    fn freeze_errors_when_home_is_empty() {
        let raw = RawCapabilities {
            fs: Some(FsPolicy {
                read_prefixes: vec!["~/x".into()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let err = raw.freeze(&test_ctx("")).unwrap_err();
        assert!(err.contains("HOME"), "got {err}");
    }

    fn test_ctx(home: &str) -> crate::path::sigil::FreezeCtx<'_> {
        crate::path::sigil::FreezeCtx {
            home,
            cwd: std::path::Path::new("/"),
        }
    }
}
