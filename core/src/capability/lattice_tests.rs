//! Lattice-algebra tests for the capability types.
//!
//! In-crate rather than under `core/tests/` because these reach crate-private
//! doors: `decode_capability_map`, `FrozenPath::for_test`,
//! `RealPath::assumed`.

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

/// A path literal in the host's normal form (`/usr/bin` → `\usr\bin` on
/// Windows), so the Unix-shaped fixtures below still mean something there.
fn np(s: &str) -> String {
    nprefix(s).into_string()
}

/// A prefix witness for the fixtures that build `FsPolicy`/`ExecGrant`
/// directly rather than through `decode_capability_map`.
fn nprefix(s: &str) -> crate::path::FrozenPath {
    crate::path::FrozenPath::from_surface(s)
}

fn break_msg(e: PolicyError) -> String {
    e.message
}

fn grant<const N: usize>(entries: [(ExecKey, Verdict); N]) -> ExecGrant {
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
    Program::File {
        path: real.into(),
        real: RealPath::assumed(real),
        #[cfg(target_os = "linux")]
        image: None,
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
            (name("cargo"), Verdict::Allow),
            (name("git"), Verdict::Allow),
            (path_key("/opt/tool"), Verdict::Allow),
            (dir_key("/usr/bin"), Verdict::Allow),
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
            (name("cargo"), Verdict::Allow),
            (name("ls"), Verdict::Allow),
            (path_key("/opt/tool"), Verdict::Deny),
            (dir_key("/usr/bin"), Verdict::Allow),
            (dir_key("/usr/local/bin"), Verdict::Allow),
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

fn witness_c() -> Capabilities {
    Capabilities {
        exec: Some(grant([(name("cargo"), Verdict::Allow)])),
        fs: Some(FsPolicy {
            read_prefixes: vec![nprefix("/tmp")],
            write_prefixes: Vec::new(),
            deny_paths: Vec::new(),
        }),
        net: None,
        editor: None,
        shell: None,
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

/// A veto is a floor: an overlay allowing `name bash` over a base
/// denying it leaves bash denied.  To permit it, change the base.
#[test]
fn widen_exec_regrant_does_not_lift_deny() {
    let caps = |v: Verdict| Capabilities {
        exec: Some(grant([(name("bash"), v)])),
        ..Default::default()
    };
    let widened = caps(Verdict::Deny).widen(caps(Verdict::Allow));
    assert_eq!(
        widened.exec.unwrap().get(&name("bash")),
        Some(&Verdict::Deny)
    );
}

/// A one-sided `Deny` is a floor under widening too: an extension opening one
/// command must not re-admit every shell the base pinned out.
#[test]
fn widen_exec_keeps_one_sided_deny() {
    let exec = grant([(name("bash"), Verdict::Deny)]).widen(grant([(name("rg"), Verdict::Allow)]));
    assert_eq!(exec.get(&name("bash")), Some(&Verdict::Deny));
    assert_eq!(exec.get(&name("rg")), Some(&Verdict::Allow));
}

/// Deny-overrides on dirs: re-granting the exact denied tree does not lift it.
#[test]
fn widen_exec_dirs_regrant_does_not_lift_deny() {
    let exec = exec_deny_of(&nprefix("/x")).widen(exec_of(&nprefix("/x")));
    assert_eq!(exec.get(&dir_key("/x")), Some(&Verdict::Deny));
}

#[test]
fn widen_exec_dirs_keep_one_sided_deny() {
    let exec = exec_deny_of(&nprefix("/opt/danger")).widen(exec_of(&nprefix("/usr/bin")));
    assert_eq!(exec.get(&dir_key("/opt/danger")), Some(&Verdict::Deny));
    assert_eq!(exec.get(&dir_key("/usr/bin")), Some(&Verdict::Allow));
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
    grant([(ExecKey::Dir(p.clone()), Verdict::Allow)])
}

/// Both dimensions at once, so the laws below also exercise the `Option` lift
/// and the cross-field composition, not one policy type in isolation.
fn caps_of(p: &crate::path::FrozenPath) -> Capabilities {
    Capabilities {
        exec: Some(exec_of(p)),
        fs: Some(fs_of(p)),
        ..Default::default()
    }
}

/// Covers nesting, aliasing (`/a/alias` resolves to `/a`) and symlink
/// divergence (`/a/link` resolves to `/elsewhere`).
fn prefix_universe() -> Vec<crate::path::FrozenPath> {
    vec![
        crate::path::FrozenPath::for_test("/a", "/a"),
        crate::path::FrozenPath::for_test("/a/sub", "/a/sub"),
        crate::path::FrozenPath::for_test("/a/alias", "/a"),
        crate::path::FrozenPath::for_test("/a/link", "/elsewhere"),
    ]
}

#[test]
fn widen_commutative_over_prefix_universe() {
    let u = prefix_universe();
    for a in &u {
        for b in &u {
            assert_eq!(fs_of(a).widen(fs_of(b)), fs_of(b).widen(fs_of(a)));
            assert_eq!(exec_of(a).widen(exec_of(b)), exec_of(b).widen(exec_of(a)));
            assert_eq!(caps_of(a).widen(caps_of(b)), caps_of(b).widen(caps_of(a)));
        }
    }
}

#[test]
fn widen_associative_over_prefix_universe() {
    let u = prefix_universe();
    for a in &u {
        for b in &u {
            for c in &u {
                assert_eq!(
                    fs_of(a).widen(fs_of(b).widen(fs_of(c))),
                    fs_of(a).widen(fs_of(b)).widen(fs_of(c))
                );
                assert_eq!(
                    exec_of(a).widen(exec_of(b).widen(exec_of(c))),
                    exec_of(a).widen(exec_of(b)).widen(exec_of(c))
                );
                assert_eq!(
                    caps_of(a).widen(caps_of(b).widen(caps_of(c))),
                    caps_of(a).widen(caps_of(b)).widen(caps_of(c))
                );
            }
        }
    }
}

#[test]
fn widen_idempotent_over_prefix_universe() {
    for a in &prefix_universe() {
        assert_eq!(fs_of(a).widen(fs_of(a)), fs_of(a));
        assert_eq!(exec_of(a).widen(exec_of(a)), exec_of(a));
        assert_eq!(caps_of(a).widen(caps_of(a)), caps_of(a));
    }
}

fn exec_deny_of(p: &crate::path::FrozenPath) -> ExecGrant {
    grant([(ExecKey::Dir(p.clone()), Verdict::Deny)])
}

fn stack_of(exec: ExecGrant) -> GrantStack {
    GrantStack::of(Capabilities {
        exec: Some(exec),
        ..Capabilities::root()
    })
}

fn allow_dirs(exec: &ExecGrant) -> Vec<&crate::path::FrozenPath> {
    (exec.iter())
        .filter_map(|(key, v)| match key {
            ExecKey::Dir(dir) if !v.is_denied() => Some(dir),
            _ => None,
        })
        .collect()
}

/// The in-process guard judges dirs on their resolved forms, so eviction keys on those
/// alone: an allow and a deny sharing a surface but resolving apart are two
/// directories, and neither clash strips the other.  Checked through the
/// guard as well as the allow dirs.
#[test]
fn exec_widen_keeps_allow_and_deny_that_share_a_surface_but_resolve_apart() {
    // Spelled for the host: a rooted path with no drive is not absolute to
    // Windows.
    let (surface, divergent, allowed, denied) = if cfg!(windows) {
        (r"C:\x", r"C:\y", r"C:\x\bin", r"C:\y\bin")
    } else {
        ("/x", "/y", "/x/bin", "/y/bin")
    };
    let allow = crate::path::FrozenPath::for_test(surface, surface);
    let deny = crate::path::FrozenPath::for_test(surface, divergent);

    let composed = exec_of(&allow).widen(exec_deny_of(&deny));
    assert_eq!(
        allow_dirs(&composed).len(),
        1,
        "a different directory is not evicted"
    );
    let grants = stack_of(composed);
    assert!(
        admits(&grants, allowed),
        "the allow still covers its own directory"
    );
    assert!(
        !admits(&grants, denied),
        "the deny covers where it resolves"
    );
}

/// Two symlinks resolving to one directory clash: the deny evicts the allow
/// though neither spelling is the other's.
#[test]
fn exec_widen_drops_allow_resolving_to_the_same_dir_as_a_deny() {
    let (link_a, link_b, target, candidate) = if cfg!(windows) {
        (r"C:\a", r"C:\b", r"C:\x", r"C:\x\bin")
    } else {
        ("/a", "/b", "/x", "/x/bin")
    };
    let allow = crate::path::FrozenPath::for_test(link_a, target);
    let deny = crate::path::FrozenPath::for_test(link_b, target);

    let composed = exec_of(&allow).widen(exec_deny_of(&deny));
    assert!(
        allow_dirs(&composed).is_empty(),
        "the deny must evict the allow it shares a target with, got {composed:?}",
    );
    assert!(
        !admits(&stack_of(composed), candidate),
        "a binary under the shared target must be denied"
    );
}

/// A deny dir that is a symlink vetoes where it points, so it evicts an
/// allow written as that target.
#[test]
fn exec_widen_drops_allow_naming_a_deny_dirs_target() {
    let (link, target, candidate) = if cfg!(windows) {
        (r"C:\l", r"C:\x", r"C:\x\bin")
    } else {
        ("/l", "/x", "/x/bin")
    };
    let allow = crate::path::FrozenPath::for_test(target, target);
    let deny = crate::path::FrozenPath::for_test(link, target);

    let composed = exec_of(&allow).widen(exec_deny_of(&deny));
    assert!(
        allow_dirs(&composed).is_empty(),
        "the deny must evict the allow on its target, got {composed:?}",
    );
    assert!(
        !admits(&stack_of(composed), candidate),
        "a binary under the deny's target must be denied"
    );
}

/// The same clash without shared bytes: `/private/tmp/x` and `/tmp/x` name
/// one macOS firmlink-aliased directory, so eviction keys on `evicts`
/// and not byte equality.  `capability/exec.rs` pins the guard half.
#[cfg(target_os = "macos")]
#[test]
fn exec_widen_drops_allow_clashing_with_deny_on_firmlink_alias() {
    let allow = exec_of(&crate::path::FrozenPath::from_surface("/private/tmp/x"));
    let deny = exec_deny_of(&crate::path::FrozenPath::from_surface("/tmp/x"));

    let composed = allow.widen(deny);
    assert!(
        allow_dirs(&composed).is_empty(),
        "the deny must evict the alias-clashing allow, got {composed:?}",
    );
    assert!(
        !admits(&stack_of(composed), "/tmp/x/bin"),
        "a binary under the aliased surface must be denied"
    );
}

/// U6: a deny holds every spelling of its name, so it evicts an allow on
/// another spelling of its directory, and not one on its parent.
#[test]
fn exec_widen_drops_allow_on_another_spelling_of_a_deny() {
    let p = |s: &str| crate::path::FrozenPath::from_surface(s);
    let composed = exec_of(&p("/a/b")).widen(exec_deny_of(&p("/a/B")));
    assert!(
        allow_dirs(&composed).is_empty(),
        "a deny on `/a/B` must evict the allow on `/a/b`, got {composed:?}",
    );
    let composed = exec_of(&p("/a/b")).widen(exec_deny_of(&p("/a/B/c")));
    assert_eq!(
        allow_dirs(&composed).len(),
        1,
        "a deny on `/a/B/c` must not evict the allow on `/a/b`"
    );
}

/// The prefix universe folded through an allow-only and a deny-only grant
/// alike, so the laws below reach deny-overrides — the dimension `exec_of` on
/// its own never enters.
fn exec_universe() -> Vec<ExecGrant> {
    prefix_universe()
        .iter()
        .flat_map(|p| [exec_of(p), exec_deny_of(p)])
        .collect()
}

#[test]
fn exec_widen_commutative_with_denies() {
    let u = exec_universe();
    for a in &u {
        for b in &u {
            assert_eq!(a.clone().widen(b.clone()), b.clone().widen(a.clone()));
        }
    }
}

#[test]
fn exec_widen_associative_with_denies() {
    let u = exec_universe();
    for a in &u {
        for b in &u {
            for c in &u {
                assert_eq!(
                    a.clone().widen(b.clone().widen(c.clone())),
                    a.clone().widen(b.clone()).widen(c.clone())
                );
            }
        }
    }
}

#[test]
fn exec_widen_idempotent_with_denies() {
    for a in &exec_universe() {
        assert_eq!(a.clone().widen(a.clone()), a.clone());
    }
}

#[test]
fn widen_commutative() {
    let a = witness_a();
    let b = witness_b();
    assert_eq!(a.clone().widen(b.clone()), b.widen(a));
}

#[test]
fn widen_associative() {
    let a = witness_a();
    let b = witness_b();
    let c = witness_c();
    assert_eq!(
        a.clone().widen(b.clone().widen(c.clone())),
        a.widen(b).widen(c)
    );
}

#[test]
fn widen_idempotent() {
    let a = witness_a();
    assert_eq!(a.clone().widen(a.clone()), a);
}

/// An omitted extension axis is the identity.
#[test]
fn widen_omitted_extension_axis_is_the_identity() {
    let a = witness_a();
    assert_eq!(a.clone().widen(Capabilities::default()), a);
}

/// An omitted base verb axis absorbs: it is every verb, which nothing widens.
#[test]
fn widen_omitted_base_verb_axis_absorbs() {
    let a = witness_a();
    let widened = Capabilities::default().widen(a.clone());
    assert_eq!(
        widened,
        Capabilities {
            editor: None,
            shell: None,
            ..a
        }
    );
}

fn shell_of(verbs: &[ShellVerb]) -> Capabilities {
    Capabilities {
        shell: Some(verbs.iter().copied().collect()),
        ..Default::default()
    }
}

/// A base withholding `detach` regains it from an extension; no base can veto
/// a verb against one.
#[test]
fn widen_joins_verb_sets() {
    let widened = shell_of(&[ShellVerb::Chdir]).widen(shell_of(&[ShellVerb::Detach]));
    assert_eq!(
        widened.shell,
        Some(ShellVerb::VARIANTS.iter().copied().collect())
    );
}

/// A base that omits `shell` keeps every verb whatever the extension lists.
#[test]
fn widen_over_a_silent_base_keeps_every_verb() {
    for extension in [&[][..], &[ShellVerb::Detach], ShellVerb::VARIANTS] {
        let widened = Capabilities::default().widen(shell_of(extension));
        assert_eq!(widened.shell, None);
        let stack = GrantStack::of(widened);
        assert!(every_flag().into_iter().all(|f| stack.permits(f)));
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
        (key("/a/x", "/real/x"), Verdict::Allow),
        (key("/b/x", "/real/x"), Verdict::Allow),
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
        ..crate::protocol::Run::captured("echo hi", "<test>")
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

/// The offline veto is a floor under base extension, and an extension's verbs
/// join a base's verb set.
#[test]
fn widen_keeps_the_net_veto_and_unions_verbs() {
    let widened = witness_a().widen(witness_b());
    assert_eq!(widened.net, Some(Net::Offline));
    assert_eq!(
        widened.editor,
        Some(EditorVerb::VARIANTS.iter().copied().collect())
    );
    assert_eq!(widened.shell, witness_a().shell);
}

#[test]
fn widen_exec_widens_verdicts_and_unions_keys() {
    let exec = witness_a().widen(witness_b()).exec.unwrap();
    assert_eq!(exec.get(&name("cargo")), Some(&Verdict::Allow));
    assert_eq!(exec.get(&name("ls")), Some(&Verdict::Allow));
    assert_eq!(exec.get(&name("git")), Some(&Verdict::Allow));
    assert_eq!(exec.get(&path_key("/opt/tool")), Some(&Verdict::Deny));
}

/// A `deny_path` is a sticky veto under widening as under meet, so an
/// extension silent on a base carve-out cannot erode it.
#[test]
fn widen_fs_unions_prefixes_and_denies() {
    let m = witness_a().widen(witness_b());
    let fs = m.fs.unwrap();
    assert!(fs.read_prefixes.iter().any(|p| p == np("/tmp").as_str()));
    assert!(
        fs.read_prefixes
            .iter()
            .any(|p| p == np("/tmp/work").as_str())
    );
    assert!(
        fs.deny_paths
            .iter()
            .any(|p| p == np("/tmp/secret").as_str())
    );
    assert!(
        fs.deny_paths
            .iter()
            .any(|p| p == np("/tmp/work/.exarch.toml").as_str())
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
        err.contains("`xdg-config,"),
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
    assert!(err.hint.unwrap().contains("`dir `absolute /etc`"));
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
    for ctx in [test_ctx(""), no_home_ctx()] {
        let err = break_msg(decode_capability_map(&v, "test", &ctx).unwrap_err());
        assert!(err.contains("HOME"), "got {err}");
    }
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
        err.contains("`cwd proj`"),
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
    FreezeCtx {
        home: Some(home.into()),
        cwd: crate::path::test_cwd().into(),
    }
}

fn no_home_ctx() -> FreezeCtx {
    FreezeCtx {
        home: None,
        cwd: crate::path::test_cwd().into(),
    }
}
