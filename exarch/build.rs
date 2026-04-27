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

    // Authoritative `name : type` listing for every prelude binding,
    // derived from the same `Scheme` table the exarch runs with.  We
    // pair each scheme with its `## doc` comment from prelude.ral so
    // the prompt carries types AND human-readable purpose.
    let docs = prelude_docs(&src);
    let mut entries: Vec<(String, String)> = schemes
        .iter()
        .filter(|(n, _)| !n.starts_with('_'))
        .map(|(n, s)| (n.clone(), ral_core::typecheck::fmt_scheme(s)))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut listing = String::new();
    for (name, ty) in &entries {
        match docs.get(name.as_str()) {
            Some(d) => listing.push_str(&format!("    {name} : {ty} — {d}\n")),
            None => listing.push_str(&format!("    {name} : {ty}\n")),
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
