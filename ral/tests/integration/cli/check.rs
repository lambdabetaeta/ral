//! `--check` agrees with a real run.

use crate::common::{self, Scratch};
use serde_json::{Value, json};
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(10);

const ED_INSERT_PIPED_TO_FROM_JSON: &str = r#"_ed-insert "hi" | from-json"#;

#[test]
fn check_typechecks_ed_insert_as_external_in_batch() {
    let r = common::run_with_timeout(
        "batch_check_ed_insert",
        &["--check"],
        ED_INSERT_PIPED_TO_FROM_JSON,
        WAIT,
    )
    .expect("ral --check must not hang");
    assert_eq!(
        r.status, 0,
        "expected `--check` to typecheck _ed-insert as an external command; stderr was:\n{}",
        r.stderr
    );
}

#[test]
fn run_agrees_ed_insert_is_external_in_batch() {
    let r = common::run_with_timeout(
        "batch_run_ed_insert",
        &[],
        ED_INSERT_PIPED_TO_FROM_JSON,
        WAIT,
    )
    .expect("ral must not hang");
    assert_ne!(r.status, 0, "an unresolvable external command must fail");
    assert!(
        r.stderr.contains("_ed-insert: command not found"),
        "expected an external-exec failure, not a type error; stderr was:\n{}",
        r.stderr
    );
}

#[test]
fn check_links_modules_without_evaluating_them_or_the_importer() {
    let dir = Scratch::new("check_no_evaluation");
    let module = dir.join("module.ral");
    let module_ran = dir.join("module-ran");
    let importer_ran = dir.join("importer-ran");
    std::fs::write(
        &module,
        format!(
            "echo module > '{}'\nlet id = {{ |value| return $value }}\n",
            ral_core::path::shown(&module_ran)
        ),
    )
    .unwrap();
    let source = format!(
        "echo importer > '{}'\nlet imported = use '{}'\necho !{{$imported[id] 1}} !{{$imported[id] 'a'}}\n",
        ral_core::path::shown(&importer_ran),
        ral_core::path::shown(&module),
    );
    let checked = common::run_with_timeout(
        "check_no_evaluation",
        &["--check", "--audit"],
        &source,
        WAIT,
    )
    .expect("check must not hang");
    assert_eq!(checked.status, 0, "{checked:?}");
    assert!(checked.stdout.is_empty(), "{checked:?}");
    let report: Value = serde_json::from_str(&common::diagnostics(&checked.stderr))
        .expect("successful check must have an audit envelope");
    assert_eq!(report, json!({"outcome": {"tag": "checked"}, "trail": []}));
    assert!(!module_ran.exists(), "checking must not evaluate a module");
    assert!(
        !importer_ran.exists(),
        "checking must not evaluate the importer"
    );

    let ran =
        common::run_with_timeout("run_after_check", &[], &source, WAIT).expect("run must not hang");
    assert_eq!(ran.status, 0, "{ran:?}");
    assert_eq!(ran.stdout, "1 a\n", "{ran:?}");
    assert_eq!(std::fs::read_to_string(module_ran).unwrap(), "module\n");
    assert_eq!(std::fs::read_to_string(importer_ran).unwrap(), "importer\n");
}

#[test]
fn module_failures_refuse_the_whole_unit_before_evaluation() {
    let dir = Scratch::new("check_module_refusals");
    for (name, source, diagnostic, status) in [
        ("missing", None, "U0001", 1),
        ("parse", Some("let = "), "P0001", 2),
        ("type", Some("$[1 + true]"), "T0010", 1),
        (
            "undecided",
            Some("let decode = { |text| to-string $text | from-json }"),
            "T0082",
            1,
        ),
        ("cycle", Some("use './cycle.ral'"), "module load cycle", 1),
    ] {
        let module = dir.join(&format!("{name}.ral"));
        if let Some(source) = source {
            std::fs::write(&module, format!("echo module-ran\n{source}\n")).unwrap();
        }
        let importer = format!(
            "echo importer-ran\nuse '{}'\n",
            ral_core::path::shown(&module)
        );
        for flags in [&["--check", "--audit"][..], &["--audit"][..]] {
            let out = common::run_with_timeout("module_refusal", flags, &importer, WAIT)
                .expect("module refusal must not hang");
            assert_eq!(out.status, status, "{name}, {flags:?}: {out:?}");
            assert!(out.stdout.is_empty(), "nothing may run: {out:?}");
            let report: Value = serde_json::from_str(&common::diagnostics(&out.stderr))
                .expect("static refusal must have an audit envelope");
            assert_eq!(report["outcome"]["tag"], "static", "{report}");
            let refusal = &report["outcome"]["payload"];
            assert_eq!(refusal["status"], status, "{report}");
            let rendered = refusal["rendered"].as_str().unwrap();
            assert!(rendered.contains(diagnostic), "{name}: {report}");
            if name == "missing" {
                assert!(
                    rendered.contains(&ral_core::path::shown(&module)),
                    "{report}"
                );
                assert!(rendered.contains("could not be read"), "{report}");
            }
            assert_eq!(report["trail"], json!([]), "{report}");
        }
    }
}

#[test]
fn check_and_run_deny_the_same_module_read_and_stamp_its_use() {
    let dir = Scratch::new("check_profile_denial");
    let allowed = dir.join("allowed.ral");
    let denied = dir.join("denied.ral");
    for module in [&allowed, &denied] {
        std::fs::write(module, "echo module-ran\nlet exported = 1\n").unwrap();
    }
    let profile = dir.join("profile.ral");
    std::fs::write(
        &profile,
        format!(
            "return [fs: [read: [{}], deny: [{}]]]\n",
            common::places(&[&dir.0]),
            common::places(&[&denied]),
        ),
    )
    .unwrap();
    let importer = dir.join("importer.ral");
    std::fs::write(&importer, "echo importer-ran\nuse './denied.ral'\n").unwrap();
    let mut run_outcome = None;
    for (flags, expected_stdout) in [
        (
            &["--capabilities", profile.to_str().unwrap()][..],
            "importer-ran\nmodule-ran\n",
        ),
        (
            &["--check", "--capabilities", profile.to_str().unwrap()][..],
            "",
        ),
    ] {
        let control = common::run_with_timeout(
            "profile_allowed_module",
            flags,
            &format!(
                "echo importer-ran\nuse '{}'\n",
                ral_core::path::shown(&allowed)
            ),
            WAIT,
        )
        .expect("allowed module must not hang");
        assert_eq!(control.status, 0, "{flags:?}: {control:?}");
        assert_eq!(control.stdout, expected_stdout, "{control:?}");

        let child = common::ral_command()
            .args(flags)
            .arg("--audit")
            .arg(&importer)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn ral");
        let out = common::wait_with_timeout(child, WAIT).expect("denied module must not hang");
        assert_eq!(out.status, 1, "{flags:?}: {out:?}");
        assert!(out.stdout.is_empty(), "nothing may run: {out:?}");
        let report: Value = serde_json::from_str(&common::diagnostics(&out.stderr))
            .expect("static refusal must retain its audit trail");
        assert_eq!(report["outcome"]["tag"], "static", "{report}");
        let rendered = report["outcome"]["payload"]["rendered"].as_str().unwrap();
        assert!(rendered.contains("U0001"), "{report}");
        assert!(rendered.contains("denied by grant"), "{report}");
        assert!(
            rendered.contains("reading this module was denied"),
            "{report}"
        );
        assert!(
            rendered.contains("an allowance cannot override an exclusion"),
            "{report}"
        );
        if let Some(outcome) = &run_outcome {
            assert_eq!(&report["outcome"], outcome, "check must agree with run");
        } else {
            run_outcome = Some(report["outcome"].clone());
        }
        let trail = report["trail"].as_array().unwrap();
        assert_eq!(trail.len(), 1, "{report}");
        let observation = &trail[0];
        assert_eq!(
            observation["site"],
            json!({"script": ral_core::path::shown(&importer), "line": 2, "col": 5}),
            "the denial belongs to the written use: {report}"
        );
        assert_eq!(observation["what"]["tag"], "check", "{report}");
        let check = &observation["what"]["payload"];
        assert_eq!(check["resource"]["tag"], "fs", "{report}");
        assert_eq!(check["decision"]["tag"], "denied", "{report}");
        assert_eq!(check["fields"]["op"], "read", "{report}");
        assert_eq!(
            check["fields"]["path"],
            ral_core::path::shown(dir.join("./denied.ral")),
            "{report}"
        );
    }
}

#[test]
fn check_evaluates_profiles_under_the_recursion_limit() {
    let dir = Scratch::new("check_boot_recursion");
    let profile = dir.join("profile.ral");
    std::fs::write(
        &profile,
        "echo profile-boot\n\
         let f = { |n| if $[$n <= 0] { return 0 } else { let prev = f $[$n - 1]; return $[$n + $prev] } }\n\
         let total = f 100\n\
         return [net: `offline]\n",
    )
    .unwrap();
    for (limit, status) in [("4096", 0), ("64", 2)] {
        let out = common::run_with_timeout(
            "check_boot_recursion",
            &[
                "--check",
                "--capabilities",
                profile.to_str().unwrap(),
                "--recursion-limit",
                limit,
            ],
            "echo importer-ran",
            WAIT,
        )
        .expect("profile evaluation must not hang");
        assert_eq!(out.status, status, "limit {limit}: {out:?}");
        assert_eq!(out.stdout, "profile-boot\n", "{out:?}");
        if status != 0 {
            assert!(out.stderr.contains("--capabilities"), "{out:?}");
            assert!(
                out.stderr.contains("recursion limit exceeded (64 frames)"),
                "{out:?}"
            );
        }
    }
}

#[test]
fn removed_static_flags_are_unknown() {
    for flag in ["--dump-ast", "--dump-ir", "-n"] {
        let out = common::run_with_timeout("removed_static_flag", &[flag], "echo ran", WAIT)
            .expect("unknown flag must not hang");
        assert_eq!(out.status, 2, "{flag}: {out:?}");
        assert!(out.stdout.is_empty(), "{out:?}");
        assert!(
            out.stderr
                .contains(&format!("unexpected argument '{flag}'")),
            "{out:?}"
        );
    }
}
