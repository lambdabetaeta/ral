//! An object is held for one operation: the handle is not copied.

use ral_core::path::Located;

fn twice(located: &Located) -> Located {
    located.clone()
}

fn main() {}
