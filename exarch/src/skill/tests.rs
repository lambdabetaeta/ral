use super::*;

/// A [`Boot`] under a stack that restricts nothing, so a
/// fixture is judged by the real checks and refused by none.
fn boot<'a>(
    grants: &'a GrantStack,
    baseline: &'a Baseline,
    env: &'a EnvVars,
    cwd: &'a CwdPath,
) -> Boot<'a> {
    Boot::new(grants, baseline, env, cwd)
}

/// The skill whose directory is `dir`, found as the boot finds one.
fn found(dir: &Path) -> Found {
    let (grants, baseline, env) = (GrantStack::root(), Baseline::default(), EnvVars::new());
    let cwd = CwdPath::new(dir.to_path_buf()).unwrap();
    let mut reader = boot(&grants, &baseline, &env, &cwd);
    let file = reader.file(&reader.anchor(&dir.join("SKILL.md"))).unwrap();
    let name = dir.file_name().unwrap().to_str().unwrap().to_owned();
    Found {
        name,
        source: Source::Disk {
            dir: dir.to_path_buf(),
            file: Box::new(file),
        },
    }
}

/// The directory a found skill was read from; `None` for a built-in.
fn dir_of(found: &Found) -> Option<&Path> {
    match &found.source {
        Source::Disk { dir, .. } => Some(dir),
        Source::Builtin(_) => None,
    }
}

#[test]
fn valid_names() {
    assert!(valid_skill_name("pdf"));
    assert!(valid_skill_name("pdf-processing"));
    assert!(valid_skill_name("code-review-2"));
    assert!(valid_skill_name("a"));
    assert!(valid_skill_name("3d-printing"), "a digit may lead");
    assert!(valid_skill_name(&"a".repeat(64)), "64 is the longest");
}

#[test]
fn invalid_names() {
    assert!(!valid_skill_name(""));
    assert!(!valid_skill_name("-pdf"));
    assert!(!valid_skill_name("pdf-"));
    assert!(!valid_skill_name("pdf--processing"));
    assert!(!valid_skill_name("PDF"));
    assert!(!valid_skill_name("pdf_processing"));
    assert!(!valid_skill_name(&"a".repeat(65)));
}

#[test]
fn parse_valid_skill() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("my-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: my-skill\ndescription: does things\n---\n\n# Body\ncontent\n",
    )
    .unwrap();
    let skill = parse_skill(&found(&skill_dir)).unwrap();
    assert_eq!(skill.name, "my-skill");
    assert_eq!(skill.description, "does things");
}

/// Each way a frontmatter can fail, against the one good spelling.
#[test]
fn a_frontmatter_must_declare_a_matching_valid_name_and_a_description() {
    let parse = |raw| skill_from_frontmatter(raw, "my-skill", &SkillPlace::Builtin);
    assert!(parse("---\nname: my-skill\ndescription: x\n---\n").is_ok());
    for bad in [
        "# no frontmatter at all\n",
        "---\nname: [unclosed\ndescription: x\n---\n",
        "---\nname: 7\ndescription: x\n---\n",
        "---\nname: My-Skill\ndescription: x\n---\n",
        "---\nname: other\ndescription: x\n---\n",
        "---\nname: my-skill\n---\n",
        "---\ndescription: x\n---\n",
    ] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn read_body_after_frontmatter() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("my-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: my-skill\ndescription: d\n---\n\n# Hello\nworld\n",
    )
    .unwrap();
    let body = read_skill_body(&found(&skill_dir)).unwrap();
    assert_eq!(body, "# Hello\nworld");
}

#[test]
fn shared_roots_follow_project_then_personal_precedence() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    let cwd = repo.join("src");
    let config = tmp.path().join("config");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(repo.join(".git"), "gitdir: elsewhere\n").unwrap();
    let expected = [
        cwd.join(".exarch/skills"),
        cwd.join(".agents/skills"),
        repo.join(".agents/skills"),
        config.join("skills"),
        home.join(".agents/skills"),
    ];
    assert_eq!(skill_roots(&cwd, Some(&config), Some(&home)), expected);
    for (index, root) in expected.iter().enumerate() {
        for name in ["dup".to_owned(), format!("skill-{index}")] {
            let dir = root.join(&name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: root {index}\n---\nbody {index}\n"),
            )
            .unwrap();
        }
    }
    let (grants, baseline, env) = (GrantStack::root(), Baseline::default(), EnvVars::new());
    let at = CwdPath::new(cwd.clone()).unwrap();
    let mut reader = boot(&grants, &baseline, &env, &at);
    let metadata = discover_metadata(&cwd, Some(&config), Some(&home), &mut reader);
    assert_eq!(metadata.len(), 6 + BUILTIN.len());
    for index in 0..expected.len() {
        assert!(metadata.iter().any(|s| s.name == format!("skill-{index}")));
    }
    let all = discover_all(&cwd, Some(&config), Some(&home), &mut reader);
    let dup: Vec<_> = all.iter().filter(|s| s.name == "dup").collect();
    assert_eq!(dup.len(), 1);
    assert_eq!(dir_of(dup[0]), Some(expected[0].join("dup").as_path()));
    assert_eq!(read_skill_body(dup[0]).unwrap(), "body 0");
}

/// Discovery is judged by the stack the agent runs under: a skill directory
/// it refuses, and a root it does not read, are not found.
#[test]
fn discovery_under_a_restricted_stack_finds_only_what_it_may_read() {
    use ral_core::authority::policy::{Capabilities, FsPolicy};
    use ral_core::path::{FrozenPath, utf8};
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let (work, config) = (root.join("work"), root.join("config"));
    for dir in [
        work.join(".exarch/skills/alpha"),
        work.join(".exarch/skills/beta"),
        work.join(".agents/skills/shared"),
        work.join(".agents/skills/hidden"),
        config.join("skills/gamma"),
        config.join(".agents/skills/personal"),
    ] {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\ndescription: d\n---\n").unwrap();
    }
    let frozen = |path: &Path| FrozenPath::from_surface(utf8(path).unwrap());
    let grants = ral_core::test_helper::stack([ral_core::test_helper::layer(Capabilities {
        fs: Some(FsPolicy {
            read_prefixes: vec![frozen(&work)],
            deny_paths: vec![
                frozen(&work.join(".exarch/skills/beta")),
                frozen(&work.join(".agents/skills/hidden")),
            ],
            ..FsPolicy::default()
        }),
        ..Capabilities::default()
    })]);
    let (baseline, env) = (Baseline::default(), EnvVars::new());
    let at = CwdPath::new(work.clone()).unwrap();
    let all = discover_all(
        &work,
        Some(&config),
        Some(&config),
        &mut boot(&grants, &baseline, &env, &at),
    );
    let names: Vec<_> = all.iter().map(|found| found.name.as_str()).collect();
    let expected: Vec<_> = ["alpha", "shared"]
        .into_iter()
        .chain(BUILTIN.map(|(name, _)| name))
        .collect();
    assert_eq!(names, expected);
}

/// Every built-in parses as the skill it is listed as, with a body to load.
#[test]
fn builtins_parse_under_their_names() {
    for (name, text) in BUILTIN {
        let skill = skill_from_frontmatter(text, name, &SkillPlace::Builtin).unwrap();
        assert_ne!(skill.description, "");
        let found = builtin(name).unwrap();
        assert_ne!(read_skill_body(&found).unwrap().trim(), "");
    }
}

/// With no root holding a skill, the built-ins are listed, after any found on disk.
#[test]
fn builtins_are_listed_when_no_root_has_them() {
    let cwd = tempfile::tempdir().unwrap();
    let (grants, baseline, env) = (GrantStack::root(), Baseline::default(), EnvVars::new());
    let at = CwdPath::new(cwd.path().to_path_buf()).unwrap();
    let mut reader = boot(&grants, &baseline, &env, &at);
    let metadata = discover_metadata(cwd.path(), None, None, &mut reader);
    let names: Vec<_> = metadata.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, BUILTIN.map(|(name, _)| name));
}

/// A project's skill of a built-in's name shadows the built-in.
#[test]
fn a_project_skill_shadows_a_builtin() {
    let cwd = tempfile::tempdir().unwrap();
    let (name, _) = BUILTIN[0];
    let local = cwd.path().join(".exarch").join("skills").join(name);
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: mine\n---\nlocal body\n"),
    )
    .unwrap();
    let (grants, baseline, env) = (GrantStack::root(), Baseline::default(), EnvVars::new());
    let at = CwdPath::new(cwd.path().to_path_buf()).unwrap();
    let all = discover_all(
        cwd.path(),
        None,
        None,
        &mut boot(&grants, &baseline, &env, &at),
    );
    let hits: Vec<_> = all.iter().filter(|found| found.name == name).collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(dir_of(hits[0]), Some(local.as_path()));
    assert_eq!(read_skill_body(hits[0]).unwrap(), "local body");
}

/// A skill directory `name` under `root`, its `SKILL.md` holding `md`.
fn write_skill(root: &Path, name: &str, md: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), md).unwrap();
}

fn good(name: &str) -> String {
    format!("---\nname: {name}\ndescription: d\n---\n")
}

/// The names found under `cwd`'s project root, built-ins aside.
fn discovered(cwd: &Path) -> Vec<String> {
    let (grants, baseline, env) = (GrantStack::root(), Baseline::default(), EnvVars::new());
    let at = CwdPath::new(cwd.to_path_buf()).unwrap();
    let mut reader = boot(&grants, &baseline, &env, &at);
    let mut names: Vec<_> = discover_all(cwd, None, None, &mut reader)
        .into_iter()
        .map(|found| found.name)
        .filter(|name| BUILTIN.iter().all(|(builtin, _)| builtin != name))
        .collect();
    names.sort();
    names
}

#[test]
fn only_directories_holding_a_skill_md_are_skills() {
    let cwd = tempfile::tempdir().unwrap();
    let root = cwd.path().join(".exarch/skills");
    write_skill(&root, "real", &good("real"));
    write_skill(&root, ".hidden", &good(".hidden"));
    std::fs::create_dir_all(root.join("empty")).unwrap();
    std::fs::write(root.join("loose.md"), good("loose")).unwrap();
    assert_eq!(discovered(cwd.path()), ["real"]);
}

#[cfg(unix)]
#[test]
fn a_linked_skill_directory_is_a_skill() {
    let cwd = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    write_skill(elsewhere.path(), "linked", &good("linked"));
    let root = cwd.path().join(".exarch/skills");
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(elsewhere.path().join("linked"), root.join("linked")).unwrap();
    assert_eq!(discovered(cwd.path()), ["linked"]);
}

/// A skill whose frontmatter fails is found but not listed.
#[test]
fn metadata_silently_drops_a_skill_whose_frontmatter_fails() {
    let cwd = tempfile::tempdir().unwrap();
    let root = cwd.path().join(".exarch/skills");
    write_skill(&root, "fine", &good("fine"));
    write_skill(&root, "broken", "no frontmatter\n");
    write_skill(&root, "renamed", &good("other"));
    assert_eq!(discovered(cwd.path()), ["broken", "fine", "renamed"]);
    let (grants, baseline, env) = (GrantStack::root(), Baseline::default(), EnvVars::new());
    let at = CwdPath::new(cwd.path().to_path_buf()).unwrap();
    let mut reader = boot(&grants, &baseline, &env, &at);
    let listed = discover_metadata(cwd.path(), None, None, &mut reader);
    let names: Vec<_> = listed.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        std::iter::once("fine")
            .chain(BUILTIN.map(|(n, _)| n))
            .collect::<Vec<_>>()
    );
}

#[test]
fn frontmatter_failures_keep_the_skill_place_field_and_actual_yaml_head() {
    let place = SkillPlace::Disk("/skills/my-skill".into());
    for (raw, cause) in [
        ("plain body", FrontmatterFailure::Missing),
        (
            "---\n- x\n---\n",
            FrontmatterFailure::NotMapping(FrontmatterFailureNotMapping {
                found: PodKind::Array,
            }),
        ),
        (
            "---\ndescription: x\n---\n",
            FrontmatterFailure::MissingField(SkillField::Name),
        ),
        (
            "---\nname: my-skill\n---\n",
            FrontmatterFailure::MissingField(SkillField::Description),
        ),
        (
            "---\nname: 7\ndescription: x\n---\n",
            FrontmatterFailure::FieldShape(FrontmatterFailureFieldShape {
                field: SkillField::Name,
                found: PodKind::Integer,
            }),
        ),
        (
            "---\nname: my-skill\ndescription: true\n---\n",
            FrontmatterFailure::FieldShape(FrontmatterFailureFieldShape {
                field: SkillField::Description,
                found: PodKind::Boolean,
            }),
        ),
        (
            "---\nname: My-Skill\ndescription: x\n---\n",
            FrontmatterFailure::InvalidName(FrontmatterFailureInvalidName {
                name: "My-Skill".into(),
            }),
        ),
        (
            "---\nname: other\ndescription: x\n---\n",
            FrontmatterFailure::DirectoryNameMismatch(FrontmatterFailureDirectoryNameMismatch {
                declared: "other".into(),
                directory: "my-skill".into(),
            }),
        ),
    ] {
        assert_eq!(
            skill_from_frontmatter(raw, "my-skill", &place).unwrap_err(),
            SkillFailure::Frontmatter(SkillFailureFrontmatter {
                name: "my-skill".into(),
                place: place.clone(),
                cause
            })
        );
    }
    for (pod, kind) in [
        (gray_matter::Pod::Null, PodKind::Null),
        (gray_matter::Pod::String("x".into()), PodKind::String),
        (gray_matter::Pod::Integer(1), PodKind::Integer),
        (gray_matter::Pod::Float(1.0), PodKind::Float),
        (gray_matter::Pod::Boolean(false), PodKind::Boolean),
        (gray_matter::Pod::Array(vec![]), PodKind::Array),
        (
            gray_matter::Pod::Hash(std::collections::HashMap::default()),
            PodKind::Hash,
        ),
    ] {
        assert_eq!(pod_kind(&pod), kind);
    }
}

#[test]
fn yaml_parser_detail_is_foreign_and_body_loading_still_needs_no_metadata() {
    let broken = Found {
        name: "broken".into(),
        source: Source::Builtin("---\nname: [unclosed\n---\nbody"),
    };
    let error = read_skill_body(&broken).unwrap_err();
    let Reason::Host(host) = error.reason else {
        panic!("host failure")
    };
    let HostFailure::Exarch(ExarchFailure::Skill(SkillFailure::Yaml(failure))) = *host else {
        panic!("YAML parser failure")
    };
    assert_eq!(failure.name, "broken");
    assert_eq!(failure.place, SkillPlace::Builtin);
    assert_ne!(failure.detail, "");
    for body in ["plain body", "---\nanything: 1\n---\nplain body"] {
        let found = Found {
            name: "body-only".into(),
            source: Source::Builtin(body),
        };
        assert_eq!(read_skill_body(&found).unwrap(), "plain body");
        assert!(parse_skill(&found).is_err());
    }
}

#[test]
fn skill_text_keeps_utf8_offsets_instead_of_a_read_error_sentence() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("SKILL.md"), [b'a', 0xc3]).unwrap();
    let found = found(root.path());
    let error = read_skill_body(&found).unwrap_err();
    let Reason::Invalid(failure) = error.reason else {
        panic!("invalid UTF-8")
    };
    assert_eq!(failure.operation, "skill");
    assert_eq!(
        failure.cause,
        InvalidCause::Codec(CodecFailure::Utf8(CodecFailureUtf8 {
            subject: Utf8Subject::Path(ral_core::path::shown(root.path().join("SKILL.md"))),
            cause: Utf8Failure {
                valid_up_to: 1usize.into(),
                error_len: None
            },
        }))
    );
}
