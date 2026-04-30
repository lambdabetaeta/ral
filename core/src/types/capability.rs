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

/// Exec policy value for a single command in a `grant` exec map.
///
/// Forms a three-point lattice with `Allow` at top, `Subcommands(_)`
/// in the middle (more elements = more authority), and `Deny` at
/// bottom.  An explicit `Deny` is a sticky veto: it survives meet
/// against absence in another layer's map (so a base ceiling can
/// pin a command name out without restrict files having to repeat
/// it) and overrides directory-based admission via `exec_dirs`.
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

/// The OS-renderable projection of the effective capability grant.
///
/// Produced by `capability::EffectiveGrant::sandbox_projection` after
/// meet-folding the dynamic stack; consumed by the platform sandbox
/// backends (`sandbox::linux`, `sandbox::macos`) and ferried across
/// the IPC boundary in the internal `--sandbox-projection` flag.
///
/// This is distinct from `Capabilities` (one stack frame, possibly
/// extending authority) — a `SandboxProjection` is the reduced fs+net
/// shape no further composition can widen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxProjection {
    pub fs: FsPolicy,
    /// Final network verdict after reducing the capability stack.
    pub net: bool,
}

impl Default for SandboxProjection {
    fn default() -> Self {
        Self { fs: FsPolicy::default(), net: true }
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
    pub fn bind_spec(&self) -> SandboxBindSpec {
        SandboxBindSpec {
            read_prefixes: unique_strings(self.fs.read_prefixes.iter().cloned()),
            write_prefixes: unique_strings(self.fs.write_prefixes.iter().cloned()),
            deny_paths: unique_strings(self.fs.deny_paths.iter().cloned()),
        }
    }

    pub fn check_spec(&self, shell: &Shell) -> SandboxCheckSpec {
        let resolve = |prefixes: &[String]| -> Vec<String> {
            unique_strings(prefixes.iter().map(|p| {
                shell.dynamic.resolver().check(p).to_string_lossy().into_owned()
            }))
        };
        SandboxCheckSpec {
            read_prefixes: resolve(&self.fs.read_prefixes),
            write_prefixes: resolve(&self.fs.write_prefixes),
            deny_paths: resolve(&self.fs.deny_paths),
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
    #[serde(default)]
    pub exec: Option<BTreeMap<String, ExecPolicy>>,
    #[serde(default)]
    pub exec_dirs: Option<Vec<String>>,
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
    #[serde(default)]
    pub exec: Option<BTreeMap<String, ExecPolicy>>,
    #[serde(default)]
    pub exec_dirs: Option<Vec<String>>,
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
            exec_dirs: Some(Vec::new()),
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
            || self.exec_dirs.is_some()
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
            exec_dirs: Some(Vec::new()),
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
            || self.exec_dirs.is_some()
            || self.fs.is_some()
            || self.net.is_some()
            || self.editor.is_some()
            || self.shell.is_some()
    }

    /// Reject unknown `xdg:` tokens in any path list.  Pure
    /// syntactic check; cheap, no env or filesystem access.
    /// Production loaders should call [`Self::freeze`], which
    /// subsumes this check and resolves the tokens to concrete
    /// paths under `home`.
    pub fn validate_paths(&self) -> Result<(), String> {
        use crate::path::sigil::validate_xdg_tokens;
        if let Some(dirs) = &self.exec_dirs {
            validate_xdg_tokens(dirs)?;
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
        if let Some(dirs) = self.exec_dirs.as_deref_mut() {
            freeze_path_list(dirs, ctx)?;
        }
        if let Some(fs) = self.fs.as_mut() {
            freeze_path_list(&mut fs.read_prefixes, ctx)?;
            freeze_path_list(&mut fs.write_prefixes, ctx)?;
            freeze_path_list(&mut fs.deny_paths, ctx)?;
        }
        Ok(Capabilities {
            exec: self.exec,
            exec_dirs: self.exec_dirs,
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
        Self {
            exec:      combine_opt(self.exec,      other.exec,      meet_exec),
            exec_dirs: combine_opt(self.exec_dirs, other.exec_dirs, |a, b| {
                intersect_prefix_strings(&a, &b)
            }),
            fs:        combine_opt(self.fs,        other.fs,        meet_fs),
            net:       combine_opt(self.net,       other.net,       |a, b| a && b),
            editor:    combine_opt(self.editor,    other.editor,    meet_editor),
            shell:     combine_opt(self.shell,     other.shell,     meet_shell),
            audit:     self.audit || other.audit,
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
        Self {
            exec:      combine_opt(self.exec,      other.exec,      join_exec),
            exec_dirs: combine_opt(self.exec_dirs, other.exec_dirs, union_prefix_strings),
            fs:        combine_opt(self.fs,        other.fs,        join_fs),
            net:       combine_opt(self.net,       other.net,       |a, b| a || b),
            editor:    combine_opt(self.editor,    other.editor,    join_editor),
            shell:     combine_opt(self.shell,     other.shell,     join_shell),
            audit:     self.audit || other.audit,
        }
    }
}

/// `None` is the identity for `f`: if either side has no opinion the other
/// survives; if both do, combine with `f`.
fn combine_opt<T>(a: Option<T>, b: Option<T>, f: impl FnOnce(T, T) -> T) -> Option<T> {
    match (a, b) {
        (None, None) => None,
        (None, Some(x)) | (Some(x), None) => Some(x),
        (Some(a), Some(b)) => Some(f(a, b)),
    }
}

/// Intersect two exec maps.  Allow-sided keys survive only when
/// present in both sides (per-command policies meet).  `Deny`
/// entries are sticky: a `Deny` on either side propagates into the
/// result even if the other side does not list the name, so a base
/// ceiling's veto cannot be lost when a restrict file fails to
/// repeat it.
fn meet_exec(
    a: BTreeMap<String, ExecPolicy>,
    b: BTreeMap<String, ExecPolicy>,
) -> BTreeMap<String, ExecPolicy> {
    let mut out = BTreeMap::new();
    for (name, pa) in &a {
        match b.get(name) {
            Some(pb) => { out.insert(name.clone(), meet_exec_policy(pa.clone(), pb.clone())); }
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

fn meet_exec_policy(a: ExecPolicy, b: ExecPolicy) -> ExecPolicy {
    match (a, b) {
        (ExecPolicy::Deny, _) | (_, ExecPolicy::Deny) => ExecPolicy::Deny,
        (ExecPolicy::Allow, ExecPolicy::Allow) => ExecPolicy::Allow,
        (ExecPolicy::Allow, ExecPolicy::Subcommands(s))
        | (ExecPolicy::Subcommands(s), ExecPolicy::Allow) => {
            ExecPolicy::Subcommands(unique_strings(s))
        }
        (ExecPolicy::Subcommands(s1), ExecPolicy::Subcommands(s2)) => {
            ExecPolicy::Subcommands(unique_strings(s1.into_iter().filter(|x| s2.contains(x))))
        }
    }
}

/// Intersect read & write prefix sets; union deny_paths.
fn meet_fs(a: FsPolicy, b: FsPolicy) -> FsPolicy {
    FsPolicy {
        read_prefixes:  intersect_prefix_strings(&a.read_prefixes,  &b.read_prefixes),
        write_prefixes: intersect_prefix_strings(&a.write_prefixes, &b.write_prefixes),
        deny_paths:     unique_strings(a.deny_paths.into_iter().chain(b.deny_paths)),
    }
}

/// Prefix-set intersection: keep the deeper prefix from each overlapping pair.
///
/// Lexical-only: this lattice operation runs without a `Dynamic`, so it
/// cannot canonicalise paths.  Two layers that name the same directory
/// through different symlinks (`/tmp` vs `/private/tmp`) will not be
/// recognised as overlapping here.  The runtime fold in
/// `capability::prefix::intersect_grant_paths` does symlink-aware overlap
/// when reducing the dynamic stack at sandbox-render time.
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

fn meet_editor(a: EditorPolicy, b: EditorPolicy) -> EditorPolicy {
    EditorPolicy { read: a.read && b.read, write: a.write && b.write, tui: a.tui && b.tui }
}

fn meet_shell(a: ShellPolicy, b: ShellPolicy) -> ShellPolicy {
    ShellPolicy { chdir: a.chdir && b.chdir }
}

/// Union two exec maps: allow-sided commands from either side
/// survive; shared commands have their policies joined.  `Deny`
/// only survives when both sides agree to deny — a one-sided veto
/// is dominated by the other side's authority and is dropped from
/// the join.
fn join_exec(
    a: BTreeMap<String, ExecPolicy>,
    b: BTreeMap<String, ExecPolicy>,
) -> BTreeMap<String, ExecPolicy> {
    let mut out = BTreeMap::new();
    for (name, pa) in &a {
        match b.get(name) {
            Some(pb) => { out.insert(name.clone(), join_exec_policy(pa.clone(), pb.clone())); }
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

fn join_exec_policy(a: ExecPolicy, b: ExecPolicy) -> ExecPolicy {
    match (a, b) {
        (ExecPolicy::Allow, _) | (_, ExecPolicy::Allow) => ExecPolicy::Allow,
        (ExecPolicy::Deny, p) | (p, ExecPolicy::Deny) => p,
        (ExecPolicy::Subcommands(s1), ExecPolicy::Subcommands(s2)) => {
            ExecPolicy::Subcommands(unique_strings(s1.into_iter().chain(s2)))
        }
    }
}

/// Union read & write prefix sets; intersect deny_paths.
fn join_fs(a: FsPolicy, b: FsPolicy) -> FsPolicy {
    FsPolicy {
        read_prefixes:  union_prefix_strings(a.read_prefixes,  b.read_prefixes),
        write_prefixes: union_prefix_strings(a.write_prefixes, b.write_prefixes),
        deny_paths:     a.deny_paths.into_iter().filter(|p| b.deny_paths.contains(p)).collect(),
    }
}

fn union_prefix_strings(a: Vec<String>, b: Vec<String>) -> Vec<String> {
    unique_strings(a.into_iter().chain(b))
}

fn join_editor(a: EditorPolicy, b: EditorPolicy) -> EditorPolicy {
    EditorPolicy { read: a.read || b.read, write: a.write || b.write, tui: a.tui || b.tui }
}

fn join_shell(a: ShellPolicy, b: ShellPolicy) -> ShellPolicy {
    ShellPolicy { chdir: a.chdir || b.chdir }
}

// ── Lattice tests ─────────────────────────────────────────────────────────
#[cfg(test)]
mod lattice_tests {
    use super::*;

    fn witness_a() -> RawCapabilities {
        RawCapabilities {
            exec: Some(BTreeMap::from([
                ("cargo".into(), ExecPolicy::Allow),
                ("git".into(), ExecPolicy::Subcommands(vec!["log".into(), "status".into()])),
            ])),
            exec_dirs: Some(vec!["/usr/bin".into()]),
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
            ])),
            exec_dirs: Some(vec!["/usr/bin".into(), "/usr/local/bin".into()]),
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
            exec_dirs: None,
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
        assert!(m.exec_dirs.expect("exec_dirs retained").is_empty());
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
        assert_eq!(exec.len(), 1);
        assert!(exec.contains_key("cargo"));
        match exec.get("cargo").unwrap() {
            ExecPolicy::Subcommands(s) => assert_eq!(s, &vec!["build".to_string()]),
            other => panic!("unexpected: {other:?}"),
        }
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
            exec_dirs: Some(vec!["xdg:bin".into(), "/usr/bin".into()]),
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
            exec_dirs: Some(vec!["xdg:bin".into(), "/usr/bin".into()]),
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
        let dirs = caps.exec_dirs.unwrap();
        assert_eq!(dirs[0], "/h/.local/bin");
        assert_eq!(dirs[1], "/usr/bin");
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
