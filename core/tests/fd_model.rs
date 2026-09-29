//! ral's fd model: the identity dups `1>&1` and `2>&2` and the stderr fold
//! `2>&1` are admitted by the parser.  The refusals of fd-numbered redirects
//! that name no stream are pinned end to end in `ral/tests/cli_invocation.rs`.

#[test]
fn identity_dups_and_the_stderr_fold_are_admitted() {
    for source in [
        "/bin/echo hi >&1",
        "/bin/echo hi 1>&1",
        "/bin/echo hi 2>&2",
        "/bin/echo hi 2>&1",
    ] {
        assert!(
            ral_core::syntax::parser::parse(source).is_ok(),
            "{source:?} should parse"
        );
    }
}
