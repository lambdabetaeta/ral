//! Shapes whose depth the parser's nesting cap does not bound, compiled
//! through the public entry.  N is past what any walk survives on the 2 MB test
//! thread without it (debug frames run to 13 KB a level), and within what the
//! returned `Toplevel`'s drop glue survives there.

use ral_core::HostSurface;
use ral_core::compile::{CompileError, compile_and_typecheck};
use ral_core::source::FileId;

const N: usize = 2_000;

fn survives(src: &str) {
    let schemes = crate::common::schemes_for(&HostSurface::default());
    drop(
        compile_and_typecheck(src, schemes, FileId::DUMMY, None, None)
            .unwrap_or_else(|e| panic!("depth {N} must compile: {e}")),
    );
}

#[test]
fn deep_sum() {
    survives(&format!("return $[1{}]", "+1".repeat(N - 1)));
}

#[test]
fn deep_lambda() {
    let params = (1..=N).map(|i| format!("x{i}")).collect::<Vec<_>>();
    survives(&format!("{{ |{}| return 1 }}", params.join(" ")));
}

#[test]
fn deep_failure_chain() {
    survives(&format!("a{}", " ? a".repeat(N - 1)));
}

#[test]
fn deep_elsif_chain() {
    survives(&format!(
        "let c = true\nif $c {{ a }}{}",
        " elsif $c { a }".repeat(N - 1)
    ));
}

#[test]
fn deep_block() {
    survives(&format!("{{\n{}return $x\n}}", "let x = 1\n".repeat(N)));
}

#[test]
fn deep_block_reports_an_arithmetic_type_error() {
    let src = format!("{{\n{}return $['x' + 1]\n}}", "let x = 1\n".repeat(N));
    let schemes = crate::common::schemes_for(&HostSurface::default());
    let result = compile_and_typecheck(&src, schemes, FileId::DUMMY, None, None);
    let Err(CompileError::Types(errors)) = result else {
        panic!("invalid arithmetic at depth {N} must report type errors, got {result:?}");
    };
    let codes: Vec<_> = errors.iter().map(|error| error.kind.code()).collect();
    assert_eq!(codes, ["T0074"]);
}
