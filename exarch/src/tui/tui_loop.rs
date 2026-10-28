//! The REPL: the terminal guard, every root's worker thread, and the merged
//! render+input loop the UI thread runs beside them.

use std::{
    io::{self},
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

use crossterm::event::{Event as CtEvent, KeyEventKind, poll as ct_poll, read as ct_read};

use crate::{
    SessionInfo,
    agent::{
        Agent,
        fleet::Fleet,
        spawn::{AsyncSpawn, spawn_async},
    },
    bus::{BusReceiver, FleetBus, Pass, Signal},
    provider::Bureau,
    record::{AgentId, RECORD, Resumed, Transient},
};
use std::sync::mpsc::TryRecvError;

use super::{
    App,
    app::{CommandCtx, Job, Overlay},
    banner, commands,
    paste_burst::{Input, PasteBurst},
    prompt::{History, PromptState},
    render::{draw, emit_tab_title},
    terminal::{TerminalGuard, edit_text_in_editor},
};

/// Terminal and app, kept as two fields so the loop can borrow `guard.term()`
/// and `&mut app` at once.
struct Tui {
    guard: TerminalGuard,
    app: App,
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
    roots: Vec<(Agent, Option<Resumed>)>,
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
        let emit = bus.emitter(root.id());
        let spec = AsyncSpawn {
            name: root.handle().0.name.clone(),
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
    let (jobs, inbox) = mpsc::channel::<Job>();
    let (history, unshared) = match History::open() {
        Ok(history) => (history, None),
        Err(e) => (History::default(), Some(e)),
    };
    let mut tui = Tui {
        guard: screen,
        app: App::new(
            id,
            &born,
            Arc::clone(&fleet),
            bus.meter(),
            PromptState::new(vi, history),
            banner::session_card(info),
            jobs,
        ),
    };
    if let Some(e) = unshared {
        tui.app.push_error(
            id,
            &format!("{e}; this session's prompts will not be saved"),
        );
    }

    let ctx = CommandCtx { bureau, info };
    let r = ui_loop(&mut tui, &bus, &fleet, &inbox, &ctx);
    if r.is_err() {
        commands::quit(&fleet);
    }
    // Every root's teardown writes its log's last records.
    fleet.wait_empty();
    r.map_err(|e| e.to_string())?;

    let logs = tui.app.tabs.log_dirs();
    let usage = tui.app.total_usage();
    // Restore the terminal first, so the paths below land on the user's shell
    // rather than the alt screen.
    drop(tui);
    for dir in &logs {
        ral_core::outln!("Session log: {}/{RECORD}", dir);
    }
    ral_core::outln!("render one with `exarch export <dir>`");
    ral_core::outln!("{usage}");
    Ok(())
}

/// Route one `Signal` to its `App` entry point.
fn dispatch(tui: &mut Tui, rx: &BusReceiver, sig: Signal) {
    match sig {
        Signal::Fact(id, fact) => tui.app.fact(id, &fact),
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
    mut handle: impl FnMut(Signal) -> io::Result<()>,
) -> io::Result<Pass> {
    let mut n = 0usize;
    loop {
        if max.is_some_and(|m| n >= m) {
            return Ok(if finished { Pass::Stop } else { Pass::More });
        }
        match rx.try_recv() {
            Ok(sig) => {
                handle(sig)?;
                n += 1;
            }
            Err(TryRecvError::Empty) => {
                return Ok(if finished { Pass::Stop } else { Pass::Idle });
            }
            Err(TryRecvError::Disconnected) => return Ok(Pass::Stop),
        }
    }
}

/// The merged render + input loop, on the UI thread beside the roots' workers.
/// Returns once the fleet is empty, after one last frame that includes
/// everything they emitted.
fn ui_loop(
    tui: &mut Tui,
    bus: &FleetBus,
    fleet: &Fleet,
    inbox: &Receiver<Job>,
    ctx: &CommandCtx<'_>,
) -> io::Result<()> {
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
    let mut paste = PasteBurst::default();
    loop {
        dirty |= flush_due_input(tui, &mut paste, Instant::now(), ctx)?;
        let prev_focus = tui.app.tabs.focused();
        // The explicit-done contract, shared with the headless `Sink::drive`:
        // stop only when every root is *done*, never when the channel empties,
        // so a background agent flooding the bus cannot end the loop
        // early.  The cap bounds how long that flood starves the input poll.
        let mut handled_any = false;
        let more = match drain_signals(rx, fleet.is_empty(), Some(BATCH), |sig| {
            handled_any = true;
            guard_owner(tui, &mut paste, ctx, |tui, paste| {
                // Facts can retire a subtree; death can change the focused
                // owner.  Streaming transients must not split a concurrent paste.
                if matches!(
                    &sig,
                    Signal::Fact(..) | Signal::Transient(_, Transient::Died)
                ) {
                    deliver(tui, paste.flush_pending(), ctx)?;
                }
                dispatch(tui, rx, sig);
                Ok(())
            })
        })? {
            Pass::Stop => {
                // The cap binds even a finished drain, so `Stop` can arrive with
                // events still buffered; nothing drains after this returns, so
                // empty the channel uncapped before the final frame.
                deliver(tui, paste.finish(), ctx)?;
                drain_signals(rx, true, None, |sig| {
                    dispatch(tui, rx, sig);
                    Ok(())
                })?;
                draw(&mut tui.app, tui.guard.term(), ctx.bureau)?;
                emit_tab_title(&mut tui.app);
                return Ok(());
            }
            Pass::More => true,
            Pass::Idle => false,
        };
        for job in inbox.try_iter() {
            guard_owner(tui, &mut paste, ctx, |tui, paste| {
                deliver(tui, paste.flush_pending(), ctx)?;
                job(&mut tui.app, ctx);
                Ok(())
            })?;
            dirty = true;
        }
        if let Some(Overlay::Picker(s, _)) = &mut tui.app.overlay {
            s.poll(ctx.bureau);
        }
        dirty |= handled_any;
        let now_waiting = tui.app.focused_waiting();
        dirty |= now_waiting != prev_waiting;
        prev_waiting = now_waiting;
        // `tick` runs on every due frame, painted or not: it ages dying tabs
        // out on its own clock, independent of what gates the redraw.
        if last_draw.elapsed() >= frame {
            let ticked = guard_owner(tui, &mut paste, ctx, |tui, paste| {
                // Only a lingering (non-editable) focused tab can expire in tick.
                if !tui.app.is_steerable() {
                    dirty |= deliver(tui, paste.flush_pending(), ctx)?;
                }
                Ok(tui.app.tabs.tick())
            })?;
            let animating = tui.app.animating(frame);
            if dirty || ticked || animating {
                draw(&mut tui.app, tui.guard.term(), ctx.bureau)?;
                emit_tab_title(&mut tui.app);
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
        let timeout = paste.deadline().map_or(timeout, |at| {
            timeout.min(at.saturating_duration_since(Instant::now()))
        });
        if ct_poll(timeout)? {
            match ct_read()? {
                // The one kind filter: only presses carry input, and Windows
                // also reports releases.
                CtEvent::Key(k) if k.kind == KeyEventKind::Press => {
                    dirty = true;
                    let now = Instant::now();
                    // Deliver expired keys before retaining anything for the next owner.
                    flush_due_input(tui, &mut paste, now, ctx)?;
                    // A delivered key or paste may itself dismiss an overlay.
                    guard_owner(tui, &mut paste, ctx, |tui, paste| {
                        let inputs = if tui.app.takes_text() {
                            paste.key(k, now)
                        } else {
                            let mut inputs = paste.finish();
                            inputs.push(Input::Key(k));
                            inputs
                        };
                        deliver(tui, inputs, ctx)
                    })?;
                }
                CtEvent::Paste(s) => {
                    dirty = true;
                    deliver(tui, paste.finish(), ctx)?;
                    deliver(tui, vec![Input::Paste(s)], ctx)?;
                }
                CtEvent::Mouse(m) => {
                    dirty = true;
                    deliver(tui, paste.finish(), ctx)?;
                    tui.app.mouse(m);
                }
                CtEvent::Resize(_, _) | CtEvent::FocusLost => {
                    deliver(tui, paste.finish(), ctx)?;
                    tui.app.suspend_input();
                    dirty = true;
                }
                CtEvent::Key(_) | CtEvent::FocusGained => {}
            }
        }
        if let Some(notice) = tui.app.prompt_state.take_notice() {
            let focused = tui.app.tabs.focused();
            tui.app.push_error(focused, &notice);
            dirty = true;
        }
        // A matrix Enter, or a focused agent ending mid-drain, moves focus.
        if tui.app.tabs.focused() != prev_focus {
            dirty = true;
        }
    }
}

/// What decides where input lands; a burst held for one owner never reaches
/// another.
#[derive(PartialEq, Eq)]
struct InputOwner {
    focus: AgentId,
    overlay_gen: u64,
    overlay: bool,
    matrix: bool,
    steerable: bool,
}

impl InputOwner {
    fn of(app: &App) -> Self {
        Self {
            focus: app.tabs.focused(),
            overlay_gen: app.overlay_gen,
            overlay: app.overlay.is_some(),
            matrix: app.matrix_navigating(),
            steerable: app.is_steerable(),
        }
    }
}

/// Run `act`, then deliver whatever the burst holds if `act` moved input to
/// another owner, ending its classification there.
fn guard_owner<R>(
    tui: &mut Tui,
    paste: &mut PasteBurst,
    ctx: &CommandCtx<'_>,
    act: impl FnOnce(&mut Tui, &mut PasteBurst) -> io::Result<R>,
) -> io::Result<R> {
    let owner = InputOwner::of(&tui.app);
    let r = act(tui, paste)?;
    if InputOwner::of(&tui.app) != owner {
        deliver(tui, paste.finish(), ctx)?;
    }
    Ok(r)
}

fn flush_due_input(
    tui: &mut Tui,
    paste: &mut PasteBurst,
    now: Instant,
    ctx: &CommandCtx<'_>,
) -> io::Result<bool> {
    guard_owner(tui, paste, ctx, |tui, paste| {
        deliver(tui, paste.flush_due(now), ctx)
    })
}

/// Apply detector output through the same paths as terminal input.
fn deliver(tui: &mut Tui, inputs: Vec<Input>, ctx: &CommandCtx<'_>) -> io::Result<bool> {
    let changed = !inputs.is_empty();
    for input in inputs {
        match input {
            Input::Key(k) => {
                if tui.app.key(k, ctx) {
                    compose_in_editor(tui)?;
                }
            }
            Input::Paste(s) => tui.app.paste(&s, ctx),
        }
    }
    Ok(changed)
}

/// Drive the external-editor hatch from the UI loop, which owns the terminal the
/// editor must borrow.  Repaints over whatever the editor left on the screen.
fn compose_in_editor(tui: &mut Tui) -> io::Result<()> {
    tui.app.suspend_input();
    let draft = tui.app.prompt_state.prompt_text();
    match edit_text_in_editor(&draft)? {
        Ok(edited) => tui.app.prompt_state.adopt_draft(&edited),
        Err(message) => {
            let focused = tui.app.tabs.focused();
            tui.app.push_error(focused, &message);
        }
    }
    tui.guard.term().clear()?;
    Ok(())
}
