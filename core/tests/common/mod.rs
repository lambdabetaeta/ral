//! Shared scaffolding for `core/tests/*.rs`: a once-elaborated prelude
//! `Comp` and the schemes baked from it.  Both are memoised so the
//! prelude is parsed and elaborated exactly once per test binary.

#![allow(dead_code)] // not every test file uses every helper

use ral_core::{Comp, Scheme};
use std::sync::OnceLock;

pub fn prelude_comp() -> &'static Comp {
    static C: OnceLock<Comp> = OnceLock::new();
    C.get_or_init(|| {
        let src = include_str!("../../src/prelude.ral");
        let ast = ral_core::parse(src).expect("prelude parse");
        ral_core::elaborate(&ast, Default::default())
    })
}

pub fn prelude_schemes() -> &'static [(String, Scheme)] {
    static S: OnceLock<Vec<(String, Scheme)>> = OnceLock::new();
    S.get_or_init(|| ral_core::bake_prelude_schemes(prelude_comp()))
}
