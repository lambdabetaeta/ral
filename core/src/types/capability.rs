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
//! - [`ExecPolicy`]: per-command exec rule (`Allow` or
//!   `Subcommands`).
//! - [`FsPolicy`]: read/write prefixes and explicit `deny_paths` for
//!   single files.
//! - [`EditorPolicy`] / [`ShellPolicy`]: bit flags gating REPL-side
//!   builtins.
//! - `net`: tristate (None=inherit, Some(false)=deny, Some(true)=allow).
//! - `audit`: orthogonal flag — propagated upward by `meet` (logical OR),
//!   not part of the lattice.
//!
//! ## `SandboxPolicy` and `SandboxBindSpec` / `SandboxCheckSpec`
//!
//! [`SandboxPolicy`] is the meet-folded effective fs+net policy used to
//! render the OS sandbox profile.  The `*Spec` types are shape-converted
//! views of it for two consumers: `bind_spec` returns lexically-tagged
//! prefixes for the Seatbelt/bwrap profile renderer; `check_spec` returns
//! shell-resolved prefixes for in-ral path checks.
//!
//! Method-side enforcement (`check_fs_op`, `check_fs_read`,
//! `check_fs_write`, `sandbox_policy`) lives on `Dynamic` in `types.rs`
//! because it touches audit and resolution machinery owned there.

use serde::{Deserialize, Serialize};

use super::{Shell, unique_strings};

/// Exec policy value for a single command in a `grant` exec map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecPolicy {
    /// Allow the command with any arguments.
    Allow,
    /// Allow only when the first argument is in this list.
    Subcommands(Vec<String>),
}

/// Filesystem access policy within a `grant` block.
///
/// `deny_paths` lists exact absolute paths whose write capability is
/// stripped even when a covering `write_prefix` would otherwise allow
/// it.  Used to make individual files (e.g. the active `.exarch.toml`
/// capability profile) untouchable inside an otherwise-writable cwd.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsPolicy {
    #[serde(default)]
    pub read_prefixes: Vec<String>,
    #[serde(default)]
    pub write_prefixes: Vec<String>,
    #[serde(default)]
    pub deny_paths: Vec<String>,
}

/// Effective filesystem and network policy after meet-folding the
/// dynamic capability stack.  Used to render the OS sandbox profile and
/// ferry policy across the IPC boundary in `--sandbox-policy`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    pub fs: FsPolicy,
    /// Final network verdict after reducing the capability stack.
    pub net: bool,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        Self {
            fs: FsPolicy::default(),
            net: true,
        }
    }
}

/// Lexical view of the policy: prefixes as written, for the Seatbelt /
/// bwrap profile renderer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxBindSpec {
    pub read_prefixes: Vec<String>,
    pub write_prefixes: Vec<String>,
    pub deny_paths: Vec<String>,
}

/// Shell-resolved view of the policy: every prefix passed through
/// `Dynamic::resolve_grant_path`, for in-ral path checks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxCheckSpec {
    pub read_prefixes: Vec<String>,
    pub write_prefixes: Vec<String>,
    pub deny_paths: Vec<String>,
}

impl SandboxPolicy {
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
                shell
                    .dynamic
                    .resolve_grant_path(p)
                    .to_string_lossy()
                    .into_owned()
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
/// Controls access to `_editor` sub-commands:
/// - `read`: `get`, `history`, `parse`
/// - `write`: `set`, `push`, `accept`, `ghost`, `highlight`, `state`
/// - `tui`: `tui`
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorPolicy {
    pub read: bool,
    pub write: bool,
    pub tui: bool,
}

/// Shell policy — controls what shell operations a plugin handler may perform.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellPolicy {
    pub chdir: bool,
}

/// One layer in the dynamic capabilities stack — a bundle of policies
/// plus an `audit` flag.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Per-command exec policy.  `None` means no exec restriction.
    /// Serialised as a TOML/JSON map (`{ "cargo": "Allow", ... }`)
    /// rather than a sequence so profile authors can write it as a
    /// `[exec]` table.
    #[serde(default, with = "exec_opt_as_map")]
    pub exec: Option<Vec<(String, ExecPolicy)>>,
    /// Allow-by-directory: if a command's resolved absolute path is
    /// under any of these prefixes, the layer accepts it (with
    /// `ExecPolicy::Allow`).  Used as a name-agnostic pass for
    /// well-known toolchain dirs (`/usr/bin`, `/opt/homebrew/bin`,
    /// …).  `None` means no exec_dirs allowance; `Some(empty)` is
    /// "no dirs allowed."  Falls through only when the per-command
    /// `exec` map has no matching entry — name matches always win.
    #[serde(default)]
    pub exec_dirs: Option<Vec<String>>,
    #[serde(default)]
    pub fs: Option<FsPolicy>,
    /// Network capability. `None` means inherit; `Some(false)` denies;
    /// `Some(true)` explicitly allows.
    #[serde(default)]
    pub net: Option<bool>,
    #[serde(default)]
    pub audit: bool,
    /// Editor policy.  `None` means no restriction.
    #[serde(default)]
    pub editor: Option<EditorPolicy>,
    /// Shell policy.  `None` means no restriction.
    #[serde(default)]
    pub shell: Option<ShellPolicy>,
}

/// Serde adapter for `Option<Vec<(String, ExecPolicy)>>` so the
/// on-disk form is a map (`[exec]` table) rather than a sequence of
/// pairs.
mod exec_opt_as_map {
    use super::ExecPolicy;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        v: &Option<Vec<(String, ExecPolicy)>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match v {
            None => s.serialize_none(),
            Some(pairs) => {
                let m: BTreeMap<&String, &ExecPolicy> =
                    pairs.iter().map(|(k, v)| (k, v)).collect();
                Some(m).serialize(s)
            }
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Vec<(String, ExecPolicy)>>, D::Error> {
        let m: Option<BTreeMap<String, ExecPolicy>> = Option::deserialize(d)?;
        Ok(m.map(|m| m.into_iter().collect()))
    }
}

impl Capabilities {
    /// Ambient authority — the root of every capabilities stack.  All fields
    /// `None`: no attenuation. `Shell::new` pre-pushes this so the stack is
    /// never empty.
    pub fn root() -> Self {
        Self::default()
    }

    /// Deny every effect capability.  Used as the base for explicit grants:
    /// callers opt capabilities back in by replacing individual fields.
    pub fn deny_all() -> Self {
        Self {
            exec: Some(Vec::new()),
            exec_dirs: Some(Vec::new()),
            fs: Some(FsPolicy::default()),
            net: Some(false),
            editor: Some(EditorPolicy::default()),
            shell: Some(ShellPolicy::default()),
            audit: false,
        }
    }

    /// True for a real attenuation context, false for the ambient root.
    pub fn is_restrictive(&self) -> bool {
        self.exec.is_some()
            || self.exec_dirs.is_some()
            || self.fs.is_some()
            || self.net.is_some()
            || self.editor.is_some()
            || self.shell.is_some()
    }

    /// Lattice meet — the most-authority capability that is below both
    /// `self` and `other`.  `Capabilities::root()` is top, `deny_all()`
    /// is bottom; `meet` is commutative, associative, idempotent.
    ///
    /// Each `Option<_>` field treats `None` as ⊤ (no attenuation), so
    /// `meet(None, x) = x`.  Inner fields intersect (exec maps, fs
    /// prefixes), AND (net, editor, shell), and union (`fs.deny_paths`
    /// — more denies = less authority).
    ///
    /// `audit` is **not** part of the lattice: it propagates upward
    /// (logical OR), since asking for an audit at any layer should
    /// turn it on for the composition.
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

    /// Lattice join — the least-authority capability that is above both
    /// `self` and `other`.  Used to widen a base ceiling with an
    /// extension TOML before any attenuation runs.  Commutative,
    /// associative, idempotent.
    ///
    /// `None` on a field means "no opinion" — same identity-like role
    /// as in `meet`, but here it acts as the join identity (⊥-ish for
    /// that single field).  In particular, `join(Some(p), None) =
    /// Some(p)`: an extension that omits a field doesn't widen it.
    /// This is what extension authors expect ("I'm only adding what I
    /// list"), and matches `meet`'s `None`-is-identity discipline
    /// pointwise — the lattice element a `None` *denotes* simply
    /// depends on which operation it's flowing through.
    ///
    /// Inner fields union (exec maps, fs prefixes), OR (net, editor,
    /// shell), and intersect (`fs.deny_paths` — fewer denies = more
    /// authority).
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

/// `None` is the identity for the binary op `f`: if either side has
/// no opinion, the other survives unchanged; if both do, combine with
/// `f`.  Used by both `meet` and `join` — the *lattice element* a
/// `None` denotes depends on which `f` you pass (⊤ for meet, ⊥ for
/// join), but the algebraic shape is the same.
fn combine_opt<T>(a: Option<T>, b: Option<T>, f: impl FnOnce(T, T) -> T) -> Option<T> {
    match (a, b) {
        (None, None) => None,
        (None, Some(x)) | (Some(x), None) => Some(x),
        (Some(a), Some(b)) => Some(f(a, b)),
    }
}

/// Intersect allowed command names; meet per-command policies.
/// Output is sorted by name so `meet` is literally commutative.
fn meet_exec(
    a: Vec<(String, ExecPolicy)>,
    b: Vec<(String, ExecPolicy)>,
) -> Vec<(String, ExecPolicy)> {
    let mut out = Vec::new();
    for (name, pa) in &a {
        if let Some((_, pb)) = b.iter().find(|(n, _)| n == name) {
            out.push((name.clone(), meet_exec_policy(pa.clone(), pb.clone())));
        }
    }
    out.sort_by(|x, y| x.0.cmp(&y.0));
    out
}

fn meet_exec_policy(a: ExecPolicy, b: ExecPolicy) -> ExecPolicy {
    match (a, b) {
        (ExecPolicy::Allow, ExecPolicy::Allow) => ExecPolicy::Allow,
        (ExecPolicy::Allow, ExecPolicy::Subcommands(s))
        | (ExecPolicy::Subcommands(s), ExecPolicy::Allow) => {
            ExecPolicy::Subcommands(unique_strings(s))
        }
        (ExecPolicy::Subcommands(s1), ExecPolicy::Subcommands(s2)) => {
            ExecPolicy::Subcommands(unique_strings(
                s1.into_iter().filter(|x| s2.contains(x)),
            ))
        }
    }
}

/// Intersect read & write prefix sets (each chain keeps its deeper
/// prefix); union deny_paths.
fn meet_fs(a: FsPolicy, b: FsPolicy) -> FsPolicy {
    FsPolicy {
        read_prefixes:  intersect_prefix_strings(&a.read_prefixes,  &b.read_prefixes),
        write_prefixes: intersect_prefix_strings(&a.write_prefixes, &b.write_prefixes),
        deny_paths:     unique_strings(a.deny_paths.into_iter().chain(b.deny_paths)),
    }
}

/// Prefix-set intersection: a path is allowed by both iff there is some
/// prefix in `a` and some prefix in `b` that both cover it.  For each
/// chain, that means keeping the deeper of the two prefixes.
fn intersect_prefix_strings(a: &[String], b: &[String]) -> Vec<String> {
    fn within(p: &str, q: &str) -> bool {
        crate::path::path_within(std::path::Path::new(p), std::path::Path::new(q))
    }
    let mut out: Vec<String> = Vec::new();
    for p in a {
        if b.iter().any(|q| within(p, q)) {
            out.push(p.clone());
        }
    }
    for q in b {
        if a.iter().any(|p| within(q, p)) {
            out.push(q.clone());
        }
    }
    unique_strings(out)
}

fn meet_editor(a: EditorPolicy, b: EditorPolicy) -> EditorPolicy {
    EditorPolicy {
        read:  a.read  && b.read,
        write: a.write && b.write,
        tui:   a.tui   && b.tui,
    }
}

fn meet_shell(a: ShellPolicy, b: ShellPolicy) -> ShellPolicy {
    ShellPolicy {
        chdir: a.chdir && b.chdir,
    }
}

/// Union allowed command names; join per-command policies.  Output is
/// sorted by name so `join` is literally commutative.
fn join_exec(
    a: Vec<(String, ExecPolicy)>,
    b: Vec<(String, ExecPolicy)>,
) -> Vec<(String, ExecPolicy)> {
    let mut out: Vec<(String, ExecPolicy)> = Vec::new();
    for (name, pa) in &a {
        let merged = match b.iter().find(|(n, _)| n == name) {
            Some((_, pb)) => join_exec_policy(pa.clone(), pb.clone()),
            None => pa.clone(),
        };
        out.push((name.clone(), merged));
    }
    for (name, pb) in &b {
        if !a.iter().any(|(n, _)| n == name) {
            out.push((name.clone(), pb.clone()));
        }
    }
    out.sort_by(|x, y| x.0.cmp(&y.0));
    out
}

fn join_exec_policy(a: ExecPolicy, b: ExecPolicy) -> ExecPolicy {
    match (a, b) {
        (ExecPolicy::Allow, _) | (_, ExecPolicy::Allow) => ExecPolicy::Allow,
        (ExecPolicy::Subcommands(s1), ExecPolicy::Subcommands(s2)) => {
            ExecPolicy::Subcommands(unique_strings(s1.into_iter().chain(s2)))
        }
    }
}

/// Union read & write prefix sets; intersect deny_paths (a path is
/// denied in the join iff both sides denied it — fewer denies = more
/// authority).
fn join_fs(a: FsPolicy, b: FsPolicy) -> FsPolicy {
    FsPolicy {
        read_prefixes:  union_prefix_strings(a.read_prefixes,  b.read_prefixes),
        write_prefixes: union_prefix_strings(a.write_prefixes, b.write_prefixes),
        deny_paths:     a.deny_paths.into_iter().filter(|p| b.deny_paths.contains(p)).collect(),
    }
}

/// Concat-then-dedupe.  Used by `join` for prefix lists where coverage
/// is monotone (a path covered by either side is covered by the join).
fn union_prefix_strings(a: Vec<String>, b: Vec<String>) -> Vec<String> {
    unique_strings(a.into_iter().chain(b))
}

fn join_editor(a: EditorPolicy, b: EditorPolicy) -> EditorPolicy {
    EditorPolicy {
        read:  a.read  || b.read,
        write: a.write || b.write,
        tui:   a.tui   || b.tui,
    }
}

fn join_shell(a: ShellPolicy, b: ShellPolicy) -> ShellPolicy {
    ShellPolicy {
        chdir: a.chdir || b.chdir,
    }
}

// ── Lattice property tests for `Capabilities::meet` ───────────────────────
//
// Top: `Capabilities::root()`.  Bottom (for *positive* authority):
// `Capabilities::deny_all()` — empty exec list, empty fs prefixes,
// net=false, editor/shell zeroed.  `meet` is the semilattice operator.
//
// `audit` is OR-combined and not part of the lattice; literal equality
// still holds on these cases because root()/deny_all() have audit=false
// and the witnesses below pick a fixed audit value.
//
// Prefix lists in the witnesses are sorted+deduped so idempotence holds
// up to the syntactic equality `meet` returns (it canonicalises via
// `unique_strings`, so unsorted inputs would fail `meet(a,a) == a`).
#[cfg(test)]
mod lattice_tests {
    use super::*;

    fn ex(name: &str, p: ExecPolicy) -> (String, ExecPolicy) {
        (name.into(), p)
    }

    fn witness_a() -> Capabilities {
        Capabilities {
            exec: Some(vec![
                ex("cargo", ExecPolicy::Allow),
                ex("git", ExecPolicy::Subcommands(vec!["log".into(), "status".into()])),
            ]),
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

    fn witness_b() -> Capabilities {
        Capabilities {
            exec: Some(vec![
                ex("cargo", ExecPolicy::Subcommands(vec!["build".into()])),
                ex("ls", ExecPolicy::Allow),
            ]),
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

    fn witness_c() -> Capabilities {
        Capabilities {
            exec: Some(vec![ex("cargo", ExecPolicy::Allow)]),
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
        let lhs = a.clone().meet(b.clone().meet(c.clone()));
        let rhs = a.meet(b).meet(c);
        assert_eq!(lhs, rhs);
    }

    #[test]
    fn meet_idempotent() {
        let a = witness_a();
        assert_eq!(a.clone().meet(a.clone()), a);
    }

    #[test]
    fn meet_top_is_identity() {
        let a = witness_a();
        assert_eq!(a.clone().meet(Capabilities::root()), a.clone());
        assert_eq!(Capabilities::root().meet(a.clone()), a);
    }

    /// Bottom for positive authority: `deny_all()` zeroes every grant.
    /// `deny_paths` is not part of positive authority (more denies = less
    /// authority), so `meet(a, deny_all())` may carry over `a`'s denies;
    /// what matters is that *no* allow capabilities survive.
    #[test]
    fn meet_bottom_zeroes_authority() {
        let a = witness_a();
        let m = a.meet(Capabilities::deny_all());
        let exec = m.exec.expect("exec retained");
        assert!(exec.is_empty(), "no command names should survive");
        let exec_dirs = m.exec_dirs.expect("exec_dirs retained");
        assert!(exec_dirs.is_empty(), "no exec dirs should survive");
        let fs = m.fs.expect("fs retained");
        assert!(fs.read_prefixes.is_empty());
        assert!(fs.write_prefixes.is_empty());
        assert_eq!(m.net, Some(false));
        let editor = m.editor.expect("editor retained");
        assert!(!editor.read && !editor.write && !editor.tui);
        let shell = m.shell.expect("shell retained");
        assert!(!shell.chdir);
    }

    /// Sanity: meeting different exec maps intersects names and meets
    /// per-command policies.  `cargo` is `Allow` in `a` and
    /// `Subcommands(["build"])` in `b` → meet is `Subcommands(["build"])`.
    #[test]
    fn meet_exec_intersects_and_meets_policies() {
        let m = witness_a().meet(witness_b());
        let exec = m.exec.unwrap();
        assert_eq!(exec.len(), 1);
        assert_eq!(exec[0].0, "cargo");
        match &exec[0].1 {
            ExecPolicy::Subcommands(s) => assert_eq!(s, &vec!["build".to_string()]),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// JSON round-trip: parent serialises Capabilities into the IPC
    /// frame; child deserialises.  The exec adapter must be symmetric.
    #[test]
    fn json_roundtrip_via_exec_adapter() {
        let c = witness_a();
        let json = serde_json::to_string(&c).unwrap();
        let back: Capabilities = serde_json::from_str(&json).unwrap();
        assert_eq!(c, back);
    }

    /// Sanity: deny_paths union and prefix intersection.
    #[test]
    fn meet_fs_unions_denies_and_intersects_prefixes() {
        let m = witness_a().meet(witness_b());
        let fs = m.fs.unwrap();
        assert!(fs.read_prefixes.iter().any(|p| p == "/tmp/work"));
        assert!(fs.deny_paths.iter().any(|p| p == "/tmp/secret"));
        assert!(fs.deny_paths.iter().any(|p| p == "/tmp/work/.exarch.toml"));
    }

    // ── Lattice property tests for `Capabilities::join` ──────────────────

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
        let lhs = a.clone().join(b.clone().join(c.clone()));
        let rhs = a.join(b).join(c);
        assert_eq!(lhs, rhs);
    }

    #[test]
    fn join_idempotent() {
        let a = witness_a();
        assert_eq!(a.clone().join(a.clone()), a);
    }

    /// `None` is the identity for join: extension files leaving a field
    /// unset never widen it.
    #[test]
    fn join_none_is_identity() {
        let a = witness_a();
        assert_eq!(a.clone().join(Capabilities::default()), a.clone());
        assert_eq!(Capabilities::default().join(a.clone()), a);
    }

    /// Joining `Allow` with `Subcommands(_)` collapses to `Allow`
    /// (the wider authority); two subcommand sets union.
    #[test]
    fn join_exec_widens_policies_and_unions_names() {
        let m = witness_a().join(witness_b());
        let exec = m.exec.unwrap();
        let by_name: std::collections::BTreeMap<&str, &ExecPolicy> =
            exec.iter().map(|(k, v)| (k.as_str(), v)).collect();
        assert_eq!(by_name.get("cargo"), Some(&&ExecPolicy::Allow));
        assert!(matches!(by_name.get("ls"), Some(ExecPolicy::Allow)));
        match by_name.get("git").unwrap() {
            ExecPolicy::Subcommands(s) => {
                assert!(s.contains(&"log".into()) && s.contains(&"status".into()));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// fs join: prefix sets union, deny_paths intersect (denied iff
    /// both sides denied).
    #[test]
    fn join_fs_unions_prefixes_and_intersects_denies() {
        let m = witness_a().join(witness_b());
        let fs = m.fs.unwrap();
        assert!(fs.read_prefixes.iter().any(|p| p == "/tmp"));
        assert!(fs.read_prefixes.iter().any(|p| p == "/tmp/work"));
        assert!(fs.deny_paths.is_empty(), "no deny_path is in *both*");
    }

    // Absorption (a ⊓ (a ∨ b) = a, a ∨ (a ⊓ b) = a) holds *semantically*
    // — over the set of paths a `Capabilities` admits — but not always
    // syntactically over the `Vec<String>` representation, because
    // `intersect_prefix_strings` keeps redundant covers (both "/tmp" and
    // "/tmp/work" survive when one side has each).  Adding a syntactic
    // canonicaliser (drop p when some other prefix in the list strictly
    // contains it) would close the gap, but isn't part of this change;
    // the rest of the lattice properties above are sufficient.
}
