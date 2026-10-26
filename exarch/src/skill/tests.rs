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

#[test]
fn name_must_match_dir() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("my-skill");
    std::fs::create_dir(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: wrong-name\ndescription: x\n---\n",
    )
    .unwrap();
    assert!(parse_skill(&found(&skill_dir)).is_err());
}

#[test]
fn frontmatter_errors_name_the_problem() {
    let err = |raw| skill_from_frontmatter(raw, "my-skill").unwrap_err();
    assert!(err("---\nname: other\ndescription: x\n---\n").contains("must match directory name"));
    assert!(err("---\nname: my-skill\n---\n").contains("description"));
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
fn local_overrides_global() {
    let cwd = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let local = cwd.path().join(".exarch").join("skills").join("dup");
    let global = config.path().join("skills").join("dup");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::create_dir_all(&global).unwrap();
    std::fs::write(local.join("SKILL.md"), "---\nname: dup\n---\n").unwrap();
    std::fs::write(global.join("SKILL.md"), "---\nname: dup\n---\n").unwrap();

    let (grants, baseline, env) = (GrantStack::root(), Baseline::default(), EnvVars::new());
    let at = CwdPath::new(cwd.path().to_path_buf()).unwrap();
    let all = discover_all(
        cwd.path(),
        Some(config.path()),
        None,
        &mut boot(&grants, &baseline, &env, &at),
    );
    let dup: Vec<_> = all.iter().filter(|found| found.name == "dup").collect();
    assert_eq!(dup.len(), 1, "duplicate name should collapse to one entry");
    assert_eq!(
        dir_of(dup[0]),
        Some(local.as_path()),
        "the local skill must shadow the global one"
    );
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
        let skill = skill_from_frontmatter(text, name).unwrap();
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
