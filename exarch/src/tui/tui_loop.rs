//! REPL / UI loop orchestration — the main entry-point, the terminal guard
//! wrapper, the worker thread spawn, the merged render+input loop, and the
//! key-classification helpers.

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
    agent::{Agent, Control, ControlFlow},
    agent_registry::AgentRegistry,
    bootstrap::Scratch,
    bus::{AgentId, Emitter, FleetBus, InboxMsg, Pass, drain_pass},
    cancel,
    credential::CredentialStore,
    fleet::Fleet,
    models::{LiveSource, ModelCatalog},
    oauth,
    provider::{self, Provider},
};

use super::banner::SessionInfo;
use super::{
    App, banner, commands,
    render::draw,
    terminal::{self, TerminalGuard},
};

/// Pairs the terminal lifetime with the app so the worker thread and the UI
/// loop can split the two: the worker borrows the session through
/// [`App::handle`]'s bus, the UI loop borrows `guard.term()` alongside
/// `&mut self.app` via direct field syntax for disjoint-borrow splitting.
pub(super) struct Tui {
    pub(super) guard: TerminalGuard,
    pub(super) app: App,
}

impl Tui {
    pub fn new(
        root_id: AgentId,
        root_log_dir: &Path,
        context_window: Option<u64>,
        stderr_log: &Path,
        vi: bool,
    ) -> io::Result<Self> {
        let guard = TerminalGuard::enter(stderr_log)?;
        let app = App::new(root_id, root_log_dir, context_window, vi);
        Ok(Self { guard, app })
    }
}

/// One row of the slash-command registry: the canonical token, any aliases,
/// the argument it consumes (if any), and a one-line description for `/help`.
/// The table is metadata only — names, help, and the argument shape; dispatch
/// is a direct match by name in [`route_submit`], split by where the work must
/// run (the UI thread or the session's drive loop).
/// The agent-affecting slash command hook the worker's [`Agent::drive`]
/// calls at the turn boundary, where the drive thread owns the agent the
/// command mutates.  `/clear` rebuilds the agent's context (its viewport was
/// already cleared UI-side), `/compact` summarizes the history, `/resources`
/// surveys the agent's accumulators into one probe card, `/discuss` forks a
/// returning chair agent, and `/quit` ends the drive loop — which sets
/// `done`, so the UI loop's next drain returns `Stop` and exits.  Every other
/// command is handled UI-side and never reaches here.  Only the trunk drives
/// with this `Control` (a sub-agent uses [`NoControl`](crate::agent::NoControl)),
/// so a slash command always targets the trunk's own context and provider.
pub struct ReplControl<'a> {
    scratch: &'a Scratch,
}

impl Control for ReplControl<'_> {
    fn command(&mut self, raw: &str, session: &mut Agent, emit: &Emitter) -> ControlFlow {
        let trimmed = raw.trim();
        let (head, rest) = trimmed
            .split_once(char::is_whitespace)
            .map_or((trimmed, ""), |(h, r)| (h, r.trim()));
        if head == "/discuss" {
            let topic = rest;
            if topic.is_empty() {
                session.note_error("usage: /discuss <prompt>".into(), emit);
            } else if session.fuel() < 2 {
                // The chair needs one unit to be born and a second to spawn
                // its partner; below that the chair would seat with no
                // `amnemon` in its view and the debate could never start.
                session.note_error(
                    "discuss needs a chair and a partner — this agent's spawn \
budget is too low to seat both"
                        .into(),
                    emit,
                );
            } else {
                let receipt = crate::tools::spawn_discussion(session, topic, emit);
                session.note(format!("discussion started: {receipt}"), emit);
            }
            return ControlFlow::Continue;
        }
        if head == "/branch" {
            let prompt = (!rest.is_empty()).then_some(rest);
            let receipt = crate::tools::spawn_branch(session, prompt, emit);
            session.note(format!("branch started: {receipt}"), emit);
            return ControlFlow::Continue;
        }
        match trimmed {
            "/clear" => {
                let _ = session.clear(self.scratch);
                ControlFlow::Continue
            }
            "/compact" => {
                let p = session.current_provider();
                let token = session.cancel_token().clone();
                session.compact(&p, emit, true, &token);
                ControlFlow::Continue
            }
            // The probe fold: assembled here, on the drive thread that owns
            // the shell the rows survey, and emitted as one bus event the
            // frontend renders (appending its own rows) — never a model
            // turn.
            "/resources" => {
                session.emit_resources(emit);
                ControlFlow::Continue
            }
            "/quit" | "/exit" => ControlFlow::Quit,
            _ => ControlFlow::Continue,
        }
    }
}

/// Build the [`Tui`], banner, run the worker + UI loop, flush logs, print log
/// paths + usage on the restored shell.
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
    provider: Arc<Provider>,
    info: &banner::SessionInfo<'_>,
    store: &CredentialStore,
    catalog: &mut ModelCatalog<LiveSource>,
    scratch: &Scratch,
    run_dir: &Path,
    seed: Option<String>,
    vi: bool,
    engine: Arc<provider::Engine>,
) -> Result<(), String> {
    let caps = provider::caps_for(provider.model());
    let stderr_log = run_dir.join("stderr.log");
    let mut tui = Tui::new(
        session.id,
        session.log_dir(),
        caps.context_window,
        &stderr_log,
        vi,
    )
    .map_err(|e| format!("ratatui init: {e}"))?;
    let status_provider = oauth::provider_label(provider.subscription(), provider.id().label());
    tui.app.update_live_model(&provider, &status_provider);
    // Bind the App's inbox and focus to the trunk's shared handles, then build
    // the fleet: a session-lived bus over the trunk's inbox, plus the shared
    // registry and focus handle.  Input, the queued-user strip, async-agent
    // results, and the worker's drive loop all read and write this one inbox;
    // `TAB` and the focused agent's park predicate share one focus handle.
    tui.app.bind_inbox(session.inbox());
    tui.app.tabs.bind_focus(session.focus_handle());
    let fleet = Fleet::new(
        session.agents.clone(),
        FleetBus::session(session.inbox()),
        session.focus_handle(),
        session.interactive(),
        engine,
    );
    if let Some(s) = seed {
        session.seed(s);
    }
    tui.app
        .banner(tui.guard.term(), info, &provider)
        .map_err(|e| e.to_string())?;

    // The worker thread runs the trunk via `Agent::drive`, parking on an empty
    // inbox (the conversing trunk) until a `/quit` command tells its `Control`
    // to quit; it then sets `done`, and the UI loop's next drain returns
    // `Stop`. The UI loop renders the bus and routes input in one continuous
    // loop alongside it.  The trunk drives on its own provider handle.
    let done = AtomicBool::new(false);
    let done_ref = &done;
    let mut control = ReplControl { scratch };
    // The worker captures the trunk's emitter, not `&fleet.bus`: `FleetBus` is
    // not `Sync` (its `Receiver` is single-consumer), so the receiver stays on
    // the UI thread. The emitter is `Send` and is all the worker needs.  It
    // carries the trunk's `Transcript`, so the TUI records `transcript.jsonl`
    // too — the operational view beside `user.log`'s rendered one.
    let worker_emit = fleet.bus.emitter(session.id, session.transcript());
    // A recording emitter for the UI thread, minted from the bus *before* the
    // worker takes the trunk: it carries the trunk's `transcript()`, so a
    // UI-caused operational event — a `/model` switch — records in the trace
    // and draws through the normal bus path, exactly as a worker-raised note
    // does.  The worker takes `worker_emit`; this clone stays on the UI thread.
    let ui_emit = fleet.bus.emitter(session.id, session.transcript());
    // A `Mailbox` onto the trunk inbox, so a UI-loop failure can wake the
    // parked worker with a `/quit` before joining — without it the conversing
    // trunk parks forever and `join` would deadlock.
    let quit_mailbox = session.inbox().mailbox();
    // The UI thread's command context: the handles `route_submit` and the
    // `/model` path service a submitted line against, threaded as one.  The
    // registry is the same shared map the worker mutates, so an agent it
    // registers is visible to the UI at once — for steering, `wake`, and a
    // `/model` swap on the focused agent's handle.
    let mut cmd_ctx = CommandCtx {
        agents: &fleet.agents,
        store,
        catalog,
        info,
        emit: &ui_emit,
        engine: fleet.engine(),
    };
    std::thread::scope(|scope| -> Result<(), String> {
        let worker = std::thread::Builder::new()
            .stack_size(8 * 1024 * 1024)
            .spawn_scoped(scope, move || {
                let out = session.drive(&mut control, &worker_emit);
                done_ref.store(true, Ordering::Release);
                out
            })
            .expect("spawn agent worker");

        let r = ui_loop(&mut tui, &fleet.bus, done_ref, &mut cmd_ctx);
        if r.is_err() {
            // Best-effort unstick on the UI loop's own fatal error: the
            // process is already unwinding to report `r`'s error, so a
            // rejected `/quit` here (the inbox somehow at quota) has nowhere
            // more specific left to report to than the fatal error already
            // in flight; worst case the join below waits on a parked worker.
            let _ = quit_mailbox.push(InboxMsg::Command("/quit".into()));
        }
        let _ = worker.join();
        r.map_err(|e| e.to_string())
    })?;

    let logs = tui
        .app
        .flush_logs()
        .map_err(|e| format!("session logs: {e}"));
    let usage = tui.app.total_usage();
    // Restore the terminal before printing so log paths land on the
    // user's normal shell rather than the alt screen.
    drop(tui);
    if let Ok(paths) = &logs {
        for p in paths {
            match p.parent() {
                Some(dir) => println!("Agent logs: {} (user.log + events.json)", dir.display()),
                None => println!("Agent log: {}", p.display()),
            }
        }
    } else if let Err(e) = logs {
        eprintln!("exarch: {e}");
    }
    println!("{usage}");
    Ok(())
}

/// The long-lived handles the UI thread services a submitted line against: the
/// fleet registry (for steering, `wake`, and the focused agent's provider
/// handle a `/model` swap targets), the credential store and model catalog the
/// `/model` picker reads, the static session info, and the recording emitter a
/// UI-caused operational event (a model switch) rides.  Bundled so the command
/// path — `ui_loop` → `route_submit` → `pick_model` → `apply_model_switch` —
/// threads one context rather than a fistful of handles.
pub struct CommandCtx<'a> {
    pub(super) agents: &'a AgentRegistry,
    pub(super) store: &'a CredentialStore,
    pub(super) catalog: &'a mut ModelCatalog<LiveSource>,
    pub(super) info: &'a SessionInfo<'a>,
    pub(super) emit: &'a Emitter,
    pub(super) engine: &'a Arc<provider::Engine>,
}

/// The merged render + input loop, running on the UI thread alongside the
/// worker's [`Agent::drive`].  It drains the session-lived bus into the App
/// (the same `App::handle` the old per-turn drive used), ticks and redraws at
/// ~60 FPS, and routes the user's keystrokes: scrollback / picker keys edit the
/// App, a submitted line is routed by [`route_submit`] (view commands run here;
/// agent commands and plain prompts go onto the focused agent's inbox), and Esc
/// / Ctrl-C interrupt the focused tab's current turn (never a cascade, never a
/// kill).  A `TAB` that moves
/// focus `wake`s the de-focused and newly-focused agents so each re-evaluates
/// its park verdict.  Returns when the worker finishes (a `/quit`), draining its
/// final events for one last frame.
fn ui_loop(
    tui: &mut Tui,
    bus: &FleetBus,
    done: &AtomicBool,
    ctx: &mut CommandCtx<'_>,
) -> io::Result<()> {
    const BATCH: usize = 64;
    let frame = Duration::from_millis(16); // ~60 FPS max
    // The session inbox, so a routed line (a plain prompt, a session command)
    // reaches the worker's drive loop through the queue the App is bound to.
    let mailbox = tui.app.inbox.mailbox();
    let rx = bus.rx();
    // The frame clock: the instant the last frame was painted, seeded a frame
    // in the past so the first iteration paints at once.  Draws are gated on it
    // so the redraw rate is bounded by the frame interval independently of how
    // fast events drain — a token/tool flood coalesces into one coherent frame
    // per interval instead of a full-screen rewrite per 64-event batch (the
    // jitter that churn caused).
    let mut last_draw = Instant::now().checked_sub(frame).unwrap();
    // Whether the next due frame must actually repaint. Set below by
    // anything the frame can show that isn't already covered by `animating`:
    // a drained bus event, a consumed keystroke, a focus change, or a probe
    // flip. Seeded true so the first frame always paints.
    let mut dirty = true;
    // Sampled once per iteration from the focused tab's mailbox: flips when
    // that agent's drive loop parks or unparks, which repaints the tab title
    // and prompt chrome but raises no bus event of its own. A tab with no live
    // agent has no queue to be busy on, so it reads as idle (waiting).
    let focused_waiting = |ctx: &CommandCtx<'_>, focused| {
        ctx.agents
            .mailbox(focused)
            .is_none_or(|mb| mb.waiting_for_input())
    };
    let mut waiting_for_input = focused_waiting(ctx, tui.app.tabs.focused());
    loop {
        // Focus as of the start of this iteration; compared at the end so a
        // `TAB`, or a focused agent ending mid-drain, wakes the agents whose
        // park verdict just changed.
        let prev_focus = tui.app.tabs.focused();
        // The explicit-done completion contract (shared with the headless
        // `Sink::drive`): drain a batch, then stop only when the worker is
        // *done* — never when the channel empties or disconnects, so a detached
        // worker (a live background `agent`) flooding the bus cannot end the
        // loop early. The batch cap bounds how long a token flood can starve the
        // input poll below; `More` means events are still queued, so the frame
        // does not wait for one.
        let mut handled_any = false;
        let more = match drain_pass(rx, done, Some(BATCH), |ev| {
            handled_any = true;
            tui.app.handle(ev, rx);
        }) {
            Pass::Stop => {
                // The capped pass can report `Stop` with events still buffered
                // (the batch cap binds even a `done` drain); there is no
                // drainer after this loop returns, so empty the channel with
                // one uncapped pass before painting the frame the user sees
                // last — it must include everything the worker emitted.
                // `done` is already latched, so this pass drains to empty and
                // reports `Stop` again; its verdict is not needed.
                drain_pass(rx, done, None, |ev| tui.app.handle(ev, rx));
                tui.app.busy_off();
                let focused = tui.app.tabs.focused();
                let steerable =
                    focused == tui.app.tabs.root() || ctx.agents.mailbox(focused).is_some();
                tui.app.tabs.set_steerable(steerable);
                draw(&mut tui.app, tui.guard.term())?;
                return Ok(());
            }
            Pass::More => true,
            Pass::Idle => false,
        };
        dirty |= handled_any;
        let now_waiting = focused_waiting(ctx, tui.app.tabs.focused());
        dirty |= now_waiting != waiting_for_input;
        waiting_for_input = now_waiting;
        // Hand the sole sample to `App` so `animating`'s spinner test follows
        // the focused tab without re-reading the flag.
        tui.app.set_focused_waiting(now_waiting);
        // Paint only when a frame is due, so a multi-batch backlog still drains
        // at full throughput but redraws at most once per interval.  `tick`
        // always runs on the due frame, painted or not: it ages dying tabs out
        // on its own clock, independent of anything that gates the redraw.
        if last_draw.elapsed() >= frame {
            let ticked = tui.app.tabs.tick();
            let animating = tui.app.animating(frame);
            if dirty || ticked || animating {
                let focused = tui.app.tabs.focused();
                let steerable =
                    focused == tui.app.tabs.root() || ctx.agents.mailbox(focused).is_some();
                tui.app.tabs.set_steerable(steerable);
                draw(&mut tui.app, tui.guard.term())?;
                dirty = false;
            }
            // A skipped frame still advances the clock: the poll timeout below
            // is `frame - last_draw.elapsed()`, so a stale `last_draw` would
            // floor that at zero and spin the input poll on every iteration.
            last_draw = Instant::now();
        }
        // Poll for input every iteration, even with events still queued: a
        // backlog of streamed tokens must never starve Esc/Ctrl-C. While the
        // drain is incomplete the poll is non-blocking so draining stays prompt;
        // once the channel is empty it waits only until the next frame is due,
        // which both paces the idle loop and keeps Esc/Ctrl-C responsive.
        let timeout = if more {
            Duration::ZERO
        } else {
            frame.saturating_sub(last_draw.elapsed())
        };
        if ct_poll(timeout)? {
            match ct_read()? {
                CtEvent::Key(k) if k.kind == KeyEventKind::Press => {
                    dirty = true;
                    // A tab is steerable when it is root (slash commands and
                    // prompts) or a live peer with a registered inbox; on a
                    // steerable tab Enter submits and text entry is allowed.
                    let focused = tui.app.tabs.focused();
                    let steerable =
                        focused == tui.app.tabs.root() || ctx.agents.mailbox(focused).is_some();
                    tui.app.tabs.set_steerable(steerable);
                    match key_action(KeyMode::Running, &k, steerable) {
                        // Esc / Ctrl-C interrupt the *focused* tab's current
                        // turn — never a cascade, never a kill.  On the trunk
                        // `raise_interrupt()` unwinds the trunk's own turn via
                        // the published slot and the ral foreground.  On any
                        // other focused tab `interrupt(id)` unwinds that agent's
                        // turn alone through its registered token and eval-root;
                        // a sub-agent never publishes the slots, so the
                        // slot/foreground path would target the trunk by
                        // mistake.  Neither reaches descendants, and neither
                        // ends the agent — lifecycle death stays with `/quit`,
                        // `/clear`, the ceiling, and `agent_cancel`.
                        KeyAction::Cancel => {
                            if focused == tui.app.tabs.root() {
                                cancel::raise_interrupt();
                            } else {
                                ctx.agents.interrupt(focused);
                            }
                        }
                        KeyAction::Submit => {
                            if let Some(text) = tui.app.prompt_state.submit() {
                                // Every tab funnels through the one submit path;
                                // it owns the parse-once decision and targets the
                                // focused tab, so there is no root/non-root fork
                                // here that could mail a slash line to the model.
                                commands::route_submit(text, tui, &mailbox, ctx)?;
                            }
                        }
                        KeyAction::Edit => {
                            tui.app.key(k, steerable);
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
                _ => {}
            }
        }
        // A focus change this iteration (a `TAB`, or a focused agent ending)
        // wakes both the de-focused and newly-focused agents, so each
        // re-evaluates its park verdict: the de-focused, unscheduled, idle one
        // flips to `Quiesce` and reaps; the newly-focused one stays `Held`.
        let now_focus = tui.app.tabs.focused();
        if now_focus != prev_focus {
            dirty = true;
            if let Some(mb) = ctx.agents.mailbox(prev_focus) {
                mb.wake();
            }
            if let Some(mb) = ctx.agents.mailbox(now_focus) {
                mb.wake();
            }
            // Update the live model chrome to reflect the newly focused
            // agent'\''s provider — the banner, status bar, and ctx% gauge
            // must follow focus.
            if let Some(ph) = ctx.agents.provider(now_focus) {
                let p = ph.current();
                let status_provider = oauth::provider_label(p.subscription(), p.id().label());
                tui.app.update_live_model(&p, &status_provider);
            }
        }
    }
}

/// Open the `/model` picker over the available providers, fetch their model
/// lists (cache-first, then background), and drive the modal loop until the
/// user selects a model or dismisses it. On a selection the provider is rebuilt
/// over the same transcript, the [`ProviderHandle`] is swapped (taking effect on
/// the worker's next turn), the saved selection is updated, and the status bar
/// follows.
pub fn ctrl_key(k: &KeyEvent, c: char) -> bool {
    k.code == KeyCode::Char(c) && k.modifiers.contains(KeyModifiers::CONTROL)
}

/// The two live input contexts: the running UI loop (the worker drives the
/// whole session, so the prompt is never an idle read) and the modal `/model`
/// picker overlay.  There is no idle mode — an interactive root's worker parks
/// in [`Agent::drive`] rather than returning, so the session ends through
/// `/quit`, never a keystroke.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KeyMode {
    Running,
    Overlay,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyAction {
    Edit,
    Submit,
    Cancel,
}

pub fn key_action(mode: KeyMode, k: &KeyEvent, enter_submits: bool) -> KeyAction {
    if ctrl_key(k, 'c') {
        return KeyAction::Cancel;
    }
    if ctrl_key(k, 'd') {
        return match mode {
            KeyMode::Overlay => KeyAction::Cancel,
            KeyMode::Running => KeyAction::Edit,
        };
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
    use crate::bus::Kind;

    /// `/resources` routes exactly as `/clear` does: posted to the inbox as
    /// an `InboxMsg::Command`, drained at the turn boundary, and handled by
    /// [`ReplControl`] against the agent the drive loop owns — which
    /// assembles its probe rows and emits exactly one [`Kind::Resources`],
    /// recorded by the transcript as a `resources` line, with no
    /// model-facing side effect (the drive quiesces without a provider
    /// round-trip).
    #[test]
    fn resources_command_routes_through_drive_and_emits_once() {
        let dir =
            std::env::temp_dir().join(format!("exarch-resources-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut session = Agent::for_test(&dir, "system").unwrap();
        session
            .mailbox()
            .push(InboxMsg::Command("/resources".into()))
            .unwrap();

        let (tx, rx) = crate::bus::channel();
        let emit = Emitter::with_mailbox(tx, session.id, session.mailbox());
        let scratch = Scratch::for_test("resources-route").expect("scratch dir");
        let mut control = ReplControl { scratch: &scratch };
        let _ = session.drive(&mut control, &emit);

        let event = rx
            .try_recv()
            .expect("the /resources command must emit its fold");
        match &event.kind {
            Kind::Resources { rows, card } => {
                assert!(!rows.is_empty(), "the agent half of the fold has rows");
                assert!(
                    rows.iter().any(|r| r.name == "workers.running"),
                    "the registry chapter is surveyed"
                );
                assert_eq!(card.marks().len(), 2, "a heading and one matrix");
                // The transcript records the rows as a `resources` line —
                // the raw-fact half of the raw/rendering pairing.
                let rec = crate::transcript::event_record(0, session.id, &event.kind)
                    .expect("a resources event must reach the transcript");
                assert_eq!(rec["kind"], "resources");
                assert!(rec["rows"].is_array());
            }
            _ => panic!("expected Kind::Resources"),
        }
        assert!(
            rx.try_recv().is_err(),
            "one /resources command, exactly one event"
        );
    }
}
