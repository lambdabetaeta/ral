//! `docs/ral-wiki/invariants/core-below-shell.md` is the prose of this
//! invariant: the low modules of core name no `crate::shell` path, nor an item
//! `lib.rs` re-exports from `shell::`, directly or through `super::`, in
//! production code.

use ral_lint::{module_path, production_code, root_refs, rust_files, workspace_root};
use regex::Regex;

/// The modules below the shell; `data` is theirs too (`data::print`).
const BELOW_SHELL: [&str; 10] = [
    "authority",
    "data",
    "egress",
    "environ",
    "fact",
    "host",
    "io",
    "own",
    "path",
    "process",
];

/// The names `lib.rs` re-exports from `shell::`: `pub use shell::a::{B, C as D};`
/// puts `B` and `D` at the crate root, where `crate::B` reaches the shell.
fn shell_reexports(lib: &str) -> Vec<String> {
    let statement = Regex::new(r"pub\s+use\s+shell\s*::([^;]+);").unwrap();
    statement
        .captures_iter(&production_code(lib))
        .flat_map(|c| {
            let path = c[1].to_owned();
            let items = match path.split_once('{') {
                Some((_, group)) => group.trim_end().trim_end_matches('}').to_owned(),
                None => path,
            };
            items
                .split(',')
                .map(|item| item.rsplit("::").next().unwrap())
                .map(|item| item.rsplit(" as ").next().unwrap().trim().to_owned())
                .filter(|name| !name.is_empty())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The root references in `code` that reach the shell: `shell` itself, or a
/// name in `reexports`.
fn shell_refs(code: &str, depth: usize, reexports: &[String]) -> Vec<String> {
    root_refs(code, depth)
        .into_iter()
        .filter(|r| r == "shell" || reexports.contains(r))
        .collect()
}

#[test]
fn core_below_shell_names_no_shell_path() {
    let src = workspace_root().join("core/src");
    let reexports = shell_reexports(&std::fs::read_to_string(src.join("lib.rs")).unwrap());
    assert!(
        reexports.iter().any(|r| r == "Shell"),
        "no `pub use shell::` re-export found in lib.rs: {reexports:?}"
    );
    let mut scanned = [0usize; BELOW_SHELL.len()];
    for file in &rust_files(&src) {
        let module = module_path(&src, file);
        if module.iter().any(|m| m == "tests" || m.ends_with("_tests")) {
            continue;
        }
        let Some(at) = BELOW_SHELL.iter().position(|m| *m == module[0]) else {
            continue;
        };
        scanned[at] += 1;
        let code = production_code(&std::fs::read_to_string(file).unwrap());
        let refs = shell_refs(&code, module.len(), &reexports);
        assert!(
            refs.is_empty(),
            "{} reaches the shell through {refs:?}: lower modules exchange domain data and the \
             shell interprets it; move the shell side up (docs/ral-wiki/invariants/core-below-shell.md)",
            file.display()
        );
    }
    assert!(
        scanned.iter().all(|n| *n > 0),
        "no source found for some module of {BELOW_SHELL:?}: {scanned:?}"
    );
}

#[test]
fn shell_reexports_are_read_from_lib() {
    let lib = "pub use io::Captured;\n\
               pub use shell::boot::HostSurface;\n\
               pub use shell::runtime::{\n    Break, Error as Failure,\n    Shell,\n};\n";
    assert_eq!(
        shell_reexports(lib),
        ["HostSurface", "Break", "Failure", "Shell"]
    );
}

#[test]
fn root_reference_to_a_shell_reexport_is_caught() {
    let reexports = shell_reexports("pub use shell::runtime::{Shell, Value};");
    let caught = |code, depth| !shell_refs(code, depth, &reexports).is_empty();
    assert!(caught("use crate::Shell;", 2));
    assert!(caught("use crate::{Captured, Value};", 2));
    assert!(caught("fn f(_: crate::Shell) {}", 2));
    assert!(caught("use super::super::Shell;", 2));
    assert!(caught("use crate::shell::runtime::Shell;", 2));
    assert!(!caught("use crate::io::Captured;", 2));
    assert!(!caught("use super::Shell;", 2));
}
