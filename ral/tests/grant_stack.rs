#![allow(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::disallowed_macros,
    reason = "[test] libtest captures print macros"
)]

//! A stack's authority is the meet of its layers: exclusions join, allowances
//! meet by their stored names, and an exclusion beats every allowance however
//! specific.  Each refusal is paired with an admitted control.

mod common;

#[cfg(unix)]
use common::denied_by_active_grant;
use common::{Output, Scratch, denied_by_grant, places};
use ral_core::test_helper::confinement::admitted;
#[cfg(unix)]
use ral_core::test_helper::confinement::denied;
use std::path::Path;

/// An fs layer reading `read`, writing `write` and denying `deny`.
fn fs(read: &[&Path], write: &[&Path], deny: &[&Path]) -> String {
    format!(
        "fs: [read: [{}], write: [{}], deny: [{}]]",
        places(read),
        places(write),
        places(deny)
    )
}

/// An exec layer of `(key, verdict)` rules: a key ending in `/` is a `dir`,
/// a bare name a `name`, anything else a file.  Each layer admits `/bin` too,
/// so a script's interpreter runs wherever the kernel checks it.
#[cfg(unix)]
fn exec(rules: &[(&str, &str)]) -> String {
    let target = |key: &str| match key.strip_suffix('/') {
        Some(dir) => format!("`dir `absolute '{dir}'"),
        None if key.contains('/') => format!("`absolute '{key}'"),
        None => format!("`name {key}"),
    };
    let list = |want: &str| {
        let keys = (std::iter::once(&("/bin/", "allow")).chain(rules))
            .filter(|(_, verdict)| *verdict == want)
            .map(|(key, _)| target(key))
            .collect::<Vec<_>>();
        keys.join(", ")
    };
    format!(
        "exec: [allow: [{}], deny: [{}]]",
        list("allow"),
        list("deny")
    )
}

fn under(layer: &str, body: &str) -> Output {
    common::run("grant_stack", &format!("grant [{layer}] {{ {body} }}"))
}

/// `body` under `inner` pushed onto `outer`.
fn stacked(outer: &str, inner: &str, body: &str) -> Output {
    under(outer, &format!("grant [{inner}] {{ {body} }}"))
}

fn write(path: &Path) -> String {
    format!("echo key > '{}'; echo key", ral_core::path::shown(path))
}

fn read(path: &Path) -> String {
    format!("echo !{{from-string < '{}'}}", ral_core::path::shown(path))
}

/// Denies join: a deny only the inner layer writes holds the stack, under
/// every spelling of its name.
#[test]
fn an_inner_layers_deny_joins_the_stack() {
    let proof = ral_core::require_confinement!();
    let d = Scratch::new("stack_join");
    let outer = fs(&[&d.0], &[&d.0], &[]);
    let inner = fs(&[&d.0], &[&d.0], &[&d.join("Secrets")]);

    let control = stacked(&outer, &inner, &write(&d.join("other")));
    let out = stacked(&outer, &inner, &write(&d.join("secrets")));
    denied_by_grant(&proof, &control, &out, "key");
    assert!(!d.join("secrets").exists(), "the refused write landed");
    assert!(
        out.stderr.contains(&format!(
            "the denied {} under another spelling",
            ral_core::path::shown(d.join("Secrets"))
        )) && out.stderr.contains("a deny holds under every spelling"),
        "the refusal must name the deny and say why; stderr:\n{}",
        out.stderr
    );

    let out = stacked(&outer, &inner, &write(&d.join("other")));
    admitted(&proof, &out, "key");
}

/// Denies join, allows meet: an inner allow on a denied directory cannot
/// lift the outer deny.
#[test]
fn an_inner_allow_cannot_lift_an_outer_deny() {
    let proof = ral_core::require_confinement!();
    let d = Scratch::new("stack_lift");
    let secrets = d.join("Secrets");
    std::fs::create_dir(&secrets).unwrap();
    let inner = fs(&[&secrets], &[&secrets], &[]);

    let outer = fs(&[&d.0], &[&d.0], &[&secrets]);
    let control = stacked(&outer, &inner, "echo key");
    let out = stacked(&outer, &inner, &write(&secrets.join("x")));
    denied_by_grant(&proof, &control, &out, "key");
    assert!(!secrets.join("x").exists(), "the refused write landed");

    let outer = fs(&[&d.0], &[&d.0], &[]);
    let out = stacked(&outer, &inner, &write(&secrets.join("x")));
    admitted(&proof, &out, "key");
}

/// Allows meet by their stored names: two layers spelling one directory
/// differently grant it to neither.
#[cfg(target_os = "linux")]
#[test]
fn two_spellings_of_a_directory_meet_to_neither() {
    let proof = ral_core::require_confinement!();
    let d = Scratch::new("stack_spellings");
    let (upper, lower) = (d.join("Work"), d.join("work"));
    std::fs::create_dir(&upper).unwrap();
    std::fs::create_dir(&lower).unwrap();
    let layer = |dir: &Path| fs(&[&d.0], &[dir], &[]);

    let control = stacked(&layer(&upper), &layer(&lower), "echo key");
    for dir in [&upper, &lower] {
        let out = stacked(&layer(&upper), &layer(&lower), &write(&dir.join("f")));
        denied_by_grant(&proof, &control, &out, "key");
        assert!(!dir.join("f").exists(), "the refused write landed");
    }

    let out = stacked(&layer(&upper), &layer(&upper), &write(&upper.join("f")));
    admitted(&proof, &out, "key");
}

/// Allows meet at their resolved forms: an inner grant through a symlink
/// cannot escape the outer ceiling, in the guard or in a child.
#[cfg(unix)]
#[test]
fn a_symlinked_inner_grant_cannot_escape_the_outer_ceiling() {
    let proof = ral_core::require_confinement!();
    let d = Scratch::new("stack_symlink");
    let (inner_dir, outside) = (d.join("inner"), d.join("outside"));
    std::fs::create_dir_all(inner_dir.join("sub")).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("f"), "escaped").unwrap();
    std::fs::write(inner_dir.join("sub/f"), "escaped").unwrap();
    let link = inner_dir.join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let outer = fs(&[&inner_dir], &[], &[]);
    let (through_link, nested) = (
        fs(&[&link], &[], &[]),
        fs(&[&inner_dir.join("sub")], &[], &[]),
    );

    let control = stacked(&outer, &through_link, "echo escaped");
    for path in [outside.join("f"), link.join("f")] {
        let out = stacked(&outer, &through_link, &read(&path));
        denied_by_grant(&proof, &control, &out, "escaped");
        assert!(!out.stdout.contains("escaped"), "stdout:\n{}", out.stdout);
    }
    let out = stacked(&outer, &nested, &read(&inner_dir.join("sub/f")));
    admitted(&proof, &out, "escaped");
    assert!(out.stdout.contains("escaped"), "stdout:\n{}", out.stdout);

    let cat = |inner: &str, path: &Path| {
        stacked(
            &outer,
            inner,
            &format!("sh -c 'cat {}'", ral_core::path::shown(path)),
        )
    };
    let control = cat(&nested, &inner_dir.join("sub/f"));
    admitted(&proof, &control, "escaped");
    let out = cat(&through_link, &link.join("f"));
    denied(&proof, &control, &out, "escaped");
    assert!(!out.stdout.contains("escaped"), "stdout:\n{}", out.stdout);
}

/// Allows meet: layers with disjoint allows admit nothing, fail-closed.
#[test]
fn disjoint_layers_meet_to_nothing() {
    let proof = ral_core::require_confinement!();
    let d = Scratch::new("stack_disjoint");
    let (a, b) = (d.join("a"), d.join("b"));
    for dir in [&a, &b] {
        std::fs::create_dir(dir).unwrap();
        std::fs::write(dir.join("f"), "content").unwrap();
    }

    let control = stacked(&fs(&[&a], &[], &[]), &fs(&[&b], &[], &[]), "echo content");
    for dir in [&a, &b] {
        let out = stacked(
            &fs(&[&a], &[], &[]),
            &fs(&[&b], &[], &[]),
            &read(&dir.join("f")),
        );
        denied_by_grant(&proof, &control, &out, "content");
    }

    let out = stacked(
        &fs(&[&a], &[], &[]),
        &fs(&[&d.0], &[], &[]),
        &read(&a.join("f")),
    );
    admitted(&proof, &out, "content");
    assert!(out.stdout.contains("content"), "stdout:\n{}", out.stdout);
}

/// Exclusions join, and no allowance beats one: a deeper allow in an inner
/// layer does not lift an outer deny dir.
#[cfg(unix)]
#[test]
fn a_deeper_inner_allow_does_not_lift_an_outer_exec_deny() {
    ral_core::require_landlock!(1);
    let proof = ral_core::require_confinement!();
    let d = Scratch::new("stack_exec_deep");
    let bin = d.join("bin");
    let (good, deep) = (bin.join("good/t"), bin.join("evil/deep/t"));
    common::script(&good);
    common::script(&deep);
    let b = ral_core::path::shown(&bin);
    let outer = exec(&[(&format!("{b}/"), "allow"), (&format!("{b}/evil/"), "deny")]);
    let inner = exec(&[
        (&format!("{b}/"), "allow"),
        (&format!("{b}/evil/deep/"), "allow"),
    ]);

    let out = stacked(&outer, &inner, &ral_core::path::shown(&good));

    admitted(&proof, &out, "ran");
    let control = out;
    let out = stacked(&outer, &inner, &ral_core::path::shown(&deep));
    denied_by_active_grant(&proof, &control, &out, "ran");
}

/// An exclusion beats every allowance: an exact file allow does not reopen a
/// covering deny dir, whatever fs allows.
#[cfg(unix)]
#[test]
fn an_exact_exec_allow_does_not_reopen_a_covering_deny_dir() {
    ral_core::require_landlock!(1);
    let proof = ral_core::require_confinement!();
    let d = Scratch::new("stack_exec_exact");
    let bin = d.join("bin");
    let (tool, other) = (bin.join("tool"), bin.join("other"));
    common::script(&tool);
    common::script(&other);
    let b = ral_core::path::shown(&bin);
    let layer = exec(&[
        (&format!("{b}/"), "deny"),
        (&ral_core::path::shown(&tool), "allow"),
    ]);

    let control = under(&layer, "/bin/sh -c 'echo ran'");
    for program in [&tool, &other] {
        let out = under(&layer, &ral_core::path::shown(program));
        denied_by_active_grant(&proof, &control, &out, "ran");
        assert!(
            out.stderr
                .contains("an allowance cannot override an exclusion"),
            "the refusal must say no allowance overrides it; stderr:\n{}",
            out.stderr
        );
    }
}

/// A veto holds every spelling of the name, and refuses plainly: its key is
/// folded already, so no respelling is named.  The vetoed name is not the
/// scripts' interpreter, which the kernel would refuse the control too.
#[cfg(target_os = "linux")]
#[test]
fn a_veto_holds_every_spelling_of_the_name() {
    ral_core::require_landlock!(1);
    let proof = ral_core::require_confinement!();
    let d = Scratch::new("stack_exec_veto");
    let bin = d.join("bin");
    let (upper, longer) = (bin.join("TOOL"), bin.join("toolx"));
    common::script(&upper);
    common::script(&longer);
    let layer = exec(&[
        (&format!("{}/", ral_core::path::shown(&bin)), "allow"),
        ("tool", "deny"),
    ]);

    let control = under(&layer, &ral_core::path::shown(&longer));
    let out = under(&layer, &ral_core::path::shown(&upper));
    denied_by_active_grant(&proof, &control, &out, "ran");
    assert!(
        !out.stderr.contains("another spelling"),
        "a veto must refuse plainly; stderr:\n{}",
        out.stderr
    );
    let out = under(&layer, &ral_core::path::shown(&longer));
    admitted(&proof, &out, "ran");
}

/// Allows meet by their stored names: two layers admitting one directory
/// under different spellings admit neither.
#[cfg(target_os = "linux")]
#[test]
fn two_spellings_of_an_exec_dir_meet_to_neither() {
    ral_core::require_landlock!(1);
    let proof = ral_core::require_confinement!();
    let d = Scratch::new("stack_exec_spellings");
    let bin = d.join("bin");
    let (upper, lower) = (bin.join("Tools/t"), bin.join("tools/t"));
    common::script(&upper);
    common::script(&lower);
    let layer = |dir: &str| exec(&[(&format!("{}/{dir}/", ral_core::path::shown(&bin)), "allow")]);

    let control = stacked(&layer("Tools"), &layer("tools"), "/bin/sh -c 'echo ran'");
    for program in [&upper, &lower] {
        let out = stacked(
            &layer("Tools"),
            &layer("tools"),
            &ral_core::path::shown(program),
        );
        denied_by_active_grant(&proof, &control, &out, "ran");
    }

    let out = stacked(
        &layer("Tools"),
        &layer("Tools"),
        &ral_core::path::shown(&upper),
    );
    admitted(&proof, &out, "ran");
}
