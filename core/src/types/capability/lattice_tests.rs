//! Lattice-algebra tests for the capability types.
//!
//! In-crate rather than under `core/tests/` because these reach crate-private
//! doors: `decode_capability_map`, `NormalizedPrefix::for_test`,
//! `RealPath::assumed`.

use super::*;
use crate::capability::{Program, admits_head, decode_capability_map};
use crate::path::RealPath;
use crate::runtime::command::Head;
use crate::types::{Context, PolicyError, Value};

/// The `Value::Map` shape `decode_capability_map` receives in production.
fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

fn strs(items: &[&str]) -> Value {
    Value::list(items.iter().map(|s| Value::string(*s)).collect())
}

/// A path literal in the host's normal form (`/usr/bin` → `\usr\bin` on
/// Windows), so the Unix-shaped fixtures below still mean something there.
fn np(s: &str) -> String {
    nprefix(s).into_string()
}

/// A prefix witness for the fixtures that build `FsPolicy`/`ExecGrant`
/// directly rather than through `decode_capability_map`.
fn nprefix(s: &str) -> crate::path::NormalizedPrefix {
    crate::path::NormalizedPrefix::from_surface(s)
}

fn break_msg(e: PolicyError) -> String {
    e.message
}

fn only(subs: &[&str]) -> Verdict {
    Verdict::Only(subs.iter().map(ToString::to_string).collect())
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
    }
}

/// Whether `grants` admits running the file whose real path is `real`.
fn admits(grants: &GrantStack, real: &str) -> bool {
    let mut ctx = Context::default();
    ctx.grants = grants.clone();
    let head = Head {
        shown: real.into(),
        program: Ok(file(real)),
    };
    admits_head(&ctx, &head)
}

#[cfg(unix)]
use crate::test_env::{with_var, with_vars_cleared};

/// Clears the `XDG_*_HOME` overrides so `xdg:` sigils resolve to the
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
            (name("git"), only(&["log", "status"])),
            (path_key("/opt/tool"), Verdict::Allow),
            (dir_key("/usr/bin"), Verdict::Allow),
        ])),
        fs: Some(FsPolicy {
            read_prefixes: vec![nprefix("/tmp")],
            write_prefixes: vec![nprefix("/tmp")],
            deny_paths: vec![nprefix("/tmp/secret")],
        }),
        net: Some(true),
        detach: Some(true),
        editor: Some(EditorPolicy {
            read: true,
            write: true,
            tui: false,
        }),
        shell: Some(ShellPolicy { chdir: true }),
    }
}

fn witness_b() -> Capabilities {
    Capabilities {
        exec: Some(grant([
            (name("cargo"), only(&["build"])),
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
        net: Some(false),
        detach: Some(false),
        editor: Some(EditorPolicy {
            read: true,
            write: false,
            tui: true,
        }),
        shell: Some(ShellPolicy { chdir: false }),
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
        detach: None,
        editor: None,
        shell: None,
    }
}

/// Absence is not denial: the axis composes by meet like every other, so only
/// an explicit `detach: false` withholds, and no inner frame gives it back.
#[test]
fn detach_is_permitted_until_some_layer_withholds_it() {
    let deny = |d: Option<bool>| Capabilities {
        detach: d,
        ..Default::default()
    };
    let mut stack = GrantStack::root();
    assert!(stack.permits_detach(), "ambient authority permits");
    stack.push(Capabilities {
        fs: Some(FsPolicy::default()),
        ..Default::default()
    });
    assert!(
        stack.permits_detach(),
        "a frame that attenuates only fs leaves the verb alone"
    );
    stack.push(deny(Some(false)));
    assert!(!stack.permits_detach(), "an explicit withholding denies");
    stack.push(deny(Some(true)));
    assert!(
        !stack.permits_detach(),
        "and no inner frame can grant back what an outer one withheld"
    );
}

/// A veto is a floor: an overlay naming `bash: 'allow'` over a base
/// `bash: 'deny'` leaves bash denied.  To permit it, change the base.
#[test]
fn widen_exec_regrant_does_not_lift_deny() {
    let caps = |v: Verdict| Capabilities {
        exec: Some(grant([(name("bash"), v)])),
        ..Default::default()
    };
    let widened = caps(Verdict::Deny).widen(caps(Verdict::Allow));
    assert_eq!(
        widened.exec.unwrap().0.get(&name("bash")),
        Some(&Verdict::Deny)
    );
}

/// A one-sided `Deny` is a floor under widening too: an extension opening one
/// command must not re-admit every shell the base pinned out.
#[test]
fn widen_exec_keeps_one_sided_deny() {
    let exec = grant([(name("bash"), Verdict::Deny)]).widen(grant([(name("rg"), Verdict::Allow)]));
    assert_eq!(exec.0.get(&name("bash")), Some(&Verdict::Deny));
    assert_eq!(exec.0.get(&name("rg")), Some(&Verdict::Allow));
}

/// Deny-overrides on dirs: re-granting the exact denied tree does not lift it.
#[test]
fn widen_exec_dirs_regrant_does_not_lift_deny() {
    let exec = exec_deny_of(&nprefix("/x")).widen(exec_of(&nprefix("/x")));
    assert_eq!(exec.0.get(&dir_key("/x")), Some(&Verdict::Deny));
}

#[test]
fn widen_exec_dirs_keep_one_sided_deny() {
    let exec = exec_deny_of(&nprefix("/opt/danger")).widen(exec_of(&nprefix("/usr/bin")));
    assert_eq!(exec.0.get(&dir_key("/opt/danger")), Some(&Verdict::Deny));
    assert_eq!(exec.0.get(&dir_key("/usr/bin")), Some(&Verdict::Allow));
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

fn fs_of(p: &crate::path::NormalizedPrefix) -> FsPolicy {
    FsPolicy {
        read_prefixes: vec![p.clone()],
        write_prefixes: vec![p.clone()],
        deny_paths: Vec::new(),
    }
}

fn exec_of(p: &crate::path::NormalizedPrefix) -> ExecGrant {
    grant([(ExecKey::Dir(p.clone()), Verdict::Allow)])
}

/// Both dimensions at once, so the laws below also exercise the `Option` lift
/// and the cross-field composition, not one policy type in isolation.
fn caps_of(p: &crate::path::NormalizedPrefix) -> Capabilities {
    Capabilities {
        exec: Some(exec_of(p)),
        fs: Some(fs_of(p)),
        ..Default::default()
    }
}

/// Covers nesting, aliasing (`/a/alias` resolves to `/a`) and symlink
/// divergence (`/a/link` resolves to `/elsewhere`).
fn prefix_universe() -> Vec<crate::path::NormalizedPrefix> {
    vec![
        crate::path::NormalizedPrefix::for_test("/a", "/a"),
        crate::path::NormalizedPrefix::for_test("/a/sub", "/a/sub"),
        crate::path::NormalizedPrefix::for_test("/a/alias", "/a"),
        crate::path::NormalizedPrefix::for_test("/a/link", "/elsewhere"),
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

fn exec_deny_of(p: &crate::path::NormalizedPrefix) -> ExecGrant {
    grant([(ExecKey::Dir(p.clone()), Verdict::Deny)])
}

fn stack_of(exec: ExecGrant) -> GrantStack {
    GrantStack::of(Capabilities {
        exec: Some(exec),
        ..Capabilities::root()
    })
}

fn allow_dirs(exec: &ExecGrant) -> Vec<&crate::path::NormalizedPrefix> {
    (exec.0.iter())
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
    let allow = crate::path::NormalizedPrefix::for_test(surface, surface);
    let deny = crate::path::NormalizedPrefix::for_test(surface, divergent);

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
    let allow = crate::path::NormalizedPrefix::for_test(link_a, target);
    let deny = crate::path::NormalizedPrefix::for_test(link_b, target);

    let composed = exec_of(&allow).widen(exec_deny_of(&deny));
    assert!(
        allow_dirs(&composed).is_empty(),
        "the deny must evict the allow it shares a target with, got {:?}",
        composed.0
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
    let allow = crate::path::NormalizedPrefix::for_test(target, target);
    let deny = crate::path::NormalizedPrefix::for_test(link, target);

    let composed = exec_of(&allow).widen(exec_deny_of(&deny));
    assert!(
        allow_dirs(&composed).is_empty(),
        "the deny must evict the allow on its target, got {:?}",
        composed.0
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
    let allow = exec_of(&crate::path::NormalizedPrefix::from_surface(
        "/private/tmp/x",
    ));
    let deny = exec_deny_of(&crate::path::NormalizedPrefix::from_surface("/tmp/x"));

    let composed = allow.widen(deny);
    assert!(
        allow_dirs(&composed).is_empty(),
        "the deny must evict the alias-clashing allow, got {:?}",
        composed.0
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
    let p = |s: &str| crate::path::NormalizedPrefix::from_surface(s);
    let composed = exec_of(&p("/a/b")).widen(exec_deny_of(&p("/a/B")));
    assert!(
        allow_dirs(&composed).is_empty(),
        "a deny on `/a/B` must evict the allow on `/a/b`, got {:?}",
        composed.0
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

#[test]
fn widen_none_is_identity() {
    let a = witness_a();
    assert_eq!(a.clone().widen(Capabilities::default()), a);
    assert_eq!(Capabilities::default().widen(a.clone()), a);
}

/// Boolean vetoes are floors under base extension: an extension may add an
/// opinion where the base is silent, but may not turn a base `false` into `true`.
#[test]
fn widen_boolean_vetoes_are_sticky() {
    let widened = witness_a().widen(witness_b());
    let editor = widened.editor.unwrap();
    let shell = widened.shell.unwrap();

    assert_eq!(widened.net, Some(false));
    assert_eq!(widened.detach, Some(false));
    assert!(editor.read);
    assert!(!editor.write);
    assert!(!editor.tui);
    assert!(!shell.chdir);
}

#[test]
fn widen_exec_widens_verdicts_and_unions_keys() {
    let exec = witness_a().widen(witness_b()).exec.unwrap();
    assert_eq!(exec.0.get(&name("cargo")), Some(&Verdict::Allow));
    assert_eq!(exec.0.get(&name("ls")), Some(&Verdict::Allow));
    assert_eq!(exec.0.get(&name("git")), Some(&only(&["log", "status"])));
    assert_eq!(exec.0.get(&path_key("/opt/tool")), Some(&Verdict::Deny));
}

/// The stack keeps a one-sided `Only` restriction: layer A restricts `git`
/// to `status` over an allowed binary directory, layer B repeats only the
/// directory allow.  The stack meets the layers' tables pointwise, so the
/// restriction survives whichever layer sits on top.
#[test]
fn stack_keeps_a_one_sided_subcommand_restriction() {
    // What counts as absolute is the host's own answer: a leading `/` roots
    // a path on Unix and is merely drive-relative on Windows.  So the pair is
    // spelled for the host running the test, as the guard's own dir-match
    // tests are.
    let (bin_dir, git_path) = if cfg!(windows) {
        (r"C:\ral-test\bin", r"C:\ral-test\bin\git")
    } else {
        ("/ral-test/bin", "/ral-test/bin/git")
    };
    let restricting = Capabilities {
        exec: Some(grant([
            (path_key(git_path), only(&["status"])),
            (dir_key(bin_dir), Verdict::Allow),
        ])),
        ..Default::default()
    };
    let silent = Capabilities {
        exec: Some(exec_of(&nprefix(bin_dir))),
        ..Default::default()
    };
    for (first, second) in [(restricting.clone(), silent.clone()), (silent, restricting)] {
        let mut shell = crate::types::Shell::default();
        shell.with_capabilities(first, |sh| {
            sh.with_capabilities(second, |sh| {
                let check = |sh: &mut crate::types::Shell, arg: &str| {
                    sh.check_exec("git", file(git_path), vec![arg.to_string()])
                        .map(drop)
                };
                check(sh, "push")
                    .expect_err("A's restriction must survive whichever layer sits on top");
                check(sh, "status").expect("the admitted subcommand must still be allowed");
            });
        });
    }
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

/// With the `XDG_*_HOME` overrides cleared every token resolves under the
/// synthetic home, so the escape guard passes and the freeze succeeds.
// Unix-only: a driveless `/usr/bin` fails the post-freeze absoluteness
// check on Windows.
#[cfg(unix)]
#[test]
fn decode_accepts_known_tokens() {
    let v = map(vec![
        (
            "exec",
            map(vec![
                ("xdg:bin/", Value::string("allow")),
                ("/usr/bin/", Value::string("allow")),
            ]),
        ),
        (
            "fs",
            map(vec![
                (
                    "read",
                    strs(&["xdg:config", "xdg:data/agda", "~/.cache", "/etc"]),
                ),
                ("write", strs(&["xdg:cache"])),
                ("deny", strs(&["xdg:config/secret"])),
            ]),
        ),
    ]);
    with_xdg_defaults(|| decode_capability_map(&v, "test", &test_ctx("/h")))
        .expect("known tokens should decode and freeze");
}

/// A typo in the `xdg:` namespace fails at decode, before any resolution
/// against the environment, rather than silently matching nothing at runtime.
#[test]
fn decode_rejects_xdg_typo() {
    let v = map(vec![("fs", map(vec![("read", strs(&["xdg:cofnig"]))]))]);
    let err = break_msg(decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err());
    assert!(err.contains("xdg:cofnig"), "got {err}");
    assert!(err.contains("config"), "should list known kinds: {err}");
}

/// Decode rewrites every sigil to a concrete absolute path, so matching is
/// decoupled from any later env mutation.
// Unix-only: sigil expansion joins via `PathBuf`, which on Windows yields
// backslashes against the synthetic `/h` home, so `/h/.local/bin` never lands.
#[cfg(unix)]
#[test]
fn decode_rewrites_sigils_to_concrete_paths() {
    let v = map(vec![
        (
            "exec",
            map(vec![
                ("xdg:bin/", Value::string("allow")),
                ("/usr/bin/", Value::string("allow")),
            ]),
        ),
        ("fs", map(vec![("read", strs(&["~/notes", "/etc"]))])),
    ]);
    let caps = with_xdg_defaults(|| decode_capability_map(&v, "test", &test_ctx("/h")))
        .expect("known sigils freeze");
    // Dir keys are stored slash-free.
    let exec = caps.exec.unwrap();
    let allowed = allow_dirs(&exec);
    assert!(allowed.iter().any(|p| p.as_str() == "/h/.local/bin"));
    assert!(allowed.iter().any(|p| p.as_str() == "/usr/bin"));
    let reads = caps.fs.unwrap().read_prefixes;
    assert_eq!(reads[0], "/h/notes");
    assert_eq!(reads[1], "/etc");
}

/// The trailing slash is all that separates a directory grant from a literal
/// one, so omitting it would decode to a grant on a binary that cannot exist
/// and fail closed at use time as a bare "denied by active grant".
#[cfg(unix)]
#[test]
fn decode_rejects_directory_as_literal_command() {
    let v = map(vec![("exec", map(vec![("/etc", Value::string("allow"))]))]);
    let err = break_msg(decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err());
    assert!(err.contains("/etc/"), "should hint the slash: {err}");
}

/// Defence in depth: `XDG_DATA_HOME=/etc` must not widen a policy naming
/// `xdg:data`; decode rejects it and names the offending env var.
// Unix-only: the boundary check compares Unix path prefixes.
#[cfg(unix)]
#[test]
fn decode_rejects_xdg_var_outside_home() {
    let v = map(vec![("fs", map(vec![("read", strs(&["xdg:data"]))]))]);
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

/// No home is a configuration error, not a silent allow — whether it arrives
/// as the absence the readers report or as the empty binding `HOME=` gives.
#[test]
fn decode_errors_when_there_is_no_home() {
    let v = map(vec![("fs", map(vec![("read", strs(&["~/x"]))]))]);
    for ctx in [test_ctx(""), no_home_ctx()] {
        let err = break_msg(decode_capability_map(&v, "test", &ctx).unwrap_err());
        assert!(err.contains("HOME"), "got {err}");
    }
}

/// A bare relative prefix survives freeze unchanged and would anchor to the
/// live cwd at check time, so the same grant would shift meaning after a `cd`.
#[test]
fn decode_rejects_bare_relative_fs_path() {
    let v = map(vec![("fs", map(vec![("read", strs(&["proj"]))]))]);
    let err = break_msg(decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err());
    assert!(err.contains("proj"), "should name the entry: {err}");
    assert!(err.contains("cwd:"), "should hint the cwd: sigil: {err}");
}

#[test]
fn decode_rejects_dot_relative_fs_paths() {
    let v = map(vec![("fs", map(vec![("read", strs(&["./a", "../b"]))]))]);
    assert!(decode_capability_map(&v, "test", &test_ctx("/h")).is_err());
}

/// It carries a `/`, so it is a path, not a bare command name.
#[test]
fn decode_rejects_relative_exec_literal() {
    let v = map(vec![("exec", map(vec![("./foo", Value::string("allow"))]))]);
    assert!(decode_capability_map(&v, "test", &test_ctx("/h")).is_err());
}

/// A bare name is a name, not a path, so the absoluteness rule spares it.
#[test]
fn decode_accepts_bare_exec_name() {
    let v = map(vec![("exec", map(vec![("git", Value::string("allow"))]))]);
    let caps =
        decode_capability_map(&v, "test", &test_ctx("/h")).expect("bare command name is exempt");
    assert!(caps.exec.unwrap().0.contains_key(&name("git")));
}

/// `cwd:proj` freezes to an absolute path — the sanctioned "relative to here"
/// the bare-relative rejection points at.
// Unix-only: the joined `/h` cwd is driveless, so Windows calls it relative.
#[cfg(unix)]
#[test]
fn decode_accepts_cwd_relative_fs_path() {
    let v = map(vec![("fs", map(vec![("read", strs(&["cwd:proj"]))]))]);
    decode_capability_map(&v, "test", &test_ctx("/h"))
        .expect("cwd: sigil freezes to an absolute path");
}

/// A non-Bool is a hard decode error naming the expected type, not a silent
/// fold to `false` that would quietly deny the capability.
#[test]
fn decode_rejects_non_bool_editor_field() {
    let v = map(vec![("editor", map(vec![("write", Value::string("yes"))]))]);
    let err = break_msg(decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err());
    assert!(err.contains("Bool"), "should name the expected type: {err}");
}

#[test]
fn decode_rejects_non_bool_shell_field() {
    let v = map(vec![("shell", map(vec![("chdir", Value::Int(5))]))]);
    let err = break_msg(decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err());
    assert!(err.contains("Bool"), "should name the expected type: {err}");
}

#[test]
fn decode_accepts_bool_dimension_fields() {
    let v = map(vec![
        (
            "editor",
            map(vec![
                ("read", Value::Bool(true)),
                ("write", Value::Bool(false)),
                ("tui", Value::Bool(true)),
            ]),
        ),
        ("shell", map(vec![("chdir", Value::Bool(true))])),
        ("net", Value::Bool(false)),
    ]);
    let caps = decode_capability_map(&v, "test", &test_ctx("/h"))
        .expect("genuine Bools decode to policy fields");
    assert_eq!(
        caps.editor,
        Some(EditorPolicy {
            read: true,
            write: false,
            tui: true,
        })
    );
    assert_eq!(caps.shell, Some(ShellPolicy { chdir: true }));
    assert_eq!(caps.net, Some(false));
}

fn test_ctx(home: &str) -> crate::path::sigil::FreezeCtx<'_> {
    crate::path::sigil::FreezeCtx {
        home: Some(home),
        cwd: crate::path::test_cwd(),
    }
}

fn no_home_ctx() -> crate::path::sigil::FreezeCtx<'static> {
    crate::path::sigil::FreezeCtx {
        home: None,
        cwd: crate::path::test_cwd(),
    }
}
