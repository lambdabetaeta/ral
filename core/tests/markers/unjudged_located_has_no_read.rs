//! What the walk returns holds no bytes: `walk(p)?.read()` does not compile.

use ral_core::path::{LeafMode, LexicalPath, walk};

fn read(spelling: &LexicalPath) {
    let _ = walk(spelling, LeafMode::Follow).unwrap().read();
}

fn main() {}
