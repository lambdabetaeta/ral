use super::*;

/// A grant that reads and writes `G` and denies `G/p/secret`, which holds
/// the secret's bytes, with `G/q` off the deny's spine: `G` and its policy.
fn spine(tag: &str) -> (std::path::PathBuf, SandboxProjection) {
    let g = std::fs::canonicalize(workdir(tag)).expect("the work dir is real");
    let secret = g.join("p/secret");
    std::fs::create_dir_all(&secret).unwrap();
    std::fs::create_dir(g.join("q")).unwrap();
    std::fs::write(secret.join("data"), "SECRET-BYTES").unwrap();
    let policy = restricted(&[&g], &[&g], &[&secret]);
    (g, policy)
}

/// The hole: a confined `mv G/p G/p2` carried the secret to a name no
/// later grant denied.  Every directory on the spine is a mountpoint, which
/// rename and removal refuse with `EBUSY`; its entries stay writable, the
/// secret stays masked, so the pins went on before the mask, and a sibling
/// off the spine still moves.
#[test]
fn a_denys_spine_cannot_be_moved_and_its_entries_stay_writable() {
    let (g, policy) = spine("pin-spawn");
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let script = format!(
        "mv '{g}/p' '{g}/p2' 2>&1 | grep -qi busy && echo P-PINNED\n\
         rmdir '{g}' 2>&1 | grep -qi busy && echo G-UNREMOVABLE\n\
         mv '{g}' '{g}2' 2>&1 | grep -qi busy && echo G-PINNED\n\
         echo NEW-BYTES > '{g}/p/new' && echo WROTE\n\
         cat '{g}/p/secret/data' 2>/dev/null\n\
         mv '{g}/q' '{g}/q2' && echo SIBLING-MOVED\n",
        g = g.display(),
    );
    let (stdout, stderr) = confined(&env, &policy, &script);
    for marker in [
        "P-PINNED",
        "G-UNREMOVABLE",
        "G-PINNED",
        "WROTE",
        "SIBLING-MOVED",
    ] {
        assert!(
            emitted(&stdout, marker),
            "missing {marker}: {stdout}: {stderr}"
        );
    }
    assert!(!stdout.contains("SECRET-BYTES"), "{stdout}");
    assert!(g.join("p/secret/data").is_file() && !g.join("p2").exists());
    assert_eq!(
        std::fs::read_to_string(g.join("p/new")).unwrap(),
        "NEW-BYTES\n"
    );
    assert!(g.join("q2").is_dir() && !g.join("q").exists());
    let _ = std::fs::remove_dir_all(&g);
}

/// ral's own `mv` and `rm` run confined like any program, so the spine holds
/// them too.
#[cfg(feature = "coreutils")]
#[cfg_attr(
    ctor_without_argv,
    ignore = "a test binary serves bundled tools before main, where musl gives std no argv"
)]
#[test]
fn rals_own_mv_and_rm_cannot_move_a_denys_spine() {
    let (g, policy) = spine("pin-bundled");
    let Some(env) = envelope_launches(&policy) else {
        return;
    };
    let p = g.join("p").display().to_string();
    for (tool, args) in [
        ("mv", [p.clone(), format!("{p}2")]),
        ("rm", ["-r".to_string(), p]),
    ] {
        let out = run_admitted(&env, &policy, &admitted(Program::Tool(tool)), &args)
            .expect("spawn bwrap");
        assert!(!out.status.success(), "{tool}: {out:?}");
    }
    assert!(g.join("p/secret/data").is_file() && !g.join("p2").exists());
    let _ = std::fs::remove_dir_all(&g);
}
