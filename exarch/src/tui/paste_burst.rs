// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 OpenAI

//! Timing-based fallback for unbracketed paste, inspired by `OpenAI` Codex's
//! `codex-rs/tui/src/bottom_pane/paste_burst.rs` (Apache-2.0).
//!
//! Timing cannot distinguish every paste from typing. This is not a security
//! boundary; bracketed paste is safest. Text normalization belongs to the owner.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const CHAR_GAP: Duration = Duration::from_millis(8);
const SUPPRESS: Duration = Duration::from_millis(120);
#[cfg(windows)]
const IDLE: Duration = Duration::from_millis(60);
#[cfg(not(windows))]
const IDLE: Duration = CHAR_GAP;
const CHUNK_BYTES: usize = 4096;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Input {
    Key(KeyEvent),
    Paste(String),
}

#[derive(Default)]
enum Buffer {
    #[default]
    Empty,
    /// One text key, held until the gap shows whether a burst follows.
    First { key: KeyEvent, ch: char },
    /// A burst's text not yet delivered; empty once delivered mid-burst.
    Burst(String),
}

/// All time is supplied by the caller; storage is bounded to one UTF-8 chunk.
#[derive(Default)]
pub(super) struct PasteBurst {
    buffer: Buffer,
    deadline: Option<Instant>,
    suppress_until: Option<Instant>,
}

impl PasteBurst {
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub(super) fn flush_due(&mut self, now: Instant) -> Vec<Input> {
        if self.suppress_until.is_some_and(|at| now >= at) {
            self.suppress_until = None;
        }
        if self.deadline.is_some_and(|at| now >= at) {
            self.drain()
        } else {
            Vec::new()
        }
    }

    /// Deliver text before a possible owner change without splitting its timing window.
    pub(super) fn flush_pending(&mut self) -> Vec<Input> {
        match &mut self.buffer {
            Buffer::First { key, .. } => {
                let key = *key;
                self.buffer = Buffer::Burst(String::new());
                vec![Input::Key(key)]
            }
            Buffer::Burst(s) if !s.is_empty() => {
                vec![Input::Paste(std::mem::take(s))]
            }
            _ => Vec::new(),
        }
    }

    /// Flush in order before a non-text event or a confirmed owner transition.
    pub(super) fn finish(&mut self) -> Vec<Input> {
        self.suppress_until = None;
        self.drain()
    }

    fn drain(&mut self) -> Vec<Input> {
        self.deadline = None;
        match std::mem::take(&mut self.buffer) {
            Buffer::Empty => Vec::new(),
            Buffer::First { key, .. } => vec![Input::Key(key)],
            Buffer::Burst(s) if s.is_empty() => Vec::new(),
            Buffer::Burst(s) => vec![Input::Paste(s)],
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent, now: Instant) -> Vec<Input> {
        let mut out = self.flush_due(now);
        let text = text_char(key);
        let pending = !matches!(self.buffer, Buffer::Empty);
        let control = matches!(key.code, KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab)
            && (key.modifiers - KeyModifiers::SHIFT).is_empty()
            && (pending || self.suppress_until.is_some());
        if let Some(c) = text.or_else(|| {
            control.then_some(if key.code == KeyCode::Enter {
                '\n'
            } else {
                '\t'
            })
        }) {
            let mut burst = match std::mem::take(&mut self.buffer) {
                Buffer::Empty if !control => {
                    self.buffer = Buffer::First { key, ch: c };
                    self.deadline = Some(now + CHAR_GAP);
                    return out;
                }
                Buffer::Empty => String::new(),
                Buffer::First { ch, .. } => ch.to_string(),
                Buffer::Burst(s) => s,
            };
            burst.push(c);
            if burst.len() >= CHUNK_BYTES {
                out.push(Input::Paste(std::mem::take(&mut burst)));
            }
            self.buffer = Buffer::Burst(burst);
            self.suppress_until = Some(now + SUPPRESS);
            self.deadline = Some(now + IDLE);
        } else {
            out.extend(self.finish());
            out.push(Input::Key(key));
        }
        out
    }
}

fn text_char(key: KeyEvent) -> Option<char> {
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    let plain = (key.modifiers - KeyModifiers::SHIFT).is_empty();
    // Windows reports AltGr text as Ctrl+Alt.
    let altgr = cfg!(windows)
        && key
            .modifiers
            .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && (key.modifiers - KeyModifiers::CONTROL - KeyModifiers::ALT - KeyModifiers::SHIFT)
            .is_empty();
    ((plain || altgr) && !c.is_control()).then_some(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn solitary_character_keeps_its_key_semantics_after_eight_ms() {
        let now = Instant::now();
        let mut burst = PasteBurst::default();
        let k = KeyEvent::new(KeyCode::Char('λ'), KeyModifiers::SHIFT);
        assert_eq!(burst.key(k, now), []);
        assert_eq!(
            burst.flush_due(
                (now + CHAR_GAP)
                    .checked_sub(Duration::from_nanos(1))
                    .unwrap()
            ),
            []
        );
        assert_eq!(burst.flush_due(now + CHAR_GAP), vec![Input::Key(k)]);
        assert_eq!(burst.flush_due(now + SUPPRESS), []);
    }

    #[test]
    fn rapid_controls_are_text_and_later_enter_is_one_key() {
        let now = Instant::now();
        let mut burst = PasteBurst::default();
        for (i, code) in [
            KeyCode::Char('x'),
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Char('λ'),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                burst.key(key(code), now + Duration::from_millis(i as u64)),
                []
            );
        }
        assert_eq!(
            burst.flush_due(now + IDLE + Duration::from_millis(3)),
            vec![Input::Paste("x\n\tλ".into())]
        );
        let enter = key(KeyCode::Enter);
        assert_eq!(
            burst.key(enter, now + SUPPRESS + Duration::from_millis(3)),
            vec![Input::Key(enter)]
        );
        assert_eq!(burst.finish(), []);
    }

    #[test]
    fn controls_stay_suppressed_after_an_idle_flush() {
        let now = Instant::now();
        let mut burst = PasteBurst::default();
        burst.key(key(KeyCode::Char('a')), now);
        burst.key(key(KeyCode::Char('b')), now);
        assert_eq!(burst.flush_due(now + IDLE), vec![Input::Paste("ab".into())]);
        assert_eq!(burst.key(key(KeyCode::Enter), now + IDLE), []);
        assert_eq!(burst.finish(), vec![Input::Paste("\n".into())]);
    }

    #[test]
    fn incoming_key_observes_expiration_without_a_scheduler_tick() {
        let now = Instant::now();
        let mut burst = PasteBurst::default();
        let a = key(KeyCode::Char('a'));
        let enter = key(KeyCode::Enter);
        burst.key(a, now);
        assert_eq!(
            burst.key(enter, now + CHAR_GAP),
            vec![Input::Key(a), Input::Key(enter)]
        );
        assert_eq!(burst.finish(), []);
    }

    #[test]
    fn explicit_boundary_orders_text_and_cancels_control_suppression() {
        let now = Instant::now();
        let mut burst = PasteBurst::default();
        burst.key(key(KeyCode::Char('a')), now);
        burst.key(key(KeyCode::Char('b')), now);
        let mut delivered = burst.finish();
        delivered.push(Input::Paste("explicit".into()));
        assert_eq!(
            delivered,
            vec![Input::Paste("ab".into()), Input::Paste("explicit".into())]
        );
        let enter = key(KeyCode::Enter);
        assert_eq!(burst.key(enter, now), vec![Input::Key(enter)]);
    }

    #[test]
    fn shifted_controls_are_also_text_inside_a_burst() {
        let now = Instant::now();
        let mut burst = PasteBurst::default();
        burst.key(key(KeyCode::Char('a')), now);
        for code in [KeyCode::Enter, KeyCode::Tab, KeyCode::BackTab] {
            assert_eq!(burst.key(KeyEvent::new(code, KeyModifiers::SHIFT), now), []);
        }
        assert_eq!(burst.finish(), vec![Input::Paste("a\n\t\t".into())]);
    }

    #[test]
    fn possible_owner_change_flush_preserves_the_classifier() {
        let now = Instant::now();
        let mut burst = PasteBurst::default();
        let a = key(KeyCode::Char('a'));
        burst.key(a, now);
        assert_eq!(burst.flush_pending(), vec![Input::Key(a)]);
        assert_eq!(burst.flush_pending(), []);
        assert_eq!(burst.key(key(KeyCode::Enter), now), []);
        assert_eq!(burst.flush_pending(), vec![Input::Paste("\n".into())]);
        assert_eq!(burst.key(key(KeyCode::Tab), now), []);
        assert_eq!(burst.finish(), vec![Input::Paste("\t".into())]);
    }

    #[test]
    fn modifiers_flush_before_the_shortcut_and_reset_classification() {
        let now = Instant::now();
        let mut burst = PasteBurst::default();
        let a = key(KeyCode::Char('a'));
        burst.key(a, now);
        let shortcut = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert_eq!(
            burst.key(shortcut, now),
            vec![Input::Key(a), Input::Key(shortcut)]
        );
        assert_eq!(
            burst.key(key(KeyCode::Enter), now),
            vec![Input::Key(key(KeyCode::Enter))]
        );
    }

    #[test]
    fn finish_after_a_due_flush_only_ends_control_suppression() {
        let now = Instant::now();
        let mut burst = PasteBurst::default();
        burst.key(key(KeyCode::Char('a')), now);
        burst.key(key(KeyCode::Char('b')), now);
        assert_eq!(burst.flush_due(now + IDLE), vec![Input::Paste("ab".into())]);
        assert_eq!(burst.finish(), []);
        let enter = key(KeyCode::Enter);
        assert_eq!(burst.key(enter, now + IDLE), vec![Input::Key(enter)]);
    }

    #[test]
    fn finish_is_idempotent() {
        let now = Instant::now();
        let mut burst = PasteBurst::default();
        let a = key(KeyCode::Char('a'));
        burst.key(a, now);
        assert_eq!(burst.finish(), vec![Input::Key(a)]);
        assert_eq!(burst.finish(), []);
        assert_eq!(burst.deadline(), None);
    }

    #[test]
    fn chunks_are_bounded_without_ending_control_suppression() {
        let now = Instant::now();
        let mut burst = PasteBurst::default();
        let mut text = String::new();
        for _ in 0..10_000 {
            for input in burst.key(key(KeyCode::Char('λ')), now) {
                let Input::Paste(chunk) = input else {
                    panic!("burst emitted a key")
                };
                assert!(chunk.len() <= CHUNK_BYTES + 3);
                text.push_str(&chunk);
            }
        }
        assert_eq!(burst.key(key(KeyCode::Tab), now), []);
        for input in burst.finish() {
            let Input::Paste(chunk) = input else {
                panic!("burst emitted a key")
            };
            text.push_str(&chunk);
        }
        assert_eq!(text, format!("{}\t", "λ".repeat(10_000)));
    }
}
