//! Embedding a `Shell` in a host process.
//!
//! A host (the interactive `ral` REPL, `exarch`, a test binary) needs the
//! same three things before it runs any turn: the prelude as a baked
//! [`Comp`], the prelude's top-level [`Scheme`] list, and a `Shell`
//! constructed-seeded-and-loaded from both.  This module names the
//! missing type, [`BakedPrelude`], and the one function that performs the
//! embedding, [`boot_shell`].  Everything host-specific — terminal
//! handling, output capture, watchdogs, capability frames, host builtins,
//! rc files — is interposed by the host before or after the call.
//!
//! The prelude is baked ahead of time: the host's build script calls
//! [`bake_prelude_to_out_dir`], which parses, elaborates, and
//! [`bake_prelude`](crate::bake_prelude)s `prelude.ral` into two postcard
//! blobs in `OUT_DIR`; the host then embeds them with the
//! [`baked_prelude!`](crate::baked_prelude) macro.  Core cannot bake its
//! own prelude at build time — a crate's build script cannot depend on
//! the crate it is building — so a test binary, which has no build-time
//! blob, takes [`BakedPrelude::bake_runtime`] instead.

use crate::io::TerminalState;
use crate::ir::Comp;
use crate::typecheck::Scheme;
use crate::types::Shell;
use std::sync::{Arc, OnceLock};

/// A build-time-baked prelude: the two postcard blobs and their
/// once-decoded forms.  `ir` is the annotated prelude [`Comp`] whose
/// `Bind` nodes carry the checker's schemes; `scheme_bytes` is the
/// top-level scheme list harvested in the same pass.  Both decode lazily
/// on first access and memoise.
pub struct BakedPrelude {
    ir: &'static [u8],
    scheme_bytes: &'static [u8],
    comp: OnceLock<Arc<Comp>>,
    schemes: OnceLock<Vec<(String, Scheme)>>,
}

impl BakedPrelude {
    /// Wrap the two blobs a host embedded via
    /// [`baked_prelude!`](crate::baked_prelude).  `const` so it can
    /// initialise a `static`.
    pub const fn from_blobs(ir: &'static [u8], schemes: &'static [u8]) -> Self {
        Self {
            ir,
            scheme_bytes: schemes,
            comp: OnceLock::new(),
            schemes: OnceLock::new(),
        }
    }

    /// The annotated prelude comp, decoded on first use.
    pub fn comp(&self) -> &Arc<Comp> {
        self.comp.get_or_init(|| {
            Arc::new(postcard::from_bytes(self.ir).expect("prelude IR deserialization failed"))
        })
    }

    /// The prelude's top-level schemes, decoded on first use.
    pub fn schemes(&self) -> &[(String, Scheme)] {
        self.schemes.get_or_init(|| {
            postcard::from_bytes(self.scheme_bytes).expect("prelude schemes deserialization failed")
        })
    }

    /// Bake the prelude at runtime from core's embedded source, for test
    /// binaries that have no build-time blob.  Parse, elaborate, and
    /// [`bake_prelude`](crate::bake_prelude) `prelude.ral`, then carry the
    /// results in `OnceLock`s already filled (the blobs are unused).
    pub fn bake_runtime() -> Self {
        let src = include_str!("prelude.ral");
        let ast = crate::parse(src).expect("prelude parse");
        let comp = crate::elaborate(&ast, Default::default());
        let (annotated, schemes) = crate::bake_prelude(&comp);
        let this = Self::from_blobs(&[], &[]);
        let _ = this.comp.set(Arc::new(annotated));
        let _ = this.schemes.set(schemes);
        this
    }
}

/// Expand in a host crate whose build script called
/// [`bake_prelude_to_out_dir`](crate::host::bake_prelude_to_out_dir).
/// The `include_bytes!` must expand in the host crate, against its own
/// `OUT_DIR`; pinning the filename contract here keeps it in the same
/// crate as the writer.
#[macro_export]
macro_rules! baked_prelude {
    () => {
        $crate::host::BakedPrelude::from_blobs(
            include_bytes!(concat!(env!("OUT_DIR"), "/prelude_baked.bin")),
            include_bytes!(concat!(env!("OUT_DIR"), "/prelude_schemes.bin")),
        )
    };
}

/// Construct, seed, and load the prelude into a fresh `Shell`.  Everything
/// a turn-evaluating host needs before its own builtins, rc files, or
/// capability frames; terminal handling, output capture, and watchdogs
/// remain host concerns interposed around this call.
pub fn boot_shell(terminal: TerminalState, prelude: &BakedPrelude) -> Shell {
    let mut shell = Shell::new(terminal);
    shell.seed_default_env_vars();
    crate::builtins::register(&mut shell, prelude.comp());
    crate::builtins::misc::register_prelude_type_hints(prelude.schemes());
    shell
}

/// The build-script half of the bake: parse, elaborate, and
/// [`bake_prelude`](crate::bake_prelude) the embedded prelude source,
/// write both postcard blobs to `OUT_DIR`, and emit rerun-if-changed
/// lines for every shape-defining file (absolute paths via
/// `CARGO_MANIFEST_DIR`, so the lines are host-independent).
///
/// postcard carries no schema, so a field added to
/// [`CompKind`](crate::ir::CompKind), [`Val`](crate::ir::Val),
/// [`Pattern`](crate::syntax::ast::Pattern), or the scheme's type
/// vocabulary silently invalidates a previously-emitted bake; the rerun
/// lines force a re-bake when any of those files change.
pub fn bake_prelude_to_out_dir() {
    let core = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for shape_file in [
        "src/prelude.ral",
        "src/ir.rs",
        "src/syntax/ast.rs",
        "src/mode.rs",
        "src/typecheck/ty.rs",
        "src/typecheck/scheme.rs",
    ] {
        println!("cargo:rerun-if-changed={}", core.join(shape_file).display());
    }

    let src = include_str!("prelude.ral");
    let ast = crate::parse(src).unwrap_or_else(|e| {
        eprintln!("build: prelude parse error: {e}");
        std::process::exit(1);
    });
    let comp = crate::elaborate(&ast, Default::default());

    let (annotated, schemes) = crate::bake_prelude(&comp);
    let ir_bytes = postcard::to_allocvec(&annotated).expect("prelude IR serialization failed");
    let scheme_bytes =
        postcard::to_allocvec(&schemes).expect("prelude schemes serialization failed");

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("prelude_baked.bin"), ir_bytes)
        .expect("failed to write prelude_baked.bin");
    std::fs::write(out.join("prelude_schemes.bin"), scheme_bytes)
        .expect("failed to write prelude_schemes.bin");
}
