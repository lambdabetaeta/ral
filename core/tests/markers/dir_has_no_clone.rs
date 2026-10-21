//! A directory is held for one operation: the handle is not copied.

use ral_core::path::Dir;

fn twice(dir: &Dir) -> Dir {
    dir.clone()
}

fn main() {}
