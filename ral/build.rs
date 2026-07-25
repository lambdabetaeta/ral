use std::process::Command;

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:git-probe] build-time git probe for the version hash; not turn-time model I/O"
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

    ral_core::boot::bake_prelude_to_out_dir();
}
