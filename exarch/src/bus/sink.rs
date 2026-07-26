//! The consuming end of the bus: one presentation surface, [`Sink`], and the
//! single pass that decides when an exchange's event loop ends.
//!
//! [`drain_pass`] is the shared completion contract — its [`Pass`] verdict
//! stops on the worker's explicit `done`, never on the channel emptying or
//! disconnecting, so a detached worker holding a sender clone forever cannot
//! keep an exchange alive. [`pump`] runs a worker on a scoped thread over a
//! [`crate::bus::FleetBus`]'s channel and drives a [`Sink`] against it until
//! that `done` fires.

use super::{AgentId, BusReceiver, Emitter, Event, FleetBus, Kind, WORKER_PANIC_PREFIX};
use crate::agent::transcript::Transcript;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::time::Duration;

/// How often the completion-aware drain loop wakes to re-check the `done`
/// flag while no event is arriving.  Small enough that an exchange returns
/// promptly after its worker finishes, large enough not to spin.
const DRAIN_POLL: Duration = Duration::from_millis(10);

/// The verdict of one [`drain_pass`]: the explicit-done completion contract,
/// shared by every driver.
pub(crate) enum Pass {
    /// The worker is done (or the channel disconnected) and the buffered
    /// batch has been handled — render a final frame and return.
    Stop,
    /// The channel went empty and the worker is not done: the loop is idle
    /// until the next event arrives.
    Idle,
    /// The batch cap was reached before the channel emptied: more events are
    /// already queued, so drain again without waiting.
    More,
}

/// One pass of the explicit-done completion contract — the single place that
/// decides when an exchange's event loop ends, shared by the headless default
/// [`Sink::drive`] and the TUI's `tui_loop::ui_loop`.
///
/// Drains up to `max` available events through `handle`, then reports the
/// channel's state. **Completion is `done` being set — the worker finished —
/// never the channel emptying or disconnecting:** a detached worker (a
/// `spawn`ed server, a live background `agent`) may hold a sender clone forever
/// and keep the channel non-empty, but it never decides the exchange is over,
/// because the loop stops on the explicit `done` flag, not on the channel.
/// This is the daemon-task-hang fix, factored so the two drivers cannot drift
/// on it.
///
/// `done` is checked *before each receive*, so the pass ends the moment the
/// worker finishes even while concurrent background producers keep the channel
/// full — it does not wait for a momentarily-empty channel, which under a
/// background flood would never come. On `done` it drains the buffered batch up
/// to `max` (so the caller can render a final frame including the worker's last
/// events) and returns [`Pass::Stop`]; any events still buffered past `max` are
/// left for the caller's next pass — the TUI's exit path runs one final
/// uncapped pass for exactly this reason. `None` `max` drains every buffered event
/// (headless, which has nothing to render between them); `Some(n)` caps one
/// pass so a flood cannot starve the TUI's input poll between passes, reporting
/// [`Pass::More`] so the caller drains again. Disconnect also stops.
pub(crate) fn drain_pass(
    rx: &BusReceiver,
    done: &AtomicBool,
    max: Option<usize>,
    mut handle: impl FnMut(Event),
) -> Pass {
    // Latch `done` once at the top: the worker sets it after `work` returns,
    // so reading it before draining means a finishing worker's already-queued
    // events are still handled in this pass, and the pass cannot loop forever
    // chasing a channel that a background producer keeps non-empty.
    let finished = done.load(Ordering::Acquire);
    let mut n = 0usize;
    loop {
        if max.is_some_and(|m| n >= m) {
            // The batch cap bounds even a `done` drain, so a huge backlog from
            // the finished worker (or background producers) cannot block the
            // final frame; the caller drains the rest from the idle path.
            return if finished { Pass::Stop } else { Pass::More };
        }
        match rx.try_recv() {
            Ok(ev) => {
                handle(ev);
                n += 1;
            }
            // The buffered batch is exhausted. Stop if the worker has
            // finished; otherwise the loop is idle until the next event.
            Err(TryRecvError::Empty) => {
                return if finished { Pass::Stop } else { Pass::Idle };
            }
            Err(TryRecvError::Disconnected) => return Pass::Stop,
        }
    }
}

/// One presentation surface.
///
/// [`Self::handle`] consumes a single event synchronously; the default
/// [`Self::drive`] — what headless and the tests run — drains the channel
/// through [`drain_pass`] until the worker's explicit `done`, blocking
/// between passes.  The TUI is not a `Sink`: its `ui_loop` drains the same
/// [`drain_pass`] on its own render cadence, so the completion contract
/// stays identical across both.
pub trait Sink {
    fn handle(&mut self, e: Event);

    /// # Errors
    /// Returns `Err` if an implementation's surface write fails; the default
    /// drain-and-render loop is infallible.
    fn drive(&mut self, rx: &BusReceiver, done: &AtomicBool) -> io::Result<()> {
        loop {
            // The shared completion contract. `None` max drains every buffered
            // event — headless has nothing to render between them, so it never
            // needs the TUI's batch cap.
            match drain_pass(rx, done, None, |ev| self.handle(ev)) {
                Pass::Stop => return Ok(()),
                // Idle (an uncapped pass never reports `More`): block on the
                // channel for the next event, waking each `DRAIN_POLL` to
                // re-check `done`. A detached worker's sender keeps the channel
                // from disconnecting, so the timeout — not disconnect — is what
                // lets the next `done` re-check run.
                Pass::Idle | Pass::More => match rx.recv_timeout(DRAIN_POLL) {
                    Ok(ev) => self.handle(ev),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                },
            }
        }
    }
}

/// Run `work` on a scoped thread over `bus`'s channel, drive `sink`, join.
///
/// A worker panic is reported through the still-open [`crate::bus::Emitter`] as a
/// final [`crate::bus::Kind::Error`]; the function returns `None` in that case.
///
/// The channel belongs to `bus`, not to `pump`: a session-lived bus keeps it
/// open across exchanges (the TUI, so a background `agent` streams a live tab),
/// while a per-exchange bus closes it when the worker finishes (headless / tests).
/// Either way, completion follows [`drain_pass`]'s explicit-done contract.
///
/// # Errors
/// Returns `Err` if driving `sink` over the bus fails (propagated from
/// [`Sink::drive`]).
pub(crate) fn pump<S, R>(
    sink: &mut S,
    bus: &FleetBus,
    root_id: AgentId,
    transcript: Transcript,
    work: impl Send + FnOnce(&Emitter) -> R,
) -> io::Result<Option<R>>
where
    S: Sink,
    R: Send,
{
    // Declared outside the scope so the borrow into both the worker thread
    // and `drive` outlives the spawned thread's `'env`.
    let done = AtomicBool::new(false);
    let done_ref = &done;
    let emit = bus.emitter(root_id, transcript);
    std::thread::scope(|s| -> io::Result<Option<R>> {
        let h = s.spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&emit)));
            if let Err(p) = &r {
                let msg = crate::agent::panic_msg(p);
                emit.emit(Kind::Error(format!("{WORKER_PANIC_PREFIX}{msg}")));
            }
            // Signal completion before the worker's `emit` (and its sender
            // clone) drops: the exchange is over because the worker finished.
            done_ref.store(true, Ordering::Release);
            r.ok()
        });
        sink.drive(bus.rx(), done_ref)?;
        Ok(h.join().ok().flatten())
    })
}

#[cfg(test)]
mod tests {
    use super::{Pass, Sink, drain_pass, pump};
    use crate::agent::transcript::Transcript;
    use crate::bus::{Emitter, Event, FleetBus, Inbox, Kind, channel};
    use crate::provider::Tuning;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// The headless default [`Sink::drive`] and the TUI's `tui_loop::ui_loop`
    /// share one completion contract: [`drain_pass`]. It stops when the worker is
    /// *done*, never when the channel empties or disconnects — so a detached
    /// worker holding a sender clone cannot keep an exchange alive. Pinning the
    /// shared primitive directly is what keeps the two drivers from drifting on
    /// the daemon-task-hang fix.
    #[test]
    fn drain_pass_stops_on_done_with_a_live_detached_sender() {
        let (tx, rx) = channel();
        let done = AtomicBool::new(false);
        // A detached holder keeps a sender clone alive forever — the channel
        // never disconnects, exactly as a `spawn`ed server would.
        let holder = tx.clone();

        tx.send(Event {
            id: 0,
            kind: Kind::Step {
                n: 1,
                tuning: Tuning::default(),
            },
        })
        .unwrap();
        tx.send(Event {
            id: 0,
            kind: Kind::Step {
                n: 2,
                tuning: Tuning::default(),
            },
        })
        .unwrap();
        done.store(true, Ordering::Release);

        let mut seen = 0usize;
        // `None` max is the headless drain; `Some(BATCH)` would be the TUI's.
        // Both reach `Stop` here: `done` is set and the buffered events drain.
        assert!(
            matches!(drain_pass(&rx, &done, None, |_| seen += 1), Pass::Stop),
            "must stop once the worker is done"
        );
        assert_eq!(seen, 2, "every buffered event is handled before stopping");
        // The detached sender is still alive — completion did not depend on it.
        assert!(
            holder
                .send(Event {
                    id: 0,
                    kind: Kind::Died
                })
                .is_ok(),
            "the detached sender outlived the stop"
        );
    }

    /// The batch cap bounds one pass (TUI policy: don't let a token flood
    /// starve the input poll), reporting `More`; an empty channel with the
    /// worker not done reports `Idle` (the headless wait state).
    #[test]
    fn drain_pass_caps_batch_as_more_and_reports_idle_when_empty() {
        let (tx, rx) = channel();
        let done = AtomicBool::new(false);
        for _ in 0..3 {
            tx.send(Event {
                id: 0,
                kind: Kind::Boundary,
            })
            .unwrap();
        }

        let mut seen = 0usize;
        // Cap below the queue depth: the cap is hit before the channel empties.
        assert!(
            matches!(drain_pass(&rx, &done, Some(2), |_| seen += 1), Pass::More),
            "a full batch reports More"
        );
        assert_eq!(seen, 2, "the batch cap bounds one pass");
        // Drain the remainder: the channel empties with the worker not done.
        assert!(
            matches!(drain_pass(&rx, &done, Some(2), |_| seen += 1), Pass::Idle),
            "an empty channel with no done reports Idle"
        );
        assert_eq!(seen, 3, "the rest drains on the next pass");
    }

    /// The session-lifetime refinement: a finished foreground exchange must stop
    /// even while the channel is *non-empty*, because concurrent background
    /// producers (a live async `agent`) keep it full and the old
    /// "stop only on a momentarily-empty channel" rule would never fire. On
    /// `done`, `drain_pass` drains the buffered batch (up to the cap) and
    /// returns `Stop`; anything still buffered past the cap is left for the
    /// caller's next pass. Without the fix the foreground exchange would hang
    /// exactly when a background agent is flooding the bus.
    #[test]
    fn drain_pass_stops_on_done_even_while_a_background_producer_floods() {
        let (tx, rx) = channel();
        let done = AtomicBool::new(false);
        // A background producer keeps sending — the channel is never empty.
        // A reserved kind (never merged) keeps each send its own queue entry,
        // so this test isolates the batch cap from the merge rule, which has
        // its own coverage below (`bus_queue_token_flood_coalesces_...`).
        let background = tx;
        for _ in 0..200 {
            background
                .send(Event {
                    id: 9,
                    kind: Kind::ToolResult("x".into()),
                })
                .unwrap();
        }
        // The foreground worker finishes while the channel is still full.
        done.store(true, Ordering::Release);

        // A capped pass (the TUI) drains its batch and stops, never `More`-ing
        // forever against the flood; the cap bounds the final frame's work.
        let mut seen = 0usize;
        assert!(
            matches!(drain_pass(&rx, &done, Some(64), |_| seen += 1), Pass::Stop),
            "a finished worker stops the pass even though the channel is non-empty"
        );
        assert_eq!(seen, 64, "the buffered batch is drained up to the cap");
        // The background producer is unaffected — completion never depended on
        // the channel draining or disconnecting.
        assert!(
            background
                .send(Event {
                    id: 9,
                    kind: Kind::Died
                })
                .is_ok(),
            "the background producer outlives the foreground stop"
        );
    }

    /// Completion is the worker finishing, not the channel disconnecting.
    /// A "detached holder" keeps a clone of the worker's [`Emitter`] (hence a
    /// live `Sender`) alive past the worker's return — modelling a `spawn`ed
    /// server that never terminates.  `pump` must still return promptly,
    /// driven by the explicit `done` flag, while that sender is still alive.
    /// Regression for the daemon-task hang.
    #[test]
    fn pump_returns_on_worker_done_not_sender_disconnect() {
        struct CountSink(usize);
        impl Sink for CountSink {
            fn handle(&mut self, _e: Event) {
                self.0 += 1;
            }
        }

        let mut sink = CountSink(0);
        // A session-lived bus, as the TUI uses it: its sender outlives the
        // exchange, so a detached worker's clone never disconnects the channel.
        let bus = FleetBus::session(&Inbox::new());
        // Outlives `pump`: holds an `Emitter` clone whose `Sender` keeps the
        // channel from ever disconnecting, exactly as a detached worker would.
        let holder: Mutex<Option<Emitter>> = Mutex::new(None);

        let t0 = Instant::now();
        let r = pump(&mut sink, &bus, 0, Transcript::none(), |emit| {
            *holder.lock().unwrap() = Some(emit.clone());
            emit.emit(Kind::Step {
                n: 1,
                tuning: Tuning::default(),
            });
            "done"
        })
        .expect("pump returns Ok");

        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "pump must return on the explicit done signal, not wait for sender disconnect (took {:?})",
            t0.elapsed()
        );
        assert_eq!(r, Some("done"), "pump returns the worker's value");
        assert_eq!(sink.0, 1, "the worker's one event was delivered");
        // The detached sender is still alive — proof completion did not
        // depend on it dropping.
        assert!(holder.lock().unwrap().is_some());
    }
}
