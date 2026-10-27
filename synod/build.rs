#![allow(
    clippy::disallowed_methods,
    reason = "a build script reads cargo's environment"
)]

fn main() {
    if matches!(
        std::env::var("CARGO_CFG_TARGET_OS").as_deref(),
        Ok("macos" | "windows")
    ) {
        tauri_build::build();
    }
}
