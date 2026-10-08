//! The consuming end of the bus: the [`Sink`] presentation surface and
//! [`drain_signals`], the one rule for when an exchange's signal loop ends.
//!
//! That rule is the worker's explicit `done` flag — never the channel emptying
//! or disconnecting, since a detached worker (a `spawn`ed server, a background
//! `agent`) may hold a sender clone forever. Both the headless [`Sink::drive`]
//! and the TUI's `ui_loop` in `tui/tui_loop.rs` drive it, so they cannot drift.

use super::{BusReceiver, Emitter, FleetBus, Signal, WORKER_PANIC_PREFIX};
use crate::record::{AgentId, Forensic, Record, Recorded, Transient};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::time::Duration;

/// How often a blocked drain wakes to re-check `done`. A detached worker's
/// sender keeps the channel from ever disconnecting, so this timeout — not a
/// disconnect — is what lets a finished worker be noticed.
const DRAIN_POLL: Duration = Duration::from_millis(10);

/// The verdict of one [`drain_signals`] pass: end the loop, wait for the next
/// signal, or drain again at once.
pub(crate) enum Pass {
    Stop,
    Idle,
    More,
}

/// Drain up to `max` buffered signals through `handle`, then report where the
/// loop stands.
///
/// `done` is latched *before* the first receive: a finished worker ends the
/// pass even while background producers keep the channel full, where waiting
/// for a momentarily-empty channel would wait forever. Its buffered batch still
/// drains up to `max` — the caller renders a final frame from it — and the
/// remainder is left for the caller, which is why the TUI's exit path runs one
/// last uncapped pass. `None` `max` drains everything (headless renders nothing
/// between signals); `Some(n)` bounds a pass so a token flood cannot starve the
/// TUI's input poll, reporting [`Pass::More`]. Disconnect also stops.
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

/// One frontend: itself a fold over the one log, stepping its own memo per
/// record and drawing the increment that step reports.
///
/// The default [`Self::drive`] — headless and the tests — blocks between
/// drain passes; the TUI is not a `Sink`, but its `ui_loop` drains
/// [`drain_signals`] on its render cadence, so completion is identical on both.
pub trait Sink {
    /// A durable fact reaching the sink live, as [`Signal::Fact`] carries it —
    /// witnessed, since a fold is stepped by the record *and* its locus.
    ///
    /// The default does nothing: a frontend draws whichever half of the seam
    /// it has a use for, and one that folds a session into blocks (headless)
    /// or narrates it as a stream (synod) overrides this.  A sink is handed
    /// the record only to step its own memo — never to render the record
    /// vocabulary from it, which it draws from
    /// [`BlockKind`](crate::record::BlockKind) off the memo instead, so a
    /// second hand-rolled projection cannot compile.
    fn fact(&mut self, _id: AgentId, _rec: &Recorded<Record>) {}

    /// A [`Transient`] delta reaching the sink live — no durable form and no
    /// sequence number, so only a frontend folding over `Transient` directly
    /// has a use for one.
    fn transient(&mut self, _id: AgentId, _t: &Transient) {}

    /// # Errors
    /// Propagates an implementation's surface write failure; the default
    /// drain-and-render loop is infallible.
    fn drive(&mut self, rx: &BusReceiver, done: &AtomicBool) -> io::Result<()> {
        loop {
            match drain_signals(rx, done, None, |sig| self.accept(sig)) {
                Pass::Stop => return Ok(()),
                // An uncapped pass never reports `More`.
                Pass::Idle | Pass::More => match rx.recv_timeout(DRAIN_POLL) {
                    Ok(sig) => self.accept(sig),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                },
            }
        }
    }

    /// Route one raw signal to `fact` or `transient`.
    fn accept(&mut self, sig: Signal) {
        match sig {
            Signal::Fact(id, fact) => self.fact(id, &fact),
            Signal::Transient(id, t) => self.transient(id, &t),
        }
    }
}

/// Run `work` on a scoped thread over `bus`'s channel, drive `sink`, join.
///
/// A worker panic rides out through `recorder` as a [`Forensic::Error`] and
/// `pump` returns `None`. The channel is `bus`'s, not `pump`'s, so it outlives
/// the exchange whenever the bus does (the TUI's session bus, streaming a
/// background `agent`); completion follows [`drain_signals`] regardless.
///
/// # Errors
/// Propagates a failure from [`Sink::drive`].
pub(crate) fn pump<S, R>(
    sink: &mut S,
    bus: &FleetBus,
    root_id: AgentId,
    recorder: &crate::record::Emitter,
    work: impl Send + FnOnce(&Emitter) -> R,
) -> io::Result<Option<R>>
where
    S: Sink,
    R: Send,
{
    // Outside the scope, so the borrow shared by the worker thread and `drive`
    // outlives the spawned thread's `'env`.
    let done = AtomicBool::new(false);
    let done_ref = &done;
    let emit = bus.emitter(root_id);
    std::thread::scope(|s| -> io::Result<Option<R>> {
        let h = s.spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&emit)));
            if let Err(p) = &r {
                let msg = panic_msg(p);
                let text = format!("{WORKER_PANIC_PREFIX}{msg}");
                if let Err(e) = recorder.emit(Forensic::Error { text }) {
                    recorder.report_fault(&e);
                }
            }
            // Set before `emit` — and its sender clone — drops: the drain stops
            // on this, not on the channel closing.
            done_ref.store(true, Ordering::Release);
            r.ok()
        });
        sink.drive(bus.rx(), done_ref)?;
        Ok(h.join().ok().flatten())
    })
}

/// Every [`Record`] a test's own channel has buffered, so a test asserts what
/// a producer actually recorded.
#[cfg(test)]
pub(crate) fn drain_records(rx: &BusReceiver) -> Vec<Record> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .filter_map(|sig| match sig {
            Signal::Fact(_, rec) => Some(rec.into_value()),
            Signal::Transient(..) => None,
        })
        .collect()
}

/// The message of a recovered panic payload, for either string shape.
pub(crate) fn panic_msg(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&'static str>().map(|s| (*s).into()))
        .unwrap_or_else(|| "non-string payload".into())
}

#[cfg(test)]
mod tests;
