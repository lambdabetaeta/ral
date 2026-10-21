//! The authority markers of `Located` and `Dir`, pinned by what does not
//! compile.
//!
//! A handle carries in its type who vouched for it, and each door asks for
//! the voucher it needs: `walk(p)?.read()` and `walk(p)?.stat()` are errors,
//! in this crate and in another.  A case is a file under `tests/markers/`
//! with the compiler's complaint beside it; `pass/` holds what must keep
//! compiling.  These are the `pub` surface.  What needs a `pub(crate)` door
//! is pinned from inside the crate, by `path::walk::tests::markers`, and the
//! two tokens' construction sites by its source scan.
//!
//! After a deliberate change to a diagnostic, regenerate the `.stderr` files
//! with `TRYBUILD=overwrite cargo test -p ral-core --test markers` and read
//! the diff.

#[test]
fn the_markers_refuse_what_they_have_not_vouched_for() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/markers/*.rs");
    cases.pass("tests/markers/pass/*.rs");
}
