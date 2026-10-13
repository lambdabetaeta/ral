#![allow(
    clippy::disallowed_macros,
    reason = "[silent:build-script] cargo reads these lines as directives"
)]

fn main() {
    ral_core::boot::bake_prelude_to_out_dir();
    // As in core's build.rs: std has no argv before `main` here.
    println!("cargo:rustc-check-cfg=cfg(ctor_without_argv)");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("gnu")
    {
        println!("cargo:rustc-cfg=ctor_without_argv");
    }
}
