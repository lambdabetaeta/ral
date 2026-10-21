//! Only the sandbox side holds a position as `Installing`, and proves it by
//! a token no other crate can name: `open_nofollow` is out of reach.

use ral_core::path::RealPath;

fn hold(position: &RealPath) {
    let _ = position.open_nofollow();
}

fn main() {}
