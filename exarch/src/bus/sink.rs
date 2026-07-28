//! The consuming end of the bus: the [`Sink`] presentation surface and
//! [`drain_pass`], the one rule for when an exchange's event loop ends.
//!
//! That rule is the worker's explicit `done` flag — never the channel emptying
//! or disconnecting, since a detached worker (a `spawn`ed server, a background
//! `agent`) may hold a sender clone forever. Both the headless [`Sink::drive`]
//! and the TUI's `ui_loop` in `tui/tui_loop.rs` drive it, so they cannot drift.

use super::{AgentId, BusReceiver, Emitter, Event, FleetBus, Kind, WORKER_PANIC_PREFIX};
use crate::agent::transcript::Transcript;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::time::Duration;

/// How often a blocked drain wakes to re-check `done`. A detached worker's
/// sender keeps the channel from ever disconnecting, so this timeout — not a
/// disconnect — is what lets a finished worker be noticed.
const DRAIN_POLL: Duration = Duration::from_millis(10);

/// The verdict of one [`drain_pass`]: end the loop, wait for the next event,
/// or drain again at once.
pub(crate) enum Pass {
    Stop,
    Idle,
    More,
}

/// Drain up to `max` buffered events through `handle`, then report where the
/// loop stands.
///
/// `done` is latched *before* the first receive: a finished worker ends the
/// pass even while background producers keep the channel full, where waiting
/// for a momentarily-empty channel would wait forever. Its buffered batch still
/// drains up to `max` — the caller renders a final frame from it — and the
/// remainder is left for the caller, which is why the TUI's exit path runs one
/// last uncapped pass. `None` `max` drains everything (headless renders nothing
/// between events); `Some(n)` bounds a pass so a token flood cannot starve the
/// TUI's input poll, reporting [`Pass::More`]. Disconnect also stops.
pub(crate) fn drain_pass(
    rx: &BusReceiver,
    done: &AtomicBool,
    max: Option<usize>,
    mut handle: impl FnMut(Event),
) -> Pass {
    let finished = done.load(Ordering::Acquire);
    let mut n = 0usize;
    loop {
        if max.is_some_and(|m| n >= m) {
            return if finished { Pass::Stop } else { Pass::More };
        }
        match rx.try_recv() {
            Ok(ev) => {
                handle(ev);
                n += 1;
            }
            Err(TryRecvError::Empty) => {
                return if finished { Pass::Stop } else { Pass::Idle };
            }
            Err(TryRecvError::Disconnected) => return Pass::Stop,
        }
    }
}

/// One presentation surface. The default [`Self::drive`] — headless and the
/// tests — blocks between [`drain_pass`] passes; the TUI is not a `Sink`, but
/// its `ui_loop` drains the same [`drain_pass`] on its render cadence, so
/// completion is identical on both.
pub trait Sink {
    fn handle(&mut self, e: Event);

    /// # Errors
    /// Propagates an implementation's surface write failure; the default
    /// drain-and-render loop is infallible.
    fn drive(&mut self, rx: &BusReceiver, done: &AtomicBool) -> io::Result<()> {
        loop {
            match drain_pass(rx, done, None, |ev| self.handle(ev)) {
                Pass::Stop => return Ok(()),
                // An uncapped pass never reports `More`.
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
/// A worker panic rides out through the still-open [`Emitter`] as a final
/// [`Kind::Error`] and `pump` returns `None`. The channel is `bus`'s, not
/// `pump`'s, so it outlives the exchange whenever the bus does (the TUI's
/// session bus, streaming a background `agent`); completion follows
/// [`drain_pass`] regardless.
///
/// # Errors
/// Propagates a failure from [`Sink::drive`].
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
    // Outside the scope, so the borrow shared by the worker thread and `drive`
    // outlives the spawned thread's `'env`.
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
            // Set before `emit` — and its sender clone — drops: the drain stops
            // on this, not on the channel closing.
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

    /// Completion is the worker's `done`, not the channel: a detached sender
    /// clone — a `spawn`ed server's — stays live across the stop.
    #[test]
    fn drain_pass_stops_on_done_with_a_live_detached_sender() {
        let (tx, rx) = channel();
        let done = AtomicBool::new(false);
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
        assert!(
            matches!(drain_pass(&rx, &done, None, |_| seen += 1), Pass::Stop),
            "must stop once the worker is done"
        );
        assert_eq!(seen, 2, "every buffered event is handled before stopping");
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

    /// The batch cap bounds one pass and reports `More`; an empty channel with
    /// the worker unfinished reports `Idle`.
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
        assert!(
            matches!(drain_pass(&rx, &done, Some(2), |_| seen += 1), Pass::More),
            "a full batch reports More"
        );
        assert_eq!(seen, 2, "the batch cap bounds one pass");
        assert!(
            matches!(drain_pass(&rx, &done, Some(2), |_| seen += 1), Pass::Idle),
            "an empty channel with no done reports Idle"
        );
        assert_eq!(seen, 3, "the rest drains on the next pass");
    }

    /// A finished worker stops the pass while the channel is never empty — the
    /// shape that hangs a foreground exchange when a background `agent` floods
    /// the bus.
    #[test]
    fn drain_pass_stops_on_done_even_while_a_background_producer_floods() {
        let (tx, rx) = channel();
        let done = AtomicBool::new(false);
        // `ToolResult` is a reserved kind, so the channel's merge rule never
        // coalesces these sends: each is its own entry and the count measures
        // the cap, not the merge.
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

        let mut seen = 0usize;
        assert!(
            matches!(drain_pass(&rx, &done, Some(64), |_| seen += 1), Pass::Stop),
            "a finished worker stops the pass even though the channel is non-empty"
        );
        assert_eq!(seen, 64, "the buffered batch is drained up to the cap");
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

    /// `pump` returns on the worker's `done` while a holder keeps an `Emitter`
    /// clone — a live sender — past the worker's return, as a `spawn`ed server
    /// that never terminates would.
    #[test]
    fn pump_returns_on_worker_done_not_sender_disconnect() {
        struct CountSink(usize);
        impl Sink for CountSink {
            fn handle(&mut self, _e: Event) {
                self.0 += 1;
            }
        }

        let mut sink = CountSink(0);
        let bus = FleetBus::session(&Inbox::new());
        // Outlives `pump`, holding an `Emitter` clone whose sender keeps the
        // channel from ever disconnecting.
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
        assert!(holder.lock().unwrap().is_some());
    }
}
