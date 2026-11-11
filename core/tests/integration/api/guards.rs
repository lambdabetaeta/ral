//! Lexical temp paths disappear without changing how their body ended.

use ral_core::fact::Reason;
use ral_core::protocol::Run;
use ral_core::shell::run::RunReport;
use ral_core::shell::runtime::{End, Value};
use ral_core::source::SourceOrigin;
use ral_core::test_helper::fresh_shell;

#[test]
fn temp_wrappers_release_on_return_failure_and_exit_even_if_already_removed() {
    for (wrapper, predicate, remove) in [
        ("with-temp-dir", "is-dir", "rm -rf"),
        ("with-temp-file", "is-file", "rm -f"),
    ] {
        for ending in ["return 42", "fail body", "exit 3"] {
            for removed in [false, true] {
                let source = format!(
                    "{wrapper} {{ |t| \
                     if !{{ {predicate} $t }} {{ echo $t }} else {{ fail 'no temp path' }}; \
                     {removal} {ending} }}",
                    removal = if removed {
                        format!("{remove} $t;")
                    } else {
                        String::new()
                    },
                );
                let mut shell = fresh_shell();
                let report = shell.run(Run::captured(
                    source.as_str(),
                    SourceOrigin::named("<test>"),
                ));
                let RunReport::Ran {
                    ending: actual,
                    captured: Some(captured),
                    ..
                } = report
                else {
                    panic!("the temp wrapper must run: {source}");
                };
                let stdout =
                    String::from_utf8(captured.stdout.bytes).expect("a temp path is UTF-8");
                let paths: Vec<_> = stdout.lines().collect();
                assert_eq!(
                    paths.len(),
                    1,
                    "the body must see one live path: {source}: {stdout:?}"
                );
                assert!(
                    !std::path::Path::new(paths[0]).exists(),
                    "{source}: {} leaked",
                    paths[0]
                );
                match ending {
                    "return 42" => assert!(
                        matches!(actual, End::Ok(Value::Int(42))),
                        "{source}: {actual:?}"
                    ),
                    "fail body" => assert!(
                        matches!(&actual, End::Err(error) if error.reason == Reason::raised("body")),
                        "{source}: {actual:?}"
                    ),
                    "exit 3" => assert!(matches!(actual, End::Exited(3)), "{source}: {actual:?}"),
                    _ => unreachable!(),
                }
            }
        }
    }
}
