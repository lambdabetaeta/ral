#![allow(clippy::disallowed_methods)]

//! A stack's authority is the meet of its layers: denies join, allows meet
//! by their stored names, and in each table the most specific rule decides.
//! Each refusal is paired with an admitted control.

mod common;

use common::{Output, Scratch, assert_admitted, assert_refused, quoted};
use std::path::Path;

/// An fs layer reading `read`, writing `write` and denying `deny`.
fn fs(read: &[&Path], write: &[&Path], deny: &[&Path]) -> String {
    format!(
        "fs: [read: [{}], write: [{}], deny: [{}]]",
        quoted(read),
        quoted(write),
        quoted(deny)
    )
}

/// An exec layer of `rules`, a directory key ending in `/`.  Each admits
/// `/bin/` too, so a script's interpreter runs wherever the kernel checks it.
fn exec(rules: &[(&str, &str)]) -> String {
    let rules: Vec<String> = (std::iter::once(&("/bin/", "allow")).chain(rules))
        .map(|(key, verdict)| format!("'{key}': '{verdict}'"))
        .collect();
    format!("exec: [{}]", rules.join(", "))
}

fn under(layer: &str, body: &str) -> Output {
    common::run("grant_stack", &format!("grant [{layer}] {{ {body} }}"))
}

/// `body` under `inner` pushed onto `outer`.
fn stacked(outer: &str, inner: &str, body: &str) -> Output {
    under(outer, &format!("grant [{inner}] {{ {body} }}"))
}

fn write(path: &Path) -> String {
    format!("echo key > '{}'", path.display())
}

fn read(path: &Path) -> String {
    format!("echo !{{from-string < '{}'}}", path.display())
}

fn assert_ran(out: &Output, what: &str) {
    assert_eq!(out.status, 0, "{what} must run; stderr:\n{}", out.stderr);
    assert!(
        out.stdout.contains("ran"),
        "{what} did not run; stdout:\n{}",
        out.stdout
    );
}

fn assert_not_run(out: &Output, what: &str) {
    assert_ne!(
        out.status, 0,
        "{what} must be refused; stderr:\n{}",
        out.stderr
    );
    assert!(
        !out.stdout.contains("ran"),
        "{what} ran; stdout:\n{}",
        out.stdout
    );
    assert!(
        out.stderr.contains("denied by active grant"),
        "{what} must be refused by the grant; stderr:\n{}",
        out.stderr
    );
}

/// Denies join: a deny only the inner layer writes holds the stack, under
/// every spelling of its name.
#[test]
fn an_inner_layers_deny_joins_the_stack() {
    let d = Scratch::new("stack_join");
    let outer = fs(&[&d.0], &[&d.0], &[]);
    let inner = fs(&[&d.0], &[&d.0], &[&d.join("Secrets")]);

    let out = stacked(&outer, &inner, &write(&d.join("secrets")));
    assert_refused(
        &out,
        "a write to `secrets` under an inner deny on `Secrets`",
    );
    assert!(!d.join("secrets").exists(), "the refused write landed");
    if cfg!(not(windows)) {
        assert!(
            out.stderr.contains(&format!(
                "the denied {} under another spelling",
                d.join("Secrets").display()
            )) && out.stderr.contains("a deny holds under every spelling"),
            "the refusal must name the deny and say why; stderr:\n{}",
            out.stderr
        );
    }

    let out = stacked(&outer, &inner, &write(&d.join("other")));
    assert_admitted(&out, "a write to `other`");
}

/// Denies join, allows meet: an inner allow on a denied directory cannot
/// lift the outer deny.
#[test]
fn an_inner_allow_cannot_lift_an_outer_deny() {
    let d = Scratch::new("stack_lift");
    let secrets = d.join("Secrets");
    std::fs::create_dir(&secrets).unwrap();
    let inner = fs(&[&secrets], &[&secrets], &[]);

    let outer = fs(&[&d.0], &[&d.0], &[&secrets]);
    let out = stacked(&outer, &inner, &write(&secrets.join("x")));
    assert_refused(&out, "a write under an outer deny an inner layer allows");
    assert!(!secrets.join("x").exists(), "the refused write landed");

    let outer = fs(&[&d.0], &[&d.0], &[]);
    let out = stacked(&outer, &inner, &write(&secrets.join("x")));
    assert_admitted(&out, "a write under `Secrets` with no deny");
}

/// Allows meet by their stored names: two layers spelling one directory
/// differently grant it to neither.
#[cfg(target_os = "linux")]
#[test]
fn two_spellings_of_a_directory_meet_to_neither() {
    let d = Scratch::new("stack_spellings");
    let (upper, lower) = (d.join("Work"), d.join("work"));
    std::fs::create_dir(&upper).unwrap();
    std::fs::create_dir(&lower).unwrap();
    let layer = |dir: &Path| fs(&[&d.0], &[dir], &[]);

    for dir in [&upper, &lower] {
        let out = stacked(&layer(&upper), &layer(&lower), &write(&dir.join("f")));
        assert_refused(&out, &format!("a write into {}", dir.display()));
        assert!(!dir.join("f").exists(), "the refused write landed");
    }

    let out = stacked(&layer(&upper), &layer(&upper), &write(&upper.join("f")));
    assert_admitted(&out, "a write into `Work` granted by both layers");
}

/// Allows meet at their resolved forms: an inner grant through a symlink
/// cannot escape the outer ceiling, in the guard or in a child.
#[cfg(unix)]
#[test]
fn a_symlinked_inner_grant_cannot_escape_the_outer_ceiling() {
    let d = Scratch::new("stack_symlink");
    let (inner_dir, outside) = (d.join("inner"), d.join("outside"));
    std::fs::create_dir_all(inner_dir.join("sub")).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("f"), "escaped").unwrap();
    std::fs::write(inner_dir.join("sub/f"), "nested").unwrap();
    let link = inner_dir.join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let outer = fs(&[&inner_dir], &[], &[]);
    let (through_link, nested) = (
        fs(&[&link], &[], &[]),
        fs(&[&inner_dir.join("sub")], &[], &[]),
    );

    for path in [outside.join("f"), link.join("f")] {
        let out = stacked(&outer, &through_link, &read(&path));
        assert_refused(&out, &format!("a read of {}", path.display()));
        assert!(!out.stdout.contains("escaped"), "stdout:\n{}", out.stdout);
    }
    let out = stacked(&outer, &nested, &read(&inner_dir.join("sub/f")));
    assert_admitted(&out, "a read under a nested inner grant");
    assert!(out.stdout.contains("nested"), "stdout:\n{}", out.stdout);

    #[cfg(target_os = "linux")]
    if !common::bwrap_functional() {
        eprintln!("skip: bwrap cannot confine a child here");
        return;
    }
    let cat = |inner: &str, path: &Path| {
        stacked(&outer, inner, &format!("sh -c 'cat {}'", path.display()))
    };
    let out = cat(&nested, &inner_dir.join("sub/f"));
    if common::refused_by_a_confined_runner(&out) {
        return;
    }
    assert_admitted(&out, "a child's read under a nested inner grant");
    assert_eq!(out.stdout, "nested");
    let out = cat(&through_link, &link.join("f"));
    assert_ne!(
        out.status, 0,
        "a child's read through the link must be refused"
    );
    assert!(!out.stdout.contains("escaped"), "stdout:\n{}", out.stdout);
}

/// Allows meet: layers with disjoint allows admit nothing, fail-closed.
#[test]
fn disjoint_layers_meet_to_nothing() {
    let d = Scratch::new("stack_disjoint");
    let (a, b) = (d.join("a"), d.join("b"));
    for dir in [&a, &b] {
        std::fs::create_dir(dir).unwrap();
        std::fs::write(dir.join("f"), "content").unwrap();
    }

    for dir in [&a, &b] {
        let out = stacked(
            &fs(&[&a], &[], &[]),
            &fs(&[&b], &[], &[]),
            &read(&dir.join("f")),
        );
        assert_refused(&out, &format!("a read under {}", dir.display()));
    }

    let out = stacked(
        &fs(&[&a], &[], &[]),
        &fs(&[&d.0], &[], &[]),
        &read(&a.join("f")),
    );
    assert_admitted(&out, "a read under nested layers");
    assert!(out.stdout.contains("content"), "stdout:\n{}", out.stdout);
}

/// Denies join, and the most specific rule decides: a deeper allow in an
/// inner layer does not lift an outer deny dir.
#[cfg(unix)]
#[test]
fn a_deeper_inner_allow_does_not_lift_an_outer_exec_deny() {
    let d = Scratch::new("stack_exec_deep");
    let bin = d.join("bin");
    let (good, deep) = (bin.join("good/t"), bin.join("evil/deep/t"));
    common::script(&good);
    common::script(&deep);
    let b = bin.display();
    let outer = exec(&[(&format!("{b}/"), "allow"), (&format!("{b}/evil/"), "deny")]);
    let inner = exec(&[
        (&format!("{b}/"), "allow"),
        (&format!("{b}/evil/deep/"), "allow"),
    ]);

    let out = stacked(&outer, &inner, &good.display().to_string());
    if common::refused_by_a_confined_runner(&out) {
        return;
    }
    assert_ran(&out, "`good/t`");
    let out = stacked(&outer, &inner, &deep.display().to_string());
    assert_not_run(&out, "`evil/deep/t`");
}

/// The most specific rule decides: an exact file allow beats a covering
/// deny dir, which no fs allow can do.
#[cfg(unix)]
#[test]
fn an_exact_exec_allow_beats_a_covering_deny_dir() {
    let d = Scratch::new("stack_exec_exact");
    let bin = d.join("bin");
    let (tool, other) = (bin.join("tool"), bin.join("other"));
    common::script(&tool);
    common::script(&other);
    let b = bin.display();
    let layer = exec(&[
        (&format!("{b}/"), "deny"),
        (&tool.display().to_string(), "allow"),
    ]);

    let out = under(&layer, &tool.display().to_string());
    if common::refused_by_a_confined_runner(&out) {
        return;
    }
    assert_ran(&out, "`tool`");
    let out = under(&layer, &other.display().to_string());
    assert_not_run(&out, "`other`");
}

/// A veto holds every spelling of the name, and refuses plainly: its key is
/// folded already, so no respelling is named.
#[cfg(target_os = "linux")]
#[test]
fn a_veto_holds_every_spelling_of_the_name() {
    let d = Scratch::new("stack_exec_veto");
    let bin = d.join("bin");
    let (upper, longer) = (bin.join("SH"), bin.join("shx"));
    common::script(&upper);
    common::script(&longer);
    let layer = exec(&[(&format!("{}/", bin.display()), "allow"), ("sh", "deny")]);

    let out = under(&layer, &upper.display().to_string());
    assert_not_run(&out, "`SH` under a veto on `sh`");
    assert!(
        !out.stderr.contains("another spelling"),
        "a veto must refuse plainly; stderr:\n{}",
        out.stderr
    );
    let out = under(&layer, &longer.display().to_string());
    assert_ran(&out, "`shx`, a different name");
}

/// Allows meet by their stored names: two layers admitting one directory
/// under different spellings admit neither.
#[cfg(target_os = "linux")]
#[test]
fn two_spellings_of_an_exec_dir_meet_to_neither() {
    let d = Scratch::new("stack_exec_spellings");
    let bin = d.join("bin");
    let (upper, lower) = (bin.join("Tools/t"), bin.join("tools/t"));
    common::script(&upper);
    common::script(&lower);
    let layer = |dir: &str| exec(&[(&format!("{}/{dir}/", bin.display()), "allow")]);

    for program in [&upper, &lower] {
        let out = stacked(
            &layer("Tools"),
            &layer("tools"),
            &program.display().to_string(),
        );
        assert_not_run(&out, &program.display().to_string());
    }

    let out = stacked(
        &layer("Tools"),
        &layer("Tools"),
        &upper.display().to_string(),
    );
    assert_ran(&out, "`Tools/t` admitted by both layers");
}
