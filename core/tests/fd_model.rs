//! ral's fd model: the stderr fold `2>&1` is admitted; the identity dups
//! `1>&1` and `2>&2` are refused by the lexer.  The refusals of fd-numbered
//! redirects that name no stream are pinned end to end in
//! `ral/tests/cli_invocation.rs`.

#[test]
fn the_stderr_fold_is_admitted_and_identity_dups_are_refused() {
    assert!(ral_core::syntax::parser::parse("/bin/echo hi 2>&1").is_ok());
    for source in ["/bin/echo hi >&1", "/bin/echo hi 1>&1", "/bin/echo hi 2>&2"] {
        let err =
            ral_core::syntax::parser::parse(source).expect_err("an identity dup must not parse");
        assert!(
            err.message.contains("names the stream it already is"),
            "{source:?} gave {}",
            err.message
        );
    }
}
