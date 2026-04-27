// Integration tests: spawned threads inherit the dynamic context (`DynContext`)
// from the parent — specifically the `handler_stack` (`within`) and
// capabilities stack (`grant`) that was active at spawn time.
#![cfg(unix)]

mod common;

use common::run;

// A named handler installed via `within` must be visible in a spawned thread.
#[test]
fn spawn_inherits_within_handlers() {
    let out = run("ral_spawn_dyn", r#"
        within [handlers: [mycmd: { echo "handled" }]] {
            let h = spawn { mycmd }
            let r = await $h
            echo !{to-bytes $r[stdout] | from-string}
        }
    "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("handled"),
        "handler not inherited by spawn: stdout={:?}",
        out.stdout
    );
}

// Environment overrides from `within [env:]` must reach the spawned thread.
#[test]
fn spawn_inherits_within_env() {
    let out = run("ral_spawn_dyn", r#"
        within [env: [MY_DYN_VAR: hello-from-dyn]] {
            let h = spawn { printenv MY_DYN_VAR }
            let r = await $h
            echo !{to-bytes $r[stdout] | from-string}
        }
    "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("hello-from-dyn"),
        "env override not inherited by spawn: stdout={:?}",
        out.stdout
    );
}

// `within [dir:]` must set the cwd for code running in a spawned thread.
#[test]
fn spawn_inherits_within_dir() {
    let out = run("ral_spawn_dyn", r#"
        within [dir: /tmp] {
            let h = spawn { pwd }
            let r = await $h
            echo !{to-bytes $r[stdout] | from-string}
        }
    "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    // macOS resolves /tmp -> /private/tmp; match the common suffix.
    assert!(
        out.stdout.contains("tmp"),
        "dir not inherited by spawn: stdout={:?}",
        out.stdout
    );
}

// A named handler must fire inside `par`-spawned tasks.
// Uses `return` to surface the handler result as a par output value, since
// par tasks buffer stdout independently and the test checks return values.
#[test]
fn par_inherits_within_handlers() {
    let out = run("ral_spawn_dyn", r#"
        within [handlers: [mycmd: { |args| return "handled" }]] {
            let r = par { |x| mycmd $x } [a, b] 0
            echo ...$r
        }
    "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("handled"),
        "handler not inherited by par: stdout={:?}",
        out.stdout
    );
}

// A handler installed inside `spawn` must NOT leak to the parent or to
// sibling spawned threads.
#[test]
fn spawn_handler_does_not_leak_to_parent() {
    let out = run("ral_spawn_dyn", r#"
        let h = spawn {
            within [handlers: [localcmd: { echo "child-handler" }]] {
                localcmd
            }
        }
        let r = await $h
        echo !{to-bytes $r[stdout] | from-string}
        try { localcmd 2>/dev/null } { |_| echo "parent: no handler" }
    "#);
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("child-handler"),
        "child handler did not fire: stdout={:?}",
        out.stdout
    );
    assert!(
        out.stdout.contains("parent: no handler"),
        "handler leaked to parent: stdout={:?}",
        out.stdout
    );
}
