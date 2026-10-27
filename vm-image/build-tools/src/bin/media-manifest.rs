#![allow(
    clippy::disallowed_macros,
    reason = "[silent:build-tool] the packaging script reads this result"
)]

fn main() {
    print!("{}", ral_vm_image_build_tools::manifest::lines());
}
