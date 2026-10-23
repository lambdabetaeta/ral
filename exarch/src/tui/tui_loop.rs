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

use crossterm::event::{Event as CtEvent, poll as ct_poll, read as ct_read};

use crate::{
    SessionInfo,
    agent::{
        Avatar,
        fleet::Fleet,
        spawn::{AsyncSpawn, spawn_async},
    },
    bus::{BusReceiver, FleetBus, Pass, Signal},
    provider::Bureau,
    record::{RECORD, Resumed},
};
use std::sync::mpsc::TryRecvError;

use super::{
    App,
    app::{CommandCtx, Job, Overlay},
    banner, commands,
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
    roots: Vec<(Avatar, Option<Resumed>)>,
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
    let (jobs, inbox) = mpsc::channel::<Job>();
    let mut tui = Tui {
        guard: screen,
        app: App::new(
            id,
            &born,
            Arc::clone(&fleet),
            bus.meter(),
            vi,
            banner::session_card(info),
            jobs,
        ),
    };

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
                draw(&mut tui.app, tui.guard.term(), ctx.bureau)?;
                emit_tab_title(&mut tui.app);
                return Ok(());
            }
            Pass::More => true,
            Pass::Idle => false,
        };
        for job in inbox.try_iter() {
            job(&mut tui.app, ctx);
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
            let ticked = tui.app.tabs.tick();
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
        if ct_poll(timeout)? {
            match ct_read()? {
                CtEvent::Key(k) => {
                    dirty = true;
                    if tui.app.key(k, ctx) {
                        compose_in_editor(tui)?;
                    }
                }
                CtEvent::Paste(s) => {
                    dirty = true;
                    tui.app.paste(&s, ctx);
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
        // A matrix Enter, or a focused agent ending mid-drain, moves focus.
        if tui.app.tabs.focused() != prev_focus {
            dirty = true;
        }
    }
}

/// Drive the `C-x C-e` hatch from the UI loop, which owns the terminal the
/// editor must borrow.  Repaints over whatever the editor left on the screen.
fn compose_in_editor(tui: &mut Tui) -> io::Result<()> {
    let draft = tui.app.prompt_state.prompt_text();
    if let Some(edited) = edit_text_in_editor(&draft)? {
        tui.app.prompt_state.adopt_draft(&edited);
    }
    tui.guard.term().clear()?;
    Ok(())
}
