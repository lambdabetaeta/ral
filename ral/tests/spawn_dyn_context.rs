// Integration tests: spawned threads inherit the dynamic context (`DynContext`)
// from the parent — specifically the `handler_stack` (`within`) and
// capabilities stack (`grant`) that was active at spawn time.

mod common;

use common::run;

// A named handler installed via `within` must be visible in a spawned thread.
#[test]
fn spawn_inherits_within_handlers() {
    let out = run(
        "ral_spawn_dyn",
        r#"
        within [handlers: [mycmd: { |args| echo "handled" }]] {
            let h = spawn { mycmd }
            let res = await $h
            echo !{to-bytes $res[stdout] | from-string}
        }
    "#,
    );
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
    let out = run(
        "ral_spawn_dyn",
        r"
        within [env: [MY_DYN_VAR: hello-from-dyn]] {
            let h = spawn { printenv MY_DYN_VAR }
            let res = await $h
            echo !{to-bytes $res[stdout] | from-string}
        }
    ",
    );
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
    let out = run(
        "ral_spawn_dyn",
        r"
        let target = temp-dir
        within [dir: $target] {
            let h = spawn { cwd }
            let res = await $h
            if !{equal $res[value] $target} { echo 'dir inherited' } else { echo 'dir mismatch' }
        }
        rm -rf $target
    ",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        out.stdout.contains("dir inherited"),
        "dir not inherited by spawn: stdout={:?}",
        out.stdout
    );
}

// A named handler must fire inside `par`-spawned concurrent blocks.
// The arm preserves `mycmd`'s external byte-output mode (it `echo`s) and
// surfaces its result as a par output value, since par's workers buffer
// stdout independently and the test checks the collected return values.
#[test]
fn par_inherits_within_handlers() {
    let out = run(
        "ral_spawn_dyn",
        r#"
        within [handlers: [mycmd: { |args| echo handled; return "handled" }]] {
            let res = par { |x| mycmd $x } [a, b] 0
            echo ...$res
        }
    "#,
    );
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
    let out = run(
        "ral_spawn_dyn",
        r#"
        let h = spawn {
            within [handlers: [localcmd: { |args| echo "child-handler" }]] {
                localcmd
            }
        }
        let res = await $h
        echo !{to-bytes $res[stdout] | from-string}
        try { localcmd } { |_| echo "parent: no handler" }
    "#,
    );
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

// A `grant` in force at spawn time must still bind inside the worker: the
// capability stack travels with the cloned context, so a denied command stays
// denied across the thread boundary — and only within the grant's extent.
#[test]
fn spawn_inherits_grant_denial() {
    let out = run(
        "ral_spawn_dyn",
        r"
        grant [exec: [ls: 'deny']] {
            let h = spawn { ls . }
            try {
                let result = await $h
                echo LEAKED
            } { |_|
                echo 'denied in worker'
            }
        }
        let ok = !{ls . | from-lines}
        echo 'parent still allowed'
    ",
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);
    assert!(
        !out.stdout.contains("LEAKED"),
        "grant did not reach the worker: stdout={:?}",
        out.stdout
    );
    assert!(
        out.stdout.contains("denied in worker"),
        "worker did not report the denial: stdout={:?}",
        out.stdout
    );
    assert!(
        out.stdout.contains("parent still allowed"),
        "denial escaped the grant's extent: stdout={:?}",
        out.stdout
    );
}
