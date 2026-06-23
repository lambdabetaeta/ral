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
//! - [`ExecMap`]: exec authority partitioned into `literals` (keyed by
//!   command name or absolute path, carrying [`ExecPolicy`]) and `dirs`
//!   (absolute directory prefixes, carrying [`ExecDir`]).
//! - [`FsPolicy`]: read/write prefixes and explicit `deny_paths` for
//!   single files.
//! - [`EditorPolicy`] / [`ShellPolicy`]: bit flags gating REPL-side builtins.
//! - `net`: tristate (None=inherit, Some(false)=deny, Some(true)=allow).
//! - `audit`: orthogonal flag — propagated upward by `meet` (logical OR),
//!   not part of the lattice.
//!
//! ## `SandboxProjection`
//!
//! [`SandboxProjection`] is the meet-folded effective fs+net+exec policy
//! used to render the OS sandbox profile and ferry policy across the IPC
//! boundary.  Produced by `capability::sandbox_projection`, which folds
//! the whole stack; consumed only by sandbox backends.

use crate::path::NormalizedPrefix;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

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

/// Widen a base ceiling with an extension at load time, before any
/// attenuation runs (`base.join(extension)`).  Adds the extension's
/// positive authority but preserves every veto — `Deny`s and
/// `deny_paths` are sticky on both sides — so this is a widening
/// join-semilattice, not the strict order-dual of [`Meet`]: a deny
/// survives any composition, and only an explicit same-key re-grant
/// lifts one.
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

/// Exec verdict for a single literal key in an [`ExecMap`].
///
/// Forms a three-point lattice with `Allow` at top, `Subcommands(_)`
/// in the middle (more elements = more authority), and `Deny` at
/// bottom.  An explicit `Deny` is a sticky veto: it survives both meet
/// and join against absence in another layer's map (so a base ceiling
/// can pin a command name out without restrict *or* extend-base files
/// having to repeat it) and beats subpath admission elsewhere in the
/// same map.  No composition lifts it — deny-overrides holds under both
/// meet and join — so to permit a denied name you choose a base that
/// does not deny it, rather than re-granting it from an overlay.
///
/// Borne by [`ExecMap::literals`], whose keys are bare command names
/// (`git`) or absolute literal paths (`/usr/bin/git`).  Directory
/// prefixes live in [`ExecMap::dirs`] under the two-valued [`ExecDir`].
/// Literal keys beat dir prefixes, and the deepest dir prefix wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecPolicy {
    /// Allow the command with any arguments.
    Allow,
    /// Allow only when the first argument is in this set.  A
    /// `BTreeSet<String>` is sorted and deduped by construction, so
    /// `Eq`, `meet`, and `join` all agree and the idempotence law holds
    /// without any explicit canonicalization.
    Subcommands(BTreeSet<String>),
    /// Reject the command outright, even if a covering directory
    /// prefix would admit the resolved path.  Lattice bottom.
    Deny,
}

/// A directory prefix's exec verdict — two-valued: a directory admits
/// or denies binaries resolving inside it but cannot name subcommands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecDir {
    Allow,
    Deny,
}

/// Exec authority, partitioned by key kind at the type level.
///
/// `literals` keys are bare command names and absolute literal paths,
/// carrying the full three-valued [`ExecPolicy`].  `dirs` keys are
/// absolute directory prefixes (stored without a trailing slash),
/// carrying the two-valued [`ExecDir`].  Literal keys beat dir
/// prefixes when both cover a candidate, and the deepest dir prefix
/// wins among the dirs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecMap {
    #[serde(default)]
    pub literals: BTreeMap<String, ExecPolicy>,
    #[serde(default)]
    pub dirs: BTreeMap<String, ExecDir>,
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
    pub read_prefixes: Vec<NormalizedPrefix>,
    #[serde(default)]
    pub write_prefixes: Vec<NormalizedPrefix>,
    #[serde(default)]
    pub deny_paths: Vec<NormalizedPrefix>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", content = "policy", rename_all = "snake_case")]
pub enum FsProjection {
    #[default]
    Unrestricted,
    Restricted(FsPolicy),
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
/// `allow_dirs` subpath roots) plus explicit `deny_paths` and
/// `deny_dirs` carved out of those admits.  Anything
/// outside admits is denied at the OS layer too, closing the
/// `sh -c "PATH=…; cmd"` route by which a sandboxed child re-execs
/// binaries the in-ral gate never sees.
///
/// Empty `Restricted { allow_paths: [], allow_dirs: [], deny_paths: [], deny_dirs: [] }`
/// means a layer opted in to exec restriction and admitted nothing —
/// the OS profile emits no `(allow process-exec …)` rule and the
/// deny-default kills any spawn from inside the grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecProjection {
    #[default]
    Unrestricted,
    Restricted {
        allow_paths: Vec<String>,
        allow_dirs: Vec<String>,
        #[serde(default)]
        deny_paths: Vec<String>,
        #[serde(default)]
        deny_dirs: Vec<String>,
    },
}

/// The OS-renderable projection of the effective capability grant.
///
/// Produced by `capability::sandbox_projection` after meet-folding the
/// dynamic stack; consumed by the platform sandbox backends
/// (`sandbox::linux`, `sandbox::macos`) and ferried across the IPC
/// boundary in the internal `--sandbox-projection` flag.
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

impl SandboxProjection {
    /// Lexical-form bind spec for the OS profile renderer.  Returns an
    /// empty spec when fs is `Unrestricted` — the renderer should not
    /// emit per-prefix rules in that case (it emits broad allows).
    pub fn bind_spec(&self) -> SandboxBindSpec {
        let Some(fs) = self.fs.as_policy() else {
            return SandboxBindSpec::default();
        };
        let dedup = |ps: &[NormalizedPrefix]| {
            ps.iter()
                .map(|p| p.as_str().to_string())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        SandboxBindSpec {
            read_prefixes: dedup(&fs.read_prefixes),
            write_prefixes: dedup(&fs.write_prefixes),
            deny_paths: dedup(&fs.deny_paths),
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

/// One layer of the dynamic capabilities stack — per-effect policies
/// plus an `audit` flag, every `~` / `xdg:` / `cwd:` / `tempdir:` sigil
/// already resolved to a concrete path.
///
/// The sole non-trivial constructor is
/// [`crate::capability::decode_capability_map`], which walks a `grant`
/// (or `--capabilities` profile) `Value::Map` and resolves every sigil
/// against a `FreezeCtx { home, cwd }` before returning — so a
/// `Capabilities` is resolved *by construction*, with no syntactic stage
/// to mishandle.  The path-free [`Capabilities::root`] /
/// [`Capabilities::deny_all`] / [`Capabilities::default`] are the only
/// other ways in.  The lattice [`Capabilities::meet`] /
/// [`Capabilities::join`] compose two resolved bundles into a third.
///
/// `Serialize`/`Deserialize` back the wire mirror in `subprocess.rs` —
/// the grants stack rides `WireContext` across the re-exec'd-child
/// protocol, a trusted boundary between cooperating ral processes,
/// where the parent has already resolved every path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Exec authority, partitioned into `literals` and `dirs`.  See
    /// [`ExecMap`].
    #[serde(default)]
    pub exec: Option<ExecMap>,
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

/// The dynamic stack of capability layers.
///
/// Conceptually a `Vec<Capabilities>` with the ambient root at index 0
/// and the innermost `grant { ... }` layer at the top.  Reified as a
/// newtype so the operations that fold the stack — restrictiveness,
/// audit emission opt-in, push/pop framing — live next to each other
/// instead of being reconstructed from `iter().any(...)` at each call
/// site.
///
/// Wire-transparent (`#[serde(transparent)]`): the on-disk and IPC
/// forms are identical to the bare `Vec<Capabilities>` it replaces, so
/// the wrapper is free at every boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GrantStack(Vec<Capabilities>);

impl GrantStack {
    /// Root stack — a single ambient `Capabilities::root()` layer.  Every
    /// shell starts here; `grant { ... }` blocks and the session-wide
    /// `--capabilities` ceiling push attenuating layers on top.
    pub fn root() -> Self {
        Self(vec![Capabilities::root()])
    }

    /// True iff any layer attenuates authority — i.e., a real grant
    /// sits above the ambient root.  The signal `has_active_capabilities`
    /// exposes on `Shell`.
    pub fn is_restrictive(&self) -> bool {
        self.0.iter().any(Capabilities::is_restrictive)
    }

    /// True iff any layer opts into capability-check audit emission
    /// (SPEC §11.4–11.5).  Pair with `Audit::active()` for the final
    /// gate.
    pub fn any_audits(&self) -> bool {
        self.0.iter().any(|ctx| ctx.audit)
    }

    pub fn push(&mut self, layer: Capabilities) {
        self.0.push(layer);
    }

    pub fn pop(&mut self) -> Option<Capabilities> {
        self.0.pop()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Capabilities> {
        self.0.iter()
    }

    /// The exec map of each layer that constrains exec, in stack order.
    /// Layers with no exec opinion are skipped; the capability folds
    /// intersect the rest.
    pub fn exec(&self) -> impl Iterator<Item = &ExecMap> {
        self.0.iter().filter_map(|c| c.exec.as_ref())
    }

    /// The fs policy of each layer that constrains fs, in stack order.
    pub fn fs(&self) -> impl Iterator<Item = &FsPolicy> {
        self.0.iter().filter_map(|c| c.fs.as_ref())
    }

    /// The net verdict of each layer that constrains net, in stack order.
    pub fn net(&self) -> impl Iterator<Item = bool> {
        self.0.iter().filter_map(|c| c.net)
    }
}

impl<'a> IntoIterator for &'a GrantStack {
    type Item = &'a Capabilities;
    type IntoIter = std::slice::Iter<'a, Capabilities>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Capabilities {
    /// Lattice bottom for positive authority: every effect explicitly
    /// pinned to its most-restrictive value (empty exec/fs, net off,
    /// editor/shell flags clear), so a `meet` against it zeroes
    /// authority along every dimension.
    pub fn deny_all() -> Self {
        Self {
            exec: Some(ExecMap::default()),
            fs: Some(FsPolicy::default()),
            net: Some(false),
            editor: Some(EditorPolicy::default()),
            shell: Some(ShellPolicy::default()),
            audit: false,
        }
    }

    /// True iff this layer attenuates authority along any dimension —
    /// i.e. some effect is `Some(_)` rather than the inheriting `None`.
    pub fn is_restrictive(&self) -> bool {
        self.exec.is_some()
            || self.fs.is_some()
            || self.net.is_some()
            || self.editor.is_some()
            || self.shell.is_some()
    }

    /// Ambient authority — the root of every capabilities stack.
    /// All fields `None`: no attenuation.  Trivially frozen
    /// (no paths).
    pub fn root() -> Self {
        Self::default()
    }

    /// True when these capabilities, applied as a session frame over the
    /// ambient root, engage the OS sandbox — i.e. they impose fs or net
    /// restrictions an external process must be confined to.  Mirrors the
    /// grant stack [`crate::types::Shell::new`] installs (root, then this
    /// frame), so a host can decide whether to stand up sandbox
    /// machinery without constructing a whole `Shell` to probe the
    /// projection.
    pub fn engages_sandbox(&self) -> bool {
        let context = crate::types::Context {
            grants: {
                let mut grants = GrantStack::root();
                grants.push(self.clone());
                grants
            },
            ..crate::types::Context::default()
        };
        crate::capability::sandbox_projection(&context).is_some()
    }
}

impl Capabilities {
    /// Lattice meet — the most-authority capability below both
    /// `self` and `other`.  `Capabilities::default()` is top,
    /// [`Capabilities::deny_all`] is bottom; `meet` is commutative,
    /// associative, idempotent.
    ///
    /// Each `Option<_>` field treats `None` as ⊤, so
    /// `meet(None, x) = x`.  Inner fields intersect (exec maps,
    /// fs prefixes), AND (net, editor, shell), and union
    /// (`fs.deny_paths` — more denies = less authority).
    /// `audit` is not part of the lattice: it propagates upward
    /// (logical OR).  Both bundles are already resolved, so the
    /// prefix intersections compare concrete paths.
    pub fn meet(self, other: Self) -> Self {
        // Per-field meets via the lattice trait (Option<T>: Meet does
        // the None-as-identity lift; ExecMap, bool, FsPolicy,
        // EditorPolicy, ShellPolicy each impl Meet directly).
        Self {
            exec: self.exec.meet(other.exec),
            fs: self.fs.meet(other.fs),
            net: self.net.meet(other.net),
            editor: self.editor.meet(other.editor),
            shell: self.shell.meet(other.shell),
            audit: self.audit || other.audit,
        }
    }

    /// Widen `self` with `other` — the composition `--extend-base`
    /// runs to lift a base ceiling before any attenuation.
    /// Commutative, associative, idempotent.
    ///
    /// `None` on a field acts as the join identity.  Positive authority
    /// widens — exec allows and fs prefixes union, net/editor/shell OR
    /// — while every veto is preserved: `fs.deny_paths` and exec `Deny`s
    /// survive (deny-overrides, the same conflict rule as `meet`), so an
    /// extension can add authority where the base was silent but can
    /// never re-admit a denied region.  This is not the order-dual of
    /// [`meet`](Self::meet): a deny is a floor under both, never lifted
    /// by composition — only by choosing a different base.
    pub fn join(self, other: Self) -> Self {
        // Per-field joins via the lattice trait — symmetric to `meet`.
        Self {
            exec: self.exec.join(other.exec),
            fs: self.fs.join(other.fs),
            net: self.net.join(other.net),
            editor: self.editor.join(other.editor),
            shell: self.shell.join(other.shell),
            audit: self.audit || other.audit,
        }
    }
}

// ── Lattice impls ─────────────────────────────────────────────────────────
//
// One impl per lattice type, both Meet and Join.  Map-level meets/joins
// (over the unified exec map) live below as free fns because they need
// the partition-by-shape that the per-element trait can't see.

impl ExecPolicy {
    /// The legend label for an admitted command: `name` under `Allow`,
    /// `name[sub1,sub2,…]` under `Subcommands` (the set iterates sorted,
    /// so the label is deterministic), `None` under `Deny`.
    pub fn admit_label(&self, name: &str) -> Option<String> {
        match self {
            Self::Allow => Some(name.to_string()),
            Self::Subcommands(subs) => Some(format!(
                "{name}[{}]",
                subs.iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            Self::Deny => None,
        }
    }

    /// True iff this verdict is the lattice bottom `Deny`.
    pub fn is_denied(&self) -> bool {
        matches!(self, ExecPolicy::Deny)
    }
}

impl Meet for ExecPolicy {
    fn meet(self, other: Self) -> Self {
        match (self, other) {
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Allow, Self::Allow) => Self::Allow,
            (Self::Allow, Self::Subcommands(s)) | (Self::Subcommands(s), Self::Allow) => {
                Self::Subcommands(s)
            }
            (Self::Subcommands(s1), Self::Subcommands(s2)) => Self::Subcommands(&s1 & &s2),
        }
    }
}

impl Join for ExecPolicy {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            // Deny-overrides: a veto wins under widening exactly as it
            // does under `meet`, so `--extend-base` can never re-admit a
            // command the base denies.  Allows still widen (subcommand
            // sets union) where neither side vetoes.
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Allow, _) | (_, Self::Allow) => Self::Allow,
            (Self::Subcommands(s1), Self::Subcommands(s2)) => Self::Subcommands(&s1 | &s2),
        }
    }
}

impl Meet for FsPolicy {
    fn meet(self, other: Self) -> Self {
        Self {
            read_prefixes: intersect_prefixes(&self.read_prefixes, &other.read_prefixes),
            write_prefixes: intersect_prefixes(&self.write_prefixes, &other.write_prefixes),
            deny_paths: self
                .deny_paths
                .into_iter()
                .chain(other.deny_paths)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        }
    }
}

impl Join for FsPolicy {
    fn join(self, other: Self) -> Self {
        Self {
            read_prefixes: union_prefixes(self.read_prefixes, other.read_prefixes),
            write_prefixes: union_prefixes(self.write_prefixes, other.write_prefixes),
            // Denies union, exactly as in `meet`: a `deny_path` is a
            // sticky veto, not erodable authority.  An `--extend-base`
            // overlay that is silent on a base carve-out must not lift
            // it — so widening preserves every deny from either side.
            deny_paths: union_prefixes(self.deny_paths, other.deny_paths),
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
        Self {
            chdir: self.chdir.meet(other.chdir),
        }
    }
}

impl Join for ShellPolicy {
    fn join(self, other: Self) -> Self {
        Self {
            chdir: self.chdir.join(other.chdir),
        }
    }
}

/// Meet two exec maps.  Both halves split each verdict by sign and
/// recombine per the [`ExecDir`] lattice.  In `dirs`: allow-regions
/// intersect (a prefix survives only where BOTH sides admit it, the
/// deeper one winning), deny-regions union (a `Deny` is sticky, so it
/// propagates from either side), and on an exact-key clash the deny
/// lands last so meet's bottom — `Deny` — wins.  The `literals` half
/// mirrors this through [`meet_literal_exec`].
impl Meet for ExecMap {
    fn meet(self, other: Self) -> Self {
        let (a_allow, a_deny) = partition_exec_dirs(&self.dirs);
        let (b_allow, b_deny) = partition_exec_dirs(&other.dirs);
        let mut dirs = BTreeMap::new();
        for path in intersect_prefix_strings(&a_allow, &b_allow) {
            dirs.insert(path, ExecDir::Allow);
        }
        for path in union_prefix_strings(a_deny, b_deny) {
            dirs.insert(path, ExecDir::Deny);
        }
        Self {
            literals: meet_literal_exec(self.literals, other.literals),
            dirs,
        }
    }
}

/// Join two exec maps — the widening composition `--extend-base` runs.
/// In `dirs`: allow-regions union (either side widens) and deny-regions
/// union too (a dir `Deny` is a sticky veto, kept from either side), so
/// an extension silent on a base's denied tree cannot re-admit it.  On
/// an exact-key clash the deny lands last, so deny-overrides: a base
/// veto on a directory survives even an overlay that re-grants it,
/// exactly as under `meet`.  The `literals` half mirrors this through
/// [`join_literal_exec`].
impl Join for ExecMap {
    fn join(self, other: Self) -> Self {
        let (a_allow, a_deny) = partition_exec_dirs(&self.dirs);
        let (b_allow, b_deny) = partition_exec_dirs(&other.dirs);
        let mut dirs = BTreeMap::new();
        for path in union_prefix_strings(a_allow, b_allow) {
            dirs.insert(path, ExecDir::Allow);
        }
        for path in union_prefix_strings(a_deny, b_deny) {
            dirs.insert(path, ExecDir::Deny);
        }
        Self {
            literals: join_literal_exec(self.literals, other.literals),
            dirs,
        }
    }
}

/// Split a dir map's keys by verdict into the allow-key list and the
/// deny-key list, so the two signs can be combined under their own
/// lattice operation (allows intersect under meet, denies union; dual
/// under join).
fn partition_exec_dirs(dirs: &BTreeMap<String, ExecDir>) -> (Vec<String>, Vec<String>) {
    let mut allow = Vec::new();
    let mut deny = Vec::new();
    for (path, verdict) in dirs {
        match verdict {
            ExecDir::Allow => allow.push(path.clone()),
            ExecDir::Deny => deny.push(path.clone()),
        }
    }
    (allow, deny)
}

/// Per-name meet over the `literals` half of an exec map.  Allow-sided
/// keys must appear on both sides (uses `ExecPolicy::meet`); `Deny`
/// propagates from either side even when absent on the other.
///
/// Exposed crate-wide so projection-time reduction (which folds the
/// `literals` map directly) doesn't have to re-implement it.
pub(crate) fn meet_literal_exec(
    a: BTreeMap<String, ExecPolicy>,
    b: BTreeMap<String, ExecPolicy>,
) -> BTreeMap<String, ExecPolicy> {
    let mut out = BTreeMap::new();
    for (name, pa) in &a {
        match b.get(name) {
            Some(pb) => {
                out.insert(name.clone(), pa.clone().meet(pb.clone()));
            }
            None if matches!(pa, ExecPolicy::Deny) => {
                out.insert(name.clone(), ExecPolicy::Deny);
            }
            None => {}
        }
    }
    for (name, pb) in &b {
        if a.contains_key(name) {
            continue;
        }
        if matches!(pb, ExecPolicy::Deny) {
            out.insert(name.clone(), ExecPolicy::Deny);
        }
    }
    out
}

/// Per-name join over the `literals` half of an exec map — dual of
/// [`meet_literal_exec`].  Shared keys combine via [`ExecPolicy::join`]
/// (deny-overrides — a `Deny` on either side wins).  One-sided keys
/// survive verbatim: an absent key is the join identity (`p ⊔ ⊥ = p`),
/// so silence on one side lifts neither a base's grant nor its veto.
fn join_literal_exec(
    a: BTreeMap<String, ExecPolicy>,
    b: BTreeMap<String, ExecPolicy>,
) -> BTreeMap<String, ExecPolicy> {
    let mut out = BTreeMap::new();
    for (name, pa) in &a {
        match b.get(name) {
            Some(pb) => {
                out.insert(name.clone(), pa.clone().join(pb.clone()));
            }
            None => {
                out.insert(name.clone(), pa.clone());
            }
        }
    }
    for (name, pb) in &b {
        if !a.contains_key(name) {
            out.insert(name.clone(), pb.clone());
        }
    }
    out
}

/// Prefix-set intersection: keep the deeper prefix from each
/// overlapping pair.  Delegates to the shared
/// `crate::path::meet_prefix_sets_by` combinator, judging overlap on
/// the resolved strings (lexical, no symlink resolution); the runtime
/// fold in `crate::path::PrefixSet`'s `Meet` impl uses the same
/// combinator with canonical-form overlap when reducing the dynamic
/// stack at sandbox-render time.
fn intersect_prefix_strings(a: &[String], b: &[String]) -> Vec<String> {
    crate::path::meet_prefix_sets_by(a, b, |s| s.as_str())
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn union_prefix_strings(a: Vec<String>, b: Vec<String>) -> Vec<String> {
    a.into_iter()
        .chain(b)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// [`NormalizedPrefix`] counterpart of [`intersect_prefix_strings`]:
/// keep the deeper prefix of each overlapping pair, overlap judged on
/// the frozen string via the same alias-aware combinator.
fn intersect_prefixes(a: &[NormalizedPrefix], b: &[NormalizedPrefix]) -> Vec<NormalizedPrefix> {
    crate::path::meet_prefix_sets_by(a, b, |p| p.as_str())
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn union_prefixes(a: Vec<NormalizedPrefix>, b: Vec<NormalizedPrefix>) -> Vec<NormalizedPrefix> {
    a.into_iter()
        .chain(b)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod lattice_tests;
