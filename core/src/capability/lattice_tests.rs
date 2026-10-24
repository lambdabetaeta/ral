//! Lattice-algebra tests for the capability types.
//!
//! In-crate rather than under `core/tests/` because these reach crate-private
//! entries: `decode_capability_map`, the layers' tables, `Fragment::of`.

use super::exec::{ExecRules, Subject};
use super::*;
use crate::guard::decode_capability_map;
use crate::guard::freeze::{FreezeCtx, PolicyError};
use crate::path::{self, Excluded, FrozenPath, Trace};
use crate::test_helper::{self, Tree, layer, trace};
use crate::types::Value;
use std::collections::BTreeSet;
use std::fmt::Debug;
use std::path::{Path, PathBuf};
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

/// `p`, `/`-rooted, as an absolute path on this host that nothing occupies.
fn at(p: &str) -> PathBuf {
    Tree::nowhere().path(p)
}

/// `p` as a grant writes it.
fn spelled(p: &Path) -> FrozenPath {
    FrozenPath::from_surface(path::utf8(p).unwrap())
}

/// A prefix witness for the fixtures that build `FsPolicy`/`ExecGrant`
/// directly rather than through `decode_capability_map`.
fn nprefix(s: &str) -> FrozenPath {
    spelled(&at(s))
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

/// An authored layer of exec alone, resolved.
fn with_exec(exec: ExecGrant) -> GrantLayer {
    layer(Capabilities {
        exec: Some(exec),
        ..Capabilities::default()
    })
}

/// An authored layer of fs alone, resolved.
fn with_fs(fs: FsPolicy) -> GrantLayer {
    layer(Capabilities {
        fs: Some(fs),
        ..Capabilities::default()
    })
}

/// A layer of exec rules already resolved.
fn exec_layer(rules: ExecRules) -> GrantLayer {
    GrantLayer::new(None, Some(rules), None, None, None)
}

/// A host file at `real`, which exists and is also its launch path.
fn file(real: impl AsRef<Path>) -> Program {
    let path = real.as_ref().to_path_buf();
    Program::File {
        located: Box::new(test_helper::located(&path)),
        arg0: path,
    }
}

/// Whether `grants` admits running the file at `real`.
fn admits(grants: &GrantStack, real: impl AsRef<Path>) -> bool {
    grants.admits(&Floor::default(), &file(real))
}

/// What the exec `rules` hold of running `access`.
fn runs(rules: &ExecRules, access: &Trace) -> Evidence {
    rules.evidence(Subject::Program(path::Subject::Access(access)))
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
            (path_key("/ral-lattice/opt/tool"), Evidence::ALLOW),
            (dir_key("/ral-lattice/usr/bin"), Evidence::ALLOW),
        ])),
        fs: Some(FsPolicy {
            read_prefixes: vec![nprefix("/ral-lattice/tmp")],
            write_prefixes: vec![nprefix("/ral-lattice/tmp")],
            deny_paths: vec![nprefix("/ral-lattice/tmp/secret")],
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
            (path_key("/ral-lattice/opt/tool"), Evidence::DENY),
            (dir_key("/ral-lattice/usr/bin"), Evidence::ALLOW),
            (dir_key("/ral-lattice/usr/local/bin"), Evidence::ALLOW),
        ])),
        fs: Some(FsPolicy {
            read_prefixes: vec![nprefix("/ral-lattice/tmp/work")],
            write_prefixes: vec![nprefix("/ral-lattice/tmp/work")],
            deny_paths: vec![nprefix("/ral-lattice/tmp/work/.exarch.toml")],
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
    let verbs = |v: &[ShellVerb]| layer(shell_of(v));
    let mut stack = GrantStack::root();
    assert!(stack.permits(detach), "ambient authority permits");
    stack.push(with_fs(FsPolicy::default()));
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

/// A stack crosses the wire as its layers' traces, which the peer takes hold
/// of again: the rules, the exec keys and the exclusions' holds survive it.
#[test]
fn ipc_roundtrip_preserves_a_frozen_grant_stack() {
    let stack = test_helper::stack([layer(witness_a()), layer(witness_b())]);
    let json = serde_json::to_string(&stack).unwrap();
    let back: GrantStack = serde_json::from_str(&json).unwrap();
    assert_eq!(stack, back);
}

/// `p` as an allowance in force.
fn allowing(p: &FrozenPath) -> GrantPath<Extent> {
    GrantPath::<Trace>::walk(p.clone()).expect("a test spelling walks")
}

/// `p` as an exclusion in force.
fn excluding(p: &FrozenPath) -> GrantPath<Excluded> {
    GrantPath::<Excluded>::walk(p.clone()).expect("a test spelling walks")
}

fn fs_policy(p: &FrozenPath) -> FsPolicy {
    FsPolicy {
        read_prefixes: vec![p.clone()],
        write_prefixes: vec![p.clone()],
        deny_paths: Vec::new(),
    }
}

fn fs_of(p: &FrozenPath) -> FsLayer {
    FsLayer::new([allowing(p)], [allowing(p)], [])
}

fn fs_deny_of(p: &FrozenPath) -> FsLayer {
    FsLayer::new([], [], [excluding(p)])
}

fn exec_of(p: &FrozenPath) -> ExecRules {
    std::iter::once(Rule::Allow(ExecAllow::Dir(allowing(p)))).collect()
}

fn exec_deny_of(p: &FrozenPath) -> ExecRules {
    std::iter::once(Rule::Deny(Box::new(ExecDeny::Dir(excluding(p))))).collect()
}

/// A link to `target` at `to`.
fn link(target: &Path, to: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, to).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(target, to).unwrap();
}

/// A tempdir and the real path of its root.
fn scratch() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = test_helper::real(tmp.path()).as_path().to_path_buf();
    (tmp, root)
}

/// Covers nesting, aliasing (`a/alias` resolves to `a`) and symlink
/// divergence (`a/link` resolves to `elsewhere`), over a tempdir tree.
fn prefix_universe(root: &Path) -> Vec<FrozenPath> {
    for dir in ["a/sub", "elsewhere"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    link(&root.join("a"), &root.join("a/alias"));
    link(&root.join("elsewhere"), &root.join("a/link"));
    ["a", "a/sub", "a/alias", "a/link"]
        .map(|dir| spelled(&root.join(dir)))
        .into()
}

/// The prefix universe folded through an allow-only and a deny-only table
/// alike, so the laws below reach exclusions, which `fs_of` on its own never
/// enters.
fn fs_universe(prefixes: &[FrozenPath]) -> Vec<FsLayer> {
    prefixes
        .iter()
        .flat_map(|p| [fs_of(p), fs_deny_of(p)])
        .collect()
}

/// As [`fs_universe`], for exec.
fn exec_universe(prefixes: &[FrozenPath]) -> Vec<ExecRules> {
    prefixes
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
    let stack = test_helper::stack([layer(Capabilities::deny_all())]);
    for flag in every_flag() {
        assert!(!stack.permits(flag), "{} must be withheld", flag.key());
    }
}

/// What a stack decides: each verb, an exec admission, and the net opinions.
fn judgments(stack: &GrantStack, ls: &str) -> (Vec<bool>, bool, Vec<Net>) {
    (
        every_flag().into_iter().map(|f| stack.permits(f)).collect(),
        admits(stack, ls),
        stack.net().collect(),
    )
}

/// Layers that supply an axis only to list every verb, or none at all.
fn neutral_layers() -> [GrantLayer; 4] {
    let editor: BTreeSet<_> = EditorVerb::VARIANTS.iter().copied().collect();
    let shell: BTreeSet<_> = ShellVerb::VARIANTS.iter().copied().collect();
    [
        GrantLayer::root(),
        layer(Capabilities {
            editor: Some(editor.clone()),
            ..Default::default()
        }),
        layer(Capabilities {
            shell: Some(shell.clone()),
            ..Default::default()
        }),
        layer(Capabilities {
            editor: Some(editor),
            shell: Some(shell),
            ..Default::default()
        }),
    ]
}

/// `()`, a full editor list, a full shell list and both judge as ambient
/// authority does, and leave a stack inactive.
#[test]
fn neutral_layers_judge_as_ambient_and_leave_it_inactive() {
    let tmp = tempfile::tempdir().unwrap();
    let ls = Tree::lay(tmp.path(), ["/bin/ls"]).at("/bin/ls");
    let ambient = GrantStack::root();
    for layer in neutral_layers() {
        assert!(!layer.is_restrictive(), "{layer:?} must be neutral");
        let mut stack = GrantStack::root();
        stack.push(layer);
        assert!(!stack.is_restrictive());
        assert_eq!(judgments(&stack, &ls), judgments(&ambient, &ls));
    }
}

/// A neutral layer, or all of them nested, changes neither the judgments nor
/// the activation of a stack withholding a shell verb and the net.
#[test]
fn neutral_layers_leave_a_restrictive_stack_as_it_was() {
    let tmp = tempfile::tempdir().unwrap();
    let ls = Tree::lay(tmp.path(), ["/bin/ls"]).at("/bin/ls");
    let mut base = GrantStack::root();
    base.push(layer(Capabilities {
        net: Some(Net::Offline),
        ..shell_of(&[ShellVerb::Chdir])
    }));
    let judged = judgments(&base, &ls);
    assert!(base.is_restrictive() && !base.permits(Flag::Shell(ShellVerb::Detach)));

    let mut nested = base.clone();
    for layer in neutral_layers() {
        let mut stack = base.clone();
        stack.push(layer.clone());
        nested.push(layer);
        for stack in [&stack, &nested] {
            assert!(stack.is_restrictive());
            assert_eq!(judgments(stack, &ls), judged);
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
            .exec()
            .cloned()
            .expect("exec supplied")
    };
    let (rel, abs) = (
        tagged("cwd", "ral-lattice/tools/x"),
        tagged("absolute", &Tree::nowhere().at("/ral-lattice/tools/x")),
    );
    let forward = decode(vec![rel.clone(), abs.clone()]);
    assert_eq!(forward.allows().count(), 1);
    assert_eq!(forward, decode(vec![abs, rel]));

    let (_tmp, root) = scratch();
    std::fs::create_dir(root.join("real")).unwrap();
    std::fs::write(root.join("real/x"), "").unwrap();
    for alias in ["a", "b"] {
        link(&root.join("real"), &root.join(alias));
    }
    let key = |alias: &str| ExecKey::Path(spelled(&root.join(alias).join("x")));
    let linked: ExecGrant = [(key("a"), Evidence::ALLOW), (key("b"), Evidence::ALLOW)]
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
    let tools = Tree::nowhere().at("/tools");
    assert_eq!(path_key("/tools").to_string(), format!("`absolute {tools}"));
    assert_eq!(
        dir_key("/tools").to_string(),
        format!("`dir `absolute {tools}")
    );
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
    let caps = test_helper::stack([layer(witness_a()), layer(witness_b())]);
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
    let layer = with_xdg_defaults(|| decode_capability_map(&v, "test", &test_ctx("/h")))
        .expect("known places freeze");
    let allowed: Vec<&str> = (layer.exec().unwrap().allows())
        .filter_map(|a| match a {
            ExecAllow::Dir(dir) => Some(dir.written.as_str()),
            _ => None,
        })
        .collect();
    assert!(allowed.contains(&"/h/.local/bin"));
    assert!(allowed.contains(&"/usr/bin"));
    // A list is a set, so it freezes in the written paths' own order.
    let reads: Vec<&str> = (layer.fs().unwrap().table(&FsOp::Read).allows())
        .filter_map(|a| match a {
            FsAllow::Prefix(prefix) => Some(prefix.written.as_str()),
            FsAllow::RunPlace(_) => None,
        })
        .collect();
    assert_eq!(reads, ["/etc", "/h/notes"]);
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

/// A bare name is a name, not a path, so no place rule applies to it; it
/// resolves at decode to what this host finds under it.
// Unix-only: `sh` is on the `PATH` of every Unix host.
#[cfg(unix)]
#[test]
fn decode_accepts_bare_exec_name() {
    let v = map(vec![(
        "exec",
        map(vec![("allow", Value::list(vec![tagged("name", "sh")]))]),
    )]);
    let layer = decode_capability_map(&v, "test", &test_ctx("/h")).expect("a bare command name");
    let exec = layer.summary().and_then(|s| s.exec).expect("exec supplied");
    assert_eq!(exec.allows, ["`name sh"]);
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
    let layer = decode_capability_map(&v, "test", &test_ctx("/h")).expect("verbs and `offline`");
    let shown = layer.summary().expect("a layer that withholds");
    assert_eq!(
        shown.editor,
        Some([EditorVerb::Read, EditorVerb::Tui].into())
    );
    assert_eq!(shown.shell, Some([ShellVerb::Chdir].into()));
    assert_eq!(shown.net, Some(Net::Offline));
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

/// The root of the tree [`at`] names places in.
fn test_cwd_path() -> crate::path::CwdPath {
    crate::path::CwdPath::new(Tree::nowhere().root().to_path_buf())
        .expect("a fixture cwd is absolute")
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
    let every = grant([(name("git"), Evidence::BOTH)]).with_every();
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

/// `⊕` is commutative, associative and idempotent on `universe`.
fn semilattice<T: Join + Clone + PartialEq + Debug>(universe: &[T]) {
    for a in universe {
        for b in universe {
            assert_eq!(a.clone().join(b.clone()), b.clone().join(a.clone()));
            for c in universe {
                assert_eq!(
                    a.clone().join(b.clone().join(c.clone())),
                    a.clone().join(b.clone()).join(c.clone())
                );
            }
        }
        assert_eq!(a.clone().join(a.clone()), *a);
    }
}

/// `⊕` is a semilattice on the fs layers and the exec tables, exclusions
/// included.
#[test]
fn join_is_a_semilattice_over_the_prefix_universe() {
    let (_tmp, root) = scratch();
    let prefixes = prefix_universe(&root);
    semilattice(&fs_universe(&prefixes));
    semilattice(&exec_universe(&prefixes));
}

/// An allowance and an exclusion of one key are Both, which `↓` refuses,
/// whichever is joined first.
#[test]
fn joining_an_allowance_and_an_exclusion_is_both() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = Tree::lay(tmp.path(), ["/x/bin"]);
    let x = spelled(&tree.path("/x"));
    let (allow, deny) = (exec_of(&x), exec_deny_of(&x));
    let inside = trace(tree.path("/x/bin"));
    for joined in [allow.clone().join(deny.clone()), deny.join(allow)] {
        assert_eq!(runs(&joined, &inside), Evidence::BOTH);
        let stack = test_helper::stack([exec_layer(joined)]);
        assert!(!admits(&stack, tree.path("/x/bin")));
    }
}

fn fragment(caps: Capabilities) -> Fragment {
    Fragment::of(layer(caps))
}

/// An omitted fragment axis is the identity, and an omitted base axis stays
/// unrestricted under a positive-only fragment.
#[test]
fn extension_reads_omission_by_role() {
    let a = layer(witness_a());
    assert_eq!(a.clone().extend(fragment(Capabilities::default())), a);
    let positive = fragment(Capabilities {
        exec: Some(grant([(name("git"), Evidence::ALLOW)])),
        fs: Some(fs_policy(&nprefix("/ral-lattice/tmp"))),
        ..Capabilities::default()
    });
    let extended = GrantLayer::root().extend(positive);
    assert!(!extended.is_restrictive(), "{extended:?}");
    let stack = test_helper::stack([extended]);
    assert!(stack.admits_fs(&Floor::default(), &FsOp::Read, &trace(at("/anywhere"))));
}

/// An exclusion in the fragment over an omitted base axis is "every request,
/// less the exclusion".
#[test]
fn extension_with_an_exclusion_over_an_omitted_axis_is_every_less_it() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = Tree::lay(tmp.path(), ["/anywhere/else", "/bad/prog", "/secret/f"]);
    let excluding = fragment(Capabilities {
        exec: Some(grant([(
            ExecKey::Dir(spelled(&tree.path("/bad"))),
            Evidence::DENY,
        )])),
        fs: Some(FsPolicy {
            deny_paths: vec![spelled(&tree.path("/secret"))],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    });
    let extended = GrantLayer::root().extend(excluding);
    assert!(extended.is_restrictive());
    assert!((extended.exec()).is_some_and(|r| r.allows().any(|a| matches!(a, ExecAllow::Any))));
    let stack = test_helper::stack([extended]);
    assert!(admits(&stack, tree.path("/anywhere/else")));
    assert!(!admits(&stack, tree.path("/bad/prog")));
    for op in [FsOp::Read, FsOp::Write] {
        let floor = Floor::default();
        assert!(stack.admits_fs(&floor, &op, &trace(tree.path("/anywhere"))));
        assert!(!stack.admits_fs(&floor, &op, &trace(tree.path("/secret/f"))));
    }
}

/// Extension joins verb sets by union, and the net veto is a floor.
#[test]
fn extension_unions_verbs_and_keeps_the_net_veto() {
    let permits_all = |layer: GrantLayer| {
        let stack = test_helper::stack([layer]);
        every_flag().into_iter().all(|f| stack.permits(f))
    };
    let extended =
        layer(shell_of(&[ShellVerb::Chdir])).extend(fragment(shell_of(&[ShellVerb::Detach])));
    assert!(permits_all(extended));
    for extension in [&[][..], &[ShellVerb::Detach], ShellVerb::VARIANTS] {
        let extended = GrantLayer::root().extend(fragment(shell_of(extension)));
        assert_eq!(extended, GrantLayer::root());
        assert!(permits_all(extended));
    }
    let offline = Capabilities {
        net: Some(Net::Offline),
        ..Capabilities::default()
    };
    let nets = |layer: GrantLayer| test_helper::stack([layer]).net().collect::<Vec<_>>();
    assert_eq!(
        nets(GrantLayer::root().extend(fragment(offline.clone()))),
        [Net::Offline]
    );
    assert_eq!(
        nets(layer(offline).extend(fragment(Capabilities::default()))),
        [Net::Offline]
    );
}

/// A base that omits exec but restricts fs, extended with a run allowance by
/// name: the allowance stays beside `every`, so what it names is read, and a
/// file at another real path under the same name still runs.
#[cfg(unix)]
#[test]
fn an_extension_run_allowance_over_an_omitted_exec_reads_what_it_names() {
    let base = with_fs(FsPolicy {
        read_prefixes: vec![nprefix("/work")],
        ..FsPolicy::default()
    });
    let extended = base.extend(fragment(Capabilities {
        exec: Some(grant([(
            path_key("/ral-lattice/opt/prog"),
            Evidence::ALLOW,
        )])),
        ..Capabilities::default()
    }));
    let stack = test_helper::stack([extended]);
    let floor = Floor::default();
    assert!(
        stack
            .read(&floor, &trace(at("/ral-lattice/opt/prog")))
            .is_ok()
    );
    assert!(
        stack
            .read(&floor, &trace(at("/ral-lattice/opt/other")))
            .is_err()
    );
}

// ── Random policies ───────────────────────────────────────────────────────

use super::exec::tests::Rng;

const LAW_DIRS: [&str; 4] = ["/a", "/a/b", "/z", "/Z"];
const LAW_NAMES: [&str; 3] = ["x", "ls", "cat"];
const LAW_FILES: [&str; 6] = ["/a/x", "/a/b/x", "/a/b/c/x", "/z/x", "/q/x", "/Z/x"];

/// The random policies' universe, laid on disk: the directories they name and
/// the files they judge, one of each object the volume holds.  A volume that
/// folds case holds `z` and `Z` as one, and the laws run over that one.
struct Universe {
    _tmp: tempfile::TempDir,
    dirs: Vec<String>,
    files: Vec<String>,
}

impl Universe {
    fn lay() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = LAW_DIRS.map(|dir| format!("{dir}/"));
        let tree = Tree::lay(tmp.path(), dirs.iter().map(String::as_str).chain(LAW_FILES));
        let objects = |spelled: &[&str]| {
            let mut seen = BTreeSet::new();
            let held = spelled.iter().map(|name| tree.at(name));
            held.filter(|path| seen.insert(trace(path).leaf_id()))
                .collect()
        };
        Self {
            dirs: objects(&LAW_DIRS),
            files: objects(&LAW_FILES),
            _tmp: tmp,
        }
    }
}

fn some(rng: &mut Rng, n: usize) -> bool {
    rng.below(n) == 0
}

/// A random exec grant over the universe, `every` set one time in four where
/// the grant is a policy and not an authored fragment.
fn rand_exec(u: &Universe, rng: &mut Rng, every_ok: bool) -> Option<ExecGrant> {
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
    for d in &u.dirs {
        if some(rng, 3) {
            entries.push((ExecKey::Dir(FrozenPath::from_surface(d)), evidence(rng)));
        }
    }
    for f in &u.files {
        if some(rng, 4) {
            entries.push((ExecKey::Path(FrozenPath::from_surface(f)), evidence(rng)));
        }
    }
    for n in LAW_NAMES {
        if some(rng, 3) {
            entries.push((ExecKey::Name(n.into()), evidence(rng)));
        }
    }
    let grant: ExecGrant = entries.into_iter().collect();
    Some(if every_ok && some(rng, 4) {
        grant.with_every()
    } else {
        grant
    })
}

fn rand_fs(u: &Universe, rng: &mut Rng) -> Option<FsPolicy> {
    if some(rng, 3) {
        return None;
    }
    let pick = |rng: &mut Rng, n| {
        (u.dirs.iter())
            .filter(|_| some(rng, n))
            .map(|p| FrozenPath::from_surface(p))
            .collect::<Vec<_>>()
    };
    Some(FsPolicy {
        read_prefixes: pick(rng, 3),
        write_prefixes: pick(rng, 3),
        deny_paths: pick(rng, 5),
    })
}

/// The layer of an authored `exec` and `fs`.
fn rand_layer_of(exec: Option<ExecGrant>, fs: Option<FsPolicy>) -> GrantLayer {
    layer(Capabilities {
        exec,
        fs,
        ..Capabilities::default()
    })
}

fn rand_layer(u: &Universe, rng: &mut Rng) -> GrantLayer {
    rand_layer_of(rand_exec(u, rng, true), rand_fs(u, rng))
}

fn rand_stack(u: &Universe, rng: &mut Rng) -> GrantStack {
    let mut stack = GrantStack::default();
    (0..=rng.below(3)).for_each(|_| stack.push(rand_layer(u, rng)));
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
    rules(stack).is_none_or(|r| runs(&r, &trace(real)).permits())
}

/// Law 4 on exec: an allowance never removes authority, an exclusion always
/// wins, and nothing is permitted that neither side supports.
#[test]
fn law_4_extension_invariant_on_exec() {
    let (u, mut rng) = (Universe::lay(), Rng(0x1357_9bdf_2468_ace0));
    for _ in 0..1500 {
        let (base, fragment_exec) = (
            rand_exec(&u, &mut rng, true),
            rand_exec(&u, &mut rng, false),
        );
        let base_layer = rand_layer_of(base.clone(), None);
        let frag = rand_layer_of(fragment_exec.clone(), None);
        let extended = base_layer.clone().extend(Fragment::of(frag.clone()));
        for f in &u.files {
            let fe = (frag.exec()).map_or(Evidence::NEITHER, |t| runs(t, &trace(f)));
            let (b, e) = (
                exec_permits(&test_helper::stack([base_layer.clone()]), f),
                exec_permits(&test_helper::stack([extended.clone()]), f),
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
    let (u, mut rng) = (Universe::lay(), Rng(0x0fed_cba9_8765_4321));
    let floor = Floor::default();
    for _ in 0..1500 {
        let (base, frag) = (rand_fs(&u, &mut rng), rand_fs(&u, &mut rng));
        let (base_layer, frag_layer) = (
            rand_layer_of(None, base.clone()),
            rand_layer_of(None, frag.clone()),
        );
        let extended = base_layer.clone().extend(Fragment::of(frag_layer.clone()));
        let evidence = |layer: &GrantLayer, op: &FsOp, f: &str| {
            let stack = test_helper::stack([layer.clone()]);
            region(&stack, op, &floor).map(|r| r.evidence(path::Subject::Access(&trace(f))))
        };
        for op in [FsOp::Read, FsOp::Write] {
            for f in &u.files {
                let fe = evidence(&frag_layer, &op, f).unwrap_or(Evidence::NEITHER);
                let permits =
                    |layer: &GrantLayer| evidence(layer, &op, f).is_none_or(Evidence::permits);
                let (b, e) = (permits(&base_layer), permits(&extended));
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
    let (u, mut rng) = (Universe::lay(), Rng(0x0bad_cafe_f00d_5eed));
    let floor = Floor::default();
    for _ in 0..1500 {
        let base = rand_layer_of(rand_exec(&u, &mut rng, true), rand_fs(&u, &mut rng));
        let (frag_exec, frag_fs) = (rand_exec(&u, &mut rng, false), rand_fs(&u, &mut rng));
        let frag = rand_layer_of(frag_exec.clone(), frag_fs.clone());
        let extended = base.clone().extend(Fragment::of(frag));
        let own = rand_layer_of(frag_exec.clone(), Some(frag_fs.clone().unwrap_or_default()));
        let read = |layer: &GrantLayer, f: &str| {
            running_region(&test_helper::stack([layer.clone()]), &floor)
                .map_or(Evidence::ALLOW, |r| {
                    r.evidence(path::Subject::Access(&trace(f)))
                })
        };
        for f in &u.files {
            let (fe, b, e) = (
                read(&own, f),
                read(&base, f).permits(),
                read(&extended, f).permits(),
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
        let caps = |rng: &mut Rng| Capabilities {
            shell: rand_verbs(rng),
            editor: rand_verbs(rng),
            net: some(rng, 2).then_some(Net::Offline),
            ..Capabilities::default()
        };
        let (base, frag) = (caps(&mut rng), caps(&mut rng));
        let extended = layer(base.clone()).extend(Fragment::of(layer(frag.clone())));
        let (b, f, e) = (
            test_helper::stack([layer(base)]),
            test_helper::stack([layer(frag.clone())]),
            test_helper::stack([extended]),
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
/// where the spelling walks to, as the projection decides it.
// Unix-only: Windows has no `/` to link to.
#[cfg(unix)]
#[test]
fn a_root_reached_through_a_symlink_denotes_top() {
    let (_tmp, root) = scratch();
    std::os::unix::fs::symlink("/", root.join("root-alias")).unwrap();
    std::fs::create_dir(root.join("work")).unwrap();
    let writes_top = |dir: &str| {
        let writing = FsLayer::new([], [allowing(&spelled(&root.join(dir)))], []);
        test_helper::stack([GrantLayer::new(Some(writing), None, None, None, None)]).fs_is_top()
    };
    assert!(writes_top("root-alias"));
    assert!(!writes_top("work"));
}

/// An allowance keeps what its walk passed: a chain's middle link lies on
/// neither the spelling nor the position.
#[cfg(unix)]
#[test]
fn an_allowance_keeps_each_link_its_spelling_passed() {
    let (_tmp, root) = scratch();
    std::fs::create_dir(root.join("target")).unwrap();
    link(&root.join("target"), &root.join("l2"));
    link(&root.join("l2"), &root.join("l1"));
    let allowed = allowing(&spelled(&root.join("l1")));
    let wire = serde_json::to_string(&allowed).unwrap();
    assert_eq!(
        serde_json::from_str::<GrantPath<Extent>>(&wire).unwrap(),
        allowed
    );
    let GrantPath {
        real: Extent::Under { position, passed },
        ..
    } = allowed
    else {
        panic!("a link to a directory is not the root");
    };
    assert_eq!(position.as_path(), root.join("target"));
    let passed: Vec<PathBuf> = passed.iter().map(|p| p.as_path().to_path_buf()).collect();
    assert_eq!(passed, [root.join("l1"), root.join("l2")]);
}

/// Activation reads denotations: an axis that denotes `t` activates nothing.
#[test]
fn activation_is_denotational() {
    let every = grant([(name("git"), Evidence::ALLOW)]).with_every();
    let writes_root = FsPolicy {
        write_prefixes: vec![FrozenPath::root()],
        ..FsPolicy::default()
    };
    let top = layer(Capabilities {
        exec: Some(every),
        fs: Some(writes_root.clone()),
        ..Capabilities::default()
    });
    assert!(top.exec().is_some_and(Axis::is_top));
    assert!(top.fs().is_some_and(Axis::is_top));
    assert!(!top.is_restrictive());
    assert!(!test_helper::stack([top]).is_restrictive());
    let excluding = with_exec(grant([(dir_key("/bad"), Evidence::DENY)]).with_every());
    assert!(
        excluding.is_restrictive(),
        "an `every` table with exclusions restricts"
    );
    let denying = with_fs(FsPolicy {
        deny_paths: vec![nprefix("/bad")],
        ..writes_root
    });
    assert!(!denying.fs().is_some_and(Axis::is_top));
    assert!(with_exec(ExecGrant::default()).is_restrictive());
    assert!(
        layer(Capabilities {
            net: Some(Net::Offline),
            ..Capabilities::default()
        })
        .is_restrictive()
    );
}

/// The read judgment agrees with the region of run places after `↓`, and cites
/// an exclusion that applies; law 7 for a host file and for a bundled tool.
#[test]
fn the_read_and_run_judgments_agree_with_their_regions() {
    let (u, mut rng) = (Universe::lay(), Rng(0x9876_5432_10fe_dcba));
    let floor = Floor::default();
    for _ in 0..800 {
        let stack = rand_stack(&u, &mut rng);
        let full = running_region(&stack, &floor);
        for f in &u.files {
            let access = trace(f);
            let subject = path::Subject::Access(&access);
            let read = stack.read(&floor, &access);
            let want = full.as_ref().is_none_or(|r| r.evidence(subject).permits());
            assert_eq!(read.is_ok(), want, "{f}\nstack = {stack:?}");
            if let Err(Withheld::Excluded { scope, matched_by }) = &read {
                use super::table::Denies;
                assert_eq!(
                    scope.matches(subject),
                    Some(*matched_by),
                    "{scope:?} does not hold {f} as cited"
                );
            }
            let runs = stack.admit(&floor, file(f), vec![]).is_ok();
            assert_eq!(
                runs,
                exec_permits(&stack, f) && want,
                "{f}\nstack = {stack:?}"
            );
        }
        let tool = (stack.admit(&floor, Program::Tool("ls".into()), vec![])).is_ok();
        let tool_exec = rules(&stack).is_none_or(|r| r.evidence(Subject::Tool("ls")).permits());
        assert_eq!(tool, tool_exec, "stack = {stack:?}");
    }
}

/// Law 8: whatever the stack lets be written, it lets be read.
#[test]
fn law_8_writing_reads() {
    let (u, mut rng) = (Universe::lay(), Rng(0x0bad_cafe_f00d_1234));
    let floor = Floor::default();
    for _ in 0..1000 {
        let stack = rand_stack(&u, &mut rng);
        for f in &u.files {
            let access = trace(f);
            assert!(
                stack.write(&floor, &access).is_err() || stack.read(&floor, &access).is_ok(),
                "{f}\nstack = {stack:?}"
            );
        }
    }
}

/// Law 10: pushing a layer permits nothing the stack did not, for running,
/// reading and writing alike.
#[test]
fn law_10_restriction_attenuates() {
    let (u, mut rng) = (Universe::lay(), Rng(0x5555_aaaa_3333_cccc));
    let floor = Floor::default();
    for _ in 0..1000 {
        let stack = rand_stack(&u, &mut rng);
        let mut pushed = stack.clone();
        pushed.push(rand_layer(&u, &mut rng));
        for f in &u.files {
            let access = trace(f);
            let msg = format!("{f}\nstack = {stack:?}\npushed = {pushed:?}");
            assert!(
                pushed.admit(&floor, file(f), vec![]).is_err()
                    || stack.admit(&floor, file(f), vec![]).is_ok(),
                "run {msg}"
            );
            assert!(
                pushed.read(&floor, &access).is_err() || stack.read(&floor, &access).is_ok(),
                "read {msg}"
            );
            assert!(
                pushed.write(&floor, &access).is_err() || stack.write(&floor, &access).is_ok(),
                "write {msg}"
            );
        }
    }
}

/// Under a ceiling that reads nothing and omits exec, a nested layer allowing
/// `File(s)` makes `s` neither readable nor runnable.
#[test]
fn a_nested_run_allowance_cannot_make_what_the_ceiling_withholds_readable() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = Tree::lay(tmp.path(), ["/opt/prog"]);
    let prog = tree.path("/opt/prog");
    let mut stack = test_helper::stack([with_fs(FsPolicy::default())]);
    stack.push(with_exec(grant([(
        ExecKey::Path(spelled(&prog)),
        Evidence::ALLOW,
    )])));
    let floor = Floor::default();
    assert!(stack.read(&floor, &trace(&prog)).is_err());
    assert!(stack.admit(&floor, file(&prog), vec![]).is_err());
}

/// A bundled tool runs wherever exec permits it and no fs exclusion covers
/// ral's binary; an exec exclusion of ral leaves it alone.
#[test]
fn a_bundled_tool_reads_ral_s_binary() {
    crate::sandbox::pin();
    let Some(own) = ral_binary() else { return };
    let floor = Floor::default();
    let stack = |fs: FsPolicy| test_helper::stack([with_fs(fs)]);
    let tool = |s: &GrantStack| (s.admit(&floor, Program::Tool("ls".into()), vec![])).is_ok();
    let own_file = spelled(own.real().as_path());
    let own_dir = spelled(own.real().as_path().parent().unwrap());
    let excluding = stack(FsPolicy {
        read_prefixes: vec![FrozenPath::root()],
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
        tool(&test_helper::stack([with_exec(exec)])),
        "an exec exclusion of ral's binary leaves a bundled tool alone"
    );
}

/// A bundled tool reads ral's binary, so an fs deny of that file under another
/// name — a hard link beside it — refuses the tool: the exclusion holds the
/// object, not the spelling.
#[cfg(unix)]
#[test]
fn a_bundled_tool_is_refused_when_ral_s_binary_is_denied_through_a_hard_link() {
    crate::sandbox::pin();
    let Some(own) = ral_binary() else { return };
    let beside = tempfile::tempdir_in(own.real().as_path().parent().unwrap()).unwrap();
    let link = beside.path().join("ral-linked");
    if std::fs::hard_link(own.real().as_path(), &link).is_err() {
        eprintln!("skip: ral's binary cannot be hard-linked beside itself here");
        return;
    }
    let floor = Floor::default();
    let tool = |fs: FsPolicy| {
        let stack = test_helper::stack([with_fs(fs)]);
        stack
            .admit(&floor, Program::Tool("ls".into()), vec![])
            .is_ok()
    };
    assert!(
        !tool(FsPolicy {
            read_prefixes: vec![FrozenPath::root()],
            deny_paths: vec![spelled(&link)],
            ..FsPolicy::default()
        }),
        "the binary is denied under the link's name"
    );
    assert!(
        tool(FsPolicy {
            read_prefixes: vec![FrozenPath::root()],
            ..FsPolicy::default()
        }),
        "and the link alone, undenied, refuses nothing"
    );
}

/// `dir /` runs every program beneath it, narrowed by what it meets, and runs
/// no bundled tool; alone it reads what it runs (law 7) with no fs grant.
#[test]
fn a_root_directory_runs_files_not_tools_and_reads_what_it_runs() {
    let (_tmp, root) = scratch();
    for dir in ["opt", "usr/bin"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        std::fs::write(root.join(dir).join("t"), "").unwrap();
    }
    let (opt_t, bin_t) = (root.join("opt/t"), root.join("usr/bin/t"));
    let floor = Floor::default();
    let program = |p: &Path| Program::File {
        located: Box::new(test_helper::located(p)),
        arg0: p.to_path_buf(),
    };
    let under_root = ExecKey::Dir(FrozenPath::root());
    let runs_anywhere = with_exec(grant([(under_root.clone(), Evidence::ALLOW)]));
    let runs_in_opt = with_exec(grant([(
        ExecKey::Dir(spelled(&root.join("opt"))),
        Evidence::ALLOW,
    )]));

    let met = test_helper::stack([runs_anywhere.clone(), runs_in_opt]);
    assert!(met.admits(&floor, &program(&opt_t)));
    assert!(!met.admits(&floor, &program(&bin_t)));

    let alone = test_helper::stack([runs_anywhere]);
    assert!(alone.admits(&floor, &program(&bin_t)));
    assert!(!alone.admits(&floor, &Program::Tool("ls".into())));

    let reads_only_what_it_runs = test_helper::stack([layer(Capabilities {
        exec: Some(grant([(under_root, Evidence::ALLOW)])),
        fs: Some(FsPolicy::default()),
        ..Capabilities::default()
    })]);
    assert!(reads_only_what_it_runs.read(&floor, &trace(&bin_t)).is_ok());
}

/// The root spelling is the allowance everywhere on every host, made with no
/// walk; as an exclusion it is refused.
#[test]
fn the_root_key_resolves_with_no_walk() {
    let before = crate::test_access::walk_steps();
    let root = GrantPath::<Trace>::walk(FrozenPath::root()).unwrap();
    assert_eq!(root.real, Extent::Everywhere);
    assert_eq!(crate::test_access::walk_steps(), before);
    assert!(GrantPath::<Excluded>::walk(FrozenPath::root()).is_err());
}

/// The counter the test above reads sees a walk of the root, which opens the
/// root and takes no component step.
#[cfg(unix)]
#[test]
fn a_walk_of_the_root_is_counted() {
    let root = crate::path::LexicalPath::absolute(std::path::Path::new("/")).unwrap();
    let before = crate::test_access::walk_steps();
    drop(crate::path::walk(&root, crate::path::LeafMode::Follow).unwrap());
    assert!(crate::test_access::walk_steps() > before);
}
