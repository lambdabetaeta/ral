//! `file-info` on a denied name is refused, so a stat is judged:
//! `walk(p)?.stat()` does not compile.

use ral_core::path::{LeafMode, LexicalPath, walk};

fn stat(spelling: &LexicalPath) {
    let _ = walk(spelling, LeafMode::Follow).unwrap().stat();
}

fn main() {}
