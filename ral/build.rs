use std::process::Command;

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:git-probe] build-time git probe for the version hash; not turn-time model I/O"
)]
fn main() {
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs/heads");

    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=RAL_GIT_HASH={hash}");

    ral_core::host::bake_prelude_to_out_dir();
}
