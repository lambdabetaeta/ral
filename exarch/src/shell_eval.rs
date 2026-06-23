//! In-process ral evaluation against a persistent `Shell`.
//!
//! Runs each tool call as a top-level turn under a pushed capabilities
//! frame with stdout/stderr captured.  The capabilities come from the user's grant
//! policy; there is no source-level `grant { … }` wrapper around the
//! model's body — the boundary is enforced by `eval_top_level` plus the
//! pushed frame, not by surface syntax the model could evade.
//!
//! Each tool call's stdout and stderr are captured into in-memory
//! buffers, replayed to the model in conversation history and written to
//! the session log in full.  Nothing streams live to the user; the rail
//! surfaces tool summaries, patches, writes, and tasks instead.

use crate::agent_registry::AgentRegistry;
use crate::bus::{Emitter, InboxMsg, Kind, Mailbox, SessionId};
use crate::card::{done_card, io_card, value_to_card, value_to_done, value_to_io, value_to_pin};
use ral_core::types::{Boundary, BoundarySink, Break, Escape};
use ral_core::{
    EventSink, RequestedTerminalAccess, Shell, StaticDiagnostics, TurnIo, TurnReport, TurnRequest,
    TurnStdin, Value as RalValue, diagnostic,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Lifetime ceiling armed on every detached `spawn` worker: an
/// abandoned worker is reaped one hour after it is spawned, well past
/// the 30 s foreground wall but bounded so a long-running agent cannot
/// accumulate immortal zombies.
const DETACHED_WORKER_CEILING: Duration = Duration::from_secs(60 * 60);

/// The prelude baked into this binary at build time by `build.rs`.
pub static PRELUDE: ral_core::driver::BakedPrelude = ral_core::baked_prelude!();

/// A successful tool run, broken into named pieces so the caller can
/// render twice (full / capped) without parsing the rendered form.
pub struct ToolResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub value: Option<String>,
    pub exit: i32,
}

/// What `run_shell` produces.  `Static` is for parse / type errors —
/// already-formatted ariadne text with no further structure to cap.
pub enum Outcome {
    Ran(ToolResult),
    Static(String),
}

/// The agent's structured-event surface: the turn-local [`EventSink`] exarch
/// installs for a *foreground* tool call.  It decodes each `Value` the
/// `surface` builtin hands it ([`decode_surface`]) and emits the resulting
/// [`Kind`] on the presentation bus through a clone of the call's [`Emitter`],
/// live, now.  A value that decodes to nothing is dropped, the same graceful
/// degradation as [`value_to_card`].  Detached workers never receive this
/// sink: core buffers their `surface` calls and either replays them on
/// `await`/`race` or flushes them to the [`InboxBoundary`] at completion, so a
/// clone of the bus `Emitter` can never outlive the tool turn.
///
/// [`Card`]: crate::card::Card
/// A shared, session-owned register of current pinned-state digests
/// (`key → one-line summary`), written by the live surface sink as
/// `` `pin ``/`` `unpin `` flow by and read by the nudge facility to describe
/// what the model has pinned.  The session clones a handle into each turn's
/// [`AgentSink`]; `None` (tests, any path with no nudge layer) disables the
/// mirror.  The session is otherwise pin-blind — pins flow past it to the
/// frontend — so this small mirror is how the boundary nudge can name them.
pub type PinDigests = Arc<Mutex<std::collections::BTreeMap<String, String>>>;

struct AgentSink {
    emit: Emitter,
    pins: Option<PinDigests>,
}

impl EventSink for AgentSink {
    fn emit(&self, ev: &RalValue) {
        if let Some(kind) = decode_surface(ev) {
            // Mirror pinned state into the session register so the nudge layer
            // can describe it; the bus event remains the rendering path.
            if let Some(pins) = &self.pins
                && let Ok(mut m) = pins.lock()
            {
                match &kind {
                    Kind::Pin { key, card } => {
                        m.insert(key.clone(), crate::card::summary_line(card));
                    }
                    Kind::Unpin { key } => {
                        m.remove(key);
                    }
                    _ => {}
                }
            }
            self.emit.emit(kind);
        }
    }
}

/// Decode one surfaced `Value` into the [`Kind`] it renders as — the single
/// decoder both delivery regimes share.  The live foreground sink
/// ([`AgentSink`]) calls it to emit now; the deferred boundary's `commit_turn`
/// arm calls the *same* function to mint the identical events at the turn
/// boundary.  Three shapes arrive on the one `surface` channel:
///
///   * a structural I/O event core emits (a read, write, exec, or grep) — a
///     `Map` tagged by its `io` field, decoded into a typed [`IoEvent`] and
///     paired with the [`Card`] composed from it ([`Kind::Io`]);
///   * a render document a ral kit composed (a `` `card `` variant of Bertin
///     marks), decoded into a [`Card`] ([`Kind::Card`]); and
///   * the `` `done `` completion event a detached worker flushes at the end of
///     its deferred batch, composed into its one-line outcome [`Card`].
///
/// A fourth shape rides the same channel as a render *disposition*, tried
/// first: a `` `pin ``/`` `unpin `` wrapper carrying *state* rather than an
/// event ([`value_to_pin`]) — a render document keyed to a register slot,
/// overwritten in place on re-pin ([`Kind::Pin`]/[`Kind::Unpin`]) rather than
/// appended to scrollback.
///
/// They cannot collide — io is a `Map`; a card, a `done`, and a pin are
/// distinct `Variant` labels — and the order (pin, then io, then card, then
/// done) is the tried-first sequence, so the raw effect record always reaches
/// the bus beside its rendering.  A value that is none of these returns `None`
/// and is dropped.
///
/// [`Card`]: crate::card::Card
/// [`IoEvent`]: crate::card::IoEvent
pub fn decode_surface(ev: &RalValue) -> Option<Kind> {
    if let Some((key, body)) = value_to_pin(ev) {
        Some(match body {
            Some(card) => Kind::Pin { key, card },
            None => Kind::Unpin { key },
        })
    } else if let Some(event) = value_to_io(ev) {
        let card = io_card(&event);
        Some(Kind::Io { event, card })
    } else if let Some(card) = value_to_card(ev) {
        Some(Kind::Card(card))
    } else if let Some((cmd, outcome)) = value_to_done(ev) {
        Some(Kind::Card(done_card(&cmd, &outcome)))
    } else {
        None
    }
}

/// The deferred half of `surface`: the session-lived [`BoundarySink`] a
/// detached `spawn` worker flushes its buffered batch to at completion.  It is
/// surface's *deferred destination* — not a new channel — so it carries the
/// same ordinary surface vocabulary the live sink does, posted through the
/// session's own [`Mailbox`] as an [`InboxMsg::Surface`] for the host to render
/// at the next turn boundary (the [`Card`]/`Io` events the live path mints now,
/// minted later) — the spawn worker flushes its deferred surface batch into the
/// agent that ran the spawn.  The id it stamps is the **root** session's, so a
/// spawn worker's cards land in the root viewport — a spawn worker registers no
/// tab of its own.
///
/// The generation guard mirrors the async `agent`'s exactly: it captures the
/// [`AgentRegistry`]'s generation at construction and, in [`Self::deliver`],
/// re-reads the live counter.  A `/clear` between spawn and flush bumps that
/// counter (`AgentRegistry::clear`), so a stale batch is dropped before it can
/// reach a rebuilt context — the session epoch the ADR calls for, reusing the
/// one counter rather than minting a parallel one.  The inbox's `clear` empties
/// the deque, so a batch already queued when `/clear` runs is dropped for free.
///
/// [`Card`]: crate::card::Card
struct InboxBoundary {
    /// The session's own inbox sender; a spawn worker flushes its deferred
    /// surface batch into the agent that ran the spawn.
    mailbox: Mailbox,
    /// The root session id, stamped on every batch so a spawn worker's cards
    /// render in the root viewport.
    root: SessionId,
    registry: AgentRegistry,
    /// The registry generation captured at construction; a batch flushed after
    /// a `/clear` advanced it is dropped.
    generation: u64,
}

impl BoundarySink for InboxBoundary {
    fn deliver(&self, batch: Vec<RalValue>, joined: Arc<Mutex<bool>>) {
        // A `/clear` since this worker was spawned bumped the registry
        // generation; its batch belongs to a context that no longer exists, so
        // drop it rather than post it into the rebuilt session — the deferred
        // twin of the async agent's stale-result rejection.
        if self.registry.generation() != self.generation {
            return;
        }
        self.mailbox.push(InboxMsg::Surface {
            id: self.root,
            values: batch,
            joined,
        });
    }
}

/// Build the deferred [`Boundary`] a tool turn installs: an [`InboxBoundary`]
/// over `emit`'s session inbox, stamping batches with `root` and guarding them
/// with `registry`'s current generation.  Cloned into the worker's turn state
/// by core, so a nested `spawn` inherits it and flushes at its own completion.
pub fn boundary_sink(emit: &Emitter, root: SessionId, registry: &AgentRegistry) -> Boundary {
    Arc::new(InboxBoundary {
        mailbox: emit.mailbox(),
        root,
        registry: registry.clone(),
        generation: registry.generation(),
    })
}

/// Evaluate `cmd` against `shell`, wrapped in `caps`, capturing
/// stdout and stderr into buffers.  Returns the result as named pieces
/// so the caller can render it twice — once full for the terminal,
/// once with per-section caps for the conversation history — without
/// having to parse the rendered form back apart.
pub fn run_shell(
    shell: &mut Shell,
    caps: &ral_core::types::Capabilities,
    cmd: &str,
    timeout_secs: u64,
    emit: &Emitter,
    boundary: Option<Boundary>,
    pins: Option<PinDigests>,
) -> Outcome {
    let name = "<tool>";

    emit.emit(Kind::Phase("evaluating".into()));

    // One synchronous turn: core captures stdout/stderr into buffers it
    // returns, arms the per-tool wall (`turn_limit`), installs the agent
    // surface for this turn only, and reaps detached workers at the 1 h
    // ceiling.  Completion is this call returning — a detached server worker
    // holds a bounded deferred surface, never a clone of the bus `Emitter`.
    //
    // Trace-only timing: the kernel-denial window now lives in core
    // (`sandbox::diag`), so this instant feeds the debug trace alone and
    // is gated to that build — release otherwise sees an unused binding.
    #[cfg(debug_assertions)]
    let tool_start = std::time::Instant::now();
    let report = shell.run_source_turn(
        cmd,
        TurnRequest {
            script_name: name,
            caps: caps.clone(),
            turn_limit: Some(Duration::from_secs(timeout_secs)),
            detached_limit: Some(DETACHED_WORKER_CEILING),
            io: TurnIo::Capture,
            terminal: RequestedTerminalAccess::Denied,
            stdin: TurnStdin::Empty,
            surface: Some(Arc::new(AgentSink {
                emit: emit.clone(),
                pins,
            })),
            boundary,
            lifecycle: Box::new(()),
        },
    );

    let (result, single_command, captured, timed_out) = match report {
        TurnReport::Static { diagnostics } => {
            return match diagnostics {
                StaticDiagnostics::Parse(e) => {
                    Outcome::Static(diagnostic::format_parse_error_ariadne(name, cmd, &e))
                }
                StaticDiagnostics::Types(errs) => Outcome::Static(
                    errs.iter()
                        .map(|e| diagnostic::format_type_error_ariadne(name, cmd, e))
                        .collect(),
                ),
            };
        }
        TurnReport::Ran {
            result,
            single_command,
            captured,
            timed_out,
            ..
        } => (result, single_command, captured, timed_out),
    };

    ral_core::dbg_trace!(
        "shell",
        "eval in {:?} (timed_out={timed_out})",
        tool_start.elapsed()
    );

    let captured = captured.expect("TurnIo::Capture returns captured buffers");
    let stdout_bytes = captured.stdout;
    let mut stderr_bytes = captured.stderr;

    let (exit, value) = match &result {
        Ok(v) => (0, Some(v.clone())),
        Err(Break::Escape(Escape::Exit(code))) => ((*code).clamp(0, 255), None),
        Err(Break::Error(e)) => {
            // If our watchdog fired, the unwind came from the timeout
            // cancel-scope; the generic "cancelled" message would hide
            // that.  Synthesize a clearer error with the conventional
            // timeout(1) exit code so the model can distinguish "I ran
            // too long" from "I was Ctrl-C'd".
            let e = if timed_out {
                ral_core::types::Error::new(
                    format!(
                        "ral tool: timed out after {timeout_secs}s — work this long must not run inline. \
                         Spawn it (`let h = spawn {{ … }}`) and let the turn return: the host notifies you \
                         at the next turn boundary when it settles and renders its output on the rail. \
                         `await $h` when you want its value record — you need not poll."
                    ),
                    124,
                )
            } else {
                e.clone()
            };
            let rendered =
                diagnostic::format_runtime_error_auto(shell.sources(), &e, single_command);
            stderr_bytes.extend_from_slice(rendered.as_bytes());
            let is_cmd_exit = matches!(
                &e.status,
                ral_core::types::Status::Process(ral_core::process::CommandFailure::ExitCode(_))
            );
            if is_cmd_exit {
                let mut tip = String::from(
                    "\nrecovery: this non-zero exit raised. If the exit code is the tool's own \
                     signal rather than a failure (grep no-match=1, diff differs=1, test false=1, \
                     valgrind --error-exitcode=N), its stdout/stderr were captured — read them as \
                     data with `audit { … }`, which does not raise, or catch with \
                     `try { … } { |err| … }`. For a yes/no check use `succeeds { … }`.",
                );
                if !single_command {
                    tip.push_str(
                        " A non-zero exit also aborts the rest of this command and discards earlier \
                         bindings; wrap risky tools in `audit`/`try`, or split them out.",
                    );
                }
                tip.push('\n');
                stderr_bytes.extend_from_slice(tip.as_bytes());
            }
            (e.exit_code().clamp(0, 255), None)
        }
        #[cfg(unix)]
        Err(Break::Escape(Escape::Stopped { .. })) => (1, None),
    };

    let value_str = match value {
        // A unit value has no `VALUE` section.  Everything else decodes to
        // JSON with byte fields read as lossy UTF-8 rather than the integer
        // arrays `to-json` round-trips — a job's or audit node's captured
        // `stderr` is text the model must read, not data — and then renders
        // through the shared `json_to_text` rule.  That decode walk is total,
        // so a value carrying a thunk or a non-finite float still renders
        // instead of collapsing to nothing.
        Some(v) if !matches!(v, RalValue::Unit) => {
            json_to_text(&ral_core::builtins::value_to_json_lossy_bytes(&v))
        }
        _ => None,
    };

    Outcome::Ran(ToolResult {
        stdout: stdout_bytes,
        stderr: stderr_bytes,
        value: value_str,
        exit,
    })
}

/// Render a JSON value as the text a tool result carries.
///
/// A JSON **string** passes through raw, so a markdown report keeps real
/// newlines rather than the escaped `\n` a serializer would emit; any other
/// shape is **pretty-printed** so its structure stays legible; a JSON **null**
/// renders to nothing (`None`).
///
/// Shared by two callers with the same rendering need: the `ral` value section
/// — which decodes its [`RalValue`] to JSON with `value_to_json_lossy_bytes`
/// first, so byte fields read as text — and the `reply` tool, whose argument
/// arrives as JSON already.  Keeping one rule means a sub-agent's markdown
/// reply and a `view-text` value clip and elide identically downstream.
pub(crate) fn json_to_text(json: &serde_json::Value) -> Option<String> {
    match json {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => serde_json::to_string_pretty(other).ok(),
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    //! Documented-semantics tests for exarch's tool-call evaluator.
    //!
    //! These hold a single [`Shell`] across two `run_shell` calls — the
    //! exact harness shape exarch uses between consecutive tool calls in
    //! a session — and verify the three top-level properties that
    //! motivate routing through `evaluator::eval_top_level`:
    //!
    //!   1. `let` bindings persist across calls.
    //!   2. Effects before a failing line still persist; effects after
    //!      the failing line do not appear (the line never ran).
    //!   3. `cd` persists across calls.
    //!
    //! Equivalent end-to-end coverage of the top-level contract itself
    //! lives in `core/tests/top_level_vs_block.rs`; the exarch-tier
    //! tests below pin that `run_shell`'s wrapping (capabilities frame,
    //! stdout/stderr tees, location bookkeeping) does not perturb the
    //! mobile-install contract.

    use super::*;
    use crate::agent_builtins;
    use crate::bus::{Emitter, Inbox, Row};
    use ral_core::types::Capabilities;
    use std::sync::mpsc;

    /// Render a path without a trailing platform separator.  Some hosts
    /// return `"/tmp/"` from `std::env::temp_dir()`; `Shell::cwd()` never
    /// carries a trailing separator, so trimming here makes the
    /// comparison portable while preserving the `/var` ↔ `/private/var`
    /// firmlink fallback used below.  The `len > 1` guard keeps "/"
    /// itself intact.
    fn display_no_trailing_sep(path: &std::path::Path) -> String {
        let s = path.display().to_string();
        if s.len() > 1 {
            s.trim_end_matches(std::path::MAIN_SEPARATOR).to_string()
        } else {
            s
        }
    }

    /// Build a `Shell` that mirrors `bootstrap::boot_shell` without
    /// signal-handler installation (which is global, racey under
    /// `cargo test`, and not under test here).
    fn fresh_shell() -> Shell {
        let mut shell = ral_core::driver::boot_shell(Default::default(), &PRELUDE);
        agent_builtins::install_on(&mut shell);
        agent_builtins::install_agent_library(&mut shell).expect("embedded agent library");
        crate::bootstrap::seed_no_color(&mut shell);
        shell
    }

    /// Make an `Emitter` whose receiver is held alive locally so sends
    /// don't fail.  Tests never assert on what was emitted — they only
    /// need `run_shell` not to error on the send path.
    fn dummy_emitter() -> (Emitter, mpsc::Receiver<crate::bus::Event>) {
        let (tx, rx) = mpsc::channel();
        (Emitter::new(tx, 0), rx)
    }

    /// Drive one tool call through the real `run_shell` entry point.
    /// `Capabilities::root()` matches exarch's least-restricted default,
    /// which lets every test source compile and run without exercising
    /// the OS sandbox (that path is covered separately in
    /// `core/tests/top_level_vs_block.rs`'s sandbox-parity tests).
    fn run_once(shell: &mut Shell, cmd: &str) -> ToolResult {
        let (emit, _rx) = dummy_emitter();
        match run_shell(shell, &Capabilities::root(), cmd, 30, &emit, None, None) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static (parse/type) failure: {s}"),
        }
    }

    /// Capability frame with an `fs` projection over the whole tree while
    /// admitting ordinary Unix tools.  This mirrors a restrictive exarch
    /// base: the body evaluates locally, and any external command it spawns
    /// is confined per-command under the OS sandbox.
    #[cfg(unix)]
    fn projecting_caps() -> Capabilities {
        Capabilities {
            fs: Some(ral_core::types::FsPolicy {
                read_prefixes: vec!["/".into()],
                write_prefixes: vec!["/".into()],
                deny_paths: Vec::new(),
            }),
            ..Capabilities::root()
        }
    }

    /// `view-text` is the one read primitive: a sourced prelude pipeline over
    /// the `line-hash` builtin plus coreutils.  `echo "alpha" | view-text 1 2`
    /// must number the line and tag it with its content hash, which means
    /// `line-hash` dispatches as a builtin rather than falling through to
    /// an external-command exec lookup ("command 'line-hash' not found on
    /// PATH").  Each row is `<n>\t<hash>\t<text>`.
    #[cfg(unix)]
    #[test]
    fn view_tags_lines_with_hash() {
        let mut shell = fresh_shell();
        let r = run_once(&mut shell, "echo \"alpha\" | view-text 1 2");
        assert_eq!(
            r.exit,
            0,
            "view-text must run; stderr was: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let out = String::from_utf8_lossy(&r.stdout);
        let row = out.lines().next().unwrap_or_default();
        let parts: Vec<&str> = row.splitn(3, '\t').collect();
        assert_eq!(
            parts.len(),
            3,
            "expected `<n>\\t<hash>\\t<text>`, got {row:?}"
        );
        assert_eq!(parts[0].trim(), "1", "first row is line 1");
        let hash = parts[1];
        assert_eq!(
            hash.len(),
            7,
            "hash is an `h` tag plus six hex, got {hash:?}"
        );
        assert!(
            hash.starts_with('h') && hash[1..].bytes().all(|b| b.is_ascii_hexdigit()),
            "hash is `h` followed by six hex chars, got {hash:?}"
        );
        assert_eq!(parts[2], "alpha");
    }

    /// Two-call sequence: `let persist_n = 41` then `$[$persist_n + 1]`.
    /// The second call must see the binding from the first — locks in
    /// that exarch's per-call `eval_top_level` install survives across
    /// the tool-call boundary.
    #[test]
    fn tool_call_let_persists_across_calls() {
        let mut shell = fresh_shell();
        let _ = run_once(&mut shell, "let persist_n = 41");
        let second = run_once(&mut shell, "return $[$persist_n + 1]");
        assert_eq!(
            second.exit,
            0,
            "second tool call must succeed; stderr was: {}",
            String::from_utf8_lossy(&second.stderr)
        );
        // Tool results render scalars as raw strings (see the
        // `RalValue::String` branch in `run_shell`'s `value_str`);
        // structured non-string scalars route through JSON pretty.  An
        // Int returns its JSON form.
        assert_eq!(
            second.value.as_deref(),
            Some("42"),
            "expected the second call's returned value to be 42"
        );
    }

    /// Single tool call: `let pre_x = 1; cat /nonexistent; let post_y = 2`.
    /// The middle command fails, so the line fails; a follow-up call
    /// must see `pre_x` defined and `post_y` undefined.  Locks in the
    /// install-on-Error rule across the tool-call boundary.
    #[test]
    fn tool_call_partial_effects_persist_on_error() {
        let mut shell = fresh_shell();
        let failing = run_once(
            &mut shell,
            "let pre_x = 1\ncat /nonexistent\nlet post_y = 2",
        );
        assert_ne!(
            failing.exit, 0,
            "the failing tool call must surface a non-zero exit"
        );

        // Use a second tool call to check what survived: read both
        // bindings via `try` so the test stays green regardless of
        // which one was defined.  An undefined name elaborates to a
        // type error (caught as Static), which is the wrong shape
        // here; instead we materialise the pre-call env into the
        // second call's bindings via the live shell and inspect
        // `mobile.scope` directly.  Same observation, lower noise.
        assert!(
            shell.scope_lookup("pre_x").is_some(),
            "pre-failure `let` must persist into the next tool call"
        );
        assert!(
            shell.scope_lookup("post_y").is_none(),
            "post-failure `let` never ran, must not be present"
        );
    }

    /// `cd <tmp>` in one tool call must be observable to `cwd` in the
    /// next.  Locks in `logical_cwd` riding the mobile across the
    /// tool-call boundary.
    #[test]
    fn tool_call_cd_persists_across_calls() {
        let mut shell = fresh_shell();
        let tmp = std::env::temp_dir();
        let tmp_disp = display_no_trailing_sep(&tmp);
        let cd = run_once(&mut shell, &format!("cd '{tmp_disp}'"));
        assert_eq!(
            cd.exit,
            0,
            "cd should succeed; stderr was: {}",
            String::from_utf8_lossy(&cd.stderr)
        );
        let pwd = run_once(&mut shell, "cwd");
        assert_eq!(pwd.exit, 0, "cwd in the second call should succeed");
        let canon = display_no_trailing_sep(&tmp.canonicalize().unwrap_or(tmp.clone()));
        let got = pwd
            .value
            .as_deref()
            .expect("cwd must return a String tool value");
        assert!(
            got == tmp_disp || got == canon,
            "expected the second call's cwd to be {tmp_disp:?} or {canon:?}, got {got:?}"
        );
    }

    /// Exarch sources agent helpers at boot and registers the Rust
    /// atoms they depend on as host builtins.  `view-text` / `view-text-around` are
    /// source-loaded ral names (in `mobile.scope`); `window-hash` /
    /// `grep-files` / `edit` are now host builtins (reachable by name via the
    /// builtin registry, not bound in lexical scope).  This test also locks in
    /// `run_shell` passing live bindings into elaboration.
    #[test]
    fn agent_helpers_are_loaded_into_tool_shell() {
        let mut shell = fresh_shell();
        assert!(shell.scope_lookup("view-text").is_some());
        assert!(shell.scope_lookup("view-text-around").is_some());
        for builtin in ["window-hash", "grep-files", "edit"] {
            assert!(
                shell.lookup_value_name(builtin).is_some(),
                "{builtin} must resolve as a host builtin"
            );
            assert!(
                shell.scope_lookup(builtin).is_none(),
                "{builtin} is a builtin, not a lexical binding"
            );
        }
        let result = run_once(
            &mut shell,
            r#"
let dir = temp-dir
to-string "alpha
beta
alpha beta
" > "$dir/test.txt"
cd $dir
let hits = grep-files 'alpha'
edit $hits[0][file] [[$hits[0][hash], 'ALPHA']]
let after = !{from-string < $hits[0][file]}
return [count: !{length $hits}, after: $after]
"#,
        );
        assert_eq!(
            result.exit,
            0,
            "agent helper command failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let value: serde_json::Value =
            serde_json::from_str(result.value.as_deref().expect("structured result"))
                .expect("json tool value");
        assert_eq!(value["count"], 2);
        assert_eq!(value["after"], "ALPHA\nbeta\nalpha beta\n");
    }

    /// The witness folds in ±3 lines of context, so two lines with the
    /// same text but different surroundings get distinct witnesses and are
    /// each addressable — what a bare line hash could not do. A genuine
    /// collision (a line whose whole neighbourhood repeats, deep in an
    /// identical run) is still ambiguous and rejected, file untouched.
    #[test]
    fn edit_window_hash_addresses_repeated_lines() {
        let mut shell = fresh_shell();
        let tmp = std::env::temp_dir().join(format!("exarch-window-edit-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);

        // Two `target` lines, different context: distinguishable.
        let repeated = tmp.join("repeated.txt");
        let original = "\
section one:
target
    delete me

section two:
target
    keep me
";
        std::fs::write(&repeated, original).expect("write repeated fixture");
        let repeated_str = display_no_trailing_sep(&repeated);
        // `target` is line 2 = index 1; its window-hash differs from line 6's.
        let edited = run_once(
            &mut shell,
            &format!(
                "let rows = _rows !{{from-string < '{repeated_str}'}}\n\
                 let wh = window-hash $rows 1\n\
                 edit '{repeated_str}' [[$wh, 'FIRST']]"
            ),
        );
        assert_eq!(
            edited.exit,
            0,
            "editing the first `target` by its window-hash must succeed; stderr was: {}",
            String::from_utf8_lossy(&edited.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(&repeated).expect("read repeated fixture"),
            "\
section one:
FIRST
    delete me

section two:
target
    keep me
",
            "only the first `target` changes; the second is untouched"
        );

        // A line buried in a run of identical lines: its whole window
        // repeats, so the witness is ambiguous and the edit is rejected.
        let run = tmp.join("run.txt");
        let run_original = "head\ndup\ndup\ndup\ndup\ndup\ndup\ndup\ndup\ntail\n";
        std::fs::write(&run, run_original).expect("write run fixture");
        let run_str = display_no_trailing_sep(&run);
        let ambiguous = run_once(
            &mut shell,
            &format!(
                "let rows = _rows !{{from-string < '{run_str}'}}\n\
                 let wh = window-hash $rows 5\n\
                 edit '{run_str}' [[$wh, 'Z']]"
            ),
        );
        let _ = std::fs::remove_dir_all(&tmp);
        assert_ne!(
            ambiguous.exit, 0,
            "a line whose entire neighbourhood repeats must be ambiguous"
        );
        assert_eq!(
            std::fs::read_to_string(&run).unwrap_or_else(|_| run_original.into()),
            run_original,
            "the rejected edit must leave the file untouched"
        );
    }

    /// The batch resolves every hash against one read of the file before
    /// it writes, so a batch is atomic and its edits never interfere —
    /// even adjacent lines, which a per-call edit could not touch together
    /// without one invalidating the next. Pins both halves: a batch with
    /// any stale hash writes nothing, and a clean batch of adjacent edits
    /// (replace, delete, and a one-to-many expansion) applies in one pass.
    #[test]
    fn edit_batch_is_atomic_and_non_interfering() {
        let mut shell = fresh_shell();
        let tmp = std::env::temp_dir().join(format!("exarch-batch-edit-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let path = tmp.join("batch.txt");
        let original = "\
keep-top
replace-me
delete-me
expand-me
keep-bottom
";
        std::fs::write(&path, original).expect("write batch fixture");
        let path_str = display_no_trailing_sep(&path);

        // One stale hash poisons the whole batch: nothing is written.
        // `hzzzzzz` is not `h` + six hex, so it can match no window-hash.
        let poisoned = run_once(
            &mut shell,
            &format!(
                "let rows = _rows !{{from-string < '{path_str}'}}\n\
                 let h1 = window-hash $rows 1\n\
                 edit '{path_str}' [[$h1, 'X'], ['hzzzzzz', 'Y']]"
            ),
        );
        assert_ne!(poisoned.exit, 0, "a batch with a stale hash must fail");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read after poisoned batch"),
            original,
            "a failed batch must leave the file untouched"
        );

        // A clean batch over adjacent lines: replace, delete, and expand
        // one line into two — all witnessed from the one read.
        let ok = run_once(
            &mut shell,
            &format!(
                "let rows = _rows !{{from-string < '{path_str}'}}\n\
                 let h1 = window-hash $rows 1\n\
                 let h2 = window-hash $rows 2\n\
                 let h3 = window-hash $rows 3\n\
                 edit '{path_str}' [[$h1, 'REPLACED'], [$h2, ''], [$h3, 'X\nY']]"
            ),
        );
        let after = std::fs::read_to_string(&path).expect("read after clean batch");
        let _ = std::fs::remove_dir_all(&tmp);

        assert_eq!(
            ok.exit,
            0,
            "a clean adjacent batch must succeed; stderr was: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
        assert_eq!(
            after,
            "\
keep-top
REPLACED
X
Y
keep-bottom
"
        );
    }

    /// `edit` is the canonical witnessed rewrite entry point. Its body
    /// in `agent.ral` hands a `` `card `` carrying one `diff` mark to the
    /// `surface` builtin after the write; `value_to_card` decodes it into a
    /// `Kind::Card` on the bus. Pins two contracts: the edit reaches the
    /// rail end-to-end across the sandbox boundary, and `del` carries the
    /// literal removed rows so the rendered diff stays human-readable.
    #[test]
    fn edit_emits_kind_card() {
        use std::io::Write;
        let mut shell = fresh_shell();
        let (tx, rx) = mpsc::channel();
        let emit = Emitter::new(tx, 0);

        // Write a 3-line file into a fresh temp dir, then cd into it
        // so `grep-files` (which walks from `.`) sees only this file.
        let tmp = std::env::temp_dir().join(format!("exarch-surface-patch-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let path = tmp.join("hello.txt");
        let mut f = std::fs::File::create(&path).expect("create");
        writeln!(f, "alpha\nunique target line\nomega").expect("write");
        drop(f);
        let tmp_str = display_no_trailing_sep(&tmp);

        let src = format!(
            r#"cd '{tmp_str}'
let hits = grep-files 'unique target'
edit $hits[0][file] [[$hits[0][hash], 'REPLACED']]"#
        );
        let r = match run_shell(&mut shell, &Capabilities::root(), &src, 30, &emit, None, None) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static failure: {s}"),
        };
        assert_eq!(
            r.exit,
            0,
            "edit must exit zero; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );

        let mut found = None;
        while let Ok(ev) = rx.try_recv() {
            if let crate::bus::Kind::Card(card) = ev.kind {
                found = Some(card);
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
        let card = found.expect("a Kind::Card must reach the bus after edit");
        let (_got_path, hunks) = card
            .single_diff()
            .expect("edit surfaces a card carrying one diff mark");
        let hunk = &hunks[0];
        // The whole-file diff groups the lone change with its ±2 context, so
        // the hunk begins at the file's first line and carries the unified
        // row list: context, the deletion (the literal removed line, not the
        // hash), the insertion, then trailing context.
        assert_eq!(
            hunk.start, 1,
            "the hunk begins at line 1 with leading context"
        );
        assert!(
            matches!(
                hunk.rows.as_slice(),
                [Row::Context(a), Row::Del(d), Row::Add(b), Row::Context(o)]
                    if a == "alpha"
                        && d == "unique target line"
                        && b == "REPLACED"
                        && o == "omega"
            ),
            "rows must be context/del/add/context with literal text, got {:?}",
            hunk.rows
        );
    }

    /// Regression: a witness hash that happens to read as a number must
    /// still round-trip from `view-text` into `edit`. The agent copies the hash
    /// out of a `view-text` result and types it as a *bare* `edit` argument, so
    /// an all-digit hash like `152347` lexes as an `Int` while the hash
    /// `edit` recomputes is a `String`; the witness check then rejects a
    /// correct hash and the agent loops forever re-issuing the same edit.
    /// The leading-zero case (`012345`) is the sharper one: its integer
    /// reading drops the zero, so no string coercion inside `edit` could
    /// recover the original — only an un-numeric witness format can.
    #[cfg(unix)]
    #[test]
    fn edit_accepts_numeric_witness_hash() {
        // The witness `view-text` shows is the `window-hash`, not the bare line
        // digest, so mirror that computation here to search for an all-digit
        // one. For a file written as "{content}\n" the line list is
        // [content, ""]; line 1 (index 0) saturates the window to the whole
        // file, so its witness is line-hash("0:" ++ lh(content) ++ lh("")).
        fn lh(s: &str) -> String {
            format!(
                "h{}",
                &blake3::hash(s.trim_end().as_bytes()).to_hex().as_str()[..6]
            )
        }
        fn view_witness_line1(content: &str) -> String {
            lh(&format!("0:{}{}", lh(content), lh("")))
        }
        fn line_with_digit_digest(leading_zero: bool) -> String {
            for n in 0u64..2_000_000 {
                let s = format!("witness fixture line {n}");
                let tag = &view_witness_line1(&s)[1..7];
                if tag.bytes().all(|b| b.is_ascii_digit()) && tag.starts_with('0') == leading_zero {
                    return s;
                }
            }
            panic!(
                "no all-digit six-hex window-hash in search space (leading_zero={leading_zero})"
            );
        }

        fn assert_round_trips(label: &str, content: &str) {
            let mut shell = fresh_shell();
            let tmp = std::env::temp_dir().join(format!(
                "exarch-numeric-witness-{}-{label}",
                std::process::id()
            ));
            let _ = std::fs::create_dir_all(&tmp);
            let path = tmp.join("fixture.txt");
            std::fs::write(&path, format!("{content}\n")).expect("write witness fixture");
            let path_str = display_no_trailing_sep(&path);

            // Read the witness exactly as the agent would: from `view-text`.
            let vr = run_once(&mut shell, &format!("view-text 1 2 < '{path_str}'"));
            assert_eq!(
                vr.exit,
                0,
                "view-text must read the fixture; stderr was: {}",
                String::from_utf8_lossy(&vr.stderr)
            );
            let stdout = String::from_utf8_lossy(&vr.stdout);
            let row = stdout.lines().next().unwrap_or_default();
            let witness = row
                .split('\t')
                .nth(1)
                .expect("view-text row tags a hash")
                .trim()
                .to_string();

            // Feed the hash straight back as a *bare* token, the way the
            // agent copies it out of the read.
            let er = run_once(
                &mut shell,
                &format!("edit '{path_str}' [[{witness}, 'REPLACED']]"),
            );
            let after = std::fs::read_to_string(&path).unwrap_or_default();
            let _ = std::fs::remove_dir_all(&tmp);

            assert_eq!(
                er.exit,
                0,
                "witnessed edit with numeric hash {witness:?} must succeed; stderr was: {}",
                String::from_utf8_lossy(&er.stderr)
            );
            assert_eq!(after.trim_end(), "REPLACED");
        }

        assert_round_trips("plain", &line_with_digit_digest(false));
        assert_round_trips("leading-zero", &line_with_digit_digest(true));
    }

    /// `AgentSink` routes each surfaced value to the right bus event: a
    /// structural io value (a `Map` tagged by its `io` field) becomes a
    /// `Kind::Io` carrying both the typed event and the card composed from
    /// it; an ordinary `` `card `` value becomes a `Kind::Card`.  The two
    /// shapes never collide — io is a `Map`, a card a `Variant` — and the
    /// io path is tried first.
    #[test]
    fn agent_sink_routes_io_and_card_distinctly() {
        use crate::card::IoEvent;
        let (emit, rx) = dummy_emitter();
        let sink = AgentSink {
            emit,
            pins: None,
        };

        // An io value routes to Kind::Io, carrying the decoded event and a
        // card rendered from it.
        sink.emit(&RalValue::map(vec![
            ("io".into(), RalValue::String("read".into())),
            ("path".into(), RalValue::String("a.rs".into())),
        ]));
        match rx.try_recv().expect("an io value emits an event").kind {
            Kind::Io { event, card } => {
                assert_eq!(
                    event,
                    IoEvent::Read {
                        path: "a.rs".into()
                    }
                );
                assert!(!card.marks().is_empty(), "the io card is composed");
            }
            _ => panic!("an io value must route to Kind::Io"),
        }

        // An ordinary card value routes to Kind::Card.
        sink.emit(&RalValue::Variant {
            label: "card".into(),
            payload: Some(Box::new(RalValue::list(vec![]))),
        });
        assert!(
            matches!(
                rx.try_recv().expect("a card value emits an event").kind,
                Kind::Card(_)
            ),
            "a card value must route to Kind::Card"
        );

        // A value that is neither is dropped.
        sink.emit(&RalValue::String("nope".into()));
        assert!(
            rx.try_recv().is_err(),
            "a non-io, non-card value emits nothing"
        );
    }

    /// The shared decoder both regimes use round-trips each surface class to
    /// its `Kind` — an io `Map` to `Kind::Io`, a `` `card `` variant to
    /// `Kind::Card`, the `` `done `` completion event to a `Kind::Card`, and a
    /// `` `pin ``/`` `unpin `` disposition to `Kind::Pin`/`Kind::Unpin` — and
    /// drops a junk value to `None`.  The live `AgentSink` emits these now; the
    /// boundary's `commit_turn` arm mints the identical ones at the turn
    /// boundary.
    #[test]
    fn decode_surface_round_trips_each_class() {
        // An io map → Kind::Io.
        assert!(matches!(
            decode_surface(&RalValue::map(vec![
                ("io".into(), RalValue::String("read".into())),
                ("path".into(), RalValue::String("a.rs".into())),
            ])),
            Some(Kind::Io { .. })
        ));
        // A `card` variant → Kind::Card.
        assert!(matches!(
            decode_surface(&RalValue::Variant {
                label: "card".into(),
                payload: Some(Box::new(RalValue::list(vec![]))),
            }),
            Some(Kind::Card(_))
        ));
        // A `done` event → Kind::Card (the done card).
        assert!(matches!(
            decode_surface(&RalValue::Variant {
                label: "done".into(),
                payload: Some(Box::new(RalValue::map(vec![
                    ("cmd".into(), RalValue::String("<block>".into())),
                    (
                        "outcome".into(),
                        RalValue::Variant {
                            label: "ok".into(),
                            payload: Some(Box::new(RalValue::Unit)),
                        },
                    ),
                ]))),
            }),
            Some(Kind::Card(_))
        ));
        // A `pin` wrapper with a non-empty body → Kind::Pin, decoded by value_to_card.
        assert!(matches!(
            decode_surface(&RalValue::Variant {
                label: "pin".into(),
                payload: Some(Box::new(RalValue::map(vec![
                    ("key".into(), RalValue::String("tasks".into())),
                    (
                        "body".into(),
                        RalValue::Variant {
                            label: "card".into(),
                            payload: Some(Box::new(RalValue::list(vec![RalValue::String(
                                "x".into(),
                            )]))),
                        },
                    ),
                ]))),
            }),
            Some(Kind::Pin { .. })
        ));
        // `unpin`, and a `pin` whose body is absent, both → Kind::Unpin.
        for label in ["unpin", "pin"] {
            assert!(matches!(
                decode_surface(&RalValue::Variant {
                    label: label.into(),
                    payload: Some(Box::new(RalValue::map(vec![(
                        "key".into(),
                        RalValue::String("tasks".into()),
                    )]))),
                }),
                Some(Kind::Unpin { .. })
            ));
        }
        // A `pin` whose body is an *empty* card also drops the slot — a pin
        // with nothing to show is the same as `unpin`.
        assert!(matches!(
            decode_surface(&RalValue::Variant {
                label: "pin".into(),
                payload: Some(Box::new(RalValue::map(vec![
                    ("key".into(), RalValue::String("tasks".into())),
                    (
                        "body".into(),
                        RalValue::Variant {
                            label: "card".into(),
                            payload: Some(Box::new(RalValue::list(vec![]))),
                        },
                    ),
                ]))),
            }),
            Some(Kind::Unpin { .. })
        ));
        // A value that is none of these → None.
        assert!(decode_surface(&RalValue::String("nope".into())).is_none());
    }

    /// The `InboxBoundary` posts a detached worker's batch as an
    /// `InboxMsg::Surface` stamped with the root id — *unless* a `/clear` has
    /// advanced the registry generation since the boundary was built, in which
    /// case the batch belongs to a context that no longer exists and is dropped
    /// (the deferred twin of the async agent's stale-result rejection).
    #[test]
    fn inbox_boundary_pushes_then_drops_after_clear() {
        let registry = AgentRegistry::new();
        let inbox = Inbox::new();
        let (tx, _rx) = mpsc::channel();
        let emit = Emitter::with_mailbox(tx, 7, inbox.mailbox());
        let boundary = boundary_sink(&emit, 7, &registry);
        let joined = Arc::new(Mutex::new(false));

        // A fresh batch reaches the inbox, stamped with the root id (7).
        boundary.deliver(vec![RalValue::Unit], joined.clone());
        match inbox.drain_turn() {
            Some(crate::bus::Turn::Surface { id, .. }) => {
                assert_eq!(id, 7, "the batch is stamped with the root session id")
            }
            other => panic!("a delivered batch surfaces as Turn::Surface, got {other:?}"),
        }

        // A `/clear` bumps the registry generation; the boundary captured the
        // old one, so a later flush is dropped rather than posted.
        registry.clear();
        boundary.deliver(vec![RalValue::Unit], Arc::new(Mutex::new(false)));
        assert!(
            inbox.is_empty(),
            "a batch flushed after /clear advanced the generation is dropped"
        );
    }

    /// Colour suppression reaches spawned commands through the
    /// environment: a child shell must see `NO_COLOR=1` and
    /// `CLICOLOR_FORCE=0` (see `bootstrap::seed_no_color`).
    #[cfg(unix)]
    #[test]
    fn spawned_commands_inherit_color_suppression() {
        let mut shell = fresh_shell();
        let r = run_once(
            &mut shell,
            "/bin/sh -c 'printf %s \"$NO_COLOR/$CLICOLOR_FORCE\"'",
        );
        assert_eq!(r.exit, 0);
        assert_eq!(String::from_utf8_lossy(&r.stdout), "1/0");
    }

    /// A `timeout` is a hard wall-clock bound, not advisory: a command
    /// that sleeps far longer than its timeout must return in ≈ the
    /// timeout with exit 124, and the *whole* spawned process tree must
    /// be dead afterward — including a grandchild the direct child
    /// forked off.
    ///
    /// The fixture is the `python runtests.py` shape that exposed the
    /// bug: `/bin/sh` forks a `sleep` grandchild that holds the stdout
    /// pipe open, then blocks in `wait`.  Before the fix the standalone
    /// external spawned with `PgidPolicy::Inherit`, so the watchdog's
    /// cancel could only `child.kill()` the `/bin/sh` leader by pid; the
    /// orphaned `sleep` kept the pipe open and the stdout-pump `drain()`
    /// join blocked for the full 30 s. With the external leading its own
    /// process group, the cancel path SIGTERMs (then SIGKILLs) the whole
    /// group, the grandchild dies, the pipe closes, and the call returns
    /// at the 2 s timeout.
    #[cfg(unix)]
    #[test]
    fn timeout_kills_external_subprocess_tree() {
        let mut shell = fresh_shell();
        let (emit, _rx) = dummy_emitter();
        // The grandchild prints its own pid so the test can prove it was
        // reaped, then sleeps far past the timeout; the `/bin/sh` leader
        // blocks in `wait`, holding the call open until the grandchild
        // exits unless the timeout tears the group down.
        let cmd = "/bin/sh -c 'sleep 30 & echo $!; wait'";
        let t0 = std::time::Instant::now();
        let r = match run_shell(&mut shell, &Capabilities::root(), cmd, 2, &emit, None, None) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static failure: {s}"),
        };
        let elapsed = t0.elapsed();

        // The timeout is enforced: returns at ≈2 s, not the 30 s the
        // `sleep` would have run for, with the conventional exit 124.
        assert!(
            elapsed.as_secs() < 10,
            "timeout must bound wall-clock: returned after {elapsed:?} (sleep was 30s)"
        );
        assert_eq!(
            r.exit, 124,
            "a timed-out call reports the timeout exit code"
        );

        // The forked grandchild must be gone: `kill(pid, 0)` returns
        // ESRCH once it is reaped.  Poll briefly to absorb the tiny
        // window between the group SIGKILL and the kernel reaping it.
        let gc_pid: i32 = String::from_utf8_lossy(&r.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .expect("the grandchild printed its pid on stdout");
        let mut alive = true;
        for _ in 0..50 {
            // SAFETY: signal 0 performs error checking without delivering
            // a signal; it never touches an unrelated process.
            if unsafe { libc::kill(gc_pid as libc::pid_t, 0) } != 0 {
                alive = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !alive,
            "the forked grandchild (pid {gc_pid}) outlived the timeout"
        );
    }

    /// The same timeout must also tear down a sandbox-confined eval child.
    /// That path is blocked in IPC in the parent while the helper runs the
    /// body, so cooperative cancel polling inside the parent is not enough.
    /// The parent must kill the confined helper's subprocess tree out of band.
    #[cfg(unix)]
    #[test]
    fn timeout_kills_sandboxed_subprocess_tree() {
        let mut shell = fresh_shell();
        let (emit, _rx) = dummy_emitter();
        let cmd = "/bin/sh -c 'sleep 30 & echo $!; wait'";
        let t0 = std::time::Instant::now();
        let r = match run_shell(&mut shell, &projecting_caps(), cmd, 2, &emit, None, None) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static failure: {s}"),
        };
        let elapsed = t0.elapsed();
        let stderr = String::from_utf8_lossy(&r.stderr);
        if r.exit != 124
            && (stderr.contains("sandbox eval")
                || stderr.contains("failed to enter sandbox")
                || stderr.contains("bwrap"))
        {
            eprintln!("skip: OS sandbox unavailable on this host: {stderr}");
            return;
        }

        assert!(
            elapsed.as_secs() < 10,
            "sandboxed timeout must bound wall-clock: returned after {elapsed:?}"
        );
        assert_eq!(
            r.exit, 124,
            "a timed-out sandboxed call reports the timeout exit code; stderr was: {stderr}"
        );

        let gc_pid: i32 = String::from_utf8_lossy(&r.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .expect("the sandboxed grandchild printed its pid on stdout");
        let mut alive = true;
        for _ in 0..50 {
            if unsafe { libc::kill(gc_pid as libc::pid_t, 0) } != 0 {
                alive = false;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !alive,
            "the sandboxed forked grandchild (pid {gc_pid}) outlived the timeout"
        );
    }

    /// `grep-files` stamps each hit's witness against *its own* file's
    /// lines.  Two files carry the same matching text in different
    /// neighbourhoods, so the window-hashes must differ — proving the
    /// single-pass scan reads each file's rows as the run reaches it
    /// rather than reusing the previous file's.
    #[test]
    fn grep_files_hashes_each_file_against_its_own_rows() {
        let mut shell = fresh_shell();
        let setup = run_once(
            &mut shell,
            r#"let dir = temp-dir
to-string "alpha
TARGET
beta
" > "$dir/a.txt"
to-string "gamma
TARGET
delta
" > "$dir/b.txt"
cd $dir
let hits = grep-files 'TARGET'
return [count: !{length $hits}, hits: $hits]
"#,
        );
        assert_eq!(
            setup.exit,
            0,
            "grep-files failed: {}",
            String::from_utf8_lossy(&setup.stderr)
        );
        let value: serde_json::Value =
            serde_json::from_str(setup.value.as_deref().expect("structured result"))
                .expect("json tool value");
        assert_eq!(value["count"], 2, "one TARGET per file");
        let hits = value["hits"].as_array().expect("hits is a list");
        let files: std::collections::BTreeSet<&str> =
            hits.iter().map(|h| h["file"].as_str().unwrap()).collect();
        assert_eq!(
            files,
            ["a.txt", "b.txt"].into_iter().collect(),
            "both files appear"
        );
        assert_ne!(
            hits[0]["hash"], hits[1]["hash"],
            "identical text in distinct neighbourhoods must witness distinctly"
        );
    }

    /// A `Bytes` field in a structured result renders to the model as
    /// lossy-UTF-8 text, not the decimal integer array `to-json` uses for
    /// data round-trips.  Captured diagnostics — a job's or `audit` node's
    /// `stderr` — are byte-typed but text by intent; the model must read
    /// the message, not a wall of codes.  Here `to-bytes` mints bytes that
    /// decode to "killed" plus one stray non-UTF-8 byte (0xff): the readable
    /// prefix survives as text and the bad byte becomes a replacement char,
    /// with no decimal code in sight.
    #[test]
    fn byte_fields_render_as_lossy_text_not_decimal() {
        let mut shell = fresh_shell();
        let r = run_once(
            &mut shell,
            "return [stderr: !{to-bytes [107, 105, 108, 108, 101, 100, 255]}]",
        );
        assert_eq!(
            r.exit,
            0,
            "minting a byte value must succeed; stderr was: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let value = r.value.as_deref().expect("structured result");
        assert!(
            value.contains("\"killed"),
            "the readable prefix renders inside a JSON string, got {value:?}"
        );
        assert!(
            !value.contains("107") && !value.contains("255"),
            "no byte renders as a decimal code, got {value:?}"
        );
    }

    /// `grep-files`' lossy scan can match a file that is not valid UTF-8 —
    /// one `edit` can never witness (no row split is possible).  Rather than
    /// failing the whole search, the builtin returns such a hit with its hash
    /// flagged as the empty string: a value no `window-hash` produces, so it
    /// resolves to no line and is unmistakably "no witness".  A file holding
    /// "match " plus a stray 0xff matches the scan but is not valid UTF-8.
    #[test]
    fn grep_files_flags_a_non_utf8_hit_without_failing() {
        let mut shell = fresh_shell();
        let r = run_once(
            &mut shell,
            r#"let dir = temp-dir
to-bytes [109, 97, 116, 99, 104, 32, 255, 10] > "$dir/bad.txt"
cd $dir
return !{grep-files 'match'}
"#,
        );
        assert_eq!(
            r.exit,
            0,
            "a non-UTF-8 matched file must not fail the call; stderr was: {}",
            String::from_utf8_lossy(&r.stderr)
        );
        let value: serde_json::Value =
            serde_json::from_str(r.value.as_deref().expect("structured result"))
                .expect("json tool value");
        let hits = value.as_array().expect("grep-files returns a list");
        assert_eq!(hits.len(), 1, "the un-witnessable file still matches");
        assert_eq!(hits[0]["file"], "bad.txt");
        assert_eq!(
            hits[0]["hash"], "",
            "an un-witnessable hit carries the empty-string flag, not a hash"
        );
    }

    /// `grep-files` over a tree of N matching files emits *exactly one* grep
    /// surface (the search is one logical effect, not one per file), returns N
    /// stamped hits, and the witnesses it stamps RESOLVE in a subsequent
    /// `edit` — the search→edit round-trip the move below the redirect frame
    /// must preserve.
    #[test]
    fn grep_files_emits_one_surface_and_witnesses_resolve() {
        use crate::card::IoEvent;
        let mut shell = fresh_shell();
        let (tx, rx) = mpsc::channel();
        let emit = Emitter::new(tx, 0);

        let tmp =
            std::env::temp_dir().join(format!("exarch-grep-one-surface-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp tree");
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(tmp.join(name), "head\nTARGET line\ntail\n").expect("write fixture");
        }
        let tmp_str = display_no_trailing_sep(&tmp);

        // Search, then edit every hit by the witness grep stamped. If any
        // witness were stale, the edit would fail writing nothing.
        let src = format!(
            r#"cd '{tmp_str}'
let hits = grep-files 'TARGET'
each {{ |h| edit $h[file] [[$h[hash], 'REPLACED']] }} $hits
return !{{length $hits}}"#
        );
        let r = match run_shell(&mut shell, &Capabilities::root(), &src, 30, &emit, None, None) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static failure: {s}"),
        };
        assert_eq!(
            r.exit,
            0,
            "grep→edit round-trip must succeed; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert_eq!(r.value.as_deref(), Some("3"), "three files matched");

        // Every matched file is now rewritten — proof the witnesses resolved.
        for name in ["a.txt", "b.txt", "c.txt"] {
            assert_eq!(
                std::fs::read_to_string(tmp.join(name)).expect("read after edit"),
                "head\nREPLACED\ntail\n",
                "{name} must have its TARGET line replaced"
            );
        }

        let mut grep_surfaces = 0;
        while let Ok(ev) = rx.try_recv() {
            if let crate::bus::Kind::Io {
                event: IoEvent::Grep { scope, pattern },
                ..
            } = ev.kind
            {
                grep_surfaces += 1;
                assert_eq!(scope, ".", "the grep scope is the search root");
                assert_eq!(pattern, "TARGET");
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(
            grep_surfaces, 1,
            "one logical search emits exactly one grep surface, not one per file"
        );
    }

    /// Drive one tool call through `run_shell` with a real bus `Emitter`,
    /// returning the result alongside every `Kind` event captured off the
    /// channel.  The end-to-end coverage harness: it exercises the whole
    /// `core surface → AgentSink::emit → Kind` path the gap tests assert on,
    /// the same wiring `edit_emits_kind_card` and friends use, hoisted so the
    /// coverage tests share it rather than re-threading the channel each time.
    fn run_capturing(shell: &mut Shell, cmd: &str) -> (ToolResult, Vec<crate::bus::Kind>) {
        let (tx, rx) = mpsc::channel();
        let emit = Emitter::new(tx, 0);
        let result = match run_shell(shell, &Capabilities::root(), cmd, 30, &emit, None, None) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static (parse/type) failure: {s}"),
        };
        // Drop the emitter so the channel disconnects and `try_recv` drains
        // cleanly to empty rather than blocking.
        drop(emit);
        let kinds: Vec<crate::bus::Kind> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|ev| ev.kind)
            .collect();
        (result, kinds)
    }

    /// The `IoEvent`s carried by the captured `Kind::Io` events, in order —
    /// the structural effect records the gap tests assert on without caring
    /// about the card composed beside each.
    fn io_events(kinds: &[crate::bus::Kind]) -> Vec<&crate::card::IoEvent> {
        kinds
            .iter()
            .filter_map(|k| match k {
                Kind::Io { event, .. } => Some(event),
                _ => None,
            })
            .collect()
    }

    /// Write `body` into a fresh per-test temp dir and return both the dir and
    /// the display path of the file inside it (no trailing separator), the
    /// fixture shape the redirect/exec coverage tests need.  Mirrors the
    /// scratch-dir pattern the surrounding edit/grep tests already use.
    fn scratch_file(tag: &str, name: &str, body: &str) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("exarch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write scratch fixture");
        let disp = display_no_trailing_sep(&path);
        (dir, disp)
    }

    /// Coverage — the READ door end-to-end: a bare `from-string < a` reads
    /// stdin through one `<` redirect, so exactly one `Kind::Io` carrying a
    /// `Read` event for that path reaches the bus.  No exec card: `from-string`
    /// is a builtin, not an external image.
    #[test]
    fn bare_read_redirect_surfaces_one_read_card() {
        use crate::card::IoEvent;
        let mut shell = fresh_shell();
        let (dir, path) = scratch_file("cov-read", "a", "hello\n");

        let (r, kinds) = run_capturing(&mut shell, &format!("from-string < '{path}'"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            r.exit,
            0,
            "the read redirect must succeed; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );

        let ios = io_events(&kinds);
        assert_eq!(
            ios.len(),
            1,
            "a bare `from-string < a` raises exactly one io card, got {ios:?}"
        );
        assert_eq!(
            ios[0],
            &IoEvent::Read { path: path.clone() },
            "the one io event is a read of the redirect path"
        );
    }

    /// Coverage — the WRITE door end-to-end: a bare `to-string "x" > b`
    /// commits an atomic write through one `>` redirect, so exactly one
    /// `Kind::Io` carrying a committed `Write` event reaches the bus.
    #[test]
    fn bare_write_redirect_surfaces_one_committed_write_card() {
        use crate::card::{IoEvent, WriteMode, WriteOutcome};
        let mut shell = fresh_shell();
        // A fresh dir with no fixture file: the write creates the target.
        let dir = std::env::temp_dir().join(format!("exarch-cov-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let path = display_no_trailing_sep(&dir.join("b"));

        let (r, kinds) = run_capturing(&mut shell, &format!("to-string 'x' > '{path}'"));
        let wrote = std::fs::read_to_string(dir.join("b")).ok();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            r.exit,
            0,
            "the write redirect must succeed; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert_eq!(wrote.as_deref(), Some("x"), "the write committed to disk");

        let ios = io_events(&kinds);
        assert_eq!(
            ios.len(),
            1,
            "a bare `to-string > b` raises exactly one io card, got {ios:?}"
        );
        assert_eq!(
            ios[0],
            &IoEvent::Write {
                path: path.clone(),
                mode: WriteMode::Write,
                outcome: WriteOutcome::Committed,
            },
            "the one io event is a committed write of the redirect path"
        );
    }

    /// Coverage — the EXEC door end-to-end: a bare external command raises
    /// exactly one `Kind::Io` carrying an `Exec` event with the resolved argv
    /// and exit status.  `/usr/bin/true` is the deterministic, always-present
    /// image the core suite already leans on.
    #[cfg(unix)]
    #[test]
    fn bare_external_surfaces_one_exec_card() {
        use crate::card::{ExecOutcome, IoEvent};
        let mut shell = fresh_shell();

        let (r, kinds) = run_capturing(&mut shell, "/usr/bin/true");
        assert_eq!(
            r.exit,
            0,
            "/usr/bin/true must exit zero; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );

        let ios = io_events(&kinds);
        assert_eq!(
            ios.len(),
            1,
            "a bare external raises exactly one io card, got {ios:?}"
        );
        assert_eq!(
            ios[0],
            &IoEvent::Exec {
                argv: vec!["/usr/bin/true".into()],
                outcome: ExecOutcome::Ok,
                status: 0,
            },
            "the one io event is a successful exec of the image"
        );
    }

    /// `view-text` is a ral closure that reads stdin, NOT an external image: a
    /// `view-text 1 2 < a` reads its input through the `<` redirect, so the door
    /// raises one READ card and no exec card — the closure dispatches in
    /// process, never spawning a command.
    #[cfg(unix)]
    #[test]
    fn view_is_a_helper_not_an_exec_image() {
        use crate::card::IoEvent;
        let mut shell = fresh_shell();
        let (dir, path) = scratch_file("cov-view", "a", "alpha\nbeta\ngamma\n");

        let (r, kinds) = run_capturing(&mut shell, &format!("view-text 1 2 < '{path}'"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            r.exit,
            0,
            "view-text must read the fixture; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );

        let ios = io_events(&kinds);
        let reads = ios
            .iter()
            .filter(|e| matches!(e, IoEvent::Read { .. }))
            .count();
        let execs = ios
            .iter()
            .filter(|e| matches!(e, IoEvent::Exec { .. }))
            .count();
        assert_eq!(reads, 1, "view-text's `< a` raises one read card");
        assert_eq!(
            execs, 0,
            "view-text is a ral closure, not an external image — no exec card, got {ios:?}"
        );
    }

    /// Two model operations in one command: `cat < a` installs a `<` redirect
    /// (one READ card) and then runs the external `cat` over that stdin (one
    /// EXEC card) — exactly two io cards, in that order, because the read door
    /// announces eagerly on install, before the body it feeds runs.
    #[cfg(unix)]
    #[test]
    fn cat_redirect_surfaces_read_then_exec_in_order() {
        use crate::card::{ExecOutcome, IoEvent};
        let mut shell = fresh_shell();
        let (dir, path) = scratch_file("cov-cat", "a", "one\ntwo\n");

        let (r, kinds) = run_capturing(&mut shell, &format!("/bin/cat < '{path}'"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            r.exit,
            0,
            "cat must read the fixture; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&r.stdout),
            "one\ntwo\n",
            "cat echoes the redirected stdin"
        );

        let ios = io_events(&kinds);
        assert_eq!(
            ios.len(),
            2,
            "cat < a is two logical operations — one read, one exec — got {ios:?}"
        );
        assert_eq!(
            ios[0],
            &IoEvent::Read { path: path.clone() },
            "the read installs first, before the body runs"
        );
        assert_eq!(
            ios[1],
            &IoEvent::Exec {
                argv: vec!["/bin/cat".into()],
                outcome: ExecOutcome::Ok,
                status: 0,
            },
            "then cat execs over that stdin"
        );
    }

    /// Documented boundary — code loading is not turn-time data I/O: sourcing
    /// a small ral file (and `use`-ing one) loads it through `std::fs` below
    /// the redirect frame, so it raises NO io card.  Only the file's own
    /// effects, if any, would surface — here the file is pure bindings, so the
    /// bus stays silent of io events.
    #[test]
    fn sourcing_a_ral_file_raises_no_io_card() {
        let mut shell = fresh_shell();
        let (dir, path) = scratch_file("cov-source", "lib.ral", "let answer = 42\n");

        let (sr, source_kinds) = run_capturing(&mut shell, &format!("source '{path}'"));
        assert_eq!(
            sr.exit,
            0,
            "source must load the file; stderr was {:?}",
            String::from_utf8_lossy(&sr.stderr)
        );
        assert!(
            io_events(&source_kinds).is_empty(),
            "code loading is not data I/O — source raises no io card, got {:?}",
            io_events(&source_kinds)
        );

        let (ur, use_kinds) = run_capturing(&mut shell, &format!("use '{path}'"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            ur.exit,
            0,
            "use must load the file; stderr was {:?}",
            String::from_utf8_lossy(&ur.stderr)
        );
        assert!(
            io_events(&use_kinds).is_empty(),
            "use is code loading too — no io card, got {:?}",
            io_events(&use_kinds)
        );
    }

    /// The transcript seam (`transcript::event_record`) is the *operational*
    /// view: a `Kind::Io` projects to `("io", { event })`, keeping the raw
    /// structural effect the rendered card erases — but NOT the card itself,
    /// which is a rendering and belongs to the TUI's `user.log`.  Driven
    /// against `event_record` directly — not a TUI render.
    #[test]
    fn io_event_record_carries_structural_event_not_card() {
        use crate::card::{IoEvent, io_card};
        let event = IoEvent::Write {
            path: "b.rs".into(),
            mode: crate::card::WriteMode::Append,
            outcome: crate::card::WriteOutcome::Committed,
        };
        let card = io_card(&event);
        let kind = Kind::Io {
            event: event.clone(),
            card,
        };
        let rec = crate::transcript::event_record(7, 3, &kind).expect("an io event records");

        assert_eq!(rec["kind"], "io", "the record is tagged io");
        // The raw structural event survives, tagged by its `io` field with the
        // mode/outcome enums as snake_case strings.
        assert_eq!(rec["event"]["io"], "write");
        assert_eq!(rec["event"]["path"], "b.rs");
        assert_eq!(rec["event"]["mode"], "append");
        assert_eq!(rec["event"]["outcome"], "committed");
        // The rendered card does NOT ride along — it is a presentation, not an
        // operational effect.
        assert!(
            rec.get("card").is_none(),
            "the operational trace drops the rendered card, got {rec:?}"
        );
    }

    /// `edit` does all its file I/O in Rust, below the redirect frame, so it is
    /// a single logical surface: it emits its diff card(s) and NO read/write io
    /// card. Capturing the sink, only the diff card appears — never a
    /// `{io: read}` or `{io: write}`.
    #[test]
    fn edit_emits_only_its_diff_card_no_io_card() {
        use crate::card::IoEvent;
        let mut shell = fresh_shell();
        let (tx, rx) = mpsc::channel();
        let emit = Emitter::new(tx, 0);

        let tmp =
            std::env::temp_dir().join(format!("exarch-edit-one-surface-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("create temp dir");
        let path = tmp.join("f.txt");
        std::fs::write(&path, "alpha\nunique target line\nomega\n").expect("write fixture");
        let tmp_str = display_no_trailing_sep(&tmp);

        // Acquire the witness through `grep-files`, whose read is also in Rust
        // (it raises a grep io card, never a read/write one), so the only
        // read/write io that *could* appear would be edit's — and there is
        // none. A redirect `< path` here would raise its own read card and
        // confound the assertion.
        let src = format!(
            r#"cd '{tmp_str}'
let hits = grep-files 'unique target'
edit $hits[0][file] [[$hits[0][hash], 'REPLACED']]"#
        );
        let r = match run_shell(&mut shell, &Capabilities::root(), &src, 30, &emit, None, None) {
            Outcome::Ran(r) => r,
            Outcome::Static(s) => panic!("static failure: {s}"),
        };
        assert_eq!(
            r.exit,
            0,
            "edit must succeed; stderr was {:?}",
            String::from_utf8_lossy(&r.stderr)
        );

        let mut diff_cards = 0;
        let mut io_cards = 0;
        while let Ok(ev) = rx.try_recv() {
            match ev.kind {
                crate::bus::Kind::Card(card) if card.has_diff() => diff_cards += 1,
                crate::bus::Kind::Io {
                    event: IoEvent::Read { .. } | IoEvent::Write { .. },
                    ..
                } => io_cards += 1,
                _ => {}
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(diff_cards, 1, "edit surfaces exactly one diff card");
        assert_eq!(
            io_cards, 0,
            "edit's read and write happen in Rust, so no read/write io card is raised"
        );
    }

    /// A JSON string renders raw — no escaping — so a markdown report keeps
    /// real newlines rather than literal `\n`.  This is the shape a `reply`
    /// payload and a `view-text` value share.
    #[test]
    fn json_to_text_passes_a_string_through_raw() {
        let v = serde_json::json!("# Report\nline one\nline two");
        assert_eq!(
            super::json_to_text(&v).as_deref(),
            Some("# Report\nline one\nline two"),
        );
    }

    /// A JSON object/array is pretty-printed, so its structure stays legible —
    /// the structured-findings case for `reply`.
    #[test]
    fn json_to_text_pretty_prints_structured_values() {
        let v = serde_json::json!({ "findings": ["a", "b"] });
        let out = super::json_to_text(&v).expect("an object renders");
        assert!(out.contains("\"findings\""));
        assert!(out.contains('\n'), "pretty-printing keeps the shape on lines");
    }

    /// A JSON null renders to nothing — the empty-reply case that settles
    /// `AgentOutcome::Empty`.
    #[test]
    fn json_to_text_renders_null_to_nothing() {
        assert_eq!(super::json_to_text(&serde_json::Value::Null), None);
    }
}
