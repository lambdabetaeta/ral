//! Build-time prelude bake.  Mirrors `ral/build.rs`: parse, elaborate, and
//! emit two postcard blobs (`prelude_baked.bin`, `prelude_schemes.bin`) to
//! `OUT_DIR` so the exarch embeds a ready-to-run prelude at startup.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=../core/src/prelude.ral");

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

    // Authoritative `name — purpose` listing.  Types are dropped: they were
    // expensive in tokens, and the universally-quantified shapes (∀α β γ δ)
    // told the model almost nothing about argument order or composition.
    // The cookbook in ral.md carries that load now; this listing is just
    // the truth-source for which names exist.
    let docs = prelude_docs(&src);
    let mut names: Vec<&str> = schemes
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| !n.starts_with('_'))
        .collect();
    names.sort_unstable();
    let mut listing = String::new();
    for name in names {
        match docs.get(name) {
            Some(d) => listing.push_str(&format!("    {name} — {d}\n")),
            None => listing.push_str(&format!("    {name}\n")),
        }
    }
    fs::write(out.join("prelude_signatures.txt"), listing)
        .expect("failed to write prelude_signatures.txt");
}

/// Map `name -> docstring` from the `## doc` comments in prelude.ral.
fn prelude_docs(src: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let mut doc: Option<String> = None;
    for line in src.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("## ") {
            doc = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = t.strip_prefix("let ") {
            if let Some(d) = doc.take()
                && let Some((name, _)) = rest.split_once('=')
            {
                map.insert(name.trim().to_string(), d);
            }
            continue;
        }
        if t.is_empty() {
            doc = None;
        }
    }
    map
}
