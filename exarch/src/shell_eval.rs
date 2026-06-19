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

use crate::bus::{Emitter, Kind};
use crate::card::value_to_card;
use ral_core::types::{Break, Escape};
use ral_core::{
    EventSink, Shell, StaticDiagnostics, TurnIo, TurnReport, TurnRequest, Value as RalValue,
    diagnostic,
};
use std::sync::Arc;
use std::time::Duration;

/// Lifetime ceiling armed on every detached `spawn` worker: an
/// abandoned worker is reaped one hour after it is spawned, well past
/// the 30 s foreground wall but bounded so a long-running agent cannot
/// accumulate immortal zombies.
const DETACHED_WORKER_CEILING: Duration = Duration::from_secs(60 * 60);

/// The prelude baked into this binary at build time by `build.rs`.
pub static PRELUDE: ral_core::host::BakedPrelude = ral_core::baked_prelude!();

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
/// installs for a tool call.  It decodes each `Value` the `surface` builtin
/// hands it into a render document (a [`Card`] of Bertin marks, via
/// [`value_to_card`]) and emits it on the presentation bus through a clone of
/// the call's [`Emitter`].  A value that is not a card is dropped — the same
/// graceful degradation the old tagged-variant decoder had.  Detached workers
/// never receive this sink: core buffers their `surface` calls and replays
/// them on `await`, so a clone of the bus `Emitter` can never outlive the
/// tool turn.
///
/// [`Card`]: crate::card::Card
struct AgentSink(Emitter);

impl EventSink for AgentSink {
    fn emit(&self, ev: &RalValue) {
        if let Some(card) = value_to_card(ev) {
            self.0.emit(Kind::Card(card));
        }
    }
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
    let report = shell.run_turn(
        cmd,
        TurnRequest {
            script_name: name,
            caps: caps.clone(),
            turn_limit: Some(Duration::from_secs(timeout_secs)),
            detached_limit: Some(DETACHED_WORKER_CEILING),
            io: TurnIo::Capture,
            surface: Some(Arc::new(AgentSink(emit.clone()))),
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
                         Spawn it (`let h = spawn {{ … }}`), let the turn return, then `poll $h` on later \
                         turns and `await $h` once it has settled."
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
        // A top-level string is the result of stringly tools like
        // `view`; JSON-encoding it would escape every newline as `\n`,
        // which the model then reads as literal backslash-n.  Pass the
        // raw text through.  Structured values go through pretty JSON so
        // the shape is legible, with byte fields decoded as lossy UTF-8
        // rather than the integer arrays `to-json` round-trips: a job's
        // or audit node's captured `stderr` is text the model must read,
        // not data.  That walk is total, so a value carrying a thunk or a
        // non-finite float still renders instead of collapsing to nothing.
        Some(RalValue::String(s)) => Some(s),
        Some(v) if !matches!(v, RalValue::Unit) => {
            let json = ral_core::builtins::value_to_json_lossy_bytes(&v);
            serde_json::to_string_pretty(&json).ok()
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

#[cfg(test)]
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
    use crate::bus::Emitter;
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
        let mut shell = ral_core::host::boot_shell(Default::default(), &PRELUDE);
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
        match run_shell(shell, &Capabilities::root(), cmd, 30, &emit) {
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

    /// `view` is the one read primitive: a sourced prelude pipeline over
    /// the `line-hash` builtin plus coreutils.  `echo "alpha" | view 1 2`
    /// must number the line and tag it with its content hash, which means
    /// `line-hash` dispatches as a builtin rather than falling through to
    /// an external-command exec lookup ("command 'line-hash' not found on
    /// PATH").  Each row is `<n>\t<hash>\t<text>`.
    #[cfg(unix)]
    #[test]
    fn view_tags_lines_with_hash() {
        let mut shell = fresh_shell();
        let r = run_once(&mut shell, "echo \"alpha\" | view 1 2");
        assert_eq!(
            r.exit,
            0,
            "view must run; stderr was: {}",
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
            shell.mobile.scope.get("pre_x").is_some(),
            "pre-failure `let` must persist into the next tool call"
        );
        assert!(
            shell.mobile.scope.get("post_y").is_none(),
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
    /// atoms they depend on as host builtins.  This test also locks in
    /// `run_shell` passing live bindings into elaboration:
    /// `view` / `view-around` / `edit` are source-loaded names, not core
    /// prelude exports.
    #[test]
    fn agent_helpers_are_loaded_into_tool_shell() {
        let mut shell = fresh_shell();
        assert!(shell.mobile.scope.get("window-hash").is_some());
        assert!(shell.mobile.scope.get("view").is_some());
        assert!(shell.mobile.scope.get("view-around").is_some());
        assert!(shell.mobile.scope.get("grep-files").is_some());
        assert!(shell.mobile.scope.get("edit").is_some());
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
                 let w = window-hash $rows 1\n\
                 edit '{repeated_str}' [[$w, 'FIRST']]"
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
                 let w = window-hash $rows 5\n\
                 edit '{run_str}' [[$w, 'Z']]"
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
                 let a = window-hash $rows 1\n\
                 edit '{path_str}' [[$a, 'X'], ['hzzzzzz', 'Y']]"
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
                 let a = window-hash $rows 1\n\
                 let b = window-hash $rows 2\n\
                 let c = window-hash $rows 3\n\
                 edit '{path_str}' [[$a, 'REPLACED'], [$b, ''], [$c, 'X\nY']]"
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
        let r = match run_shell(&mut shell, &Capabilities::root(), &src, 30, &emit) {
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
        assert_eq!(
            hunk.del,
            vec!["unique target line"],
            "del must carry the literal replaced line, not the hash"
        );
        assert_eq!(hunk.add, vec!["REPLACED"]);
        assert_eq!(hunk.start, 2, "the edit begins at line 2");
        assert_eq!(
            hunk.before,
            vec!["alpha"],
            "the line above the edit is carried as leading context"
        );
        assert_eq!(
            hunk.after,
            vec!["omega"],
            "the line below the edit is carried as trailing context"
        );
    }

    /// Regression: a witness hash that happens to read as a number must
    /// still round-trip from `view` into `edit`. The agent copies the hash
    /// out of a `view` result and types it as a *bare* `edit` argument, so
    /// an all-digit hash like `152347` lexes as an `Int` while the hash
    /// `edit` recomputes is a `String`; the witness check then rejects a
    /// correct hash and the agent loops forever re-issuing the same edit.
    /// The leading-zero case (`012345`) is the sharper one: its integer
    /// reading drops the zero, so no string coercion inside `edit` could
    /// recover the original — only an un-numeric witness format can.
    #[cfg(unix)]
    #[test]
    fn edit_accepts_numeric_witness_hash() {
        // The witness `view` shows is the `window-hash`, not the bare line
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

            // Read the witness exactly as the agent would: from `view`.
            let vr = run_once(&mut shell, &format!("view 1 2 < '{path_str}'"));
            assert_eq!(
                vr.exit,
                0,
                "view must read the fixture; stderr was: {}",
                String::from_utf8_lossy(&vr.stderr)
            );
            let stdout = String::from_utf8_lossy(&vr.stdout);
            let row = stdout.lines().next().unwrap_or_default();
            let witness = row
                .split('\t')
                .nth(1)
                .expect("view row tags a hash")
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
        let r = match run_shell(&mut shell, &Capabilities::root(), cmd, 2, &emit) {
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
        let r = match run_shell(&mut shell, &projecting_caps(), cmd, 2, &emit) {
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

    /// `grep-files`' lossy scan can match a file the strict `from-string`
    /// re-read then rejects.  The failure must name that file and point at
    /// the search scope — not surface a bare `from-string: … use from-bytes`
    /// the model never called and could not slot into a `grep-files` call.
    /// A file holding "match " plus a stray 0xff matches the scan but is not
    /// valid UTF-8.
    #[test]
    fn grep_files_localizes_a_non_utf8_file() {
        let mut shell = fresh_shell();
        let r = run_once(
            &mut shell,
            r#"let dir = temp-dir
to-bytes [109, 97, 116, 99, 104, 32, 255, 10] > "$dir/bad.txt"
cd $dir
grep-files 'match'
"#,
        );
        assert_ne!(r.exit, 0, "a non-UTF-8 matched file must fail the call");
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert!(
            stderr.contains("bad.txt") && stderr.contains("not valid UTF-8"),
            "the error names the offending file and why, got {stderr:?}"
        );
        assert!(
            !stderr.contains("from-string"),
            "the error must not name an internal codec the model never called, got {stderr:?}"
        );
    }
}
