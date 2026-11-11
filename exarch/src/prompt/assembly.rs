//! The assembled prompt's shape: section order and presence, the file chain
//! behind `Workspace`, and the grant section.

use super::*;
use ral_core::authority::grant::value::{Axes, GrantValue, LayerValue};
use ral_core::authority::policy::GrantStack;
use ral_core::data::FOValue;
use std::fs;

const HEADINGS: [&str; 9] = [
    "Ral",
    "Editing",
    "Builtins",
    "Tasks",
    "Context management",
    "Host",
    "Workspace",
    "Skills",
    "Surfacing",
];

/// A scratch tree under its canonical root, held for as long as the test runs.
fn tree() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(tmp.path()).unwrap();
    (tmp, root)
}

fn assembled(
    files: &[PathBuf],
    cwd: &Path,
    config: Option<&Path>,
    interactive: bool,
    edit: EditScheme,
) -> Result<String, String> {
    let invocation = Invocation {
        stack: GrantStack::root(),
        restricts: Vec::new(),
    };
    assemble(
        files,
        &invocation,
        &Baseline::default(),
        crate::app::EXARCH,
        &CwdPath::new(cwd.to_path_buf()).unwrap(),
        config,
        interactive,
        edit,
    )
}

/// The section headings `prompt` carries, in order.
fn headings(prompt: &str) -> Vec<&str> {
    prompt
        .lines()
        .filter_map(|line| line.strip_prefix("# "))
        .filter(|heading| HEADINGS.contains(heading))
        .collect()
}

fn plain(cwd: &Path, interactive: bool, edit: EditScheme) -> String {
    assembled(&[], cwd, None, interactive, edit).unwrap()
}

#[test]
fn sections_come_in_the_order_they_are_built() {
    let (_tmp, root) = tree();
    fs::write(root.join("AGENTS.md"), "workspace text").unwrap();
    let prompt = plain(&root, true, EditScheme::Replace);
    assert_eq!(headings(&prompt), HEADINGS);
    assert!(prompt.contains("workspace text"));
    assert!(prompt.contains(crate::skill::BUILTIN[0].0));
}

#[test]
fn workspace_appears_only_where_an_agents_file_is_found() {
    let (_tmp, root) = tree();
    assert!(!headings(&plain(&root, false, EditScheme::Replace)).contains(&"Workspace"));
    fs::write(root.join("AGENTS.md"), "found").unwrap();
    assert!(headings(&plain(&root, false, EditScheme::Replace)).contains(&"Workspace"));
}

/// `--system` replaces the persona and nothing after it.
#[test]
fn system_files_replace_only_the_persona() {
    let (_tmp, root) = tree();
    let (first, second) = (root.join("one.md"), root.join("two.md"));
    fs::write(&first, "first persona").unwrap();
    fs::write(&second, "second persona").unwrap();
    let default = plain(&root, true, EditScheme::Replace);
    let custom = assembled(&[first, second], &root, None, true, EditScheme::Replace).unwrap();
    assert!(custom.starts_with("first persona\n\nsecond persona\n\n# Ral"));
    assert!(default.starts_with(include_str!("../../data/system.md").trim_end()));
    assert_eq!(headings(&custom), headings(&default));
    assert!(custom.contains(RAL_REFERENCE.trim_end()));
}

/// The operator's file, then the repo root down to `cwd`; a directory with
/// none is skipped.
#[test]
fn agents_files_read_the_config_dir_first_then_the_repo_chain_outermost_first() {
    let (_tmp, root) = tree();
    let (config, repo) = (root.join("config"), root.join("repo"));
    let sub = repo.join("middle/sub");
    fs::create_dir_all(&config).unwrap();
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(&sub).unwrap();
    for (dir, text) in [(&config, "config"), (&repo, "repo"), (&sub, "sub")] {
        fs::write(dir.join("AGENTS.md"), text).unwrap();
    }
    let (baseline, env) = (Baseline::default(), EnvVars::new());
    let stack = GrantStack::root();
    let cwd = CwdPath::new(sub.clone()).unwrap();
    let reader = Boot::new(&stack, &baseline, &env, &cwd);
    assert_eq!(
        read_agents(&sub, Some(&config), &reader).unwrap(),
        "config\n\nrepo\n\nsub"
    );
}

#[test]
fn an_agents_file_that_exists_but_cannot_be_read_fails_the_assembly() {
    let (_tmp, root) = tree();
    fs::write(root.join("AGENTS.md"), [0xffu8, 0xfe, 0xfd]).unwrap();
    let error = assembled(&[], &root, None, false, EditScheme::Replace).unwrap_err();
    assert!(error.contains("AGENTS.md"), "{error}");
}

/// A grant value as it crosses.
fn data(axes: Axes) -> FOValue {
    GrantValue::from(axes).into()
}

/// Without a layer there is nothing to describe or to be denied: no legend,
/// no grant, where any layer brings both.
#[test]
fn a_stack_without_layers_is_ambient_and_describes_none() {
    let grants = |layers| Grants {
        layers,
        withholds_gpu: false,
    };
    let ambient = grant_section(&grants(Vec::new()));
    let empty = LayerValue {
        grant: data(Axes::default()),
        fragment: None,
    };
    let described = grant_section(&grants(vec![empty]));
    assert!(described.contains("\ngrant []\n") && !ambient.contains("grant ["));
    let legend = GRANT_LEGEND.trim_end();
    assert!(described.contains(legend) && !ambient.contains(legend));
}
