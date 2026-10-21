//! `RealPath::of(p)?.open_dir_nofollow(..)?.read_dir()` lists a denied
//! directory with no judgment, so it is out of reach.

use ral_core::path::RealPath;

fn list(position: &RealPath) {
    let _ = position.open_dir_nofollow();
}

fn main() {}
