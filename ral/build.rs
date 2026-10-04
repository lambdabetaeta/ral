use std::process::Command;

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:git-probe] build-time git probe for the version hash; not turn-time model I/O"
)]
fn main() {
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/heads");

    // `+<hash>` in a git checkout; empty in a release tarball, whose
    // version is already exact.
    let suffix = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| format!("+{s}"))
        .unwrap_or_default();
    println!("cargo:rustc-env=RAL_VERSION_SUFFIX={suffix}");

    // Windows gives the main thread 1 MiB; the recursive front end needs the
    // 8 MiB every other platform's main thread has.
    if std::env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|e| e == "msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }

    ral_core::boot::bake_prelude_to_out_dir();
}
