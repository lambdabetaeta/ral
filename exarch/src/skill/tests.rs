use super::*;

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
    let skill = parse_skill(&skill_dir, "my-skill").unwrap();
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
    assert!(parse_skill(&skill_dir, "my-skill").is_none());
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
    let body = read_skill_body(&skill_dir).unwrap();
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

    let found = discover_all(cwd.path(), config.path());
    let dup: Vec<_> = found.iter().filter(|(n, _)| n == "dup").collect();
    assert_eq!(dup.len(), 1, "duplicate name should collapse to one entry");
    assert_eq!(
        dup[0].1, local,
        "the local skill must shadow the global one"
    );
}
