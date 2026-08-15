//! The REPL: the terminal guard, the agent worker thread, and the merged
//! render+input loop the UI thread runs beside it.

use std::{
    io::{self},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crossterm::event::{
    Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, poll as ct_poll,
    read as ct_read,
};

use crate::{
    agent::{Agent, Control, Verdict, cancel},
    bus::{AgentId, BusReceiver, Emitter, FleetBus, Inbox, Pass, Post, Signal},
    fleet::{Fleet, registry::AgentRegistry},
    provider::{
        self, Provider,
        credential::CredentialStore,
        models::{LiveSource, ModelCatalog},
    },
    record::{Emitter as Recorder, Printer},
};
use std::sync::mpsc::TryRecvError;

use super::banner::SessionInfo;
use super::block::RailShape;
use super::{
    App, banner, commands, line,
    render::draw,
    terminal::{self, TerminalGuard},
};

/// Terminal and app, kept as two fields so the loop can borrow `guard.term()`
/// and `&mut app` at once.
pub(super) struct Tui {
    pub(super) guard: TerminalGuard,
    pub(super) app: App,
}

impl Tui {
    pub fn new(
        root_id: AgentId,
        root_log_dir: &Path,
        stderr_log: &Path,
        vi: bool,
        append_log: bool,
        inbox: Inbox,
        agents: AgentRegistry,
    ) -> io::Result<Self> {
        let guard = TerminalGuard::enter(stderr_log)?;
        let app = App::new(root_id, root_log_dir, vi, append_log, inbox, agents);
        Ok(Self { guard, app })
    }
}

/// The session-mutating slash commands, run by [`Agent::attend`] at the
/// exchange boundary where the attend thread owns the agent; every other
/// command is served UI-side and never arrives here.  Only the trunk attends
/// with this `Control` — sub-agents run under
/// [`NoControl`](crate::agent::NoControl) — so a command always lands on the
/// trunk's own context.
pub struct ReplControl;

impl Control for ReplControl {
    fn command(&mut self, raw: &str, session: &mut Agent, emit: &Emitter) -> Verdict {
        let trimmed = raw.trim();
        let (head, rest) = commands::split_head(trimmed);
        if head == "/branch" {
            let prompt = (!rest.is_empty()).then_some(rest);
            match crate::shell_eval::tools::spawn_branch(session, prompt, emit) {
                Ok(child) => Agent::note(
                    format!("branch {} started (agent {})", child.name, child.id),
                    session,
                ),
                Err(e) => session.note_error(format!("could not start branch: {e}")),
            }
            return Verdict::Continue;
        }
        if head == "/rewind" {
            let anchor = match rest {
                "" => {
                    session.note_error(
                        "usage: /rewind <exchange> — name a closed exchange in the current context"
                            .into(),
                    );
                    return Verdict::Continue;
                }
                text => {
                    if let Ok(anchor) = text.parse::<u64>() {
                        anchor
                    } else {
                        session.note_error(format!(
                            "/rewind expects one non-negative exchange number, got `{text}`"
                        ));
                        return Verdict::Continue;
                    }
                }
            };
            if let Err(error) = session.rewind(anchor, emit) {
                session.note_error(error);
            }
            return Verdict::Continue;
        }
        match trimmed {
            "/clear" => {
                let result = session.clear();
                session
                    .recorder()
                    .transient(crate::record::Transient::Cleared);
                if let Err(error) = result {
                    session.note_error(format!("clear failed: {error}"));
                }
                Verdict::Continue
            }
            "/compact" => {
                let p = session.current_provider();
                let token = session.cancel_token().clone();
                session.compact(&p, true, &token, None);
                Verdict::Continue
            }
            // Surveyed on the thread that owns the shell the rows describe,
            // and emitted as one bus event — never a model turn.
            "/resources" => {
                session.emit_resources(&session.recorder());
                Verdict::Continue
            }
            "/context" => {
                session.emit_context_survey();
                Verdict::Continue
            }
            "/quit" | "/exit" => Verdict::Quit,
            _ => Verdict::Continue,
        }
    }
}

/// Build the [`Tui`], run the worker beside the UI loop, then flush logs and
/// print the paths and usage on the restored shell.
///
/// # Errors
/// Returns `Err` if terminal setup fails, if drawing the banner fails, or if
/// the UI render/input loop hits a fatal terminal error.
///
/// # Panics
/// Panics if the OS refuses to spawn the agent worker thread.
#[allow(clippy::too_many_arguments)]
pub fn run(
    session: &mut Agent,
    provider: &Arc<Provider>,
    info: &banner::SessionInfo<'_>,
    store: &mut CredentialStore,
    catalog: &mut ModelCatalog<LiveSource>,
    run_dir: &Path,
    seed: Option<String>,
    vi: bool,
    engine: Arc<provider::Engine>,
) -> Result<(), String> {
    let stderr_log = run_dir.join("stderr.log");
    let mut tui = Tui::new(
        session.id,
        &session.log_dir(),
        &stderr_log,
        vi,
        session.is_resumed(),
        session.inbox(),
        session.agents.clone(),
    )
    .map_err(|e| format!("ratatui init: {e}"))?;
    tui.app.update_live_model(provider);
    // A *session*-lived bus, not per-exchange: a detached async child keeps
    // streaming to its tab after the exchange that spawned it ends.
    let fleet = Fleet {
        agents: session.agents.clone(),
        bus: FleetBus::session(&session.inbox()),
        engine,
    };
    if let Some(s) = seed {
        session.seed(s);
    }
    tui.app
        .banner(tui.guard.term(), info)
        .map_err(|e| e.to_string())?;

    // The worker parks on an empty inbox until `/quit`; latching `done` on the
    // way out is the only thing that lets the UI loop's next drain say `Stop`.
    let done = AtomicBool::new(false);
    let done_ref = &done;
    let mut control = ReplControl;
    // The worker crosses the thread boundary with an emitter, not the bus:
    // `FleetBus` holds a single-consumer `Receiver` and so is not `Sync`, while
    // an `Emitter` is `Send` and is all the worker needs.
    let worker_emit = fleet.bus.emitter(session.id);
    // The UI thread's own door onto the record seam — reachable here, before
    // the worker spawns, where `LogCell`'s no-wait rule is trivially
    // satisfied.  A cheap `Arc<Log>` clone, so a `/model` switch or a login
    // records through the same seam the worker's own commits use.
    let recorder = session.recorder();
    if let Some((exchanges, bytes)) = session.resume_summary() {
        // Fold the record log into the fold's memo *before* the note below,
        // so the note is the boundary the plan names: everything ahead of it
        // is replayed history, everything after is the live session — the
        // one call `Viewport::sync` makes with a producer other than the
        // live `push_*` half, seeding a fresh viewport that no `push_*` has
        // touched yet (`dev/docs/plans/260814_one_seam_one_log.md`, step 7).
        let record_path = session.log_dir().join("record.jsonl");
        let blocks = crate::record::replay::<crate::record::View>(&record_path)
            .map_err(|e| format!("record.jsonl did not replay cleanly: {e}"))?;
        // The fold's usage rows are cumulative, matching `total_usage`'s own
        // running sum; it carries no cache/dollar breakdown and no *last*
        // turn's prompt size, so `last_input` — the ctx gauge's numerator —
        // stays at zero until the next live turn reports one, same as a
        // fresh session's opening frame.
        tui.app.total_usage.input = blocks.input_tokens();
        tui.app.total_usage.output = blocks.output_tokens();
        if let Some(vp) = tui.app.tabs.viewport_mut(session.id) {
            vp.sync(&blocks);
        }
        // The boundary itself is chrome, never recorded: a second resume must
        // not replay a prior resume's note as if it were history.
        tui.app.push_chrome(
            session.id,
            RailShape::Plain,
            line::note(&format!(
                "resumed: {exchanges} exchanges, {} KB",
                bytes.div_ceil(1024)
            )),
        );
    }
    // Without a way to wake the parked worker with a `/quit`, the `join` below
    // would deadlock whenever the UI loop dies first.
    let quit_mailbox = session.inbox().mailbox();
    let mut cmd_ctx = CommandCtx {
        agents: &fleet.agents,
        store,
        catalog,
        info,
        recorder: &recorder,
        engine: &fleet.engine,
    };
    std::thread::scope(|scope| -> Result<(), String> {
        let worker = std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(scope, move || {
                let out = session.attend(&mut control, &worker_emit);
                done_ref.store(true, Ordering::Release);
                out
            })
            .expect("spawn agent worker");

        let r = ui_loop(&mut tui, &fleet.bus, done_ref, &mut cmd_ctx);
        if r.is_err() {
            // A rejected push (the inbox at quota) has nowhere to be reported:
            // `r`'s error is already the one in flight.
            let _ = quit_mailbox.push(Post::Command("/quit".into()));
        }
        let _ = worker.join();
        r.map_err(|e| e.to_string())
    })?;

    let logs = tui
        .app
        .flush_logs()
        .map_err(|e| format!("session logs: {e}"));
    let usage = tui.app.total_usage();
    // Restore the terminal first, so the paths below land on the user's shell
    // rather than the alt screen.
    drop(tui);
    if let Ok(paths) = &logs {
        for p in paths {
            match p.parent() {
                Some(dir) => println!("Agent logs: {} (user.log + record.jsonl)", dir.display()),
                None => println!("Agent log: {}", p.display()),
            }
        }
    } else if let Err(e) = logs {
        eprintln!("exarch: {e}");
    }
    println!("{usage}");
    Ok(())
}

/// The handles a submitted line is serviced against, bundled so the command
/// path — `route_submit` into `pick_model` / `login` — threads one context.
/// `agents` is the same shared map the worker mutates, so an agent it registers
/// is steerable at once.
pub struct CommandCtx<'a> {
    pub(super) agents: &'a AgentRegistry,
    pub(super) store: &'a mut CredentialStore,
    pub(super) catalog: &'a mut ModelCatalog<LiveSource>,
    pub(super) info: &'a SessionInfo<'a>,
    pub(super) recorder: &'a Recorder,
    pub(super) engine: &'a Arc<provider::Engine>,
}

/// Route one `Signal` to its `App` entry point.
fn dispatch(tui: &mut Tui, rx: &BusReceiver, sig: Signal) {
    match sig {
        Signal::Fact(id, fact) => tui.app.fact(id, &fact),
        Signal::Transient(id, t) => tui.app.transient(id, t, rx),
    }
}

/// The drain contract — the worker's explicit `done` flag ends a pass, never
/// the channel merely emptying — over the TUI's own dispatch loop, on its
/// render cadence rather than a `Sink`'s blocking one.
fn drain_signals(
    rx: &BusReceiver,
    done: &AtomicBool,
    max: Option<usize>,
    mut handle: impl FnMut(Signal),
) -> Pass {
    let finished = done.load(Ordering::Acquire);
    let mut n = 0usize;
    loop {
        if max.is_some_and(|m| n >= m) {
            return if finished { Pass::Stop } else { Pass::More };
        }
        match rx.try_recv() {
            Ok(sig) => {
                handle(sig);
                n += 1;
            }
            Err(TryRecvError::Empty) => {
                return if finished { Pass::Stop } else { Pass::Idle };
            }
            Err(TryRecvError::Disconnected) => return Pass::Stop,
        }
    }
}

/// The merged render + input loop, on the UI thread beside the worker's
/// [`Agent::attend`].  Returns once the worker is done, after one last frame
/// that includes everything it emitted.
fn ui_loop(
    tui: &mut Tui,
    bus: &FleetBus,
    done: &AtomicBool,
    ctx: &mut CommandCtx<'_>,
) -> io::Result<()> {
    const BATCH: usize = 64;
    let frame = Duration::from_millis(16); // ~60 FPS max
    let mailbox = tui.app.inbox.mailbox();
    let rx = bus.rx();
    // Seeded a frame in the past so the first iteration paints at once.  Gating
    // draws on it decouples the redraw rate from the drain rate: a token flood
    // coalesces into one frame per interval, not a rewrite per batch.
    let mut last_draw = Instant::now().checked_sub(frame).unwrap();
    // Set by anything visible that `animating` does not already cover.
    let mut dirty = true;
    // A park or unpark repaints the tab title and prompt chrome with no bus
    // event of its own, so the transition has to be watched for here.
    let mut prev_waiting = tui.app.focused_waiting();
    loop {
        let prev_focus = tui.app.tabs.focused();
        // The explicit-done contract, shared with the headless `Sink::drive`:
        // stop only when the worker is *done*, never when the channel empties,
        // so a detached background agent flooding the bus cannot end the loop
        // early.  The cap bounds how long that flood starves the input poll.
        let mut handled_any = false;
        let more = match drain_signals(rx, done, Some(BATCH), |sig| {
            handled_any = true;
            dispatch(tui, rx, sig);
        }) {
            Pass::Stop => {
                // The cap binds even a `done` drain, so `Stop` can arrive with
                // events still buffered; nothing drains after this returns, so
                // empty the channel uncapped before the final frame.
                drain_signals(rx, done, None, |sig| dispatch(tui, rx, sig));
                tui.app.mark_ready();
                draw(&mut tui.app, tui.guard.term())?;
                return Ok(());
            }
            Pass::More => true,
            Pass::Idle => false,
        };
        dirty |= handled_any;
        let now_waiting = tui.app.focused_waiting();
        dirty |= now_waiting != prev_waiting;
        prev_waiting = now_waiting;
        // `tick` runs on every due frame, painted or not: it ages dying tabs
        // out on its own clock, independent of what gates the redraw.
        if last_draw.elapsed() >= frame {
            let ticked = tui.app.tabs.tick();
            let animating = tui.app.animating(frame);
            if dirty || ticked || animating {
                draw(&mut tui.app, tui.guard.term())?;
                dirty = false;
            }
            // A skipped frame still advances the clock, or the poll timeout
            // below floors at zero and the loop spins.
            last_draw = Instant::now();
        }
        // Polled every iteration, even mid-backlog, so a token flood can never
        // starve Esc/Ctrl-C; the wait stays zero until the channel empties.
        let timeout = if more {
            Duration::ZERO
        } else {
            frame.saturating_sub(last_draw.elapsed())
        };
        if ct_poll(timeout)? {
            match ct_read()? {
                CtEvent::Key(k) if k.kind == KeyEventKind::Press => {
                    dirty = true;
                    let focused = tui.app.tabs.focused();
                    let steerable = tui.app.is_steerable();
                    match key_action(&k, steerable) {
                        // Every tab interrupts through the registry, which
                        // cancels the focused entry's token and its current
                        // dispatch scope by handle — published ahead of the
                        // engine lock, so the keypress cannot land on a run
                        // that just ended — and never the durable root: the
                        // exchange dies, its descendants do not, and the
                        // agent lives to take a next run. The trunk alone
                        // also raises the ambient interrupt, gated to it
                        // because only the trunk publishes the process-wide
                        // slot; that path alone re-creates the SIGINT for a
                        // foreground external child and stamps the ambient
                        // foreground cause, which needs no dispatch to name.
                        KeyAction::Cancel => {
                            ctx.agents.interrupt(focused);
                            if focused == tui.app.tabs.root() {
                                cancel::raise_interrupt();
                            }
                        }
                        KeyAction::Submit => {
                            if let Some(text) = tui.app.prompt_state.submit() {
                                // Every tab funnels through the one path, which
                                // parses before it forks on the focused tab —
                                // so no slash line can be mailed to the model.
                                commands::route_submit(text, tui, &mailbox, ctx)?;
                            }
                        }
                        KeyAction::Edit => {
                            tui.app.key(k);
                            if tui.app.prompt_state.take_editor_request() {
                                terminal::compose_in_editor(tui)?;
                            }
                        }
                    }
                }
                CtEvent::Paste(s) => {
                    dirty = true;
                    tui.app.prompt_state.paste(&s);
                }
                CtEvent::Mouse(m) => {
                    dirty = true;
                    tui.app.mouse(m);
                }
                CtEvent::Resize(_, _) => {
                    dirty = true;
                }
                _ => {}
            }
        }
        // A `TAB`, or a focused agent ending mid-drain, moves the banner and
        // ctx% gauge onto the newly focused provider.  Purely presentational:
        // nothing agent-side reads focus, so there is nothing else to wake.
        let now_focus = tui.app.tabs.focused();
        if now_focus != prev_focus {
            dirty = true;
            if let Some(ph) = ctx.agents.provider(now_focus) {
                tui.app.update_live_model(&ph.current());
            }
        }
    }
}

/// Whether `k` is `c` pressed with the Control modifier.
pub fn ctrl_key(k: &KeyEvent, c: char) -> bool {
    k.code == KeyCode::Char(c) && k.modifiers.contains(KeyModifiers::CONTROL)
}

/// What one overlay poll tick resolved to.  `model_picker`'s `drive_picker`
/// and `login`'s `drive_login` both run their modal on this tick, so their
/// cancel chord and release filtering stay identical.
pub(super) enum OverlayTick {
    Idle,
    Key(KeyCode),
    /// Ctrl-C, Ctrl-D, or Esc: every overlay's one cancel chord.
    Cancel,
    TerminalLost,
}

/// Redraw `tui.app` and poll up to 100ms for the overlay's next live key.
pub(super) fn overlay_tick(tui: &mut Tui) -> OverlayTick {
    if draw(&mut tui.app, tui.guard.term()).is_err() {
        return OverlayTick::TerminalLost;
    }
    if !ct_poll(Duration::from_millis(100)).unwrap_or(false) {
        return OverlayTick::Idle;
    }
    let Ok(CtEvent::Key(k)) = ct_read() else {
        return OverlayTick::Idle;
    };
    if k.kind != KeyEventKind::Press {
        return OverlayTick::Idle;
    }
    if ctrl_key(&k, 'c') || ctrl_key(&k, 'd') || k.code == KeyCode::Esc {
        return OverlayTick::Cancel;
    }
    OverlayTick::Key(k.code)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyAction {
    Edit,
    Submit,
    Cancel,
}

/// Classify one key press in the running UI loop: Ctrl-C and Esc cancel, a
/// bare Enter submits when the focused tab is steerable, everything else
/// edits.  The modal overlays bypass this and read their chord from
/// [`overlay_tick`].
pub fn key_action(k: &KeyEvent, enter_submits: bool) -> KeyAction {
    if ctrl_key(k, 'c') {
        return KeyAction::Cancel;
    }
    if k.code == KeyCode::Esc {
        return KeyAction::Cancel;
    }
    if enter_submits
        && k.code == KeyCode::Enter
        && !k.modifiers.contains(KeyModifiers::SHIFT)
        && !k.modifiers.contains(KeyModifiers::ALT)
    {
        KeyAction::Submit
    } else {
        KeyAction::Edit
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::record::{Forensic, Record};

    /// `/resources` travels the `/clear` route — inbox, exchange boundary,
    /// [`ReplControl`] — and folds into exactly one event with no provider
    /// round-trip.
    #[test]
    fn resources_command_routes_through_attend_and_emits_once() {
        let mut session = Agent::for_test("system").unwrap();
        session
            .mailbox()
            .push(Post::Command("/resources".into()))
            .unwrap();

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.mailbox());
        let mut control = ReplControl;
        let _ = session.attend(&mut control, &emit);

        let sig = rx
            .try_recv()
            .expect("the /resources command must publish its fold");
        match sig {
            crate::bus::Signal::Transient(
                _,
                crate::record::Transient::Resources { rows, card },
            ) => {
                assert!(!rows.is_empty(), "the agent half of the fold has rows");
                assert!(
                    rows.iter().any(|r| r.name == "workers.running"),
                    "the registry chapter is surveyed"
                );
                assert_eq!(card.marks().len(), 2, "a heading and one matrix");
            }
            _ => panic!("expected Transient::Resources"),
        }
        // The park the loop settles into announces itself, and nothing else
        // follows: a command is not a turn, so no state ran before the fold.
        assert!(
            matches!(
                rx.try_recv(),
                Ok(crate::bus::Signal::Transient(
                    _,
                    crate::record::Transient::State(crate::bus::AgentState::Ready)
                ))
            ),
            "the fold is followed by the ready-boundary state alone"
        );
        assert!(
            rx.try_recv().is_err(),
            "one /resources command, exactly one fold"
        );
    }

    #[test]
    fn rewind_command_with_argument_reaches_attend_control() {
        let mut session = Agent::for_test("system").unwrap();
        session
            .mailbox()
            .push(Post::Command("/rewind 7".into()))
            .unwrap();

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.mailbox());
        let mut control = ReplControl;
        let _ = session.attend(&mut control, &emit);

        assert!(
            crate::bus::drain_records(&rx).iter().any(|rec| matches!(
                rec,
                Record::Forensic(Forensic::Error { text }) if text.contains("exchange 7")
            )),
            "a /rewind past the last exchange reports the bad anchor"
        );
    }
}
