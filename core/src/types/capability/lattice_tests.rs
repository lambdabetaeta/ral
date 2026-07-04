//! Lattice-algebra tests for the capability types.
//!
//! Lives as a sibling of `capability.rs` (via `mod lattice_tests;` there)
//! because several tests exercise crate-private helpers (`ExecMap`'s
//! `Meet`/`Join`, `meet_literal_exec`) that an integration test in
//! `core/tests/` cannot see.

#![allow(clippy::disallowed_methods)]

use super::*;
use crate::capability::decode_capability_map;
use crate::types::{Break, Value};

/// Build a `Value::Map` from `(key, value)` pairs — the shape both the
/// `grant` builtin and the capability-file loader feed `decode_capability_map`.
fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(
        entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}

/// Build a `Value::List` of strings — a grant path list.
fn strs(items: &[&str]) -> Value {
    Value::list(items.iter().map(|s| Value::String((*s).into())).collect())
}

/// Unwrap a decode `Break` into its error message.
fn break_msg(b: Break) -> String {
    match b {
        Break::Error(e) => e.message,
        other => panic!("unexpected: {other:?}"),
    }
}

use crate::test_env::env_guard;

/// Run `f` with every `XDG_*_HOME` override removed, restoring them
/// after, so `xdg:` sigils resolve to the home-joined defaults and the
/// escape guard sees paths under the synthetic test home.
fn with_xdg_defaults<R>(f: impl FnOnce() -> R) -> R {
    let _guard = env_guard();
    const VARS: [&str; 5] = [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_BIN_HOME",
    ];
    let saved: Vec<(&str, Option<std::ffi::OsString>)> =
        VARS.iter().map(|v| (*v, std::env::var_os(v))).collect();
    for v in VARS {
        unsafe { std::env::remove_var(v) };
    }
    let out = f();
    for (v, val) in saved {
        match val {
            Some(x) => unsafe { std::env::set_var(v, x) },
            None => unsafe { std::env::remove_var(v) },
        }
    }
    out
}

// ── Capabilities lattice properties ───────────────────────────────

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
            dirs: BTreeMap::from([("/usr/bin".into(), ExecDir::Allow)]),
        }),
        fs: Some(FsPolicy {
            read_prefixes: vec!["/tmp".into()],
            write_prefixes: vec!["/tmp".into()],
            deny_paths: vec!["/tmp/secret".into()],
        }),
        net: Some(true),
        audit: false,
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
            dirs: BTreeMap::from([
                ("/usr/bin".into(), ExecDir::Allow),
                ("/usr/local/bin".into(), ExecDir::Allow),
            ]),
        }),
        fs: Some(FsPolicy {
            read_prefixes: vec!["/tmp/work".into()],
            write_prefixes: vec!["/tmp/work".into()],
            deny_paths: vec!["/tmp/work/.exarch.toml".into()],
        }),
        net: Some(false),
        audit: false,
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
            dirs: BTreeMap::new(),
        }),
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
    assert_eq!(a.clone().meet(b.clone().meet(c.clone())), a.meet(b).meet(c),);
}

#[test]
fn meet_idempotent() {
    let a = witness_a();
    assert_eq!(a.clone().meet(a.clone()), a);
}

/// Idempotence holds on a `Subcommands` verdict whose elements were
/// declared out of alphabetical order: the sole constructor
/// `decode_capability_map` canonicalizes (sorts + dedups) the allowlist,
/// and `meet` preserves that canonical form, so `Eq`, `meet`, and `join`
/// agree and `a.meet(a) == a` regardless of declaration order.
#[test]
fn meet_idempotent_subcommands_ignore_order() {
    let v = map(vec![(
        "exec",
        map(vec![("cargo", strs(&["test", "build"]))]),
    )]);
    let a =
        decode_capability_map(&v, "test", &test_ctx("/h")).expect("subcommand allowlist decodes");
    match a.exec.as_ref().unwrap().literals.get("cargo") {
        Some(ExecPolicy::Subcommands(s)) => {
            assert_eq!(
                s,
                &BTreeSet::from(["build".to_string(), "test".to_string()])
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
    assert_eq!(a.clone().meet(a.clone()), a);
}

#[test]
fn meet_top_is_identity() {
    let a = witness_a();
    assert_eq!(a.clone().meet(Capabilities::default()), a.clone());
    assert_eq!(Capabilities::default().meet(a.clone()), a);
}

#[test]
fn meet_bottom_zeroes_authority() {
    let a = witness_a();
    let m = a.meet(Capabilities::deny_all());
    let exec = m.exec.expect("exec retained");
    assert!(exec.literals.is_empty() && exec.dirs.is_empty());
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
    // one-sided so drop.  Dir half: /usr/bin is shared,
    // /usr/local/bin is one-sided so drops.
    assert!(exec.literals.contains_key("cargo"));
    match exec.literals.get("cargo").unwrap() {
        ExecPolicy::Subcommands(s) => assert_eq!(s, &BTreeSet::from(["build".to_string()])),
        other => panic!("unexpected: {other:?}"),
    }
    assert!(!exec.literals.contains_key("git"));
    assert!(!exec.literals.contains_key("ls"));
    assert!(exec.dirs.contains_key("/usr/bin"));
    assert!(!exec.dirs.contains_key("/usr/local/bin"));
}

/// `Deny` is sticky downward: a base ceiling that vetos `bash`
/// must keep that veto after meet with a restrict file that
/// does not name `bash` at all.  Without this the `reasonable`
/// base would lose its shell deny the moment any `[exec]`-bearing
/// restrict came in.
#[test]
fn meet_exec_preserves_one_sided_deny() {
    let base = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([
                ("ls".into(), ExecPolicy::Allow),
                ("bash".into(), ExecPolicy::Deny),
            ]),
            dirs: BTreeMap::new(),
        }),
        ..Default::default()
    };
    let restrict = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("ls".into(), ExecPolicy::Allow)]),
            dirs: BTreeMap::new(),
        }),
        ..Default::default()
    };
    let m = base.meet(restrict);
    let exec = m.exec.unwrap();
    assert_eq!(exec.literals.get("ls"), Some(&ExecPolicy::Allow));
    assert_eq!(exec.literals.get("bash"), Some(&ExecPolicy::Deny));
}

/// Deny-overrides under join: a veto is a floor that even an explicit
/// same-key re-grant cannot lift.  An extend-base that names
/// `bash: 'allow'` against a base `bash: 'deny'` leaves bash denied —
/// to permit bash you choose a base that allows it, not an overlay.
#[test]
fn join_exec_regrant_does_not_lift_deny() {
    let base = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("bash".into(), ExecPolicy::Deny)]),
            dirs: BTreeMap::new(),
        }),
        ..Default::default()
    };
    let extend = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("bash".into(), ExecPolicy::Allow)]),
            dirs: BTreeMap::new(),
        }),
        ..Default::default()
    };
    let j = base.join(extend);
    assert_eq!(
        j.exec.unwrap().literals.get("bash"),
        Some(&ExecPolicy::Deny)
    );
}

/// The dual of `meet_exec_preserves_one_sided_deny`: a base veto on
/// `bash` survives `--extend-base` against an extension that opens an
/// unrelated command and is silent on `bash`.  Without this, any
/// extension carrying a single exec key would silently re-admit every
/// shell the base pinned out — the extend-base footgun this fix closes.
#[test]
fn join_exec_keeps_one_sided_deny() {
    let base = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("bash".into(), ExecPolicy::Deny)]),
            dirs: BTreeMap::new(),
        }),
        ..Default::default()
    };
    let extend = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("rg".into(), ExecPolicy::Allow)]),
            dirs: BTreeMap::new(),
        }),
        ..Default::default()
    };
    let exec = base.join(extend).exec.unwrap();
    assert_eq!(exec.literals.get("bash"), Some(&ExecPolicy::Deny));
    assert_eq!(exec.literals.get("rg"), Some(&ExecPolicy::Allow));
}

/// Dir `Deny` is sticky downward, exactly as literal `Deny`: a base
/// ceiling that vetos a directory tree must keep that veto after meet
/// with a restrict file that names only an unrelated allow dir.
#[test]
fn meet_exec_dirs_preserve_one_sided_deny() {
    let base = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([
                ("/usr/bin".into(), ExecDir::Allow),
                ("/opt/danger".into(), ExecDir::Deny),
            ]),
        }),
        ..Default::default()
    };
    let restrict = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([("/usr/bin".into(), ExecDir::Allow)]),
        }),
        ..Default::default()
    };
    let dirs = base.meet(restrict).exec.unwrap().dirs;
    assert_eq!(dirs.get("/usr/bin"), Some(&ExecDir::Allow));
    assert_eq!(dirs.get("/opt/danger"), Some(&ExecDir::Deny));
}

/// Security-inversion regression: an exact-key clash between an allow
/// dir and a deny dir must meet to `Deny` (the lattice bottom), never
/// re-emit `Allow` and silently grant the denied tree.
#[test]
fn meet_exec_dirs_deny_beats_allow() {
    let allow = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([("/usr/bin".into(), ExecDir::Allow)]),
        }),
        ..Default::default()
    };
    let deny = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([("/usr/bin".into(), ExecDir::Deny)]),
        }),
        ..Default::default()
    };
    let dirs = allow.meet(deny).exec.unwrap().dirs;
    assert_eq!(dirs.get("/usr/bin"), Some(&ExecDir::Deny));
}

/// Deny-overrides on dirs too: an extend-base that re-grants the exact
/// denied tree does not lift the veto — the deny wins the exact-key
/// clash, mirroring `meet`.
#[test]
fn join_exec_dirs_regrant_does_not_lift_deny() {
    let base = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([("/x".into(), ExecDir::Deny)]),
        }),
        ..Default::default()
    };
    let extend = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([("/x".into(), ExecDir::Allow)]),
        }),
        ..Default::default()
    };
    let dirs = base.join(extend).exec.unwrap().dirs;
    assert_eq!(dirs.get("/x"), Some(&ExecDir::Deny));
}

/// Dir veto is sticky under join too: a base that denies a directory
/// tree keeps that veto when an extension opens only an unrelated tree.
#[test]
fn join_exec_dirs_keep_one_sided_deny() {
    let base = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([("/opt/danger".into(), ExecDir::Deny)]),
        }),
        ..Default::default()
    };
    let extend = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([("/usr/bin".into(), ExecDir::Allow)]),
        }),
        ..Default::default()
    };
    let dirs = base.join(extend).exec.unwrap().dirs;
    assert_eq!(dirs.get("/opt/danger"), Some(&ExecDir::Deny));
    assert_eq!(dirs.get("/usr/bin"), Some(&ExecDir::Allow));
}

/// Meet keeps an allow-region intersection AND a covering deny carved
/// out of it: `{/usr: Allow}` ⊓ `{/usr: Allow, /usr/bin: Deny}` admits
/// `/usr` except the denied `/usr/bin` subtree.
#[test]
fn meet_exec_dirs_intersect_allow_keeps_covering_deny() {
    let broad = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([("/usr".into(), ExecDir::Allow)]),
        }),
        ..Default::default()
    };
    let carved = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            dirs: BTreeMap::from([
                ("/usr".into(), ExecDir::Allow),
                ("/usr/bin".into(), ExecDir::Deny),
            ]),
        }),
        ..Default::default()
    };
    let dirs = broad.meet(carved).exec.unwrap().dirs;
    assert_eq!(dirs.get("/usr"), Some(&ExecDir::Allow));
    assert_eq!(dirs.get("/usr/bin"), Some(&ExecDir::Deny));
}

/// IPC roundtrip: a `Capabilities` survives a JSON trip through the
/// wire format unchanged.  The witness is already resolved (concrete
/// paths, no sigils), as every `Capabilities` is by construction.
#[test]
fn ipc_roundtrip_preserves_frozen_capabilities() {
    let c = witness_a();
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

/// Nesting an inner fs grant inside an outer one narrows authority to the
/// *intersection*: the inner cannot widen beyond the outer.  Meeting an
/// outer `/tmp` (read+write) with an inner `/tmp/work` (read+write) — the
/// shape `capability::sandbox_projection` folds when two `grant [fs:…]`
/// layers stack — keeps only the deeper `/tmp/work` and drops the wider
/// `/tmp`, on *both* read and write.  The wider prefix surviving would let
/// an inner grant escape its parent's bound, so its absence is the load-
/// bearing assertion.
#[test]
fn meet_fs_nested_grants_narrow_to_intersection() {
    let m = witness_a().meet(witness_b());
    let fs = m.fs.unwrap();
    let surface = |ps: &[crate::path::NormalizedPrefix]| -> Vec<String> {
        ps.iter().map(|p| p.as_str().to_string()).collect()
    };
    assert_eq!(
        surface(&fs.read_prefixes),
        ["/tmp/work"],
        "read narrows to the deeper inner prefix, outer /tmp dropped"
    );
    assert_eq!(
        surface(&fs.write_prefixes),
        ["/tmp/work"],
        "write narrows to the deeper inner prefix, outer /tmp dropped"
    );
}

/// Security regression at the composition layer.  A restrict grant whose
/// deeper prefix *lexically* nests under the base ceiling but resolves —
/// through a symlink — outside it must not survive the meet.  Before the
/// meet judged overlap on the resolved form, the escaping prefix survived
/// (dropping the shallower ceiling) and the point-of-use gate then
/// canonicalised it to its out-of-ceiling target and granted access.
/// Judging containment on the resolved form collapses it to the
/// fail-closed empty meet instead — the same guarantee
/// `PrefixSet::symlinked_grant_cannot_escape_a_shallower_ceiling` pins one
/// layer down, here proved end-to-end through `Capabilities::meet`.
#[cfg(unix)]
#[test]
fn meet_fs_symlinked_prefix_cannot_escape_ceiling() {
    use std::os::unix::fs::symlink;

    let root = std::env::temp_dir().join(format!("ral-meet-escape-{}", std::process::id()));
    std::fs::remove_dir_all(&root).ok(); // clear any leftover from a crashed run
    let ceiling = root.join("base");
    let outside = root.join("outside");
    let escape = ceiling.join("link");
    std::fs::create_dir_all(&ceiling).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    symlink(&outside, &escape).unwrap();

    let read_grant = |p: &std::path::Path| Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![crate::path::NormalizedPrefix::from_surface(
                p.to_string_lossy().into_owned(),
            )],
            ..Default::default()
        }),
        ..Default::default()
    };

    let met = read_grant(&ceiling)
        .meet(read_grant(&escape))
        .fs
        .expect("fs retained");
    assert!(
        met.read_prefixes.is_empty(),
        "a symlinked deeper prefix resolving outside the ceiling must collapse \
         to the fail-closed empty meet, got {:?}",
        met.read_prefixes
    );
    std::fs::remove_dir_all(&root).ok();
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
    assert_eq!(a.clone().join(Capabilities::default()), a.clone());
    assert_eq!(Capabilities::default().join(a.clone()), a);
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

/// Widening unions read/write prefixes and preserves every deny: a
/// `deny_path` is a sticky veto, so an extension silent on a base
/// carve-out cannot erode it.  Mirrors `meet`, which also unions denies
/// — a deny survives any composition.
#[test]
fn join_fs_unions_prefixes_and_denies() {
    let m = witness_a().join(witness_b());
    let fs = m.fs.unwrap();
    assert!(fs.read_prefixes.iter().any(|p| p == "/tmp"));
    assert!(fs.read_prefixes.iter().any(|p| p == "/tmp/work"));
    assert!(fs.deny_paths.iter().any(|p| p == "/tmp/secret"));
    assert!(fs.deny_paths.iter().any(|p| p == "/tmp/work/.exarch.toml"));
}

/// Decode accepts known `xdg:` tokens (with and without a sub-path),
/// tilde and absolute paths.  With the `XDG_*_HOME` overrides cleared,
/// every token resolves under the synthetic home, so the escape guard
/// passes and the freeze inside decode succeeds.
// Unix-only: the `/usr/bin` literal and `xdg:`/tilde tokens freeze to
// paths that only satisfy the post-freeze absoluteness check on Unix;
// on Windows a driveless `/usr/bin` is not absolute. Mirrors the gate
// on `decode_rewrites_sigils_to_concrete_paths`.
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

/// A typo in the `xdg:` namespace is caught at decode instead of
/// silently passing through to match nothing at runtime.  Mirrors the
/// `deny_unknown_fields` ethos.  Env-independent: the typo fails to
/// parse before any resolution against the environment.
#[test]
fn decode_rejects_xdg_typo() {
    let v = map(vec![("fs", map(vec![("read", strs(&["xdg:cofnig"]))]))]);
    let err = break_msg(decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err());
    assert!(err.contains("xdg:cofnig"), "got {err}");
    assert!(err.contains("config"), "should list known kinds: {err}");
}

/// Decode rewrites every sigil into a concrete absolute path: the
/// returned `Capabilities` carries no sigils, so subsequent matching is
/// decoupled from any later env mutation.
// Unix-only: sigil expansion joins via `PathBuf`, which on Windows
// produces backslashes against the synthetic `/h` home — the key
// `/h/.local/bin` never lands in the exec map.  The grant subsystem
// is Unix-only.
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
    // Dir keys are stored slash-free; sigil expansion rewrites them to
    // concrete absolute prefixes.
    let exec = caps.exec.unwrap();
    assert!(exec.dirs.contains_key("/h/.local/bin"));
    assert!(exec.dirs.contains_key("/usr/bin"));
    let reads = caps.fs.unwrap().read_prefixes;
    assert_eq!(reads[0], "/h/notes");
    assert_eq!(reads[1], "/etc");
}

/// Defence in depth: a caller who sets `XDG_DATA_HOME=/etc` must not be
/// able to widen a policy that names `xdg:data`.  Decode rejects the
/// resolution at the boundary with a message naming the offending env
/// var so the operator can diagnose it.
// Unix-only: the boundary check compares Unix path prefixes that
// don't survive Windows path normalisation.
#[cfg(unix)]
#[test]
fn decode_rejects_xdg_var_outside_home() {
    let _guard = env_guard();
    let v = map(vec![("fs", map(vec![("read", strs(&["xdg:data"]))]))]);
    let prev = std::env::var_os("XDG_DATA_HOME");
    unsafe { std::env::set_var("XDG_DATA_HOME", "/etc") };
    let err = decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err();
    match prev {
        Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
        None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
    }
    let err = break_msg(err);
    assert!(
        err.contains("XDG_DATA_HOME"),
        "should name the env var: {err}"
    );
    assert!(err.contains("/etc"), "should show the bad value: {err}");
    assert!(err.contains("HOME"), "should mention HOME: {err}");
}

/// Empty `home` is a configuration error, not a silent allow.
/// The check produces a question-shaped message — per the
/// `ral` style, we prefer prompting over guessing.
#[test]
fn decode_errors_when_home_is_empty() {
    let v = map(vec![("fs", map(vec![("read", strs(&["~/x"]))]))]);
    let err = break_msg(decode_capability_map(&v, "test", &test_ctx("")).unwrap_err());
    assert!(err.contains("HOME"), "got {err}");
}

/// A bare relative fs prefix is rejected at decode: it survives freeze
/// unchanged and would otherwise anchor to the live cwd at check time,
/// so the same grant would mean a different directory after a `cd`.
/// The error names the offending entry and points at the `cwd:` sigil.
#[test]
fn decode_rejects_bare_relative_fs_path() {
    let v = map(vec![("fs", map(vec![("read", strs(&["proj"]))]))]);
    let err = break_msg(decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err());
    assert!(err.contains("proj"), "should name the entry: {err}");
    assert!(err.contains("cwd:"), "should hint the cwd: sigil: {err}");
}

/// `.`/`..`-relative fs prefixes are rejected for the same reason.
#[test]
fn decode_rejects_dot_relative_fs_paths() {
    let v = map(vec![("fs", map(vec![("read", strs(&["./a", "../b"]))]))]);
    assert!(decode_capability_map(&v, "test", &test_ctx("/h")).is_err());
}

/// A path-shaped relative exec literal is rejected: it carries a `/`,
/// so it is a path, not a bare command name.
#[test]
fn decode_rejects_relative_exec_literal() {
    let v = map(vec![(
        "exec",
        map(vec![("./foo", Value::String("allow".into()))]),
    )]);
    assert!(decode_capability_map(&v, "test", &test_ctx("/h")).is_err());
}

/// A bare command name in the exec map is a name, not a path, so it is
/// exempt from the absoluteness rule and passes through unchanged.
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

/// `cwd:proj` freezes to an absolute path, so it is accepted — the
/// sanctioned "relative to here" form the rejection message points at.
// Unix-only: `cwd:proj` joins the synthetic `/h` cwd via `PathBuf`,
// which on Windows yields a driveless path that fails the post-freeze
// absoluteness check.
#[cfg(unix)]
#[test]
fn decode_accepts_cwd_relative_fs_path() {
    let v = map(vec![("fs", map(vec![("read", strs(&["cwd:proj"]))]))]);
    decode_capability_map(&v, "test", &test_ctx("/h"))
        .expect("cwd: sigil freezes to an absolute path");
}

/// Root authority imposes no fs or net restriction, so it must never
/// engage the OS sandbox — the projection is empty.
#[test]
fn root_does_not_engage_sandbox() {
    assert!(!Capabilities::root().engages_sandbox());
}

/// A net-deny frame is a real restriction an external process must be
/// confined to, so it engages the sandbox.
#[test]
fn net_deny_engages_sandbox() {
    let caps = Capabilities {
        net: Some(false),
        ..Default::default()
    };
    assert!(caps.engages_sandbox());
}

/// A bool-typed `editor` sub-key must hold a genuine `Bool`.  A non-Bool
/// value is a hard decode error naming the wrong type — not a silent
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

/// `shell.chdir` is bool-typed; a non-Bool value errors via `decode_bool`.
#[test]
fn decode_rejects_non_bool_shell_field() {
    let v = map(vec![("shell", map(vec![("chdir", Value::Int(5))]))]);
    let err = break_msg(decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err());
    assert!(err.contains("Bool"), "should name the expected type: {err}");
}

/// `audit` is bool-typed; a non-Bool value errors rather than silently
/// disabling auditing.
#[test]
fn decode_rejects_non_bool_audit_field() {
    let v = map(vec![("audit", Value::String("true".into()))]);
    let err = break_msg(decode_capability_map(&v, "test", &test_ctx("/h")).unwrap_err());
    assert!(err.contains("Bool"), "should name the expected type: {err}");
}

/// Genuine Bools still decode to the matching policy fields.
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
        ("audit", Value::Bool(true)),
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
    assert!(caps.audit);
}

fn test_ctx(home: &str) -> crate::path::sigil::FreezeCtx<'_> {
    crate::path::sigil::FreezeCtx {
        home,
        cwd: std::path::Path::new("/"),
    }
}
