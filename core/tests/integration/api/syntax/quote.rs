//! The quoting builtin emits source that preserves string values.

use crate::common::top_level;
use ral_core::Value;
use ral_core::test_access::binding_scheme;
use ral_core::test_helper::fresh_shell;

#[test]
fn ral_quote_roundtrips_strings_through_normal_evaluation() {
    let mut shell = fresh_shell();
    top_level(&mut shell, "let sample = ''").expect("bind a checked string");
    let scheme = binding_scheme(&shell, "sample").expect("sample carries a closed string scheme");

    for input in [
        "",
        "hello",
        "a b",
        "#hash",
        "it's",
        "a'#b",
        "two\nlines",
        "$x",
        "[a]",
        "é",
        "1.5",
        "true",
        "-x",
        "...",
        "@x",
        "a,b",
        "a:b",
        "'#'##'###",
        "\"; return 'injected'",
        "'; return injected #",
        "\nreturn injected\n",
        "!{return injected}",
        "$[1 + 2]",
        "\t\r\\\"",
    ] {
        shell.set_var("sample".into(), Value::string(input), scheme.clone());
        let emitted = top_level(&mut shell, "ral-quote $sample")
            .unwrap_or_else(|error| panic!("ral-quote failed for {input:?}: {error:?}"));
        let Value::String(word) = emitted else {
            panic!("ral-quote returned {emitted:?} for {input:?}, expected a string");
        };
        let actual = top_level(&mut fresh_shell(), &format!("return {word}"))
            .unwrap_or_else(|error| panic!("readback failed for {input:?}: {error:?}"));
        assert_eq!(actual, Value::string(input), "roundtrip for {input:?}");
    }
}
