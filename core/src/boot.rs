//! Booting a `Shell` in a host process — the interactive `ral` REPL,
//! `exarch`, a test binary.
//!
//! Probing the host *machine* (OS, cwd, git state) is [`crate::host`];
//! evaluating on the booted shell is [`crate::run`].
//!
//! The prelude arrives baked: a host's build script calls
//! [`bake_prelude_to_out_dir`], and the host embeds the blobs it wrote with
//! [`baked_prelude!`](crate::baked_prelude).  Core cannot bake its own — a
//! build script cannot depend on the crate it is building — so a test
//! binary takes [`BakedPrelude::bake_runtime`] instead.

use crate::io::TerminalState;
use crate::ir::{CompKind, Phrase, Toplevel};
use crate::typecheck::Scheme;
use crate::types::{BuiltinEntry, BuiltinTable, Shell};
use std::sync::{Arc, OnceLock};

/// A prelude baked ahead of time: the annotated [`Toplevel`] and the
/// top-level [`Scheme`] list, harvested in one checked pass, as postcard
/// blobs that decode and memoise on first access.
pub struct BakedPrelude {
    ir: &'static [u8],
    scheme_bytes: &'static [u8],
    comp: OnceLock<Arc<Toplevel>>,
    schemes: OnceLock<Vec<(String, Scheme)>>,
}

impl BakedPrelude {
    /// Wrap the blobs a host embedded via
    /// [`baked_prelude!`](crate::baked_prelude).  `const` so a host can
    /// hold its prelude in a `static`.
    pub const fn from_blobs(ir: &'static [u8], schemes: &'static [u8]) -> Self {
        Self {
            ir,
            scheme_bytes: schemes,
            comp: OnceLock::new(),
            schemes: OnceLock::new(),
        }
    }

    /// The annotated prelude toplevel.
    ///
    /// # Panics
    /// Panics if the embedded IR blob fails to deserialize.
    pub fn comp(&self) -> &Arc<Toplevel> {
        self.comp.get_or_init(|| {
            Arc::new(postcard::from_bytes(self.ir).expect("prelude IR deserialization failed"))
        })
    }

    /// The prelude's top-level schemes.
    ///
    /// # Panics
    /// Panics if the embedded scheme blob fails to deserialize.
    pub fn schemes(&self) -> &[(String, Scheme)] {
        self.schemes.get_or_init(|| {
            postcard::from_bytes(self.scheme_bytes).expect("prelude schemes deserialization failed")
        })
    }

    /// Bake the prelude from core's embedded source, for a test binary with
    /// no build-time blob: both `OnceLock`s come back filled, so the empty
    /// blobs are never read.
    ///
    /// # Panics
    /// Panics if the embedded `prelude.ral` fails to parse, elaborate, or
    /// type-check.
    pub fn bake_runtime() -> Self {
        let src = include_str!("prelude.ral");
        let ast = crate::parse(src).expect("prelude parse");
        let top = crate::elaborate(&ast, std::collections::HashSet::default(), "")
            .expect("prelude elaborate");
        let (annotated, schemes) = crate::bake_prelude(&top);
        validate_prelude_shape(&annotated);
        let this = Self::from_blobs(&[], &[]);
        let _ = this.comp.set(Arc::new(annotated));
        let _ = this.schemes.set(schemes);
        this
    }
}

/// Reject a prelude phrase that is not a `Define` of `Return(V)` — a literal
/// or a thunk — naming the bound name(s) (§6.2).
///
/// The wire ships the prelude tier by name alone, never by value: a
/// pipeline-stage helper re-derives it by running the same prelude source
/// under its own process.  A `Define` whose right-hand side is anything but
/// `Return` — an `if`, a call, a store read — could close over *this*
/// process's facts (its stdout, its args, its clock) and so differ from the
/// helper's own bake, silently.  `Return(V)` cannot: closing a literal or a
/// thunk reads nothing about the process it runs in.
///
/// # Panics
/// If any phrase fails the shape check.
fn validate_prelude_shape(top: &Toplevel) {
    for phrase in &top.phrases {
        let Phrase::Define { comp, schemes, .. } = &phrase.item else {
            panic!(
                "prelude phrase must be a `Define`, found {}",
                describe_phrase(&phrase.item)
            );
        };
        if !matches!(comp.item, CompKind::Return(_)) {
            let names: Vec<&str> = schemes.iter().map(|(n, _)| n.as_str()).collect();
            panic!(
                "prelude binding `{}` is computed at boot ({}), so it could differ between \
                 processes; bind a value or a thunk",
                names.join(", "),
                describe_comp(&comp.item),
            );
        }
    }
}

/// A phrase's shape, named for [`validate_prelude_shape`]'s message.
fn describe_phrase(phrase: &Phrase) -> &'static str {
    match phrase {
        Phrase::Define { .. } => "a `Define`",
        Phrase::Source { .. } => "a `source`",
        Phrase::Run(_) => "a bare statement",
    }
}

/// A computation's shape, named for [`validate_prelude_shape`]'s message.
fn describe_comp(kind: &CompKind) -> &'static str {
    match kind {
        CompKind::If { .. } => "`if …`",
        CompKind::Case { .. } => "`case …`",
        CompKind::Observe(_) => "a store read",
        CompKind::App { .. } => "a call",
        CompKind::Exec(_) => "a command",
        CompKind::Bind { .. } => "`to`",
        CompKind::Force(_) => "`force`",
        _ => "a computation",
    }
}

/// Expand in a host crate whose build script called
/// [`bake_prelude_to_out_dir`](crate::boot::bake_prelude_to_out_dir).
///
/// A macro, because `include_bytes!` has to expand against the *host's*
/// `OUT_DIR`; keeping it here keeps the filenames beside the writer.
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
/// One value serves both [`boot_shell`] and shell-free typechecking
/// ([`Self::builtin_table`]), so the checker's surface and the runtime's
/// cannot drift.  `Default` is a bare core shell.
#[derive(Default)]
pub struct HostSurface {
    pub statics: Vec<&'static [BuiltinEntry]>,
    /// Sets a host built at run time, closing over its own state.
    pub captured: Vec<Arc<[BuiltinEntry]>>,
}

impl HostSurface {
    /// Core plus every set here — exactly what a shell booted with this
    /// surface dispatches.  Seeds
    /// [`SessionSchemes::from_schemes`](crate::typecheck::SessionSchemes)
    /// where there is no live shell to read a table off (`--check`).
    ///
    /// # Panics
    /// Panics if a name here collides with a core builtin.
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

    /// [`Self::install_into`]'s live-shell counterpart: goes through
    /// [`Shell::install_builtins`]/[`Shell::install_captured_builtins`], which
    /// also seed the base env scope and base handler frames.
    pub(crate) fn install_into_shell(&self, shell: &mut Shell) {
        for &set in &self.statics {
            shell.install_builtins(set);
        }
        for set in &self.captured {
            shell.install_captured_builtins(set);
        }
    }
}

/// Construct a fresh `Shell`, install the host's builtin surface, seed the
/// env, and load the prelude.
///
/// `surface` lands before any rc file or user code is checked, so the
/// typechecker — which reads this shell's builtin table — and the runtime
/// agree by construction.  Terminal handling, output capture, watchdogs,
/// capability frames and rc files stay the host's, interposed around this
/// call.
///
/// # Panics
/// Panics if a name in `surface` collides with a core builtin or repeats
/// within the surface.
pub fn boot_shell(terminal: TerminalState, prelude: &BakedPrelude, surface: &HostSurface) -> Shell {
    let mut shell = Shell::new(terminal);
    surface.install_into_shell(&mut shell);
    shell.seed_default_env_vars();
    crate::builtins::register(&mut shell, prelude.comp());
    shell
}

/// The build-script half of the bake: write the two postcard blobs into
/// the calling host's `OUT_DIR`.
///
/// Name every file that shapes them in a rerun-if-changed line —
/// absolutely, since the script runs in the host.
///
/// postcard carries no schema, so a field added to
/// [`CompKind`](crate::ir::CompKind), [`Val`](crate::ir::Val),
/// [`Pattern`](crate::syntax::ast::Pattern), or the scheme's type
/// vocabulary would silently invalidate an old bake.  Those rerun lines are
/// the only thing that forces a fresh one.
///
/// # Panics
/// Panics if the prelude fails to type-check, if `OUT_DIR` is unset, or if
/// serialising or writing the blobs fails.  A parse or elaboration failure
/// exits instead, reporting the error to the build log.
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
    let top =
        crate::elaborate(&ast, std::collections::HashSet::default(), "").unwrap_or_else(|e| {
            eprintln!("build: prelude elaborate error: {e}");
            std::process::exit(1);
        });

    let (annotated, schemes) = crate::bake_prelude(&top);
    validate_prelude_shape(&annotated);
    let ir_bytes = postcard::to_allocvec(&annotated).expect("prelude IR serialization failed");
    let scheme_bytes =
        postcard::to_allocvec(&schemes).expect("prelude schemes serialization failed");

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("prelude_baked.bin"), ir_bytes)
        .expect("failed to write prelude_baked.bin");
    std::fs::write(out.join("prelude_schemes.bin"), scheme_bytes)
        .expect("failed to write prelude_schemes.bin");
}

#[cfg(test)]
mod tests {
    use super::validate_prelude_shape;

    /// A prelude phrase whose RHS is computed at boot — an `if`, here — fails
    /// the bake, naming the bound name.
    #[test]
    #[should_panic(expected = "prelude binding `x` is computed at boot")]
    fn non_return_prelude_binding_fails_the_bake() {
        let ast = crate::parse("let x = if !{_ansi-ok} { return 1 } else { return 2 }")
            .expect("parse");
        let top = crate::elaborate(&ast, std::collections::HashSet::default(), "")
            .expect("elaborate");
        let (annotated, _schemes) = crate::bake_prelude(&top);
        validate_prelude_shape(&annotated);
    }

    /// A prelude phrase whose RHS is `Return(V)` — a literal or a thunk —
    /// passes the bake.
    #[test]
    fn return_prelude_binding_passes_the_bake() {
        let ast = crate::parse("let x = 1\nlet f = { |y| return $y }").expect("parse");
        let top = crate::elaborate(&ast, std::collections::HashSet::default(), "")
            .expect("elaborate");
        let (annotated, _schemes) = crate::bake_prelude(&top);
        validate_prelude_shape(&annotated);
    }
}
