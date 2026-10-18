//! The REPL: the terminal guard, every root's worker thread, and the merged
//! render+input loop the UI thread runs beside them.

use std::{
    io::{self},
    sync::Arc,
    time::{Duration, Instant},
};

use crossterm::event::{
    Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, poll as ct_poll,
    read as ct_read,
};

use crate::{
    SessionInfo,
    agent::{
        Avatar,
        fleet::Fleet,
        spawn::{AsyncSpawn, spawn_async},
    },
    bus::{BusReceiver, FleetBus, Pass, Signal},
    provider::{Bureau, Provider},
    record::Resumed,
};
use std::sync::mpsc::TryRecvError;

use super::{
    App, banner, commands,
    render::draw,
    terminal::{self, TerminalGuard},
};

/// Terminal and app, kept as two fields so the loop can borrow `guard.term()`
/// and `&mut app` at once.
pub(super) struct Tui {
    pub(super) guard: TerminalGuard,
    pub(super) app: App,
}

/// Attend every root, the `seed` prompt going to the first, until the last
/// ends.
///
/// Each root runs on its own worker thread beside the UI loop; afterwards the
/// logs are flushed and the paths and usage printed on the restored shell.
///
/// # Errors
/// Returns `Err` if a root cannot be started, or if the UI render/input loop
/// hits a fatal terminal error.
pub fn run(
    screen: TerminalGuard,
    roots: Vec<(Avatar, Option<Resumed>)>,
    provider: &Arc<Provider>,
    info: &SessionInfo<'_>,
    bureau: &Bureau,
    mut seed: Option<String>,
    vi: bool,
) -> Result<(), String> {
    let fleet = Arc::clone(&roots.first().ok_or("there is no session to open")?.0.fleet);
    // A *session*-lived bus, not per-exchange: a root's workers keep streaming
    // to their tabs after the exchange that spawned them ends.
    let bus = FleetBus::session();
    let _signals = crate::signals::face(&fleet);
    for (root, resumed) in roots {
        let emit = bus.emitter(root.agent.id, root.agent.mailbox.clone());
        let spec = AsyncSpawn {
            name: root.agent.name.clone(),
            prompt: seed.take(),
            resumed,
        };
        spawn_async(root, spec, &emit)?;
    }
    // The first root's birth went out as the loop above minted its emitter,
    // before any thread could send: the app exists only once there is a tab
    // to focus.
    let Signal::Born(id, born) = bus.rx().recv().map_err(|e| e.to_string())? else {
        return Err(
            "the first signal of a run was not a root's birth: there is no tab to open".into(),
        );
    };
    let mut tui = Tui {
        guard: screen,
        app: App::new(
            id,
            &born,
            Arc::clone(&fleet),
            bus.meter(),
            vi,
            banner::session_card(info),
        ),
    };
    tui.app.update_live_model(provider, &bureau.available());

    let ctx = CommandCtx { bureau, info };
    let r = ui_loop(&mut tui, &bus, &fleet, &ctx);
    if r.is_err() {
        commands::quit(&fleet);
    }
    // Every root's teardown writes its log's last records.
    fleet.wait_empty();
    r.map_err(|e| e.to_string())?;

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
            if let Some(dir) = p.parent() {
                ral_core::outln!("Agent logs: {} (user.log + record.jsonl)", dir.display());
            } else {
                ral_core::outln!("Agent log: {}", p.display());
            }
        }
    } else if let Err(e) = logs {
        ral_core::errln!("exarch: {e}");
    }
    ral_core::outln!("{usage}");
    Ok(())
}

/// The handles a submitted line is serviced against, bundled so the command
/// path — `route_submit` into `pick_model` / `login` — threads one context.
/// Every agent-side reach is the tabs' own, so no fleet handle rides here.
pub struct CommandCtx<'a> {
    pub(super) bureau: &'a Bureau,
    pub(super) info: &'a SessionInfo<'a>,
}

/// Route one `Signal` to its `App` entry point.
fn dispatch(tui: &mut Tui, rx: &BusReceiver, sig: Signal) {
    match sig {
        Signal::Fact(id, segment, fact) => tui.app.fact(id, segment, &fact),
        Signal::Born(id, born) => tui.app.born(id, &born),
        Signal::Transient(id, t) => tui.app.transient(id, t, rx),
    }
}

/// The drain contract — the fleet's emptiness, read *before* the pass, ends it,
/// never the channel merely emptying — over the TUI's own dispatch loop, on its
/// render cadence rather than a `Sink`'s blocking one.
fn drain_signals(
    rx: &BusReceiver,
    finished: bool,
    max: Option<usize>,
    mut handle: impl FnMut(Signal),
) -> Pass {
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

/// The merged render + input loop, on the UI thread beside the roots' workers.
/// Returns once the fleet is empty, after one last frame that includes
/// everything they emitted.
fn ui_loop(tui: &mut Tui, bus: &FleetBus, fleet: &Fleet, ctx: &CommandCtx<'_>) -> io::Result<()> {
    const BATCH: usize = 64;
    let frame = Duration::from_millis(16); // ~60 FPS max
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
        // stop only when every root is *done*, never when the channel empties,
        // so a background agent flooding the bus cannot end the loop
        // early.  The cap bounds how long that flood starves the input poll.
        let mut handled_any = false;
        let more = match drain_signals(rx, fleet.is_empty(), Some(BATCH), |sig| {
            handled_any = true;
            dispatch(tui, rx, sig);
        }) {
            Pass::Stop => {
                // The cap binds even a finished drain, so `Stop` can arrive with
                // events still buffered; nothing drains after this returns, so
                // empty the channel uncapped before the final frame.
                drain_signals(rx, true, None, |sig| dispatch(tui, rx, sig));
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
                    let steerable = tui.app.is_steerable();
                    // An open slash-command popup owns its own keys — Enter
                    // takes the highlighted command instead of submitting the
                    // line under it, Tab walks the list instead of the tabs.
                    // The interrupt is the one key no overlay is offered;
                    // everything else, Esc included, the popup may claim —
                    // Esc dismisses it.
                    let action = key_action(&k, steerable);
                    let consumed =
                        action != KeyAction::Interrupt && tui.app.prompt_state.menu_key(k.code);
                    // Matrix navigation is modal: while it owns the keyboard
                    // every key but the interrupt is a matrix gesture or is
                    // swallowed, so Esc leaves the surface instead of
                    // cancelling the focused turn, and nothing reaches the
                    // draft.
                    if !consumed && action != KeyAction::Interrupt && tui.app.matrix_navigating() {
                        tui.app.key(k);
                    } else {
                        match action {
                            _ if consumed => {}
                            // The focused agent cancels its token and its current
                            // dispatch scope by handle — published ahead of the
                            // engine lock, so the keypress cannot land on a run
                            // that just ended — and never its durable root: the
                            // exchange dies, its descendants do not, and the
                            // agent lives to take a next run.
                            KeyAction::Cancel | KeyAction::Interrupt => {
                                if let Some(agent) = tui.app.tabs.focused_agent() {
                                    agent.interrupt();
                                }
                            }
                            KeyAction::Submit => {
                                if let Some(text) = tui.app.prompt_state.submit() {
                                    // Every tab funnels through the one path,
                                    // which parses before it forks on the
                                    // focused tab — so no slash line can be
                                    // mailed to the model.
                                    commands::route_submit(text, tui, ctx);
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
                }
                CtEvent::Paste(s) => {
                    dirty = true;
                    // A matrix cursor is a modal surface: pasted bytes must
                    // never leak into the draft underneath it.
                    if !tui.app.matrix_navigating() {
                        tui.app.prompt_state.paste(&s);
                    }
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
        // A matrix Enter, or a focused agent ending mid-drain, moves the banner
        // and ctx% gauge onto the newly focused provider. Purely presentational:
        // nothing agent-side reads focus, so there is nothing else to wake.
        let now_focus = tui.app.tabs.focused();
        if now_focus != prev_focus {
            dirty = true;
            if let Some(agent) = tui.app.tabs.agent(now_focus) {
                tui.app
                    .update_live_model(&agent.current_provider(), &ctx.bureau.available());
            }
        }
    }
}

/// Whether `k` is `c` pressed with the Control modifier.
pub fn ctrl_key(k: &KeyEvent, c: char) -> bool {
    k.code == KeyCode::Char(c) && k.modifiers.contains(KeyModifiers::CONTROL)
}

/// What one overlay poll tick resolved to.  `model_picker`'s `drive`
/// and `login`'s `drive_login` both run their modal on this tick, so their
/// cancel chord and release filtering stay identical.
pub(super) enum OverlayTick {
    Idle,
    Key(KeyCode),
    /// Ctrl-C, Ctrl-D, or Esc: every overlay's one cancel chord.
    Cancel,
    /// Ctrl-S: the save chord of an overlay that holds a form.
    Save,
    /// Bracketed paste; only an overlay with a text field reads it.
    Paste(String),
    TerminalLost,
}

/// Redraw `tui.app` and poll up to 100ms for the overlay's next live key.
pub(super) fn overlay_tick(tui: &mut Tui) -> OverlayTick {
    if draw(&mut tui.app, tui.guard.term()).is_err() {
        return OverlayTick::TerminalLost;
    }
    overlay_key()
}

/// Poll up to 100ms for an overlay's next live key, the cancel chord resolved.
pub(super) fn overlay_key() -> OverlayTick {
    if !ct_poll(Duration::from_millis(100)).unwrap_or(false) {
        return OverlayTick::Idle;
    }
    let k = match ct_read() {
        Ok(CtEvent::Key(k)) if k.kind == KeyEventKind::Press => k,
        Ok(CtEvent::Paste(text)) => return OverlayTick::Paste(text),
        _ => return OverlayTick::Idle,
    };
    if ctrl_key(&k, 'c') || ctrl_key(&k, 'd') || k.code == KeyCode::Esc {
        return OverlayTick::Cancel;
    }
    if ctrl_key(&k, 's') {
        return OverlayTick::Save;
    }
    OverlayTick::Key(k.code)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyAction {
    Edit,
    Submit,
    /// Esc — cancels, but an open overlay may claim it first to dismiss itself.
    Cancel,
    /// Ctrl-C — the interrupt, offered to no overlay: it outranks every one.
    Interrupt,
}

/// Classify one key press in the running UI loop: Ctrl-C interrupts, Esc
/// cancels, a bare Enter submits when the focused tab is steerable, everything
/// else edits.  The modal overlays bypass this and read their chord from
/// [`overlay_tick`].
pub(super) fn key_action(k: &KeyEvent, enter_submits: bool) -> KeyAction {
    if ctrl_key(k, 'c') {
        return KeyAction::Interrupt;
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
