//! Embedding and driving a `Shell` in a host process.
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
//! This is about *driving* a `Shell` in a host process; probing the
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

use crate::io::{Source, TerminalState};
use crate::ir::Comp;
use crate::process::CancelCause;
use crate::source::Span;
use crate::transport::{Program, Turn};
use crate::turn::{StaticDiagnostics, TurnLifecycle};
use crate::typecheck::Scheme;
use crate::types::{DeferredSink, Desk, Settled, Shell, SurfaceSink, Value};
use crate::types::{DefaultPolicy, Hook, HookName, HookSig, RegisterError, TerminalPolicy};
use serde::{Deserialize, Serialize};
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
        let comp = crate::elaborate(&ast, std::collections::HashSet::default());
        let (annotated, schemes) = crate::bake_prelude(&comp);
        let this = Self::from_blobs(&[], &[]);
        let _ = this.comp.set(Arc::new(annotated));
        let _ = this.schemes.set(schemes);
        this
    }
}

/// Expand in a host crate whose build script called
/// [`bake_prelude_to_out_dir`](crate::driver::bake_prelude_to_out_dir).
/// The `include_bytes!` must expand in the host crate, against its own
/// `OUT_DIR`; pinning the filename contract here keeps it in the same
/// crate as the writer.
#[macro_export]
macro_rules! baked_prelude {
    () => {
        $crate::driver::BakedPrelude::from_blobs(
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

// ── The turn entry: one synchronous, runtime-agnostic host seam ──────────────
//
// Hosts start an evaluation through exactly one door:
// `Shell::run_turn(TurnRequest)` runs one whole `Turn` — its `Program` is
// either source text or a registered hook applied to first-order arguments
// (`Shell::register_hook` stores compiled hooks by name in the session-lived
// hook table).  It returns one flat `TurnReport`.  Hosts describe *policy*
// (the protocol `Turn`, `TurnIo`, `SurfaceSink`, lifecycle hooks); core owns
// *resources* (`Sink`, `Source`, `TurnState`, guards, buffers, signal
// slots).  Completion is the call returning — never a channel disconnecting
// — so a deferred worker holding a surface clone cannot keep a turn from
// ending.  This door is the only way into evaluation: the reduction
// primitive behind it is crate-private, so a host cannot start an unframed
// evaluation that would foreground or capture against a stale frame.

/// The IO regime of a turn: intent, materialised into resources by the turn
/// doors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnIo {
    /// Run on the session's live streams: the turn's byte sinks are cloned
    /// from the ambient `shell.turn`. The interactive REPL (whose stdout is
    /// the external printer) and batch (the process streams).
    Inherit,
    /// Mint fresh stdout/stderr buffers core returns in
    /// [`TurnReport::Ran`]'s `captured`. Independent of [`TurnStdin`]: byte
    /// output regime and byte input source are separate choices. exarch's tool
    /// capture.
    Capture,
}

/// Whether a turn may hand the controlling terminal to a child.
///
/// The host-facing half of the terminal lease: the host states the turn's
/// authority, and core decides whether the session's
/// [`TerminalLease`](crate::process::TerminalLease) is reachable from it (see
/// [`Shell::terminal_lease`]). `ExplicitLoan` is deliberately absent — a host
/// cannot seed it; it is a within-turn elevation a loan token raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestedTerminalAccess {
    /// No child/job foreground handoff in this turn. exarch tool turns and any
    /// launch that does not own the terminal foreground.
    Denied,
    /// This turn may foreground terminal-bound children. The interactive REPL
    /// and a terminal-launched script.
    Leased,
}

/// The byte source a turn's stdin reads from.
///
/// Orthogonal to [`TurnIo`] (the *output* regime) and to
/// [`RequestedTerminalAccess`] (foreground authority): a piped `ral -c` is
/// `Denied` foreground yet still reads its inherited pipe (`Inherit`), while an
/// exarch tool turn is `Denied` *and* reads no terminal (`Empty`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnStdin {
    /// Use the session stdin source — the inherited fd 0, which may be a
    /// terminal, a pipe, or a redirected file.
    Inherit,
    /// Install an empty source: reads as immediate EOF, a child's stdin wires
    /// to `/dev/null`, and there is no fall-through to fd 0.
    Empty,
}

/// The engine door for one turn: the protocol [`Turn`] plus the live,
/// non-transportable handles the host lends it.  Composition, not
/// mirroring — a field added to [`Turn`] crosses the seam and reaches the
/// engine in one declaration.
pub struct TurnRequest<'a> {
    /// The turn, exactly as it crosses (or would cross) the host seam.
    pub turn: Turn,
    /// The turn-local structured-event sink, installed only for this turn.
    /// `None` is the identity (a bare REPL). Same-thread children inherit it;
    /// deferred workers buffer into bounded deferred storage instead.
    pub surface: Option<SurfaceSink>,
    /// The session-lived destination a deferred worker delivers its surface
    /// batch to when it settles, rendered by the host at the next turn
    /// boundary. `None` outside an agent host (a bare REPL): then a deferred
    /// worker's surface reaches a sink only via `await`/`race`.
    pub deferred: Option<Arc<dyn DeferredSink>>,
    /// The turn-local enquiry desk, installed only for this turn. `None` is
    /// the honest absence a host that answers no enquiries reports (a bare
    /// REPL, and exarch until the migration installs its desk). Same-thread
    /// children inherit it; deferred workers never receive it.
    pub desk: Option<Desk>,
    /// Per-turn lifecycle hooks; `Box::new(())` for a host with none.
    pub lifecycle: Box<dyn TurnLifecycle + 'a>,
}

/// The byte streams captured under [`TurnIo::Capture`], returned in
/// [`TurnReport::Ran`] and carried verbatim on the protocol
/// [`Report`](crate::transport::Report).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Resolve a turn's armed wall clock from every source that can bind it:
/// the host's requested `turn_limit` and — for a hook turn — its
/// registered budget. Both bind the same foreground scope, tightest wins
/// (`min`, not `or`), so a host wall can never be silently widened by a
/// hook's own budget or vice versa. `None` from both leaves the turn
/// unarmed, exactly as before.
fn arm_turn_wall(
    turn_limit: Option<std::time::Duration>,
    hook_budget: Option<std::time::Duration>,
    foreground: &crate::process::ForegroundScope,
) -> Option<crate::process::Deadline> {
    let effective = match (turn_limit, hook_budget) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    effective.map(|d| crate::process::arm_lifetime(foreground.as_scope().clone(), d))
}

impl Shell {
    /// Run one whole [`Turn`] under `req`, synchronously, and return one
    /// flat [`TurnReport`]. The single turn door: the turn's
    /// [`Program`] is resolved here — source text compiles and typechecks
    /// against the live session, a hook resolves in the session-lived hook
    /// table — and either program then runs through the shared framed
    /// scaffold ([`Self::run_built`]): materialising the IO regime, minting
    /// the turn's foreground scope and arming its wall, installing the
    /// turn-local surface, evaluating under the capability ceiling, and
    /// folding in the captured bytes and `timed_out`.
    ///
    /// Turn completion is *this call returning* — never a channel
    /// disconnecting. A deferred worker may hold a clone of the surface sink
    /// forever; it changes nothing, because nothing waits on that sink to
    /// decide the turn is over.
    pub fn run_turn(&mut self, mut req: TurnRequest<'_>) -> TurnReport {
        match req.turn.program {
            Program::Source(ref src) => {
                // The binding-lease ledger's committed-turn clock: one tick
                // per source dispatch, whether or not it goes on to compile —
                // a failed turn ages the ledger's scratch without renewing it
                // (`decisions/260629_agent-binding-reaping`). A no-op when
                // unarmed, so the REPL/batch pay one branch and nothing else.
                self.local.bindings.tick();

                // Mint the turn's foreground scope and arm its wall *before*
                // compiling, so the limit bounds the whole turn — compile and
                // typecheck included, not only evaluation. `compile_turn`'s
                // `process::clear` touches only the signal count, never the
                // reaper, so an entry armed here survives the compile. The
                // `Deadline` guard disarms when `wall` drops, so an early
                // `Static` return leaves no pending reaper entry.
                let foreground = self.durable_root().child();
                let wall = arm_turn_wall(req.turn.turn_limit, None, &foreground);

                let (comp, single_command) = match crate::turn::compile_turn(self, src) {
                    Ok(parts) => parts,
                    Err(diagnostics) => return TurnReport::Static { diagnostics },
                };

                // "Committed" = reached evaluation: harvest the compiled
                // program's referenced names and renew every one that is
                // already leased. Gated on `armed()` so an unarmed host
                // (REPL, batch) never pays for the walk.
                if self.local.bindings.armed() {
                    self.local
                        .bindings
                        .renew(crate::ir::referenced_names(&comp));
                }

                self.run_built(req, &foreground, wall, single_command, |s| {
                    crate::evaluator::eval_top_level(&comp, s)
                })
            }
            Program::Hook { ref name, ref args } => {
                let Some(hook) = self.mobile.context.hooks.get(name).cloned() else {
                    return TurnReport::Static {
                        diagnostics: StaticDiagnostics::Host(crate::types::Error::new(
                            format!("hook '{}' is not registered", name),
                            1,
                        )),
                    };
                };

                // The host conveys data, not closures, across the dispatch
                // boundary: hook args are first-order by type (`FOValue`).
                let args: Vec<Value> = args.iter().cloned().map(Value::from).collect();

                let foreground = self.durable_root().child();
                let wall = arm_turn_wall(req.turn.turn_limit, hook.policy.budget, &foreground);

                // Fold the hook's registered `DefaultPolicy` into the turn's
                // conditions: capture, terminal authority, and budget are the
                // hook's to decide, not the dispatching host's.
                if hook.policy.capture {
                    req.turn.io = TurnIo::Capture;
                }
                req.turn.terminal = match hook.policy.terminal {
                    TerminalPolicy::Denied => RequestedTerminalAccess::Denied,
                    TerminalPolicy::Leased => RequestedTerminalAccess::Leased,
                };

                self.run_built(req, &foreground, wall, false, |s| {
                    crate::builtins::apply(&hook.binding.value, &args, s)
                })
            }
        }
    }

    /// Register a host-held [`Value`] (a compiled `Block` or `Lambda`)
    /// as a named turn-entry point in the session-lived hook table.
    ///
    /// The hook is stored by `name`; it is never readable as `$name`
    /// and never invokable as a command — it fires only when the host
    /// dispatches a [`Program::Hook`] turn at a lifecycle moment (prompt
    /// render, startup, plugin hook, keybinding).
    ///
    /// Registration checks:
    /// 1. `value` is a `Block` or `Lambda`.
    /// 2. Its arity matches `sig` (0 for `Prompt`, 1 otherwise).
    /// 3. `Hook::validate` — a no-op seam for future mobility
    ///    checks.
    ///
    /// On success the hook is inserted into `context.hooks`,
    /// keyed by `name`.  On failure a [`RegisterError`] is returned;
    /// callers render it as a diagnostic at `origin`.
    pub fn register_hook(
        &mut self,
        name: HookName,
        value: Value,
        sig: HookSig,
        policy: DefaultPolicy,
        origin: Span,
    ) -> Result<(), RegisterError> {
        // 1. Must be Block or Lambda.
        let arity = match &value {
            Value::Block { .. } => 0,
            Value::Lambda { .. } => value.lambda_arity().unwrap_or(1),
            _ => {
                return Err(RegisterError::NotCallable {
                    name,
                    origin,
                    actual: value.type_name().to_string(),
                });
            }
        };

        // 2. Arity must match the signature.
        let expected = sig.expected_arity();
        if arity != expected {
            return Err(RegisterError::ArityMismatch {
                name,
                origin,
                expected,
                actual: arity,
                sig_label: sig.label().to_string(),
            });
        }

        // 3. Build the Binding with the same scheme inference an
        //    ordinary session `let` uses.
        let arm = match &value {
            Value::Lambda { param, body, .. } => Some((Some(param), body)),
            Value::Block { body, .. } => Some((None, body)),
            _ => unreachable!("already checked above"),
        };
        let scheme = arm.map(|(param, body)| {
            crate::typecheck::binding_value_scheme(param, body, self.session_schemes())
        });
        use crate::types::Binding;
        let binding = Binding { value, scheme };

        // 4. Check for duplicate registration.
        if self.mobile.context.hooks.contains_key(&name) {
            return Err(RegisterError::AlreadyRegistered { name, origin });
        }

        // 5. Validate arity and callability.
        let hook = Hook {
            binding,
            sig,
            policy,
            origin,
        };
        hook.validate(&name)?;

        self.mobile.context.hooks.insert(name, hook);
        Ok(())
    }

    /// Return true when a hook with the given `name` is registered.
    pub fn has_hook(&self, name: &HookName) -> bool {
        self.mobile.context.hooks.contains_key(name)
    }

    /// The framed scaffold behind the turn door: materialise the IO regime
    /// from the turn's conditions, build and install the turn frame on the
    /// pre-minted `foreground`, evaluate `body` under the capability ceiling
    /// and lifecycle hooks, then disarm the `wall` and fold the captured
    /// bytes and `timed_out` into the report. `body` is the turn's resolved
    /// program — the source arm's `eval_top_level`, the hook arm's in-frame
    /// `apply`.
    fn run_built(
        &mut self,
        req: TurnRequest<'_>,
        foreground: &crate::process::ForegroundScope,
        wall: Option<crate::process::Deadline>,
        single_command: bool,
        body: impl FnOnce(&mut Shell) -> Settled<Value>,
    ) -> TurnReport {
        let TurnRequest {
            turn,
            surface,
            deferred,
            desk,
            lifecycle,
        } = req;

        // The source text the lifecycle hooks and root context see: the
        // program itself for a source turn, empty for a hook turn (whose
        // program is an already-compiled value, not text).
        let src = match &turn.program {
            Program::Source(src) => src.as_str(),
            Program::Hook { .. } => "",
        };

        // Materialise the IO regime: `Capture` mints buffers we read back,
        // `Inherit` leaves the ambient streams to flow through `build_turn`.
        let (capture, capture_bufs) = match turn.io {
            TurnIo::Inherit => (None, None),
            TurnIo::Capture => {
                let (stdout_sink, stdout_buf) = crate::io::new_buffer();
                let (stderr_sink, stderr_buf) = crate::io::new_buffer();
                (
                    Some((stdout_sink, stderr_sink)),
                    Some((stdout_buf, stderr_buf)),
                )
            }
        };

        // Stdin source and terminal authority are independent of the output
        // regime: `Capture` no longer implies `Source::Terminal`. A tool turn
        // is `Denied` + `Empty`; a piped `ral -c` is `Denied` + `Inherit`.
        let stdin = match turn.stdin {
            TurnStdin::Inherit => Source::Terminal,
            TurnStdin::Empty => Source::Empty,
        };
        let terminal_access = match turn.terminal {
            RequestedTerminalAccess::Leased => crate::types::TerminalAccess::Leased,
            RequestedTerminalAccess::Denied => crate::types::TerminalAccess::Denied,
        };

        let next = crate::turn::build_turn(
            self,
            capture,
            stdin,
            terminal_access,
            foreground.clone(),
            turn.deferred_lease,
            turn.worker_cap,
            surface,
            deferred,
            desk,
        );
        let (result, status) = crate::turn::run_framed(
            self,
            next,
            &turn.script_name,
            src,
            turn.caps.clone(),
            lifecycle,
            body,
        );

        // Disarm the wall before reading the cause. While it stays armed the
        // reaper can still fire; classifying against a live ceiling lets a turn
        // that finished inside its budget be misread as timed out should the
        // reaper trip in the gap between eval returning and this read. Dropping
        // the guard removes the entry, so `cause` is `Deadline` below only for a
        // deadline that genuinely elapsed during the turn.
        drop(wall);

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
