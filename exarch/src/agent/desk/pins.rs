//! The pins family of the desk.

use crate::card::{Card, encode_card};
use crate::shell_eval::{surface, surface::Surface};
use ral_core::data::FOValue;
use ral_core::data::datum::Datum;
use ral_core::error::Error;
use ral_core::shell::runtime::no_desk;

use super::ExarchDesk;

impl ExarchDesk {
    pub(super) fn apply_pin(&self, key: String, card: Option<Card>) -> Result<FOValue, Error> {
        let mut body = self.loan.get().ok_or_else(no_desk)?;
        match &card {
            Some(card) => {
                body.pins.insert(key.clone(), card.clone());
            }
            None => {
                body.pins.remove(&key);
            }
        }
        let surface = match card {
            Some(card) => Surface::Pin { key, card },
            None => Surface::Unpin { key },
        };
        surface::record(&body.log.record_emitter(), &surface);
        Ok(FOValue::Unit)
    }

    pub(super) fn pin_read(&self, key: &str) -> Result<FOValue, Error> {
        let body = self.loan.get().ok_or_else(no_desk)?;
        Ok(match body.pins.get(key) {
            Some(card) => FOValue::Variant {
                label: "some".into(),
                payload: Some(Box::new(encode_card(card))),
            },
            None => FOValue::Variant {
                label: "none".into(),
                payload: None,
            },
        })
    }

    pub(super) fn pin_list(&self) -> Result<FOValue, Error> {
        let body = self.loan.get().ok_or_else(no_desk)?;
        Ok(Vec::from_iter(body.pins.keys().cloned()).encode())
    }
}
