//! Lexical temp paths disappear without changing how their body ended.

use crate::common::{diagnostics, run_with_timeout};
use std::time::Duration;

#[test]
fn temp_wrappers_release_on_return_failure_and_exit_even_if_already_removed() {
    for (wrapper, predicate, remove) in [
        ("with-temp-dir", "is-dir", "rm -rf"),
        ("with-temp-file", "is-file", "rm -f"),
    ] {
        for (ending, status) in [("return 42", 0), ("fail body", 1), ("exit 3", 3)] {
            for removed in [false, true] {
                let call = format!(
                    "{wrapper} {{ |t| \
                     if !{{ {predicate} $t }} {{ echo $t }} else {{ fail 'no temp path' }}; \
                     {removal} {ending} }}",
                    removal = if removed {
                        format!("{remove} $t;")
                    } else {
                        String::new()
                    },
                );
                let source = if status == 0 {
                    format!(
                        "let guard-value = {call}; \
                         if $[$guard-value == 42] {{}} else {{ fail 'wrong body result' }}"
                    )
                } else {
                    call
                };
                let actual =
                    run_with_timeout("ral_temp_guard", &[], &source, Duration::from_secs(10))
                        .expect("temp wrapper timed out");
                assert_eq!(actual.status, status, "{source}: {actual:?}");
                if status == 1 {
                    assert!(
                        diagnostics(&actual.stderr).contains("body"),
                        "{source}: {actual:?}"
                    );
                }
                let paths: Vec<_> = actual.stdout.lines().collect();
                assert_eq!(
                    paths.len(),
                    1,
                    "the body must see one live path: {source}: {actual:?}"
                );
                assert!(
                    !std::path::Path::new(paths[0]).exists(),
                    "{source}: {} leaked",
                    paths[0]
                );
            }
        }
    }
}
