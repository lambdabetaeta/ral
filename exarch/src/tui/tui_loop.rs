//! The terminal guard and render+input loop beside the host's agents.

use std::{
    io::{self},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

use crossterm::event::{Event as CtEvent, KeyEventKind, poll as ct_poll, read as ct_read};

use crate::{
    SessionInfo,
    port::{BusReceiver, Port, Signal},
    provider::{Bureau, Usage},
    record::{AgentId, RECORD, Transient},
};
use std::sync::mpsc::TryRecvError;

use super::{
    App,
    app::{CommandCtx, Handoff, Job, Overlay},
    banner,
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

/// Show the host's agents until the last ends or the user quits, steering the
/// first with `seed`.
///
/// # Errors
/// Returns `Err` if there is no root or the terminal input/render loop fails.
pub(crate) fn run(
    screen: TerminalGuard,
    port: Port,
    rx: &BusReceiver,
    info: &SessionInfo<'_>,
    bureau: &Bureau,
    seed: Option<String>,
    vi: bool,
) -> Result<super::Leaving, String> {
    let Signal::Born(id, born) = rx.recv().map_err(|e| e.to_string())? else {
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
            port,
            crate::app::EXARCH
                .project(info.cwd.as_path())
                .map_err(String::from),
            PromptState::new(vi, history),
            banner::session_card(info),
            jobs,
        ),
    };
    if let Some(seed) = seed
        && tui.app.tabs.port().submit(id, seed).is_err()
    {
        tui.app
            .push_error(id, "the agent ended before it could take the prompt");
    }
    if let Some(e) = unshared {
        tui.app.push_error(
            id,
            &format!("{e}; this session's prompts will not be saved"),
        );
    }

    let ctx = CommandCtx { bureau, info };
    match ui_loop(&mut tui, rx, &inbox, &ctx) {
        Ok(()) | Err(Exit::Quit) => {}
        Err(Exit::Io(e)) => return Err(e.to_string()),
    }
    let leaving = Leaving {
        logs: tui.app.tabs.log_dirs(),
        recalled: tui.app.recalled(),
    };
    drop(tui);
    Ok(leaving)
}

/// What the TUI saw, for the report `lib` prints once the host has joined.
pub(crate) struct Leaving {
    logs: Vec<ral_core::own::OwnDir>,
    recalled: Usage,
}

impl Leaving {
    /// Report the logs and the total spend after the join.
    pub(crate) fn report(&self, spent: Usage) {
        for dir in &self.logs {
            ral_core::outln!("Session log: {}/{RECORD}", dir);
        }
        ral_core::outln!("render one with `exarch export <dir>`");
        ral_core::outln!("{}", self.recalled + spent);
    }
}

/// Route one `Signal` to its `App` entry point.
fn dispatch(tui: &mut Tui, rx: &BusReceiver, sig: Signal) {
    match sig {
        Signal::Commit(id, commit) => tui.app.commit(id, &commit),
        Signal::Born(id, born) => tui.app.born(id, &born),
        Signal::Transient(id, t) => tui.app.transient(id, t, rx),
    }
}

enum Pass {
    Stop,
    Idle,
    More,
}

/// Why the loop returns early: `?` carries `/quit` out as it does a failure.
#[derive(Debug)]
enum Exit {
    Quit,
    Io(io::Error),
}

impl From<io::Error> for Exit {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Drain before checking settlement, so queued births are always observed.
fn drain_signals(
    rx: &BusReceiver,
    max: Option<usize>,
    mut handle: impl FnMut(Signal) -> Result<(), Exit>,
) -> Result<Pass, Exit> {
    let mut n = 0usize;
    loop {
        if max.is_some_and(|m| n >= m) {
            return Ok(Pass::More);
        }
        match rx.try_recv() {
            Ok(sig) => {
                handle(sig)?;
                n += 1;
            }
            Err(TryRecvError::Empty) => {
                return Ok(Pass::Idle);
            }
            Err(TryRecvError::Disconnected) => return Ok(Pass::Stop),
        }
    }
}

/// The merged render + input loop, on the UI thread beside the roots' workers.
/// Returns once the roots finish, after one last frame with all their signals.
fn ui_loop(
    tui: &mut Tui,
    rx: &BusReceiver,
    inbox: &Receiver<Job>,
    ctx: &CommandCtx<'_>,
) -> Result<(), Exit> {
    const BATCH: usize = 64;
    let frame = Duration::from_millis(16); // ~60 FPS max
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
        // The cap keeps a signal flood from starving input.
        let mut handled_any = false;
        let pass = drain_signals(rx, Some(BATCH), |sig| {
            handled_any = true;
            guard_owner(tui, &mut paste, ctx, |tui, paste| {
                // Facts can retire a subtree; death can change the focused
                // owner.  Streaming transients must not split a concurrent paste.
                if matches!(
                    &sig,
                    Signal::Commit(..) | Signal::Transient(_, Transient::Died { .. })
                ) {
                    deliver(tui, paste.flush_pending(), ctx)?;
                }
                dispatch(tui, rx, sig);
                Ok(())
            })
        })?;
        if matches!(pass, Pass::Stop) || matches!(pass, Pass::Idle) && tui.app.tabs.settled() {
            deliver(tui, paste.finish(), ctx)?;
            drain_signals(rx, None, |sig| {
                dispatch(tui, rx, sig);
                Ok(())
            })?;
            draw(&mut tui.app, tui.guard.term(), ctx.bureau)?;
            emit_tab_title(&mut tui.app);
            return Ok(());
        }
        let more = matches!(pass, Pass::More);
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
    act: impl FnOnce(&mut Tui, &mut PasteBurst) -> Result<R, Exit>,
) -> Result<R, Exit> {
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
) -> Result<bool, Exit> {
    guard_owner(tui, paste, ctx, |tui, paste| {
        deliver(tui, paste.flush_due(now), ctx)
    })
}

/// Apply detector output through the same paths as terminal input.
fn deliver(tui: &mut Tui, inputs: Vec<Input>, ctx: &CommandCtx<'_>) -> Result<bool, Exit> {
    let changed = !inputs.is_empty();
    for input in inputs {
        match input {
            Input::Key(k) => match tui.app.key(k, ctx) {
                Some(Handoff::Compose) => compose_in_editor(tui)?,
                Some(Handoff::Quit) => return Err(Exit::Quit),
                None => {}
            },
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

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
