//! The pins family of the desk.

use crate::card::{Card, encode_card};
use crate::shell_eval::{self, Surface};
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::Datum;
use ral_core::sync::LockExt;

use super::{ExarchDesk, absorb_surface};

impl ExarchDesk {
    /// `` `set ``/`` `clear `` — write or empty the register slot under
    /// `key`: update the mirror `` `read ``/`` `list `` answer from, then draw
    /// the forensic row and transient through [`absorb_surface`]. Answers
    /// `()`.
    pub(super) fn apply_pin(&self, key: String, card: Option<Card>) -> FOValue {
        {
            let mut m = self.services.agent.pins.lock_ignore_poison();
            match &card {
                Some(card) => {
                    m.insert(key.clone(), shell_eval::PinDigest::new(card.clone()));
                }
                None => {
                    m.remove(&key);
                }
            }
        }
        let recorder = self.services.log.borrow().record_emitter();
        let surface = match card {
            Some(card) => Surface::Pin { key, card },
            None => Surface::Unpin { key },
        };
        if let Err(error) = absorb_surface(&recorder, &surface) {
            recorder.report_fault(&error);
        }
        FOValue::Unit
    }

    /// `` `read `` — `` `some `` the card stored under `key` on this agent's
    /// own register, canonically re-encoded, or `` `none `` on a miss.
    /// Read-after-write within one run is sound: `` `set ``/`` `clear ``
    /// write the mirror synchronously, on this same enquiry desk.
    pub(super) fn pin_read(&self, key: &str) -> FOValue {
        let m = self.services.agent.pins.lock_ignore_poison();
        match m.get(key) {
            Some(digest) => FOValue::Variant {
                label: "some".into(),
                payload: Some(Box::new(encode_card(&digest.card))),
            },
            None => FOValue::Variant {
                label: "none".into(),
                payload: None,
            },
        }
    }

    /// `` `list `` — the keys currently occupied on this agent's own
    /// register, in `BTreeMap` order.
    pub(super) fn pin_list(&self) -> FOValue {
        let pins = self.services.agent.pins.lock_ignore_poison();
        Vec::from_iter(pins.keys().cloned()).encode()
    }
}
