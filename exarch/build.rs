#![allow(clippy::disallowed_methods, reason = "build script")]
#![allow(
    clippy::disallowed_macros,
    reason = "[silent:build-script] cargo reads these lines as directives"
)]

fn main() {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    std::fs::write(
        out.join("prelude_baked.bin"),
        ral_core::shell::boot::bake_prelude(),
    )
    .expect("failed to write prelude_baked.bin");
    // As in core's build.rs: std has no argv before `main` here.
    println!("cargo:rustc-check-cfg=cfg(ctor_without_argv)");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("gnu")
    {
        println!("cargo:rustc-cfg=ctor_without_argv");
    }
}
