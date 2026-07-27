//! Lattice-algebra tests for the capability types.
//!
//! Lives as a sibling of `capability.rs` (via `mod lattice_tests;` there)
//! because several tests exercise crate-private surfaces — the
//! `pub(crate)` `decode_capability_map` and the crate-private `FreezeCtx`
//! scaffolding — that an integration test in `core/tests/` cannot see.

use super::*;
use crate::capability::decode_capability_map;
use crate::types::{PolicyError, Value};

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

/// The platform normal form of a path literal.  The capability
/// composition stores dir keys and fs prefixes through
/// `NormalizedPrefix::from_surface`, whose `fold_dots` kernel
/// reconstructs each path with the host separator (`/usr/bin` →
/// `\usr\bin` on Windows).  Test witnesses and expected values pass
/// through the same kernel so the Unix-shaped literals stay meaningful
/// on both Unix and Windows.
fn np(s: &str) -> String {
    nprefix(s).into_string()
}

/// Mint a [`NormalizedPrefix`](crate::path::NormalizedPrefix) witness —
/// used throughout this file's fixtures, which build `FsPolicy`/`ExecMap`
/// values directly rather than through `decode_capability_map`.
fn nprefix(s: &str) -> crate::path::NormalizedPrefix {
    crate::path::NormalizedPrefix::from_surface(s)
}

/// Unwrap a decode `PolicyError` into its message.
fn break_msg(e: PolicyError) -> String {
    e.message
}

#[cfg(unix)]
use crate::test_env::{with_var, with_vars_cleared};

/// Run `f` with every `XDG_*_HOME` override removed, restoring them
/// after, so `xdg:` sigils resolve to the home-joined defaults and the
/// escape guard sees paths under the synthetic test home.
///
/// Unix-only: every consumer below is `#[cfg(unix)]` (Windows path
/// normalisation defeats the literal Unix-shaped assertions).
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
    assert_eq!(a.clone().meet(Capabilities::default()), a);
    assert_eq!(Capabilities::default().meet(a.clone()), a);
}

#[test]
fn meet_bottom_zeroes_authority() {
    let a = witness_a();
    let m = a.meet(Capabilities::deny_all());
    let exec = m.exec.expect("exec retained");
    assert!(exec.literals.is_empty() && exec.allow_dirs.is_empty() && exec.deny_dirs.is_empty());
    let fs = m.fs.expect("fs retained");
    assert!(fs.read_prefixes.is_empty());
    assert!(fs.write_prefixes.is_empty());
    assert_eq!(m.net, Some(false));
    assert_eq!(m.detach, Some(false));
    let ed = m.editor.expect("editor retained");
    assert!(!ed.read && !ed.write && !ed.tui);
    assert!(!m.shell.expect("shell retained").chdir);
}

/// Silence permits, and one `detach: false` anywhere in the stack denies
/// no matter what sits above it — the axis composes by meet like every
/// other, so an ordinary `grant fs: […]` says nothing about survivors.
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
    assert!(exec.allow_dirs.contains(&nprefix("/usr/bin")));
    assert!(!exec.allow_dirs.contains(&nprefix("/usr/local/bin")));
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
            allow_dirs: BTreeSet::new(),
            deny_dirs: BTreeSet::new(),
        }),
        ..Default::default()
    };
    let restrict = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::from([("ls".into(), ExecPolicy::Allow)]),
            allow_dirs: BTreeSet::new(),
            deny_dirs: BTreeSet::new(),
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

/// Dir `Deny` is sticky downward, exactly as literal `Deny`: a base
/// ceiling that vetos a directory tree must keep that veto after meet
/// with a restrict file that names only an unrelated allow dir.
#[test]
fn meet_exec_dirs_preserve_one_sided_deny() {
    let base = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::from([nprefix("/usr/bin")]),
            deny_dirs: BTreeSet::from([nprefix("/opt/danger")]),
        }),
        ..Default::default()
    };
    let restrict = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::from([nprefix("/usr/bin")]),
            deny_dirs: BTreeSet::new(),
        }),
        ..Default::default()
    };
    let exec = base.meet(restrict).exec.unwrap();
    assert!(exec.allow_dirs.contains(&nprefix("/usr/bin")));
    assert!(exec.deny_dirs.contains(&nprefix("/opt/danger")));
}

/// Security-inversion regression: an exact-key clash between an allow
/// dir and a deny dir must meet to `Deny` (the lattice bottom), never
/// re-emit `Allow` and silently grant the denied tree.
#[test]
fn meet_exec_dirs_deny_beats_allow() {
    let allow = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::from([nprefix("/usr/bin")]),
            deny_dirs: BTreeSet::new(),
        }),
        ..Default::default()
    };
    let deny = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::new(),
            deny_dirs: BTreeSet::from([nprefix("/usr/bin")]),
        }),
        ..Default::default()
    };
    let exec = allow.meet(deny).exec.unwrap();
    assert!(exec.deny_dirs.contains(&nprefix("/usr/bin")));
    assert!(!exec.allow_dirs.contains(&nprefix("/usr/bin")));
}

/// Deny-overrides on dirs too: an extend-base that re-grants the exact
/// denied tree does not lift the veto — the deny wins the exact-key
/// clash, mirroring `meet`.
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

/// Dir veto is sticky under join too: a base that denies a directory
/// tree keeps that veto when an extension opens only an unrelated tree.
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

/// Meet keeps an allow-region intersection AND a covering deny carved
/// out of it: `{/usr: Allow}` ⊓ `{/usr: Allow, /usr/bin: Deny}` admits
/// `/usr` except the denied `/usr/bin` subtree.
#[test]
fn meet_exec_dirs_intersect_allow_keeps_covering_deny() {
    let broad = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::from([nprefix("/usr")]),
            deny_dirs: BTreeSet::new(),
        }),
        ..Default::default()
    };
    let carved = Capabilities {
        exec: Some(ExecMap {
            literals: BTreeMap::new(),
            allow_dirs: BTreeSet::from([nprefix("/usr")]),
            deny_dirs: BTreeSet::from([nprefix("/usr/bin")]),
        }),
        ..Default::default()
    };
    let exec = broad.meet(carved).exec.unwrap();
    assert!(exec.allow_dirs.contains(&nprefix("/usr")));
    assert!(exec.deny_dirs.contains(&nprefix("/usr/bin")));
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
        [np("/tmp/work")],
        "read narrows to the deeper inner prefix, outer /tmp dropped"
    );
    assert_eq!(
        surface(&fs.write_prefixes),
        [np("/tmp/work")],
        "write narrows to the deeper inner prefix, outer /tmp dropped"
    );
}

/// A single-prefix fs grant, for folding the prefix universe below through
/// `FsPolicy::meet`/`join` without a directory tree behind it.
fn fs_of(p: &crate::path::NormalizedPrefix) -> FsPolicy {
    FsPolicy {
        read_prefixes: vec![p.clone()],
        write_prefixes: vec![p.clone()],
        deny_paths: Vec::new(),
    }
}

/// A single-prefix exec-dir grant, for the same universe folded through
/// `ExecMap`.
fn exec_of(p: &crate::path::NormalizedPrefix) -> ExecMap {
    ExecMap {
        literals: BTreeMap::new(),
        allow_dirs: BTreeSet::from([p.clone()]),
        deny_dirs: BTreeSet::new(),
    }
}

/// The same single prefix folded through both dimensions of `Capabilities`
/// at once, so the law checks below also exercise the `Option` lift and
/// the cross-field composition, not just one policy type in isolation.
fn caps_of(p: &crate::path::NormalizedPrefix) -> Capabilities {
    Capabilities {
        exec: Some(exec_of(p)),
        fs: Some(fs_of(p)),
        ..Default::default()
    }
}

/// A small universe of `{surface, resolved, namespace}` triples, minted
/// synthetically with `NormalizedPrefix::for_test`.  Covers a plain
/// prefix, a genuine nested descendant, aliasing (`/a/alias` resolves to
/// `/a`), symlink divergence (`/a/link` resolves to `/elsewhere`), and
/// the same pair in both namespaces, so cross-namespace overlap is in
/// scope for every law below.
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
fn meet_commutative_over_prefix_universe() {
    let u = prefix_universe();
    for a in &u {
        for b in &u {
            assert_eq!(fs_of(a).meet(fs_of(b)), fs_of(b).meet(fs_of(a)));
            assert_eq!(exec_of(a).meet(exec_of(b)), exec_of(b).meet(exec_of(a)));
            assert_eq!(caps_of(a).meet(caps_of(b)), caps_of(b).meet(caps_of(a)));
        }
    }
}

#[test]
fn meet_associative_over_prefix_universe() {
    let u = prefix_universe();
    for a in &u {
        for b in &u {
            for c in &u {
                assert_eq!(
                    fs_of(a).meet(fs_of(b).meet(fs_of(c))),
                    fs_of(a).meet(fs_of(b)).meet(fs_of(c))
                );
                assert_eq!(
                    exec_of(a).meet(exec_of(b).meet(exec_of(c))),
                    exec_of(a).meet(exec_of(b)).meet(exec_of(c))
                );
                assert_eq!(
                    caps_of(a).meet(caps_of(b).meet(caps_of(c))),
                    caps_of(a).meet(caps_of(b)).meet(caps_of(c))
                );
            }
        }
    }
}

#[test]
fn meet_idempotent_over_prefix_universe() {
    for a in &prefix_universe() {
        assert_eq!(fs_of(a).meet(fs_of(a)), fs_of(a));
        assert_eq!(exec_of(a).meet(exec_of(a)), exec_of(a));
        assert_eq!(caps_of(a).meet(caps_of(a)), caps_of(a));
    }
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

/// Namespace regression: overlap must key on `(namespace, resolved)`, not
/// `resolved` alone.  A host-namespace prefix and a guest-namespace prefix
/// that resolve to the identical string must never be judged overlapping —
/// if they were, a guest grant could be narrowed by a host ceiling (or vice
/// versa) into something that matches nothing on either side.  This is the
/// case `synod/src/grant.rs`'s `from_guest` prefixes depend on: a guest
/// grant must survive composition only against other guest grants.
#[test]
fn meet_fs_cross_namespace_prefixes_never_overlap() {
    use crate::path::Namespace;
    let host = crate::path::NormalizedPrefix::for_test("/work", "/work", Namespace::Host);
    let guest = crate::path::NormalizedPrefix::for_test("/work", "/work", Namespace::Guest);
    let met = fs_of(&host).meet(fs_of(&guest));
    assert!(
        met.read_prefixes.is_empty(),
        "a host prefix and a guest prefix resolving to the same string must \
         not overlap, got {:?}",
        met.read_prefixes
    );
}

/// Security regression at the composition layer, over frozen data: a
/// restrict prefix that *lexically* nests under the base ceiling but
/// resolves elsewhere — the divergent pair a symlink freezes to — must
/// not survive `Capabilities::meet`.  The layer-level pin is
/// `PrefixSet::symlinked_grant_cannot_escape_a_shallower_ceiling`;
/// `meet_fs_nested_grants_narrow_to_intersection` above is the positive
/// control showing this collapse is the escape being caught.
///
/// Premise: a symlink created *after* freeze is invisible to the meet —
/// the freeze-to-use stability window
/// (`dev/docs/260727_policy_kernel_purity.md` §5).  The gate and the
/// sandbox projection still re-resolve at use, which is why the
/// end-to-end property holds regardless.
#[test]
fn meet_fs_symlinked_prefix_cannot_escape_ceiling() {
    use crate::path::Namespace;
    let ceiling = crate::path::NormalizedPrefix::for_test("/base", "/base", Namespace::Host);
    let escape =
        crate::path::NormalizedPrefix::for_test("/base/link", "/elsewhere", Namespace::Host);
    let met = fs_of(&ceiling).meet(fs_of(&escape));
    assert!(
        met.read_prefixes.is_empty(),
        "a symlinked deeper prefix resolving outside the ceiling must collapse \
         to the fail-closed empty meet, got {:?}",
        met.read_prefixes
    );
}

/// A single-prefix exec-dir *deny*, the counterpart to `exec_of` — the
/// half of the universe the pre-fix law tests never folded in, which
/// is exactly why they missed the authority leak this file's
/// `capability.rs::Meet`/`Join for ExecMap` fix addresses.
fn exec_deny_of(p: &crate::path::NormalizedPrefix) -> ExecMap {
    ExecMap {
        literals: BTreeMap::new(),
        allow_dirs: BTreeSet::new(),
        deny_dirs: BTreeSet::from([p.clone()]),
    }
}

/// The authority-leak regression, composed both ways. An allow and a
/// deny that share a surface but were frozen against different disk
/// state — `for_test`'s divergent `resolved` pair, what `--extend-base`
/// produces when a base profile's veto is re-granted later against a
/// changed symlink — must not both survive into `allow_dirs`: the
/// derived `Eq`/`Ord` on `NormalizedPrefix` sees them as distinct
/// records (three fields, not the gate's two), so a naive `retain`
/// keying on it lets both through and the gate then admits under the
/// pre-fix tie-break. Checked under both `meet` and `join`, and through
/// the same `admits_for_test` gate `runtime::command::identity` uses,
/// not just by inspecting `allow_dirs` — the leak is only real if the
/// gate itself is fooled.
#[test]
fn exec_meet_and_join_drop_allow_clashing_with_deny_on_divergent_resolved() {
    use crate::capability::admits_for_test;
    use crate::path::Namespace;

    // Spelled for the host: the gate weighs only candidates the
    // platform calls absolute, and a rooted path with no drive is not
    // absolute to Windows — so a POSIX fixture would leave the gate
    // half of this test asserting nothing there.
    let (surface, divergent, candidate) = if cfg!(windows) {
        (r"C:\x", r"C:\y", r"C:\x\bin")
    } else {
        ("/x", "/y", "/x/bin")
    };
    let allow = crate::path::NormalizedPrefix::for_test(surface, surface, Namespace::Host);
    let deny = crate::path::NormalizedPrefix::for_test(surface, divergent, Namespace::Host);
    let allow_map = exec_of(&allow);
    let deny_map = exec_deny_of(&deny);

    for composed in [
        allow_map.clone().meet(deny_map.clone()),
        allow_map.join(deny_map),
    ] {
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
}

/// Same clash, different disguise: the allow and the deny share no
/// surface bytes at all — `/private/tmp/x` and `/tmp/x` — but name the
/// same macOS firmlink-aliased directory, which is exactly what
/// `same_gate_dir` (rather than a byte-equal `gate_key`) is for. Pins
/// the composition half of the alias-clash fix; the gate half is
/// `capability::exec::tests::longest_dir_match_firmlink_alias_does_not_outrank_deny`.
#[cfg(target_os = "macos")]
#[test]
fn exec_meet_and_join_drop_allow_clashing_with_deny_on_firmlink_alias() {
    use crate::capability::admits_for_test;

    let allow_map = exec_of(&crate::path::NormalizedPrefix::from_surface(
        "/private/tmp/x",
    ));
    let deny_map = exec_deny_of(&crate::path::NormalizedPrefix::from_surface("/tmp/x"));

    for composed in [
        allow_map.clone().meet(deny_map.clone()),
        allow_map.join(deny_map),
    ] {
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
}

/// The exec lattice laws, folded over a universe that mixes allow-only
/// and deny-only maps for every prefix. The pre-fix tests
/// (`meet_commutative_over_prefix_universe` and its siblings) only ever
/// built allow-only `ExecMap`s via `exec_of`, so deny-overrides — the
/// dimension the bug lived in — was never exercised by a law check at
/// all.
fn exec_universe() -> Vec<ExecMap> {
    prefix_universe()
        .iter()
        .flat_map(|p| [exec_of(p), exec_deny_of(p)])
        .collect()
}

#[test]
fn exec_meet_commutative_with_denies() {
    let u = exec_universe();
    for a in &u {
        for b in &u {
            assert_eq!(a.clone().meet(b.clone()), b.clone().meet(a.clone()));
        }
    }
}

#[test]
fn exec_meet_associative_with_denies() {
    let u = exec_universe();
    for a in &u {
        for b in &u {
            for c in &u {
                assert_eq!(
                    a.clone().meet(b.clone().meet(c.clone())),
                    a.clone().meet(b.clone()).meet(c.clone())
                );
            }
        }
    }
}

#[test]
fn exec_meet_idempotent_with_denies() {
    for a in &exec_universe() {
        assert_eq!(a.clone().meet(a.clone()), a.clone());
    }
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

/// The trailing slash is the only thing separating a directory grant
/// from a literal one, so omitting it on a real directory would decode
/// to a grant on a binary that cannot exist and fail closed at use time
/// as a bare "denied by active grant".  Freeze catches it and points at
/// the slash.
// Unix-only, like its sigil-freezing siblings.
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

/// Defence in depth: a caller who sets `XDG_DATA_HOME=/etc` must not be
/// able to widen a policy that names `xdg:data`.  Decode rejects the
/// resolution at the boundary with a message naming the offending env
/// var so the operator can diagnose it.
// Unix-only: the boundary check compares Unix path prefixes that
// don't survive Windows path normalisation.
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
        cwd: crate::path::test_cwd(),
    }
}
