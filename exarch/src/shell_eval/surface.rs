//! What a surfaced value becomes in the record: decoded once, then recorded
//! live as a run drains, or replayed from a `spawn` worker's deferred batch.

use crate::card::{Change, Landed, value_to_card, value_to_done, value_to_edit};
use crate::record::{Display, Emitter, Forensic, Transient};
use ral_core::carrier::Host;
use ral_core::data::FOValue;
use ral_core::data::datum::Datum as _;
use ral_core::fact::Worker;
use ral_core::protocol::EnquiryError;
use ral_core::sync::LockExt;
use ral_core::types::{Notice, Observation, Observed};
use std::collections::HashSet;
use std::sync::Mutex;

/// What the record absorbs: the shapes the `surface` channel carries, plus
/// the register writes `exarch-pins` makes — closed and named rather than
/// borrowed from the bus's vocabulary.
///
/// `Surface` carries the fact a card *is* (`Card`, `Pin`) rather than one a
/// printer merely wants a copy of — an observation, a change, a notice, and
/// a done keep only their structured value, so a printer's own mark tree is
/// built once, by whoever renders, not eagerly here and thrown away by
/// whoever records.
pub enum Surface {
    Observation(Box<Observation>),
    /// What a write or an edit did to a file.
    Change(Change),
    Card(crate::card::Card),
    Notice(Notice),
    Done {
        id: Option<u64>,
        title: String,
        elapsed: Option<std::time::Duration>,
        outcome: crate::card::DoneOutcome,
    },
    Pin {
        key: String,
        card: crate::card::Card,
    },
    Unpin {
        key: String,
    },
}

/// What [`decode_surface`] made of one surfaced value — its two silences are
/// not the same fact, so a caller that must tell them apart (the extension
/// law's loud-unknown-class clause) can.
pub enum Decoded {
    /// A recognised surface class, ready to apply.
    Surface(Surface),
    /// An observation `Landed::of` declines to render: a known shape this host
    /// chooses to keep off the rail, not a decode failure.
    Landed,
    /// No surface class recognises this value's shape at all.
    Unknown,
}

/// Decode one surfaced `Value` into the [`Surface`] it names — the single
/// decoder both delivery regimes share, so the live sink's events and the
/// deferred sink's later `deliver` cannot drift.
///
/// Every class riding the one `exarch-surface` channel is a variant with a
/// distinct label, so the arm order below carries no meaning.  Anything else
/// is [`Decoded::Unknown`].
///
/// The register is not among them: a pin is *state*, keyed to a slot and
/// overwritten in place, and `exarch-pins` is its only door.
pub fn decode_surface(ev: &FOValue) -> Decoded {
    if let Some(event) = Observation::from_surface(ev) {
        // Core reports every observation it makes and judges none of them;
        // `Landed::of` is where this host says which it wants, and a write is
        // wanted as the change it made.  One core dispatch is one
        // observation, so a rejected one is dropped outright rather than
        // offered to the decoders below — deliberately, so it is `Landed`,
        // not `Unknown`.
        match (Change::of(&event.what), Landed::of(&event.what)) {
            (Some(change), _) => Decoded::Surface(Surface::Change(change)),
            (None, Some(_)) => Decoded::Surface(Surface::Observation(Box::new(event))),
            (None, None) => Decoded::Landed,
        }
    } else if let Some(change) = value_to_edit(ev) {
        Decoded::Surface(Surface::Change(change))
    } else if let Some(notice) = Notice::from_surface(ev) {
        Decoded::Surface(Surface::Notice(notice))
    } else if let Some(card) = value_to_card(ev) {
        Decoded::Surface(Surface::Card(card))
    } else if let Some((id, title, elapsed, outcome)) = value_to_done(ev) {
        Decoded::Surface(Surface::Done {
            id: Some(id),
            title,
            elapsed: Some(elapsed),
            outcome,
        })
    } else {
        Decoded::Unknown
    }
}

/// Decode `val`, noting a shape no surface class recognises: the extension
/// law's loud case.  An observation `Landed::of` declines stays silent, since
/// that is a class this host chose not to render, not an unknown one.
fn decode_loudly(recorder: &Emitter, val: &FOValue) -> Option<Surface> {
    match decode_surface(val) {
        Decoded::Surface(surface) => Some(surface),
        Decoded::Landed => None,
        Decoded::Unknown => {
            recorder.emit_or_report(Forensic::SystemNote {
                text: format!(
                    "surface: no rendering for a {} value; it was dropped",
                    val.shape()
                ),
            });
            None
        }
    }
}

/// Records each value a run surfaces as it drains.  Every dispatch's drain
/// loop ([`ral_core::carrier::dispatch_to_report`]) reaches it through the
/// protocol, so a call's surfaced values always render off the one applier it
/// was built with.
pub(crate) struct SurfaceApplier {
    recorder: Emitter,
    /// Every `Observed::Worker` this call's drain has heard: the births the
    /// orphan sentence joins against the run-boundary `workers` probe.
    births: Mutex<HashSet<u64>>,
}

impl SurfaceApplier {
    pub(crate) fn new(recorder: Emitter) -> Self {
        Self {
            recorder,
            births: Mutex::default(),
        }
    }

    pub(crate) fn births(&self) -> HashSet<u64> {
        self.births.lock_ignore_poison().clone()
    }
}

/// An applier alone is the mute host the bare harness runs under:
/// nothing answers.
impl Host for SurfaceApplier {
    fn surface(&self, val: &FOValue) {
        let Some(surface) = decode_loudly(&self.recorder, val) else {
            return;
        };
        if let Surface::Observation(event) = &surface
            && let Observed::Worker(Worker { id, .. }) = &event.what
        {
            self.births.lock_ignore_poison().insert(id.0);
        }
        record(&self.recorder, &surface);
    }

    fn enquire(&self, _req: FOValue) -> Result<FOValue, EnquiryError> {
        Err(EnquiryError::no_desk())
    }
}

/// A `spawn` worker's deferred batch, recorded as the live drain would have.
pub(crate) fn replay(recorder: &Emitter, values: &[FOValue]) {
    for surface in values.iter().filter_map(|v| decode_loudly(recorder, v)) {
        record(recorder, &surface);
    }
}

/// Record one decoded [`Surface`].
///
/// The record carries raw facts in arrival order: one [`Display::Observation`]
/// per observation, one [`Display::Change`] per write or edit, one
/// [`Display::Card`] per card.  Grouping a call's effects and a run's changes
/// belongs to the frontend, which derives them online and so needs no
/// coalesced log to rebuild from.
///
/// A pin publishes twice: [`Forensic::Pin`]/`Unpin` is the durable breadcrumb
/// a resume replays, [`Transient::Pin`]/`Unpin` is the register the live
/// process is holding, which a resume does not restore.
pub(crate) fn record(recorder: &Emitter, surface: &Surface) {
    match surface {
        Surface::Observation(event) => recorder.emit_or_report(Display::Observation {
            value: (**event).clone().encode(),
        }),
        Surface::Change(change) => recorder.emit_or_report(Display::Change {
            change: change.clone(),
        }),
        Surface::Card(card) => recorder.emit_or_report(Display::Card { card: card.clone() }),
        Surface::Done {
            id,
            title,
            elapsed,
            outcome,
        } => recorder.emit_or_report(Display::Done {
            id: *id,
            title: title.clone(),
            elapsed_ms: elapsed.map(|e| u64::try_from(e.as_millis()).unwrap_or(u64::MAX)),
            outcome: outcome.clone(),
        }),
        Surface::Notice(Notice::Reap(reap)) => recorder.emit_or_report(Forensic::Reap {
            title: reap.title.clone(),
            cause: <&str>::from(reap.cause).to_string(),
        }),
        Surface::Notice(Notice::Prune(pruned)) => recorder.emit_or_report(Forensic::Prune {
            names: pruned.iter().map(|p| p.name.clone()).collect(),
            idle_calls: pruned.iter().map(|p| p.idle_calls).collect(),
        }),
        // Register state, not scrollback: the history informs the resume note.
        Surface::Pin { key, card } => {
            recorder.emit_or_report(Forensic::Pin { key: key.clone() });
            recorder.transient(Transient::Pin {
                key: key.clone(),
                card: card.clone(),
            });
        }
        Surface::Unpin { key } => {
            recorder.emit_or_report(Forensic::Unpin { key: key.clone() });
            recorder.transient(Transient::Unpin { key: key.clone() });
        }
    }
}
