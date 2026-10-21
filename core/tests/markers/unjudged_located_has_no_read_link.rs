//! A link's target is metadata, and metadata is judged.

use ral_core::path::Located;

fn target(located: &Located) {
    let _ = located.read_link();
}

fn main() {}
