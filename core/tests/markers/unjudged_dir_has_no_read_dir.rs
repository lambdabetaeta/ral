//! A directory the walk returns lists nothing until the stack has vouched.

use ral_core::path::Dir;

fn list(dir: &Dir) {
    let _ = dir.read_dir();
}

fn main() {}
