use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=../core/src/prelude.ral");
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

    let src =
        fs::read_to_string("../core/src/prelude.ral").expect("failed to read core/src/prelude.ral");

    let ast = ral_core::parse(&src).unwrap_or_else(|e| {
        eprintln!("build: prelude parse error: {e}");
        std::process::exit(1);
    });
    let comp = ral_core::elaborate(&ast, Default::default());

    let ir_bytes = postcard::to_allocvec(&comp).expect("prelude IR serialization failed");
    let schemes = ral_core::bake_prelude_schemes(&comp);
    let scheme_bytes =
        postcard::to_allocvec(&schemes).expect("prelude schemes serialization failed");

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out.join("prelude_baked.bin"), ir_bytes).expect("failed to write prelude_baked.bin");
    fs::write(out.join("prelude_schemes.bin"), scheme_bytes)
        .expect("failed to write prelude_schemes.bin");
}
