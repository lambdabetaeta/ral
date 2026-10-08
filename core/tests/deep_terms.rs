//! Shapes whose depth the parser's nesting cap does not bound, compiled
//! through the door.  N is past what any walk survives on the 2 MB test
//! thread without it (debug frames run to 13 KB a level), and within what the
//! returned `Toplevel`'s drop glue survives there.

mod common;

use ral_core::HostSurface;
use ral_core::compile::compile_and_typecheck;
use ral_core::source::FileId;

const N: usize = 2_000;

fn survives(src: &str) {
    let schemes = common::schemes_for(&HostSurface::default());
    let _ = compile_and_typecheck(src, schemes, FileId::DUMMY, "deep", None);
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
