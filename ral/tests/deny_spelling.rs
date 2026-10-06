#![allow(clippy::disallowed_methods)]

//! A deny holds under every spelling some filesystem takes for its name —
//! case and Unicode normalisation alike — while an allow holds the name as
//! stored.  Each denial is paired with an admitted control.

mod common;

use common::{Scratch, assert_admitted, assert_refused};
use std::path::{Path, PathBuf};

/// Run `body` under a grant reading `read`, writing `write` and denying
/// `deny`, all absolute.
fn under(read: &Path, write: &Path, deny: &[&Path], body: &str) -> common::Output {
    common::run(
        "deny_spelling",
        &format!(
            "grant [fs: [read: ['{}'], write: ['{}'], deny: [{}]]] {{ {body} }}",
            read.display(),
            write.display(),
            common::quoted(deny),
        ),
    )
}

/// `echo key > path` under a grant over `dir` denying `dir/deny`.
fn write_under_deny(dir: &Path, deny: &str, path: &Path) -> common::Output {
    let body = format!("echo key > '{}'", path.display());
    under(dir, dir, &[&dir.join(deny)], &body)
}

/// T4: an absent deny holds a case variant the create would make, and the
/// refusal names the deny and why.
#[test]
fn an_absent_deny_refuses_a_case_variant_create() {
    let d = Scratch::new("deny_spelling_case");
    let out = write_under_deny(&d.0, "Secrets", &d.join("secrets"));
    assert_refused(&out, "a create of `secrets` under a deny on `Secrets`");
    assert!(!d.join("secrets").exists(), "the refused create landed");
    // Windows' stored identity already folds ASCII case, so there the deny
    // holds the name as stored and the plain denial is the right one.
    if cfg!(not(windows)) {
        assert!(
            out.stderr
                .contains(&format!("the denied {}", d.join("Secrets").display()))
                && out.stderr.contains("a deny holds under every spelling"),
            "the denial must name the deny and say why; stderr:\n{}",
            out.stderr
        );
    }

    std::fs::create_dir(d.join("secrets")).unwrap();
    let out = write_under_deny(&d.0, "Secrets", &d.join("secrets").join("t"));
    assert_refused(&out, "a write under `secrets/` with a deny on `Secrets`");

    let out = write_under_deny(&d.0, "Secrets", &d.join("other"));
    assert_admitted(&out, "a write to `other`");
    let out = write_under_deny(&d.0, "Secrets", &d.join("Secrets.txt"));
    assert_admitted(&out, "a write to `Secrets.txt`, a different component");
}

/// T5: normalisation, as case: an NFC deny holds the NFD spelling.
#[test]
fn an_absent_nfc_deny_refuses_an_nfd_create() {
    let d = Scratch::new("deny_spelling_nfd");
    let out = write_under_deny(&d.0, "caf\u{e9}.txt", &d.join("cafe\u{301}.txt"));
    assert_refused(&out, "an NFD create under an NFC deny");
    let out = write_under_deny(&d.0, "caf\u{e9}.txt", &d.join("cafe.txt"));
    assert_admitted(&out, "a write to `cafe.txt`, a different name");
}

/// A deny holding the name as stored refuses plainly, though a deeper deny
/// holds it only by a fold: a respelling is named only when no deny holds
/// the stored name.
#[test]
fn a_deny_holding_the_stored_name_refuses_plainly() {
    let d = Scratch::new("deny_spelling_plain");
    let (secrets, upper) = (d.join("secrets"), d.join("Secrets"));
    let write = |path: &Path| format!("echo key > '{}'", path.display());
    for denies in [&[secrets.as_path()][..], &[d.0.as_path(), upper.as_path()]] {
        let out = under(&d.0, &d.0, denies, &write(&secrets));
        assert_refused(
            &out,
            &format!("a write to `secrets` under denies {denies:?}"),
        );
        assert!(
            !out.stderr.contains("another spelling"),
            "a deny holding the stored name must not claim a respelling; stderr:\n{}",
            out.stderr
        );
        assert!(!secrets.exists(), "the refused write landed");
    }
    let out = under(&d.0, &d.0, &[&secrets], &write(&d.join("other")));
    assert_admitted(&out, "a write to `other`");
}

/// T6's pairs, in a case-sensitive `d` holding the distinct `Work` and
/// `work`.  An allow on `Work` does not reach `work`; a deny on `Work` does —
/// the over-deny a deny accepts, so that it holds on every volume.
fn allow_side_pairs(d: &Path) {
    let (upper, lower) = (d.join("Work"), d.join("work"));
    std::fs::create_dir(&upper).unwrap();
    std::fs::create_dir(&lower).unwrap();
    let echo = |path: PathBuf| format!("echo key > '{}'", path.display());

    let out = under(d, &upper, &[], &echo(lower.join("f")));
    assert_ne!(
        out.status, 0,
        "an allow on `Work` must not reach the distinct `work`; stderr:\n{}",
        out.stderr
    );
    let out = under(d, &upper, &[], &echo(upper.join("f")));
    assert_admitted(&out, "a write into the granted `Work`");

    let out = under(d, d, &[&upper], &echo(lower.join("g")));
    assert_refused(
        &out,
        "a write into `work` under a deny on the distinct `Work`",
    );
    assert!(
        out.stderr.contains("a deny holds under every spelling"),
        "the over-deny must say why; stderr:\n{}",
        out.stderr
    );
}

/// T6 on Linux, whose tempdirs are case-sensitive.
#[cfg(target_os = "linux")]
#[test]
fn the_allow_side_keeps_case_on_a_case_sensitive_directory() {
    let d = Scratch::new("deny_spelling_cs");
    allow_side_pairs(&d.0);
}

/// T6 on macOS, inside a case-sensitive APFS image.  Ignored: it attaches a
/// disk image, which a CI runner may not allow.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "attaches a case-sensitive APFS image with hdiutil"]
fn the_allow_side_keeps_case_on_a_case_sensitive_volume() {
    use std::process::Command;
    let d = Scratch::new("deny_spelling_cs");
    let image = d.join("cs.dmg");
    let mount = d.join("mnt");
    std::fs::create_dir(&mount).unwrap();
    let hdiutil = |args: &[&std::ffi::OsStr]| {
        let ok = Command::new("hdiutil")
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "hdiutil {args:?} failed");
    };
    hdiutil(&[
        "create".as_ref(),
        "-size".as_ref(),
        "16m".as_ref(),
        "-fs".as_ref(),
        "Case-sensitive APFS".as_ref(),
        image.as_os_str(),
    ]);
    hdiutil(&[
        "attach".as_ref(),
        "-nobrowse".as_ref(),
        "-mountpoint".as_ref(),
        mount.as_os_str(),
        image.as_os_str(),
    ]);
    let pairs = std::panic::catch_unwind(|| allow_side_pairs(&mount));
    hdiutil(&["detach".as_ref(), mount.as_os_str()]);
    if let Err(panic) = pairs {
        std::panic::resume_unwind(panic);
    }
}

/// T6 on Windows, in a directory made case-sensitive with `fsutil`.
#[cfg(windows)]
#[test]
fn the_allow_side_keeps_case_in_a_case_sensitive_folder() {
    let d = Scratch::new("deny_spelling_cs");
    let enabled = std::process::Command::new("fsutil")
        .args(["file", "setCaseSensitiveInfo"])
        .arg(&d.0)
        .arg("enable")
        .output()
        .is_ok_and(|o| o.status.success());
    if !enabled {
        eprintln!(
            "skip: `fsutil file setCaseSensitiveInfo` is unavailable here, so no \
             case-sensitive folder can be made"
        );
        return;
    }
    allow_side_pairs(&d.0);
}

/// T7: Seatbelt already matches a deny under canonical caseless
/// equivalence, for absent names too.  An OS release that drops that fold
/// fails here.
#[cfg(target_os = "macos")]
#[test]
fn seatbelt_refuses_a_case_variant_of_an_absent_deny() {
    let d = Scratch::new("deny_spelling_seatbelt");
    let mkdir = |name: &str| {
        let body = format!("sh -c 'mkdir {}'", d.join(name).display());
        under(&d.0, &d.0, &[&d.join("Secrets")], &body)
    };
    let out = mkdir("secrets");
    if out
        .stderr
        .contains("ral: cannot enter the Seatbelt sandbox: ")
    {
        eprintln!("skip: this runner is inside a Seatbelt profile");
        return;
    }
    assert_ne!(out.status, 0, "a child's `mkdir secrets` must be refused");
    assert!(!d.join("secrets").exists(), "the child's mkdir landed");
    let out = mkdir("other");
    assert_admitted(&out, "a child's `mkdir other`");
    assert!(d.join("other").is_dir());
}

/// T9: NTFS does not fold normalisation but does fold non-ASCII case, which
/// the ASCII stored identity misses and the deny relation does not.  And
/// NTFS's `$UpCase` merges dotless ı with I, which is why the key is taken
/// on the uppercase image.
#[cfg(windows)]
#[test]
fn ntfs_non_ascii_case_meets_the_deny() {
    let d = Scratch::new("deny_spelling_ntfs");
    let out = write_under_deny(&d.0, "\u{c9}clair", &d.join("\u{e9}clair"));
    assert_refused(&out, "a create of `éclair` under a deny on `Éclair`");
    let out = write_under_deny(&d.0, "\u{c9}clair", &d.join("eclair"));
    assert_admitted(&out, "a write to `eclair`, a different name");

    std::fs::write(d.join("f\u{131}le"), "x").unwrap();
    assert!(
        d.join("FILE").exists(),
        "NTFS took `fıle` and `FILE` for two names; the collision key merges \
         them, so it over-merges one letter here: revisit the uppercase image"
    );
}
