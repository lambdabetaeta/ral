//! Lattice-algebra tests for the capability types.
//!
//! In-crate rather than under `core/tests/` because these reach crate-private
//! doors: `decode_capability_map`, `FrozenPath::for_test`,
//! `RealPath::assumed`.

#![allow(
    clippy::disallowed_methods,
    reason = "[test] probes are lexical paths already in normal form"
)]

use super::exec::{ExecRules, Subject};
use super::*;
use crate::guard::decode_capability_map;
use crate::guard::freeze::{FreezeCtx, PolicyError};
use crate::path::RealPath;
use crate::types::Value;
use std::collections::BTreeSet;
use strum::VariantArray;

/// The `Value::Map` shape `decode_capability_map` receives in production.
fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

fn tagged(label: &str, payload: &str) -> Value {
    Value::variant(label, Some(Value::string(payload)))
}

/// `xdg-KIND` place with a subpath.
#[cfg(unix)]
fn xdg(kind: &str, sub: &str) -> Value {
    tagged(&format!("xdg-{kind}"), sub)
}

/// A list of `(tag, payload)` targets, the payload itself a tagged value.
#[cfg(unix)]
fn places(entries: &[(&str, Value)]) -> Value {
    Value::list(
        entries
            .iter()
            .map(|(tag, inner)| Value::variant(*tag, Some(inner.clone())))
            .collect(),
    )
}

/// `p` as an absolute path on this host.
fn at(p: &str) -> std::path::PathBuf {
    crate::path::test_absolute(p).into()
}

/// A prefix witness for the fixtures that build `FsPolicy`/`ExecGrant`
/// directly rather than through `decode_capability_map`.
fn nprefix(s: &str) -> crate::path::FrozenPath {
    crate::path::FrozenPath::from_surface(crate::path::utf8(&at(s)).unwrap())
}

fn break_msg(e: PolicyError) -> String {
    e.message
}

fn grant<const N: usize>(entries: [(ExecKey, Evidence); N]) -> ExecGrant {
    entries.into_iter().collect()
}

fn name(s: &str) -> ExecKey {
    ExecKey::Name(s.into())
}

fn path_key(s: &str) -> ExecKey {
    ExecKey::Path(nprefix(s))
}

fn dir_key(s: &str) -> ExecKey {
    ExecKey::Dir(nprefix(s))
}

/// A host file whose real path, and launch path, is `real`.
fn file(real: &str) -> Program {
    let path = at(real);
    Program::File {
        real: RealPath::assumed(&path),
        path,
    }
}

/// Whether `grants` admits running the file whose real path is `real`.
fn admits(grants: &GrantStack, real: &str) -> bool {
    grants.admits(&file(real))
}

#[cfg(unix)]
use crate::test_env::{with_var, with_vars_cleared};

/// Clears the `XDG_*_HOME` overrides so `xdg-*` places resolve to the
/// home-joined defaults, under the synthetic home the escape guard admits.
#[cfg(unix)]
fn with_xdg_defaults<R>(f: impl FnOnce() -> R) -> R {
    with_vars_cleared(
        &[
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
            "XDG_BIN_HOME",
        ],
        f,
    )
}

fn witness_a() -> Capabilities {
    Capabilities {
        exec: Some(grant([
            (name("cargo"), Evidence::ALLOW),
            (name("git"), Evidence::ALLOW),
            (path_key("/opt/tool"), Evidence::ALLOW),
            (dir_key("/usr/bin"), Evidence::ALLOW),
        ])),
        fs: Some(FsPolicy {
            read_prefixes: vec![nprefix("/tmp")],
            write_prefixes: vec![nprefix("/tmp")],
            deny_paths: vec![nprefix("/tmp/secret")],
        }),
        net: None,
        editor: Some([EditorVerb::Read, EditorVerb::Write].into()),
        shell: Some([ShellVerb::Chdir, ShellVerb::Detach].into()),
    }
}

fn witness_b() -> Capabilities {
    Capabilities {
        exec: Some(grant([
            (name("cargo"), Evidence::ALLOW),
            (name("ls"), Evidence::ALLOW),
            (path_key("/opt/tool"), Evidence::DENY),
            (dir_key("/usr/bin"), Evidence::ALLOW),
            (dir_key("/usr/local/bin"), Evidence::ALLOW),
        ])),
        fs: Some(FsPolicy {
            read_prefixes: vec![nprefix("/tmp/work")],
            write_prefixes: vec![nprefix("/tmp/work")],
            deny_paths: vec![nprefix("/tmp/work/.exarch.toml")],
        }),
        net: Some(Net::Offline),
        editor: Some([EditorVerb::Read, EditorVerb::Tui].into()),
        shell: Some(std::collections::BTreeSet::new()),
    }
}

/// Absence is not denial: the axis composes by meet like every other, so only
/// a verb set that omits `detach` withholds, and no inner frame gives it back.
#[test]
fn detach_is_permitted_until_some_layer_withholds_it() {
    let detach = Flag::Shell(ShellVerb::Detach);
    let verbs = |v: &[ShellVerb]| Capabilities {
        shell: Some(v.iter().copied().collect()),
        ..Default::default()
    };
    let mut stack = GrantStack::root();
    assert!(stack.permits(detach), "ambient authority permits");
    stack.push(Capabilities {
        fs: Some(FsPolicy::default()),
        ..Default::default()
    });
    assert!(
        stack.permits(detach),
        "a frame that attenuates only fs leaves the verb alone"
    );
    stack.push(verbs(&[ShellVerb::Chdir]));
    assert!(!stack.permits(detach), "an omitted verb is withheld");
    stack.push(verbs(&[ShellVerb::Chdir, ShellVerb::Detach]));
    assert!(
        !stack.permits(detach),
        "and no inner frame can grant back what an outer one withheld"
    );
}

/// A stack is sigil-free by construction, so the wire form carries concrete
/// paths and the peer has nothing to re-resolve — path and dir keys
/// included, though no JSON object can be keyed by one.
#[test]
fn ipc_roundtrip_preserves_a_frozen_grant_stack() {
    let mut stack = GrantStack::root();
    stack.push(witness_a());
    stack.push(witness_b());
    let json = serde_json::to_string(&stack).unwrap();
    let back: GrantStack = serde_json::from_str(&json).unwrap();
    assert_eq!(stack, back);
}

fn fs_of(p: &crate::path::FrozenPath) -> FsPolicy {
    FsPolicy {
        read_prefixes: vec![p.clone()],
        write_prefixes: vec![p.clone()],
        deny_paths: Vec::new(),
    }
}

fn exec_of(p: &crate::path::FrozenPath) -> ExecGrant {
    grant([(ExecKey::Dir(p.clone()), Evidence::ALLOW)])
}

/// Covers nesting, aliasing (`/a/alias` resolves to `/a`) and symlink
/// divergence (`/a/link` resolves to `/elsewhere`).
fn prefix_universe() -> Vec<crate::path::FrozenPath> {
    vec![
        frozen("/a", "/a"),
        frozen("/a/sub", "/a/sub"),
        frozen("/a/alias", "/a"),
        frozen("/a/link", "/elsewhere"),
    ]
}

fn exec_deny_of(p: &crate::path::FrozenPath) -> ExecGrant {
    grant([(ExecKey::Dir(p.clone()), Evidence::DENY)])
}

fn stack_of(exec: ExecGrant) -> GrantStack {
    GrantStack::of(Capabilities {
        exec: Some(exec),
        ..Capabilities::root()
    })
}

#[cfg(unix)]
fn allow_dirs(exec: &ExecGrant) -> Vec<&crate::path::FrozenPath> {
    (exec.iter())
        .filter_map(|(key, v)| match key {
            ExecKey::Dir(dir) if v.permits() => Some(dir),
            _ => None,
        })
        .collect()
}

/// The prefix universe folded through an allow-only and a deny-only grant
/// alike, so the laws below reach exclusions, which `exec_of` on its own never
/// enters.
fn exec_universe() -> Vec<ExecGrant> {
    prefix_universe()
        .iter()
        .flat_map(|p| [exec_of(p), exec_deny_of(p)])
        .collect()
}

fn shell_of(verbs: &[ShellVerb]) -> Capabilities {
    Capabilities {
        shell: Some(verbs.iter().copied().collect()),
        ..Default::default()
    }
}

fn every_flag() -> Vec<Flag> {
    (EditorVerb::VARIANTS.iter().copied().map(Flag::Editor))
        .chain(ShellVerb::VARIANTS.iter().copied().map(Flag::Shell))
        .collect()
}

#[test]
fn deny_all_withholds_every_verb() {
    let stack = GrantStack::of(Capabilities::deny_all());
    for flag in every_flag() {
        assert!(!stack.permits(flag), "{} must be withheld", flag.key());
    }
}

/// What a stack decides: each verb, an exec admission, and the net opinions.
fn judgments(stack: &GrantStack) -> (Vec<bool>, bool, Vec<Net>) {
    (
        every_flag().into_iter().map(|f| stack.permits(f)).collect(),
        admits(stack, "/bin/ls"),
        stack.net().collect(),
    )
}

/// Layers that supply an axis only to list every verb, or none at all.
fn neutral_layers() -> [Capabilities; 4] {
    let editor: BTreeSet<_> = EditorVerb::VARIANTS.iter().copied().collect();
    let shell: BTreeSet<_> = ShellVerb::VARIANTS.iter().copied().collect();
    [
        Capabilities::root(),
        Capabilities {
            editor: Some(editor.clone()),
            ..Default::default()
        },
        Capabilities {
            shell: Some(shell.clone()),
            ..Default::default()
        },
        Capabilities {
            editor: Some(editor),
            shell: Some(shell),
            ..Default::default()
        },
    ]
}

/// `()`, a full editor list, a full shell list and both judge as ambient
/// authority does, and leave a stack inactive.
#[test]
fn neutral_layers_judge_as_ambient_and_leave_it_inactive() {
    let ambient = GrantStack::root();
    for layer in neutral_layers() {
        assert!(!layer.is_restrictive(), "{layer:?} must be neutral");
        let mut stack = GrantStack::root();
        stack.push(layer);
        assert!(!stack.is_restrictive());
        assert_eq!(judgments(&stack), judgments(&ambient));
    }
}

/// A neutral layer, or all of them nested, changes neither the judgments nor
/// the activation of a stack withholding a shell verb and the net.
#[test]
fn neutral_layers_leave_a_restrictive_stack_as_it_was() {
    let mut base = GrantStack::root();
    base.push(Capabilities {
        net: Some(Net::Offline),
        ..shell_of(&[ShellVerb::Chdir])
    });
    let judged = judgments(&base);
    assert!(base.is_restrictive() && !base.permits(Flag::Shell(ShellVerb::Detach)));

    let mut nested = base.clone();
    for layer in neutral_layers() {
        let mut stack = base.clone();
        stack.push(layer.clone());
        nested.push(layer);
        for stack in [&stack, &nested] {
            assert!(stack.is_restrictive());
            assert_eq!(judgments(stack), judged);
        }
    }
}

/// Places freezing to one surface are one exec key, in either order; two
/// spellings through a symlink stay two.
#[cfg(unix)]
#[test]
fn exec_keys_follow_the_frozen_surface() {
    let decode = |targets: Vec<Value>| {
        let exec = map(vec![("allow", Value::list(targets))]);
        decode_capability_map(&map(vec![("exec", exec)]), "test", &test_ctx("/h"))
            .expect("places decode")
            .exec
            .expect("exec supplied")
    };
    let (rel, abs) = (tagged("cwd", "tools/x"), tagged("absolute", "/tools/x"));
    let forward = decode(vec![rel.clone(), abs.clone()]);
    assert_eq!(forward.iter().count(), 1);
    assert_eq!(forward, decode(vec![abs, rel]));

    let key = |surface, real| ExecKey::Path(crate::path::FrozenPath::for_test(surface, real));
    let linked: ExecGrant = [
        (key("/a/x", "/real/x"), Evidence::ALLOW),
        (key("/b/x", "/real/x"), Evidence::ALLOW),
    ]
    .into_iter()
    .collect();
    assert_eq!(linked.iter().count(), 2);
}

/// A key displays as the target that wrote it: a directory keeps its `dir`
/// constructor, distinct from a file at the same path.
#[cfg(unix)]
#[test]
fn exec_keys_display_as_their_targets() {
    assert_eq!(name("git").to_string(), "`name git");
    assert_eq!(path_key("/tools").to_string(), "`absolute /tools");
    assert_eq!(dir_key("/tools").to_string(), "`dir `absolute /tools");
}

/// An unfrozen field is refused: a peer's stale `detach` must not vanish into
/// a wider grant.
#[test]
fn capabilities_refuse_an_unknown_wire_field() {
    let err = serde_json::from_str::<Capabilities>(r#"{"detach":false}"#)
        .expect_err("`detach` is no field of a layer");
    assert!(err.to_string().contains("detach"), "{err}");
}

/// A stack with `Offline` and partial verb sets crosses the engine protocol
/// as sequences of tag names and survives it.
#[test]
fn a_run_carries_its_grant_stack_across_the_wire() {
    let mut caps = GrantStack::root();
    caps.push(witness_a());
    caps.push(witness_b());
    let run = crate::protocol::Run {
        caps,
        ..crate::protocol::Run::captured("echo hi", crate::source::SourceOrigin::named("<test>"))
    };
    let json = serde_json::to_string(&run).unwrap();
    assert!(
        json.contains(r#""offline""#) && json.contains(r#""chdir""#),
        "{json}"
    );
    assert_eq!(
        serde_json::from_str::<crate::protocol::Run>(&json).unwrap(),
        run
    );
}

/// With the `XDG_*_HOME` overrides cleared every place resolves under the
/// synthetic home, so the escape guard passes and the freeze succeeds.
// Unix-only: a driveless `/usr/bin` is dead on Windows.
#[cfg(unix)]
#[test]
fn decode_accepts_known_places() {
    let v = map(vec![
        (
            "exec",
            map(vec![(
                "allow",
                places(&[
                    ("dir", xdg("bin", ".")),
                    ("dir", tagged("absolute", "/usr/bin")),
                ]),
            )]),
        ),
        (
            "fs",
            map(vec![
                (
                    "read",
                    Value::list(vec![
                        tagged("xdg-config", "."),
                        tagged("xdg-data", "agda"),
                        tagged("home", ".cache"),
                        tagged("absolute", "/etc"),
                    ]),
                ),
                ("write", Value::list(vec![tagged("xdg-cache", ".")])),
                ("deny", Value::list(vec![tagged("xdg-config", "secret")])),
            ]),
        ),
    ]);
    with_xdg_defaults(|| decode_capability_map(&v, "test", &test_ctx("/h")))
        .expect("known places should decode and freeze");
}

/// A typo in an `xdg-` tag fails at decode, naming the real ones, rather than
/// silently matching nothing at runtime.
#[test]
fn decode_rejects_xdg_typo() {
    let v = map(vec![(
        "fs",
        map(vec![("read", Value::list(vec![tagged("xdg-cofnig", ".")]))]),
    )]);
    let err = break_msg(decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err());
    assert!(err.contains("xdg-cofnig"), "got {err}");
    assert!(
        err.contains("`` `xdg-config ``,"),
        "should list known kinds: {err}"
    );
}

/// Decode rewrites every place to a concrete absolute path, so matching is
/// decoupled from any later env mutation.
// Unix-only: place expansion joins via `PathBuf`, which on Windows yields
// backslashes against the synthetic `/h` home, so `/h/.local/bin` never lands.
#[cfg(unix)]
#[test]
fn decode_rewrites_places_to_concrete_paths() {
    let v = map(vec![
        (
            "exec",
            map(vec![(
                "allow",
                places(&[
                    ("dir", xdg("bin", ".")),
                    ("dir", tagged("absolute", "/usr/bin")),
                ]),
            )]),
        ),
        (
            "fs",
            map(vec![(
                "read",
                Value::list(vec![tagged("home", "notes"), tagged("absolute", "/etc")]),
            )]),
        ),
    ]);
    let caps = with_xdg_defaults(|| decode_capability_map(&v, "test", &test_ctx("/h")))
        .expect("known places freeze");
    let exec = caps.exec.unwrap();
    let allowed = allow_dirs(&exec);
    assert!(allowed.iter().any(|p| p.as_str() == "/h/.local/bin"));
    assert!(allowed.iter().any(|p| p.as_str() == "/usr/bin"));
    // A list is a set, so it freezes in the frozen paths' own order.
    let reads = caps.fs.unwrap().read_prefixes;
    assert_eq!(reads[0], "/etc");
    assert_eq!(reads[1], "/h/notes");
}

/// An unwrapped place names one executable, so a directory there would
/// fail closed at use time as a bare "denied by active grant"; decode says
/// `dir` instead.
#[cfg(unix)]
#[test]
fn decode_rejects_directory_as_exec_file() {
    let v = map(vec![(
        "exec",
        map(vec![(
            "allow",
            Value::list(vec![tagged("absolute", "/etc")]),
        )]),
    )]);
    let err = decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err();
    assert!(err.hint.unwrap().contains("`` `dir `absolute /etc ``"));
}

/// Defence in depth: `XDG_DATA_HOME=/etc` must not widen a policy naming
/// `xdg-data`; decode rejects it and names the offending env var.
// Unix-only: the boundary check compares Unix path prefixes.
#[cfg(unix)]
#[test]
fn decode_rejects_xdg_var_outside_home() {
    let v = map(vec![(
        "fs",
        map(vec![("read", Value::list(vec![tagged("xdg-data", ".")]))]),
    )]);
    let err = with_var("XDG_DATA_HOME", Some("/etc"), || {
        decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err()
    });
    let err = break_msg(err);
    assert!(
        err.contains("XDG_DATA_HOME"),
        "should name the env var: {err}"
    );
    assert!(err.contains("/etc"), "should show the bad value: {err}");
    assert!(err.contains("HOME"), "should mention HOME: {err}");
}

/// No home is a configuration error, not a silent allow, whether it arrives
/// as the absence the readers report or as the empty binding `HOME=` gives.
#[test]
fn decode_errors_when_there_is_no_home() {
    let v = map(vec![(
        "fs",
        map(vec![("read", Value::list(vec![tagged("home", "x")]))]),
    )]);
    crate::test_env::with_vars_cleared(&crate::host::HOME_VARS, || {
        for ctx in [test_ctx(""), no_home_ctx()] {
            let err = break_msg(decode_capability_map(&v, "test", &ctx).unwrap_err());
            assert!(err.contains("HOME"), "got {err}");
        }
    });
}

/// A bare relative path would anchor to the live cwd at check time, so the
/// same grant would shift meaning after a `cd`.
#[test]
fn decode_rejects_relative_absolute_place() {
    let v = map(vec![(
        "fs",
        map(vec![(
            "read",
            Value::list(vec![tagged("absolute", "proj")]),
        )]),
    )]);
    let err = break_msg(decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err());
    assert!(err.contains("proj"), "should name the entry: {err}");
    assert!(
        err.contains("`` `cwd proj ``"),
        "should hint the cwd place: {err}"
    );
}

#[test]
fn decode_rejects_dot_relative_absolute_places() {
    for p in ["./a", "../b"] {
        let v = map(vec![(
            "fs",
            map(vec![("read", Value::list(vec![tagged("absolute", p)]))]),
        )]);
        assert!(decode_capability_map(&v, "test", &test_ctx("/h")).is_err());
    }
}

/// It carries a `/`, so it is a path, not a bare command name.
#[test]
fn decode_rejects_path_shaped_name() {
    let v = map(vec![(
        "exec",
        map(vec![("allow", Value::list(vec![tagged("name", "./foo")]))]),
    )]);
    assert!(decode_capability_map(&v, "test", &test_ctx("/h")).is_err());
}

/// A bare name is a name, not a path, so no place rule applies to it.
#[test]
fn decode_accepts_bare_exec_name() {
    let v = map(vec![(
        "exec",
        map(vec![("allow", Value::list(vec![tagged("name", "git")]))]),
    )]);
    let caps = decode_capability_map(&v, "test", &test_ctx("/h")).expect("a bare command name");
    assert!(caps.exec.unwrap().get(&name("git")).is_some());
}

/// `cwd proj` freezes to an absolute path: the sanctioned "relative to here"
/// the relative-path rejection points at.
// Unix-only: the joined `/h` cwd is driveless, so Windows calls it relative.
#[cfg(unix)]
#[test]
fn decode_accepts_cwd_relative_fs_place() {
    let v = map(vec![(
        "fs",
        map(vec![("read", Value::list(vec![tagged("cwd", "proj")]))]),
    )]);
    decode_capability_map(&v, "test", &test_ctx("/h"))
        .expect("a cwd place freezes to an absolute path");
}

/// A verb that is no tag is a hard decode error, not a silent fold that would
/// quietly deny the capability.
#[test]
fn decode_rejects_non_tag_verb() {
    for key in ["editor", "shell"] {
        let v = map(vec![(key, Value::list(vec![Value::Int(5)]))]);
        let err = break_msg(decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err());
        assert!(
            err.contains("expected a verb"),
            "should say what a verb is: {err}"
        );
    }
}

#[test]
fn decode_accepts_verb_lists_and_offline() {
    let v = map(vec![
        (
            "editor",
            Value::list(vec![
                Value::variant("read", None),
                Value::variant("tui", None),
            ]),
        ),
        ("shell", Value::list(vec![Value::variant("chdir", None)])),
        ("net", Value::variant("offline", None)),
    ]);
    let caps = decode_capability_map(&v, "test", &test_ctx("/h")).expect("verbs and `offline`");
    assert_eq!(
        caps.editor,
        Some([EditorVerb::Read, EditorVerb::Tui].into())
    );
    assert_eq!(caps.shell, Some([ShellVerb::Chdir].into()));
    assert_eq!(caps.net, Some(Net::Offline));
}

fn test_ctx(home: &str) -> FreezeCtx {
    FreezeCtx::at(
        std::iter::once(("HOME", home)).collect::<crate::types::EnvVars>(),
        test_cwd_path(),
    )
}

/// No `HOME` in the overlay; tests using it clear the host's.
fn no_home_ctx() -> FreezeCtx {
    FreezeCtx::at(crate::types::EnvVars::new(), test_cwd_path())
}

fn test_cwd_path() -> crate::path::CwdPath {
    crate::path::CwdPath::new(crate::path::test_cwd().into()).expect("a fixture cwd is absolute")
}

// ── Evidence ──────────────────────────────────────────────────────────────

const FOUR: [Evidence; 4] = [
    Evidence::NEITHER,
    Evidence::ALLOW,
    Evidence::DENY,
    Evidence::BOTH,
];

/// The scalar laws, exhaustively: identities, associativity, commutativity,
/// idempotence, the persistence of the negative bit, and `↓` preserving `∧`.
#[test]
fn evidence_is_a_bilattice_in_the_fragment_ral_uses() {
    for a in FOUR {
        assert_eq!(a.join(Evidence::NEITHER), a);
        assert_eq!(a.meet(Evidence::ALLOW), a);
        assert_eq!(a.join(a), a);
        assert_eq!(a.meet(a), a);
        for b in FOUR {
            assert_eq!(a.join(b), b.join(a));
            assert_eq!(a.meet(b), b.meet(a));
            assert_eq!(
                a.meet(b).permits(),
                a.permits() && b.permits(),
                "↓ preserves ∧: {a:?} {b:?}"
            );
            for op in [Evidence::join, Evidence::meet] {
                assert_eq!(op(a, b).negative, a.negative || b.negative);
            }
            for c in FOUR {
                assert_eq!(a.join(b).join(c), a.join(b.join(c)));
                assert_eq!(a.meet(b).meet(c), a.meet(b.meet(c)));
            }
        }
    }
    assert!(Evidence::ALLOW.permits());
    assert!(
        ![Evidence::NEITHER, Evidence::DENY, Evidence::BOTH]
            .iter()
            .any(|e| e.permits())
    );
}

/// The wire form is a required-field object, and `ExecGrant` a required pair
/// of fields: Both and `every` round-trip; a repeated key, a Neither value, an
/// old verdict string and a missing `every` are refused.
#[test]
fn evidence_and_exec_grants_cross_the_wire_strictly() {
    let every = grant([(name("git"), Evidence::BOTH)]).join_top().unwrap();
    assert!(every.every());
    let json = serde_json::to_string(&every).unwrap();
    assert_eq!(serde_json::from_str::<ExecGrant>(&json).unwrap(), every);
    let key = serde_json::to_string(&name("git")).unwrap();
    let both = r#"{"positive":true,"negative":true}"#;
    let neither = r#"{"positive":false,"negative":false}"#;
    let wire = |every: &str, rules: &str| format!(r#"{{{every}"rules":[{rules}]}}"#);
    let ok = wire(r#""every":false,"#, &format!("[{key},{both}]"));
    assert!(serde_json::from_str::<ExecGrant>(&ok).is_ok());
    let repeated = wire(
        r#""every":false,"#,
        &format!("[{key},{both}],[{key},{both}]"),
    );
    assert!(serde_json::from_str::<ExecGrant>(&repeated).is_err());
    let empty = wire(r#""every":false,"#, &format!("[{key},{neither}]"));
    assert!(serde_json::from_str::<ExecGrant>(&empty).is_err());
    let old = wire(r#""every":false,"#, &format!(r#"[{key},"allow"]"#));
    assert!(serde_json::from_str::<ExecGrant>(&old).is_err());
    let missing = wire("", &format!("[{key},{both}]"));
    assert!(serde_json::from_str::<ExecGrant>(&missing).is_err());
}

// ── Joining and extension ─────────────────────────────────────────────────

/// `⊕` is commutative, associative and idempotent on exec grants and fs
/// policies, exclusions included.
#[test]
fn join_is_a_semilattice_over_the_prefix_universe() {
    let u = prefix_universe();
    for a in &u {
        for b in &u {
            assert_eq!(fs_of(a).join(fs_of(b)), fs_of(b).join(fs_of(a)));
            for c in &u {
                assert_eq!(
                    fs_of(a).join(fs_of(b).join(fs_of(c))),
                    fs_of(a).join(fs_of(b)).join(fs_of(c))
                );
            }
        }
        assert_eq!(fs_of(a).join(fs_of(a)), fs_of(a));
    }
    let e = exec_universe();
    for a in &e {
        for b in &e {
            assert_eq!(a.clone().join(b.clone()), b.clone().join(a.clone()));
            for c in &e {
                assert_eq!(
                    a.clone().join(b.clone().join(c.clone())),
                    a.clone().join(b.clone()).join(c.clone())
                );
            }
        }
        assert_eq!(a.clone().join(a.clone()), a.clone());
    }
}

/// An allowance and an exclusion of one key are Both, which `↓` refuses,
/// whichever is joined first.
#[test]
fn joining_an_allowance_and_an_exclusion_is_both() {
    let (allow, deny) = (exec_of(&nprefix("/x")), exec_deny_of(&nprefix("/x")));
    for joined in [allow.clone().join(deny.clone()), deny.join(allow)] {
        assert_eq!(joined.get(&dir_key("/x")), Some(&Evidence::BOTH));
        assert!(!admits(&stack_of(joined), "/x/bin"));
    }
}

fn fragment(caps: Capabilities) -> Fragment {
    Fragment::of(caps)
}

/// An omitted fragment axis is the identity, and an omitted base axis stays
/// unrestricted under a positive-only fragment.
#[test]
fn extension_reads_omission_by_role() {
    let a = witness_a();
    assert_eq!(a.clone().extend(fragment(Capabilities::default())), a);
    let positive = fragment(Capabilities {
        exec: Some(grant([(name("git"), Evidence::ALLOW)])),
        fs: Some(fs_of(&nprefix("/tmp"))),
        ..Capabilities::default()
    });
    let extended = Capabilities::default().extend(positive);
    assert!(!extended.is_restrictive(), "{extended:?}");
    assert!(GrantStack::of(extended).admits_fs_exact(&FsOp::Read, &at("/anywhere")));
}

/// An exclusion in the fragment over an omitted base axis is "every request,
/// less the exclusion".
#[test]
fn extension_with_an_exclusion_over_an_omitted_axis_is_every_less_it() {
    let excluding = fragment(Capabilities {
        exec: Some(grant([(dir_key("/bad"), Evidence::DENY)])),
        fs: Some(FsPolicy {
            deny_paths: vec![nprefix("/secret")],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    });
    let extended = Capabilities::default().extend(excluding);
    assert!(extended.is_restrictive());
    assert!(extended.exec.as_ref().is_some_and(ExecGrant::every));
    let stack = GrantStack::of(extended);
    assert!(admits(&stack, "/anywhere/else"));
    assert!(!admits(&stack, "/bad/prog"));
    for op in [FsOp::Read, FsOp::Write] {
        assert!(stack.admits_fs_exact(&op, &at("/anywhere")));
        assert!(!stack.admits_fs_exact(&op, &at("/secret/f")));
    }
}

/// Extension joins verb sets by union, and the net veto is a floor.
#[test]
fn extension_unions_verbs_and_keeps_the_net_veto() {
    let extended = shell_of(&[ShellVerb::Chdir]).extend(fragment(shell_of(&[ShellVerb::Detach])));
    assert_eq!(
        extended.shell,
        Some(ShellVerb::VARIANTS.iter().copied().collect())
    );
    for extension in [&[][..], &[ShellVerb::Detach], ShellVerb::VARIANTS] {
        let extended = Capabilities::default().extend(fragment(shell_of(extension)));
        assert_eq!(extended.shell, None);
        let stack = GrantStack::of(extended);
        assert!(every_flag().into_iter().all(|f| stack.permits(f)));
    }
    let offline = Capabilities {
        net: Some(Net::Offline),
        ..Capabilities::default()
    };
    assert_eq!(
        Capabilities::default()
            .extend(fragment(offline.clone()))
            .net,
        Some(Net::Offline)
    );
    assert_eq!(
        offline.extend(fragment(Capabilities::default())).net,
        Some(Net::Offline)
    );
}

/// A base that omits exec but restricts fs, extended with a run allowance by
/// name: the allowance stays beside `every`, so what it names is read, and a
/// file at another real path under the same name still runs.
#[cfg(unix)]
#[test]
fn an_extension_run_allowance_over_an_omitted_exec_reads_what_it_names() {
    let base = Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![nprefix("/work")],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    };
    let extended = base.extend(fragment(Capabilities {
        exec: Some(grant([(path_key("/opt/prog"), Evidence::ALLOW)])),
        ..Capabilities::default()
    }));
    let stack = GrantStack::of(extended);
    assert!(stack.read(&at("/opt/prog")).is_ok());
    assert!(stack.read(&at("/opt/other")).is_err());
}

// ── Random policies ───────────────────────────────────────────────────────

use super::exec::tests::Rng;

const LAW_DIRS: [&str; 4] = ["/lawtest/a", "/lawtest/a/b", "/lawtest/z", "/lawtest/Z"];
const LAW_NAMES: [&str; 3] = ["x", "ls", "cat"];
const LAW_FILES: [&str; 6] = [
    "/lawtest/a/x",
    "/lawtest/a/b/x",
    "/lawtest/a/b/c/x",
    "/lawtest/z/x",
    "/lawtest/q/x",
    "/lawtest/Z/x",
];

/// A prefix spelled `surface` that froze as `real`, both on this host.
fn frozen(surface: &str, real: &str) -> crate::path::FrozenPath {
    crate::path::FrozenPath::for_test(
        &crate::path::test_absolute(surface),
        &crate::path::test_absolute(real),
    )
}

fn lit(p: &str) -> crate::path::FrozenPath {
    frozen(p, p)
}

fn some(rng: &mut Rng, n: usize) -> bool {
    rng.below(n) == 0
}

/// A random exec grant over the universe, `every` set one time in four where
/// the grant is a policy and not an authored fragment.
fn rand_exec(rng: &mut Rng, every_ok: bool) -> Option<ExecGrant> {
    if some(rng, 3) {
        return None;
    }
    let evidence = |rng: &mut Rng| {
        rng.pick(&[
            Evidence::ALLOW,
            Evidence::ALLOW,
            Evidence::DENY,
            Evidence::BOTH,
        ])
    };
    let mut entries = Vec::new();
    for d in LAW_DIRS {
        if some(rng, 3) {
            entries.push((ExecKey::Dir(lit(d)), evidence(rng)));
        }
    }
    for f in LAW_FILES {
        if some(rng, 4) {
            entries.push((ExecKey::Path(lit(f)), evidence(rng)));
        }
    }
    for n in LAW_NAMES {
        if some(rng, 3) {
            entries.push((ExecKey::Name(n.into()), evidence(rng)));
        }
    }
    let grant: ExecGrant = entries.into_iter().collect();
    Some(if every_ok && some(rng, 4) {
        grant.clone().join_top().unwrap_or(grant)
    } else {
        grant
    })
}

fn rand_fs(rng: &mut Rng) -> Option<FsPolicy> {
    if some(rng, 3) {
        return None;
    }
    let pick = |rng: &mut Rng, n| {
        LAW_DIRS
            .iter()
            .filter(|_| some(rng, n))
            .map(|p| lit(p))
            .collect::<Vec<_>>()
    };
    Some(FsPolicy {
        read_prefixes: pick(rng, 3),
        write_prefixes: pick(rng, 3),
        deny_paths: pick(rng, 5),
    })
}

fn rand_layer(rng: &mut Rng) -> Capabilities {
    Capabilities {
        exec: rand_exec(rng, true),
        fs: rand_fs(rng),
        ..Capabilities::default()
    }
}

fn rand_stack(rng: &mut Rng) -> GrantStack {
    let mut stack = GrantStack::default();
    (0..=rng.below(3)).for_each(|_| stack.push(rand_layer(rng)));
    stack
}

fn rand_verbs<V: VariantArray + Ord + Copy>(rng: &mut Rng) -> Option<BTreeSet<V>> {
    (!some(rng, 3)).then(|| {
        V::VARIANTS
            .iter()
            .copied()
            .filter(|_| some(rng, 2))
            .collect()
    })
}

fn exec_permits(stack: &GrantStack, real: &str) -> bool {
    rules(stack).is_none_or(|r| {
        r.evidence(Subject::File(&RealPath::assumed(at(real))))
            .permits()
    })
}

/// Law 4 on exec: an allowance never removes authority, an exclusion always
/// wins, and nothing is permitted that neither side supports.
#[test]
fn law_4_extension_invariant_on_exec() {
    let mut rng = Rng(0x1357_9bdf_2468_ace0);
    for _ in 0..1500 {
        let (base, fragment_exec) = (rand_exec(&mut rng, true), rand_exec(&mut rng, false));
        let base_caps = Capabilities {
            exec: base.clone(),
            ..Capabilities::default()
        };
        let frag = Capabilities {
            exec: fragment_exec.clone(),
            ..Capabilities::default()
        };
        let extended = base_caps.clone().extend(Fragment::of(frag));
        let own = fragment_exec.as_ref().map(ExecRules::compile);
        for f in LAW_FILES {
            let real = RealPath::assumed(at(f));
            let s = Subject::File(&real);
            let fe = own.as_ref().map_or(Evidence::NEITHER, |t| t.evidence(s));
            let (b, e) = (
                exec_permits(&GrantStack::of(base_caps.clone()), f),
                exec_permits(&GrantStack::of(extended.clone()), f),
            );
            let msg = format!("{f}\nbase = {base:?}\nfragment = {fragment_exec:?}");
            if fe.negative {
                assert!(!e, "an exclusion lost: {msg}");
            } else {
                assert!(!b || e, "an allowance removed authority: {msg}");
            }
            assert!(!e || b || fe.positive, "authority from nowhere: {msg}");
        }
    }
}

/// Law 4 on fs, for each op, with no exec in play.
#[test]
fn law_4_extension_invariant_on_fs() {
    let mut rng = Rng(0x0fed_cba9_8765_4321);
    for _ in 0..1500 {
        let (base, frag) = (rand_fs(&mut rng), rand_fs(&mut rng));
        let layer = |fs: &Option<FsPolicy>| Capabilities {
            fs: fs.clone(),
            ..Capabilities::default()
        };
        let extended = layer(&base).extend(Fragment::of(layer(&frag)));
        let permits = |caps: Capabilities, op: &FsOp, f: &str| {
            let stack = GrantStack::of(caps);
            region(&stack, op).is_none_or(|r| r.evidence(&at(f)).permits())
        };
        for op in [FsOp::Read, FsOp::Write] {
            for f in LAW_FILES {
                let fe = region(&GrantStack::of(layer(&frag)), &op)
                    .map_or(Evidence::NEITHER, |r| r.evidence(&at(f)));
                let b = permits(layer(&base), &op, f);
                let e = permits(extended.clone(), &op, f);
                let msg = format!("{f}\nbase = {base:?}\nfragment = {frag:?}");
                if fe.negative {
                    assert!(!e, "an exclusion lost: {msg}");
                } else {
                    assert!(!b || e, "an allowance removed authority: {msg}");
                }
                assert!(!e || b || fe.positive, "authority from nowhere: {msg}");
            }
        }
    }
}

/// Law 4 on the read region with exec present: the run places an extension's
/// exec supplies are reads, and its fs exclusions still win.
#[test]
fn law_4_extension_invariant_on_reads_with_run_places() {
    let mut rng = Rng(0x0bad_cafe_f00d_5eed);
    for _ in 0..1500 {
        let layer = |exec: &Option<ExecGrant>, fs: &Option<FsPolicy>| Capabilities {
            exec: exec.clone(),
            fs: fs.clone(),
            ..Capabilities::default()
        };
        let base = layer(&rand_exec(&mut rng, true), &rand_fs(&mut rng));
        let (frag_exec, frag_fs) = (rand_exec(&mut rng, false), rand_fs(&mut rng));
        let frag = layer(&frag_exec, &frag_fs);
        let extended = base.clone().extend(Fragment::of(frag));
        let own = layer(&frag_exec, &Some(frag_fs.clone().unwrap_or_default()));
        let read = |caps: Capabilities, f: &str| {
            running_region(&GrantStack::of(caps)).map_or(Evidence::ALLOW, |r| r.evidence(&at(f)))
        };
        for f in LAW_FILES {
            let (fe, b, e) = (
                read(own.clone(), f),
                read(base.clone(), f).permits(),
                read(extended.clone(), f).permits(),
            );
            let msg = format!("{f}\nbase = {base:?}\nfragment = {frag_exec:?} {frag_fs:?}");
            if fe.negative {
                assert!(!e, "an exclusion lost: {msg}");
            } else {
                assert!(!b || e, "an allowance removed authority: {msg}");
            }
            assert!(!e || b || fe.positive, "authority from nowhere: {msg}");
        }
    }
}

/// Law 4 on verbs and net.
#[test]
fn law_4_extension_invariant_on_verbs_and_net() {
    let mut rng = Rng(0x2468_ace0_1357_9bdf);
    for _ in 0..500 {
        let layer = |rng: &mut Rng| Capabilities {
            shell: rand_verbs(rng),
            editor: rand_verbs(rng),
            net: some(rng, 2).then_some(Net::Offline),
            ..Capabilities::default()
        };
        let (base, frag) = (layer(&mut rng), layer(&mut rng));
        let extended = base.clone().extend(Fragment::of(frag.clone()));
        let (b, f, e) = (
            GrantStack::of(base.clone()),
            GrantStack::of(frag.clone()),
            GrantStack::of(extended.clone()),
        );
        for flag in every_flag() {
            assert!(!b.permits(flag) || e.permits(flag), "{flag:?}");
            let listed = match flag {
                Flag::Editor(v) => frag.editor.as_ref().is_some_and(|s| s.contains(&v)),
                Flag::Shell(v) => frag.shell.as_ref().is_some_and(|s| s.contains(&v)),
            };
            assert!(!e.permits(flag) || b.permits(flag) || listed, "{flag:?}");
        }
        assert!(f.net().next().is_none() || e.net().next().is_some());
        assert!(e.net().next().is_none() || b.net().next().is_some() || f.net().next().is_some());
    }
}

/// A write allowance through a symlink to `/` is universal: top is decided by
/// the frozen real identity, as the projection decides it.
#[test]
fn a_root_reached_through_a_symlink_denotes_top() {
    let through = |surface, real| FsPolicy {
        write_prefixes: vec![crate::path::FrozenPath::for_test(surface, real)],
        ..FsPolicy::default()
    };
    assert!(through("/root-alias", "/").is_top());
    assert!(!through("/", "/elsewhere").is_top());
    assert!(!through("/work", "/work").is_top());
}

/// Activation reads denotations: an axis that denotes `t` activates nothing.
#[test]
fn activation_is_denotational() {
    let every = grant([(name("git"), Evidence::ALLOW)]).join_top().unwrap();
    assert!(every.is_top());
    let writes_root = FsPolicy {
        write_prefixes: vec![crate::path::FrozenPath::root()],
        ..FsPolicy::default()
    };
    assert!(writes_root.is_top());
    let top = Capabilities {
        exec: Some(every),
        fs: Some(writes_root.clone()),
        ..Capabilities::default()
    };
    assert!(!top.is_restrictive());
    assert!(!GrantStack::of(top).is_restrictive());
    let excluding = Capabilities {
        exec: Some(
            grant([(dir_key("/bad"), Evidence::DENY)])
                .join_top()
                .unwrap(),
        ),
        ..Capabilities::default()
    };
    assert!(
        excluding.is_restrictive(),
        "an `every` table with exclusions restricts"
    );
    let denying = FsPolicy {
        deny_paths: vec![nprefix("/bad")],
        ..writes_root
    };
    assert!(!denying.is_top());
    assert!(
        Capabilities {
            exec: Some(ExecGrant::default()),
            ..Capabilities::default()
        }
        .is_restrictive()
    );
    assert!(
        Capabilities {
            net: Some(Net::Offline),
            ..Capabilities::default()
        }
        .is_restrictive()
    );
}

/// The read judgment agrees with the region of run places after `↓`, and cites
/// an exclusion that applies; law 7 for a host file and for a bundled tool.
#[test]
fn the_read_and_run_judgments_agree_with_their_regions() {
    let mut rng = Rng(0x9876_5432_10fe_dcba);
    for _ in 0..800 {
        let stack = rand_stack(&mut rng);
        let full = running_region(&stack);
        for f in LAW_FILES {
            let path = &at(f);
            let read = stack.read(path);
            let want = full.as_ref().is_none_or(|r| r.evidence(path).permits());
            assert_eq!(read.is_ok(), want, "{f}\nstack = {stack:?}");
            if let Err(Withheld::Excluded { scope, matched_by }) = &read {
                use super::table::Denies;
                assert_eq!(
                    scope.matches(path),
                    Some(*matched_by),
                    "{scope:?} does not hold {f} as cited"
                );
            }
            let runs = stack.admit(file(f), vec![]).is_ok();
            assert_eq!(
                runs,
                exec_permits(&stack, f) && want,
                "{f}\nstack = {stack:?}"
            );
        }
        let tool = stack.admit(Program::Tool("ls".into()), vec![]).is_ok();
        let tool_exec = rules(&stack).is_none_or(|r| r.evidence(Subject::Tool("ls")).permits());
        assert_eq!(tool, tool_exec, "stack = {stack:?}");
    }
}

/// Law 8: whatever the stack lets be written, it lets be read.
#[test]
fn law_8_writing_reads() {
    let mut rng = Rng(0x0bad_cafe_f00d_1234);
    for _ in 0..1000 {
        let stack = rand_stack(&mut rng);
        for f in LAW_FILES {
            let path = &at(f);
            assert!(
                stack.write(path).is_err() || stack.read(path).is_ok(),
                "{f}\nstack = {stack:?}"
            );
        }
    }
}

/// Law 10: pushing a layer permits nothing the stack did not, for running,
/// reading and writing alike.
#[test]
fn law_10_restriction_attenuates() {
    let mut rng = Rng(0x5555_aaaa_3333_cccc);
    for _ in 0..1000 {
        let stack = rand_stack(&mut rng);
        let mut pushed = stack.clone();
        pushed.push(rand_layer(&mut rng));
        for f in LAW_FILES {
            let path = &at(f);
            let msg = format!("{f}\nstack = {stack:?}\npushed = {pushed:?}");
            assert!(
                pushed.admit(file(f), vec![]).is_err() || stack.admit(file(f), vec![]).is_ok(),
                "run {msg}"
            );
            assert!(
                pushed.read(path).is_err() || stack.read(path).is_ok(),
                "read {msg}"
            );
            assert!(
                pushed.write(path).is_err() || stack.write(path).is_ok(),
                "write {msg}"
            );
        }
    }
}

/// Under a ceiling that reads nothing and omits exec, a nested layer allowing
/// `File(s)` makes `s` neither readable nor runnable.
#[test]
fn a_nested_run_allowance_cannot_make_what_the_ceiling_withholds_readable() {
    let mut stack = GrantStack::of(Capabilities {
        fs: Some(FsPolicy::default()),
        ..Capabilities::default()
    });
    stack.push(Capabilities {
        exec: Some(grant([(path_key("/opt/prog"), Evidence::ALLOW)])),
        ..Capabilities::default()
    });
    assert!(stack.read(&at("/opt/prog")).is_err());
    assert!(stack.admit(file("/opt/prog"), vec![]).is_err());
}

/// A bundled tool runs wherever exec permits it and no fs exclusion covers
/// ral's binary; an exec exclusion of ral leaves it alone.
#[test]
fn a_bundled_tool_reads_ral_s_binary() {
    let Some(own) = ral_binary() else { return };
    let stack = |fs: FsPolicy| {
        GrantStack::of(Capabilities {
            fs: Some(fs),
            ..Capabilities::default()
        })
    };
    let tool = |s: &GrantStack| s.admit(Program::Tool("ls".into()), vec![]).is_ok();
    let own_file = crate::path::FrozenPath::from_surface(crate::path::utf8(own.as_path()).unwrap());
    let own_dir = crate::path::FrozenPath::from_surface(
        crate::path::utf8(own.as_path().parent().unwrap()).unwrap(),
    );
    let excluding = stack(FsPolicy {
        read_prefixes: vec![crate::path::FrozenPath::root()],
        deny_paths: vec![own_dir.clone()],
        ..FsPolicy::default()
    });
    assert!(!tool(&excluding));
    assert!(
        tool(&stack(FsPolicy::default())),
        "an fs that merely does not read ral's binary refuses nothing"
    );
    if !crate::uutils::is_uutils_tool("ls") {
        return;
    }
    let exec = grant([
        (name("ls"), Evidence::ALLOW),
        (ExecKey::Path(own_file), Evidence::DENY),
        (ExecKey::Dir(own_dir), Evidence::DENY),
    ]);
    assert!(
        tool(&stack_of(exec)),
        "an exec exclusion of ral's binary leaves a bundled tool alone"
    );
}
