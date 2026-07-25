//! Embedding and booting a `Shell` in a host process.
//!
//! A host (the interactive `ral` REPL, `exarch`, a test binary) needs the
//! same four things before it evaluates any run: the prelude as a baked
//! [`Comp`], the prelude's top-level [`Scheme`] list, its [`HostSurface`],
//! and a `Shell` constructed-seeded-and-loaded from all three.  This
//! module names the missing type, [`BakedPrelude`], the surface type,
//! [`HostSurface`], and the one function that performs the embedding,
//! [`boot_shell`], which installs the surface before any rc file or user
//! code is checked — so the typechecker's builtin table and the runtime's
//! agree by construction.  Everything else host-specific — terminal
//! handling, output capture, watchdogs, capability frames, rc files — is
//! interposed by the host before or after the call.  Evaluating anything
//! on the booted shell goes through the run door, [`crate::run`].
//!
//! This is about *booting* a `Shell` in a host process; probing the
//! underlying host *machine* (OS, architecture, cwd, git state,
//! wall-clock) lives in [`crate::host`].
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
use crate::types::{BuiltinEntry, BuiltinTable, Shell};
use std::sync::{Arc, OnceLock};

/// A build-time-baked prelude: the two postcard blobs and their
/// once-decoded forms.
///
/// `ir` is the annotated prelude [`Comp`] whose
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
    ///
    /// # Panics
    /// Panics if the embedded prelude IR blob fails to deserialize.
    pub fn comp(&self) -> &Arc<Comp> {
        self.comp.get_or_init(|| {
            Arc::new(postcard::from_bytes(self.ir).expect("prelude IR deserialization failed"))
        })
    }

    /// The prelude's top-level schemes, decoded on first use.
    ///
    /// # Panics
    /// Panics if the embedded prelude scheme blob fails to deserialize.
    pub fn schemes(&self) -> &[(String, Scheme)] {
        self.schemes.get_or_init(|| {
            postcard::from_bytes(self.scheme_bytes).expect("prelude schemes deserialization failed")
        })
    }

    /// Bake the prelude at runtime from core's embedded source, for test
    /// binaries that have no build-time blob.  Parse, elaborate, and
    /// [`bake_prelude`](crate::bake_prelude) `prelude.ral`, then carry the
    /// results in `OnceLock`s already filled (the blobs are unused).
    ///
    /// # Panics
    /// Panics if the embedded `prelude.ral` fails to parse.
    pub fn bake_runtime() -> Self {
        let src = include_str!("prelude.ral");
        let ast = crate::parse(src).expect("prelude parse");
        let comp = crate::elaborate(&ast, std::collections::HashSet::default());
        let (annotated, schemes) = crate::bake_prelude(&comp);
        let this = Self::from_blobs(&[], &[]);
        let _ = this.comp.set(Arc::new(annotated));
        let _ = this.schemes.set(schemes);
        this
    }
}

/// Expand in a host crate whose build script called
/// [`bake_prelude_to_out_dir`](crate::boot::bake_prelude_to_out_dir).
///
/// The `include_bytes!` must expand in the host crate, against its own
/// `OUT_DIR`; pinning the filename contract here keeps it in the same
/// crate as the writer.
#[macro_export]
macro_rules! baked_prelude {
    () => {
        $crate::boot::BakedPrelude::from_blobs(
            include_bytes!(concat!(env!("OUT_DIR"), "/prelude_baked.bin")),
            include_bytes!(concat!(env!("OUT_DIR"), "/prelude_schemes.bin")),
        )
    };
}

/// A host's builtin surface beyond [`CORE_BUILTINS`](crate::builtins::CORE_BUILTINS).
///
/// The one definition [`boot_shell`] installs and shell-free typechecking
/// ([`Self::builtin_table`]) reads, so the checker surface and the runtime
/// surface cannot drift.  An empty surface (`Default`) is a bare core shell.
#[derive(Default)]
pub struct HostSurface {
    /// Process-static builtin sets.
    pub statics: Vec<&'static [BuiltinEntry]>,
    /// Runtime-owned builtin sets capturing host state.
    pub captured: Vec<Arc<[BuiltinEntry]>>,
}

impl HostSurface {
    /// The builtin table this surface presents:
    /// [`CORE_BUILTINS`](crate::builtins::CORE_BUILTINS)
    /// (via [`core_builtin_table`](crate::builtins::core_builtin_table))
    /// plus every set here — exactly what a shell booted with this
    /// surface dispatches.  Seeds
    /// [`SessionSchemes::from_schemes`](crate::typecheck::SessionSchemes)
    /// for a checker with no live shell (`--check`).
    ///
    /// # Panics
    /// Panics if a name here collides with an installed builtin.
    pub fn builtin_table(&self) -> BuiltinTable {
        let mut table = crate::builtins::core_builtin_table();
        self.install_into(&mut table);
        table
    }

    pub(crate) fn install_into(&self, table: &mut BuiltinTable) {
        for &set in &self.statics {
            table.install_static(set);
        }
        for set in &self.captured {
            table.install_arc(Arc::clone(set));
        }
    }
}

/// Construct a fresh `Shell`, install the host's builtin surface, seed the
/// env, and load the prelude.
///
/// The shell leaves here fully dressed: `surface` lands next to
/// `CORE_BUILTINS`, before any rc file or user code is checked, so the
/// typechecker (which reads this shell's builtin table) and the runtime
/// agree on the surface by construction.  Everything else host-specific —
/// terminal handling, output capture, watchdogs, capability frames, rc
/// files — remains interposed around this call.
///
/// # Panics
/// Panics if a name in `surface` collides with a core builtin or repeats
/// within the surface.
pub fn boot_shell(terminal: TerminalState, prelude: &BakedPrelude, surface: &HostSurface) -> Shell {
    let mut shell = Shell::new(terminal);
    surface.install_into(&mut shell.session.builtins);
    shell.seed_default_env_vars();
    crate::builtins::register(&mut shell, prelude.comp());
    shell
}

/// The build-script half of the bake: parse, elaborate, and
/// [`bake_prelude`](crate::bake_prelude) the embedded prelude source,
///
/// write both postcard blobs to `OUT_DIR`, and emit rerun-if-changed
/// lines for every shape-defining file (absolute paths via
/// `CARGO_MANIFEST_DIR`, so the lines are host-independent).
///
/// postcard carries no schema, so a field added to
/// [`CompKind`](crate::ir::CompKind), [`Val`](crate::ir::Val),
/// [`Pattern`](crate::syntax::ast::Pattern), or the scheme's type
/// vocabulary silently invalidates a previously-emitted bake; the rerun
/// lines force a re-bake when any of those files change.
///
/// # Panics
/// Panics if `OUT_DIR` is unset, if serialising the prelude blobs fails, or
/// if writing them into `OUT_DIR` fails.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:prelude-bake] build-script prelude bake: writes the postcard IR/scheme blobs to OUT_DIR during host setup; build-time artifact emission, not turn-time model data I/O, raises no surface card."
)]
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
    let comp = crate::elaborate(&ast, std::collections::HashSet::default());

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
