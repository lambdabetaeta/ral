//! Lattice-algebra tests for the capability types.
//!
//! In-crate rather than under `core/tests/` because these reach crate-private
//! doors: `decode_capability_map`, `NormalizedPrefix::for_test`,
//! `admits_for_test`.

use super::*;
use crate::capability::decode_capability_map;
use crate::types::{PolicyError, Value};

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
    Value::list(items.iter().map(|s| Value::String((*s).into())).collect())
}

/// A path literal in the host's normal form (`/usr/bin` → `\usr\bin` on
/// Windows), so the Unix-shaped fixtures below still mean something there.
fn np(s: &str) -> String {
    nprefix(s).into_string()
}

/// A prefix witness for the fixtures that build `FsPolicy`/`ExecMap` directly
/// rather than through `decode_capability_map`.
fn nprefix(s: &str) -> crate::path::NormalizedPrefix {
    crate::path::NormalizedPrefix::from_surface(s)
}

fn break_msg(e: PolicyError) -> String {
    e.message
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
        exec: Some(ExecMap {
            literals: BTreeMap::from([
                ("cargo".into(), ExecPolicy::Allow),
                (
                    "git".into(),
                    ExecPolicy::Subcommands(BTreeSet::from(["log".into(), "status".into()])),
                ),
            ]),
            allow_dirs: BTreeSet::from([nprefix("/usr/bin")]),
            deny_dirs: BTreeSet::new(),
        }),
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
        exec: Some(ExecMap {
            literals: BTreeMap::from([
                (
                    "cargo".into(),
                    ExecPolicy::Subcommands(BTreeSet::from(["build".into()])),
                ),
                ("ls".into(), ExecPolicy::Allow),
            ]),
            allow_dirs: BTreeSet::from([nprefix("/usr/bin"), nprefix("/usr/local/bin")]),
            deny_dirs: BTreeSet::new(),
        }),
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
        exec: Some(ExecMap {
            literals: BTreeMap::from([("cargo".into(), ExecPolicy::Allow)]),
            allow_dirs: BTreeSet::new(),
            deny_dirs: BTreeSet::new(),
        }),
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
fn join_exec_regrant_does_not_lift_deny() {
    let base = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("bash".into(), ExecPolicy::Deny)]),
            allow_dirs: BTreeSet::new(),
            deny_dirs: BTreeSet::new(),
        }),
        ..Default::default()
    };
    let extend = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("bash".into(), ExecPolicy::Allow)]),
            allow_dirs: BTreeSet::new(),
            deny_dirs: BTreeSet::new(),
        }),
        ..Default::default()
    };
    let j = base.join(extend);
    assert_eq!(
        j.exec.unwrap().literals.get("bash"),
        Some(&ExecPolicy::Deny)
    );
}

/// A one-sided `Deny` is a floor under join too: an extension opening one
/// command must not re-admit every shell the base pinned out.
#[test]
fn join_exec_keeps_one_sided_deny() {
    let base = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("bash".into(), ExecPolicy::Deny)]),
            allow_dirs: BTreeSet::new(),
            deny_dirs: BTreeSet::new(),
        }),
        ..Default::default()
    };
    let extend = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("rg".into(), ExecPolicy::Allow)]),
            allow_dirs: BTreeSet::new(),
            deny_dirs: BTreeSet::new(),
        }),
        ..Default::default()
    };
    let exec = base.join(extend).exec.unwrap();
    assert_eq!(exec.literals.get("bash"), Some(&ExecPolicy::Deny));
    assert_eq!(exec.literals.get("rg"), Some(&ExecPolicy::Allow));
}

/// Deny-overrides on dirs: re-granting the exact denied tree does not lift it.
#[test]
fn join_exec_dirs_regrant_does_not_lift_deny() {
    let base = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::new(),
            deny_dirs: BTreeSet::from([nprefix("/x")]),
        }),
        ..Default::default()
    };
    let extend = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::from([nprefix("/x")]),
            deny_dirs: BTreeSet::new(),
        }),
        ..Default::default()
    };
    let exec = base.join(extend).exec.unwrap();
    assert!(exec.deny_dirs.contains(&nprefix("/x")));
    assert!(!exec.allow_dirs.contains(&nprefix("/x")));
}

#[test]
fn join_exec_dirs_keep_one_sided_deny() {
    let base = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::new(),
            deny_dirs: BTreeSet::from([nprefix("/opt/danger")]),
        }),
        ..Default::default()
    };
    let extend = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::from([nprefix("/usr/bin")]),
            deny_dirs: BTreeSet::new(),
        }),
        ..Default::default()
    };
    let exec = base.join(extend).exec.unwrap();
    assert!(exec.deny_dirs.contains(&nprefix("/opt/danger")));
    assert!(exec.allow_dirs.contains(&nprefix("/usr/bin")));
}

/// A `Capabilities` is sigil-free by construction, so the wire form carries
/// concrete paths and the peer has nothing to re-resolve.
#[test]
fn ipc_roundtrip_preserves_frozen_capabilities() {
    let c = witness_a();
    let json = serde_json::to_string(&c).unwrap();
    let back: Capabilities = serde_json::from_str(&json).unwrap();
    assert_eq!(c, back);
}

fn fs_of(p: &crate::path::NormalizedPrefix) -> FsPolicy {
    FsPolicy {
        read_prefixes: vec![p.clone()],
        write_prefixes: vec![p.clone()],
        deny_paths: Vec::new(),
    }
}

fn exec_of(p: &crate::path::NormalizedPrefix) -> ExecMap {
    ExecMap {
        literals: BTreeMap::new(),
        allow_dirs: BTreeSet::from([p.clone()]),
        deny_dirs: BTreeSet::new(),
    }
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

/// Covers nesting, aliasing (`/a/alias` resolves to `/a`), symlink divergence
/// (`/a/link` resolves to `/elsewhere`), and both namespaces, so every law
/// below sees cross-namespace overlap too.
fn prefix_universe() -> Vec<crate::path::NormalizedPrefix> {
    use crate::path::Namespace;
    vec![
        crate::path::NormalizedPrefix::for_test("/a", "/a", Namespace::Host),
        crate::path::NormalizedPrefix::for_test("/a/sub", "/a/sub", Namespace::Host),
        crate::path::NormalizedPrefix::for_test("/a/alias", "/a", Namespace::Host),
        crate::path::NormalizedPrefix::for_test("/a/link", "/elsewhere", Namespace::Host),
        crate::path::NormalizedPrefix::for_test("/a", "/a", Namespace::Guest),
        crate::path::NormalizedPrefix::for_test("/a/sub", "/a/sub", Namespace::Guest),
    ]
}

#[test]
fn join_commutative_over_prefix_universe() {
    let u = prefix_universe();
    for a in &u {
        for b in &u {
            assert_eq!(fs_of(a).join(fs_of(b)), fs_of(b).join(fs_of(a)));
            assert_eq!(exec_of(a).join(exec_of(b)), exec_of(b).join(exec_of(a)));
            assert_eq!(caps_of(a).join(caps_of(b)), caps_of(b).join(caps_of(a)));
        }
    }
}

#[test]
fn join_associative_over_prefix_universe() {
    let u = prefix_universe();
    for a in &u {
        for b in &u {
            for c in &u {
                assert_eq!(
                    fs_of(a).join(fs_of(b).join(fs_of(c))),
                    fs_of(a).join(fs_of(b)).join(fs_of(c))
                );
                assert_eq!(
                    exec_of(a).join(exec_of(b).join(exec_of(c))),
                    exec_of(a).join(exec_of(b)).join(exec_of(c))
                );
                assert_eq!(
                    caps_of(a).join(caps_of(b).join(caps_of(c))),
                    caps_of(a).join(caps_of(b)).join(caps_of(c))
                );
            }
        }
    }
}

#[test]
fn join_idempotent_over_prefix_universe() {
    for a in &prefix_universe() {
        assert_eq!(fs_of(a).join(fs_of(a)), fs_of(a));
        assert_eq!(exec_of(a).join(exec_of(a)), exec_of(a));
        assert_eq!(caps_of(a).join(caps_of(a)), caps_of(a));
    }
}

fn exec_deny_of(p: &crate::path::NormalizedPrefix) -> ExecMap {
    ExecMap {
        literals: BTreeMap::new(),
        allow_dirs: BTreeSet::new(),
        deny_dirs: BTreeSet::from([p.clone()]),
    }
}

/// An allow and a deny sharing a surface but frozen against different disk
/// state are distinct records to `NormalizedPrefix`'s derived `Eq`/`Ord`
/// (three fields, where the gate weighs two), so eviction must key on
/// `same_gate_dir`.  Checked through the gate as well as `allow_dirs` — the
/// leak is only real if the gate itself is fooled.
#[test]
fn exec_join_drops_allow_clashing_with_deny_on_divergent_resolved() {
    use crate::capability::admits_for_test;
    use crate::path::Namespace;

    // The gate weighs only candidates the platform calls absolute, and a
    // rooted path with no drive is not absolute to Windows.
    let (surface, divergent, candidate) = if cfg!(windows) {
        (r"C:\x", r"C:\y", r"C:\x\bin")
    } else {
        ("/x", "/y", "/x/bin")
    };
    let allow = crate::path::NormalizedPrefix::for_test(surface, surface, Namespace::Host);
    let deny = crate::path::NormalizedPrefix::for_test(surface, divergent, Namespace::Host);
    let allow_map = exec_of(&allow);
    let deny_map = exec_deny_of(&deny);

    let composed = allow_map.join(deny_map);
    assert!(
        composed.allow_dirs.is_empty(),
        "the deny must evict the clashing allow from allow_dirs, got {:?}",
        composed.allow_dirs
    );
    let mut grants = GrantStack::root();
    grants.push(Capabilities {
        exec: Some(composed),
        ..Capabilities::root()
    });
    let candidate = [candidate];
    assert!(
        !admits_for_test(&grants, &candidate, &candidate),
        "a binary under the clashing surface must be denied"
    );
}

/// The same clash without shared bytes: `/private/tmp/x` and `/tmp/x` name
/// one macOS firmlink-aliased directory, so eviction keys on `same_gate_dir`
/// and not byte equality.  `capability/exec.rs` pins the gate half.
#[cfg(target_os = "macos")]
#[test]
fn exec_join_drops_allow_clashing_with_deny_on_firmlink_alias() {
    use crate::capability::admits_for_test;

    let allow_map = exec_of(&crate::path::NormalizedPrefix::from_surface(
        "/private/tmp/x",
    ));
    let deny_map = exec_deny_of(&crate::path::NormalizedPrefix::from_surface("/tmp/x"));

    let composed = allow_map.join(deny_map);
    assert!(
        composed.allow_dirs.is_empty(),
        "the deny must evict the alias-clashing allow from allow_dirs, got {:?}",
        composed.allow_dirs
    );
    let mut grants = GrantStack::root();
    grants.push(Capabilities {
        exec: Some(composed),
        ..Capabilities::root()
    });
    let candidate = ["/tmp/x/bin"];
    assert!(
        !admits_for_test(&grants, &candidate, &candidate),
        "a binary under the aliased surface must be denied"
    );
}

/// The prefix universe folded through an allow-only and a deny-only `ExecMap`
/// alike, so the laws below reach deny-overrides — the dimension `exec_of` on
/// its own never enters.
fn exec_universe() -> Vec<ExecMap> {
    prefix_universe()
        .iter()
        .flat_map(|p| [exec_of(p), exec_deny_of(p)])
        .collect()
}

#[test]
fn exec_join_commutative_with_denies() {
    let u = exec_universe();
    for a in &u {
        for b in &u {
            assert_eq!(a.clone().join(b.clone()), b.clone().join(a.clone()));
        }
    }
}

#[test]
fn exec_join_associative_with_denies() {
    let u = exec_universe();
    for a in &u {
        for b in &u {
            for c in &u {
                assert_eq!(
                    a.clone().join(b.clone().join(c.clone())),
                    a.clone().join(b.clone()).join(c.clone())
                );
            }
        }
    }
}

#[test]
fn exec_join_idempotent_with_denies() {
    for a in &exec_universe() {
        assert_eq!(a.clone().join(a.clone()), a.clone());
    }
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
    assert_eq!(a.clone().join(b.clone().join(c.clone())), a.join(b).join(c),);
}

#[test]
fn join_idempotent() {
    let a = witness_a();
    assert_eq!(a.clone().join(a.clone()), a);
}

#[test]
fn join_none_is_identity() {
    let a = witness_a();
    assert_eq!(a.clone().join(Capabilities::default()), a);
    assert_eq!(Capabilities::default().join(a.clone()), a);
}
/// Boolean vetoes are floors under base extension: an extension may add an
/// opinion where the base is silent, but may not turn a base `false` into `true`.
#[test]
fn join_boolean_vetoes_are_sticky() {
    let joined = witness_a().join(witness_b());
    let editor = joined.editor.unwrap();
    let shell = joined.shell.unwrap();

    assert_eq!(joined.net, Some(false));
    assert_eq!(joined.detach, Some(false));
    assert!(editor.read);
    assert!(!editor.write);
    assert!(!editor.tui);
    assert!(!shell.chdir);
}

#[test]
fn join_exec_widens_policies_and_unions_names() {
    let m = witness_a().join(witness_b());
    let exec = m.exec.unwrap();
    assert_eq!(exec.literals.get("cargo"), Some(&ExecPolicy::Allow));
    assert_eq!(exec.literals.get("ls"), Some(&ExecPolicy::Allow));
    match exec.literals.get("git").unwrap() {
        ExecPolicy::Subcommands(s) => {
            assert!(s.iter().any(|x| x == "log") && s.iter().any(|x| x == "status"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

/// The stack keeps a one-sided `Subcommands` restriction that
/// `Capabilities::meet` used to discard: layer A restricts `git` to
/// `status` over an allowed binary directory, layer B repeats only the
/// directory allow.  Flattening the two (the old `ExecMap::meet`) dropped A's literal
/// restriction the moment B's map had no `git` key; the stack's per-layer
/// fold (`evaluate_exec`) never flattens, so the restriction survives
/// whichever layer sits on top.
#[test]
fn stack_keeps_a_one_sided_subcommand_restriction() {
    // `longest_dir_match` only considers an absolute candidate, and what
    // counts as absolute is the host's own answer: a leading `/` roots a
    // path on Unix and is merely drive-relative on Windows.  So the pair is
    // spelled for the host running the test, as the gate's own dir-match
    // tests are.
    let (bin_dir, git_path) = if cfg!(windows) {
        (r"C:\bin", r"C:\bin\git")
    } else {
        ("/usr/bin", "/usr/bin/git")
    };
    let restricting = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([(
                "git".into(),
                ExecPolicy::Subcommands(BTreeSet::from(["status".into()])),
            )]),
            allow_dirs: BTreeSet::from([nprefix(bin_dir)]),
            deny_dirs: BTreeSet::new(),
        }),
        ..Default::default()
    };
    let silent = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::from([nprefix(bin_dir)]),
            deny_dirs: BTreeSet::new(),
        }),
        ..Default::default()
    };
    for (first, second) in [(restricting.clone(), silent.clone()), (silent, restricting)] {
        let mut shell = crate::types::Shell::default();
        shell.with_capabilities(first, |sh| {
            sh.with_capabilities(second, |sh| {
                sh.check_exec_args("git", &["git", git_path], &["push".to_string()])
                    .expect_err("A's restriction must survive whichever layer sits on top");
                sh.check_exec_args("git", &["git", git_path], &["status".to_string()])
                    .expect("the admitted subcommand must still be allowed");
            });
        });
    }
}

/// A `deny_path` is a sticky veto under join as under meet, so an extension
/// silent on a base carve-out cannot erode it.
#[test]
fn join_fs_unions_prefixes_and_denies() {
    let m = witness_a().join(witness_b());
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
                ("xdg:bin/", Value::String("allow".into())),
                ("/usr/bin/", Value::String("allow".into())),
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
                ("xdg:bin/", Value::String("allow".into())),
                ("/usr/bin/", Value::String("allow".into())),
            ]),
        ),
        ("fs", map(vec![("read", strs(&["~/notes", "/etc"]))])),
    ]);
    let caps = with_xdg_defaults(|| decode_capability_map(&v, "test", &test_ctx("/h")))
        .expect("known sigils freeze");
    // Dir keys are stored slash-free.
    let exec = caps.exec.unwrap();
    assert!(
        exec.allow_dirs
            .iter()
            .any(|p| p.as_str() == "/h/.local/bin")
    );
    assert!(exec.allow_dirs.iter().any(|p| p.as_str() == "/usr/bin"));
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
    let v = map(vec![(
        "exec",
        map(vec![("/etc", Value::String("allow".into()))]),
    )]);
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
    let v = map(vec![(
        "exec",
        map(vec![("./foo", Value::String("allow".into()))]),
    )]);
    assert!(decode_capability_map(&v, "test", &test_ctx("/h")).is_err());
}

/// A bare name is a name, not a path, so the absoluteness rule spares it.
#[test]
fn decode_accepts_bare_exec_name() {
    let v = map(vec![(
        "exec",
        map(vec![("git", Value::String("allow".into()))]),
    )]);
    let caps =
        decode_capability_map(&v, "test", &test_ctx("/h")).expect("bare command name is exempt");
    assert!(caps.exec.unwrap().literals.contains_key("git"));
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
    let v = map(vec![(
        "editor",
        map(vec![("write", Value::String("yes".into()))]),
    )]);
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
