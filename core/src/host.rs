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

use crate::io::{Source, TerminalState};
use crate::ir::Comp;
use crate::process::CancelCause;
use crate::turn::{StaticDiagnostics, TurnLifecycle};
use crate::typecheck::Scheme;
use crate::types::{Capabilities, Settled, Shell, SurfaceSink, Value};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

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

// ── The turn entry: one synchronous, runtime-agnostic host seam ────────────
//
// `Shell::run_turn(src, TurnRequest) -> TurnReport` is the only host-facing
// evaluation seam. Hosts describe *policy* (`TurnRequest`, `TurnIo`,
// `SurfaceSink`, lifecycle hooks); core owns *resources* (`Sink`, `Source`,
// `TurnState`, guards, buffers, signal slots). Completion is the call
// returning — never a channel disconnecting — so a detached worker holding a
// surface clone cannot keep a turn from ending.

/// The IO regime of a turn: intent, materialised into resources by `run_turn`.
pub enum TurnIo {
    /// Run on the session's live streams: the turn's byte sinks are cloned
    /// from the ambient `shell.turn`. The interactive REPL (whose stdout is
    /// the external printer) and batch (the process streams).
    Inherit,
    /// Mint fresh stdout/stderr buffers core returns in
    /// [`TurnReport::Ran`]'s `captured`; stdin falls through to the terminal
    /// the shell already holds. exarch's tool capture.
    Capture,
}

/// The host's per-turn policy. Everything a turn needs that is *not* a core
/// resource: the script label, the capability ceiling, the wall and detached
/// limits, the IO regime, the turn-local surface sink, and lifecycle hooks.
pub struct TurnRequest<'a> {
    /// Label for the root source context (`"<stdin>"` for the REPL,
    /// `"<tool>"` for exarch).
    pub script_name: &'a str,
    /// The capability ceiling pushed for the eval's dynamic extent.
    /// `Capabilities::root()` is the ⊤ element — the identity on authority
    /// (the REPL) — while a narrower profile attenuates the session's grant
    /// (exarch).
    pub caps: Capabilities,
    /// The turn's foreground wall: `Some(d)` arms a `Deadline` cancel on the
    /// turn's foreground scope `d` after it starts (exarch's per-tool wall);
    /// `None` leaves the turn uncapped (the REPL).
    pub turn_limit: Option<Duration>,
    /// Lifetime ceiling for workers the turn detaches at the durable root.
    /// `None` (the interactive ral host) leaves a worker until `cancel`, root
    /// abort, or session exit; `Some(d)` (an agent host) reaps an abandoned
    /// worker `d` after it is spawned.
    pub detached_limit: Option<Duration>,
    /// The byte IO regime; see [`TurnIo`].
    pub io: TurnIo,
    /// The turn-local structured-event sink, installed only for this turn.
    /// `None` is the identity (a bare REPL). Same-thread children inherit it;
    /// detached workers buffer into bounded deferred storage instead.
    pub surface: Option<SurfaceSink>,
    /// Per-turn lifecycle hooks; `Box::new(())` for a host with none.
    pub lifecycle: Box<dyn TurnLifecycle + 'a>,
}

/// The byte streams captured under [`TurnIo::Capture`], returned in
/// [`TurnReport::Ran`].
pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// One flat result the host matches once. `captured`/`timed_out` live on
/// `Ran`, where they mean something — a `Static` turn never ran.
pub enum TurnReport {
    /// A parse/type failure: the turn never reached evaluation. The host
    /// renders the diagnostics and treats the turn as status 1.
    Static { diagnostics: StaticDiagnostics },
    /// A compiled turn ran. `status` is the transport status computed once;
    /// `single_command` is whether the source compiled to a single command
    /// (for runtime-error rendering); `captured` is `Some` under
    /// [`TurnIo::Capture`]; `timed_out` is whether the wall fired.
    Ran {
        result: Settled<Value>,
        status: i32,
        single_command: bool,
        captured: Option<Captured>,
        timed_out: bool,
    },
}

impl Shell {
    /// Run one top-level turn of `src` under `req`, synchronously, and return
    /// one flat [`TurnReport`]. The single host evaluation seam: it compiles
    /// and typechecks against the live session, materialises the IO regime,
    /// mints the turn's foreground scope and arms its wall, installs the
    /// turn-local surface, evaluates under the capability ceiling, and folds
    /// the captured bytes and `timed_out` it alone knows into the report.
    ///
    /// Turn completion is *this call returning* — never a channel
    /// disconnecting. A detached worker may hold a clone of the surface sink
    /// forever; it changes nothing, because nothing waits on that sink to
    /// decide the turn is over.
    pub fn run_turn(&mut self, src: &str, req: TurnRequest<'_>) -> TurnReport {
        let (comp, single_command) = match crate::turn::compile_turn(self, src) {
            Ok(parts) => parts,
            Err(diagnostics) => return TurnReport::Static { diagnostics },
        };

        // Materialise the IO regime: `Capture` mints buffers we read back,
        // `Inherit` leaves the ambient streams to flow through `build_turn`.
        let (capture, capture_bufs) = match req.io {
            TurnIo::Inherit => (None, None),
            TurnIo::Capture => {
                let (stdout_sink, stdout_buf) = crate::io::new_buffer();
                let (stderr_sink, stderr_buf) = crate::io::new_buffer();
                (
                    Some((stdout_sink, stderr_sink, Source::Terminal)),
                    Some((stdout_buf, stderr_buf)),
                )
            }
        };

        // Mint the turn's foreground scope and arm its wall, if any. The
        // `Deadline` guard disarms when `_wall` drops at end of turn, so an
        // early-finishing turn does not leave a pending reaper entry.
        let foreground = self.durable_root().child();
        let _wall = req
            .turn_limit
            .map(|d| crate::process::arm_lifetime(foreground.as_scope().clone(), d));

        let next = crate::turn::build_turn(
            self,
            capture,
            foreground.clone(),
            req.detached_limit,
            req.surface,
        );
        let (result, status) = crate::turn::run_compiled(
            self,
            comp,
            next,
            req.script_name,
            src,
            req.caps,
            req.lifecycle,
        );

        let timed_out = foreground.cause() == Some(CancelCause::Deadline);
        let captured = capture_bufs.map(|(out, err)| Captured {
            stdout: crate::io::take_buffer(&out),
            stderr: crate::io::take_buffer(&err),
        });

        TurnReport::Ran {
            result,
            status,
            single_command,
            captured,
            timed_out,
        }
    }
}
