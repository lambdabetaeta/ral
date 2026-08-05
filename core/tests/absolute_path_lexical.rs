#![allow(clippy::disallowed_methods)]

//! `absolute-path` is lexical: the same sigil expansion and logical-cwd
//! anchoring as `resolve-path`, but no `canonicalise_strict` — symlinks
//! stay as written, missing paths are fine, and `.`/`..` fold by pure
//! string math, clamping at `/`.

mod common;

use ral_core::transport::{Program, Run};
use ral_core::types::{Capabilities, Shell, Value};
use ral_core::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, builtins};

fn fresh_shell() -> Shell {
    let mut shell = Shell::default();
    shell.seed_default_env_vars();
    builtins::register(&mut shell, common::prelude_comp());
    shell
}

fn top_level(shell: &mut Shell, source: &str) -> String {
    let report = shell.run(RunRequest {
        run: Run {
            program: Program::Source(source.into()),
            script_name: "<test>".into(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Inherit,
            terminal: RequestedTerminalAccess::Leased,
            stdin: RunStdin::Inherit,
            trail: None,
        },
        surface: None,
        deferred: None,
        desk: None,
        nursery: None,
        lifecycle: Box::new(()),
    });
    match report {
        RunReport::Ran { ending, .. } => match ending.into_result().expect("evaluation succeeds") {
            Value::String(s) => s,
            other => panic!("expected a String, got {other:?}"),
        },
        RunReport::Static { .. } => panic!("well-formed source must run: {source:?}"),
    }
}

/// Relative input (through `.`/`..`, over components that don't exist)
/// joins the `within [dir: …]` logical cwd *verbatim* — a temp root
/// behind a firmlink (`/tmp` → `/private/tmp`) comes back as spelled,
/// never as `realpath(3)` would respell it.
#[test]
fn anchors_to_within_dir_without_canonicalising() {
    let root = std::env::temp_dir().join(format!("ral-abs-lex-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut shell = fresh_shell();
    let out = top_level(
        &mut shell,
        &format!(
            "within [dir: '{}'] {{ absolute-path 'no/such/./dir/../leaf' }}",
            root.to_string_lossy()
        ),
    );
    assert_eq!(std::path::Path::new(&out), root.join("no/such/leaf"));
    std::fs::remove_dir_all(&root).ok();
}

/// The zsh-`:a` distinction: a symlink component is never resolved, and
/// `..` pops the *link*, not the link's target's parent.
#[cfg(unix)]
#[test]
fn symlink_component_survives_and_dotdot_folds_lexically() {
    let root = std::env::temp_dir().join(format!("ral-abs-link-{}", std::process::id()));
    std::fs::create_dir_all(root.join("d1/real")).unwrap();
    std::os::unix::fs::symlink(root.join("d1/real"), root.join("link")).unwrap();
    let mut shell = fresh_shell();
    let w = |body: &str| format!("within [dir: '{}'] {{ {body} }}", root.to_string_lossy());

    let out = top_level(&mut shell, &w("absolute-path 'link'"));
    assert_eq!(
        std::path::Path::new(&out),
        root.join("link"),
        "the symlink must be returned as written, not resolved to d1/real"
    );
    // Physical resolution would answer `<root>/d1/x`; lexical pops `link`.
    let out = top_level(&mut shell, &w("absolute-path 'link/../x'"));
    assert_eq!(std::path::Path::new(&out), root.join("x"));
    std::fs::remove_dir_all(&root).ok();
}

// `..` above `/` clamps at `/` — the rooted arm of `fold_dots`.
#[cfg(unix)]
#[test]
fn dotdot_clamps_at_root() {
    let mut shell = fresh_shell();
    assert_eq!(top_level(&mut shell, "absolute-path '/..'"), "/");
    assert_eq!(top_level(&mut shell, "absolute-path '/a/../../x'"), "/x");
}

// `~` expands against HOME, same stage-1 sigil pass as `resolve-path`.
#[cfg(unix)]
#[test]
fn tilde_expands_against_home() {
    let home = std::env::var("HOME").expect("HOME is set in the test environment");
    let mut shell = fresh_shell();
    let out = top_level(&mut shell, "absolute-path '~/abs-lex-probe'");
    assert_eq!(
        std::path::Path::new(&out),
        std::path::Path::new(&home).join("abs-lex-probe")
    );
}

// Empty input is the cwd itself (zsh `:a` on an empty word), and a
// trailing slash drops in the component normal form.
#[test]
fn empty_input_is_cwd_and_trailing_slash_drops() {
    let root = std::env::temp_dir().join(format!("ral-abs-edge-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut shell = fresh_shell();
    let w = |body: &str| format!("within [dir: '{}'] {{ {body} }}", root.to_string_lossy());
    assert_eq!(
        std::path::Path::new(&top_level(&mut shell, &w("absolute-path ''"))),
        root
    );
    assert_eq!(
        std::path::Path::new(&top_level(&mut shell, &w("absolute-path 'sub/'"))),
        root.join("sub")
    );
    std::fs::remove_dir_all(&root).ok();
}
