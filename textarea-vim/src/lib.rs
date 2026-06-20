//! Vim-mode emulation over [`ratatui_textarea::TextArea`].
//!
//! `ratatui-textarea` ships vim only as an *example*, not as library API.
//! This crate vendors that example's state machine — the [`Mode`],
//! [`Transition`], and [`Vim`] types — so both REPL surfaces that edit in a
//! `TextArea` (ral's structural worksheet and exarch's TUI prompt) share one
//! implementation instead of each carrying a copy.
//!
//! Usage mirrors the upstream example's driver loop: each keystroke, feed
//! [`Vim::transition`] the [`Input`] and the live `TextArea`, then fold the
//! returned [`Transition`] back into the `Vim` state:
//!
//! ```ignore
//! vim = match vim.transition(key_event.into(), &mut textarea) {
//!     Transition::Mode(m) if vim.mode() != m => Vim::new(m),
//!     Transition::Nop | Transition::Mode(_) => vim,
//!     Transition::Pending(p) => vim.with_pending(p),
//!     Transition::Quit => vim, // no editor to quit in a REPL: a no-op
//! };
//! ```
//!
//! The presentation concerns the upstream example bundled in — the mode-named
//! `Block` border and the per-mode cursor `Style` — are intentionally *not*
//! vendored: each frontend owns its own chrome and maps [`Mode`] to a cursor
//! style in its own palette.
//!
//! ## Attribution
//!
//! The [`Mode`], [`Transition`], and [`Vim`] definitions and the body of
//! [`Vim::transition`] are vendored, essentially verbatim, from
//! `ratatui-textarea` 0.9.2 `examples/vim.rs` (originally `tui-textarea`),
//! Copyright (c) 2022 rhysd, under the MIT License.  Local changes: the items
//! are made `pub`; the demo-only `Mode::block`/`Mode::cursor_style` and the
//! `fn main` driver are dropped; imports are rewritten to this crate's deps;
//! and a [`Vim::mode`] accessor is added (the example reached the field
//! directly from its own module).

use ratatui_textarea::{CursorMove, Key, Scrolling, TextArea};

pub use ratatui_textarea::Input;

use std::fmt;

/// The active editing mode of the Vim emulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Replace(bool), // true = replace once (r), false = overtype (R)
    Visual,
    Operator(char),
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        match self {
            Self::Normal => write!(f, "NORMAL"),
            Self::Insert => write!(f, "INSERT"),
            Self::Replace(_) => write!(f, "REPLACE"),
            Self::Visual => write!(f, "VISUAL"),
            Self::Operator(c) => write!(f, "OPERATOR({c})"),
        }
    }
}

/// How a keystroke moves the Vim emulation forward.
pub enum Transition {
    /// The key was consumed; stay in the current mode.
    Nop,
    /// Switch to this mode (possibly the same one).
    Mode(Mode),
    /// First key of a two-key sequence (e.g. the first `g` of `gg`); the
    /// caller folds it back in via [`Vim::with_pending`].
    Pending(Input),
    /// The `q` key in normal mode.  There is no editor to quit in a REPL
    /// prompt, so callers map this to a no-op.
    Quit,
}

/// The Vim emulation state: the current [`Mode`] plus any pending first key
/// of a two-key sequence.
pub struct Vim {
    mode: Mode,
    pending: Input, // Pending input to handle a sequence with two keys like gg
}

impl Vim {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            pending: Input::default(),
        }
    }

    pub fn with_pending(self, pending: Input) -> Self {
        Self {
            mode: self.mode,
            pending,
        }
    }

    /// The mode the emulation is currently in.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    fn is_before_line_end(textarea: &TextArea<'_>) -> bool {
        let cursor = textarea.cursor();
        cursor.1 < textarea.lines()[cursor.0].len().saturating_sub(1)
    }

    /// Apply one keystroke to `textarea`, returning the resulting state
    /// [`Transition`].  Mutates `textarea` in place for motions and edits.
    pub fn transition(&self, input: Input, textarea: &mut TextArea<'_>) -> Transition {
        if input.key == Key::Null {
            return Transition::Nop;
        }

        match self.mode {
            Mode::Normal | Mode::Visual | Mode::Operator(_) => {
                match input {
                    Input {
                        key: Key::Char('h') | Key::Left,
                        ..
                    } => textarea.move_cursor(CursorMove::Back),
                    Input {
                        key: Key::Char('j') | Key::Down,
                        ..
                    } => textarea.move_cursor(CursorMove::Down),
                    Input {
                        key: Key::Char('k') | Key::Up,
                        ..
                    } => textarea.move_cursor(CursorMove::Up),
                    Input {
                        key: Key::Char('l') | Key::Right,
                        ..
                    } => textarea.move_cursor(CursorMove::Forward),
                    Input {
                        key: Key::Char('w'),
                        ..
                    } => textarea.move_cursor(CursorMove::WordForward),
                    Input {
                        key: Key::Char('e'),
                        ctrl: false,
                        ..
                    } => {
                        textarea.move_cursor(CursorMove::WordEnd);
                        if matches!(self.mode, Mode::Operator(_)) {
                            textarea.move_cursor(CursorMove::Forward); // Include the text under the cursor
                        }
                    }
                    Input {
                        key: Key::Char('b'),
                        ctrl: false,
                        ..
                    } => textarea.move_cursor(CursorMove::WordBack),
                    Input {
                        key: Key::Char('^'),
                        ..
                    } => textarea.move_cursor(CursorMove::Head),
                    Input {
                        key: Key::Char('$'),
                        ..
                    } => textarea.move_cursor(CursorMove::End),
                    Input {
                        key: Key::Char('D'),
                        ..
                    } => {
                        textarea.delete_line_by_end();
                        return Transition::Mode(Mode::Normal);
                    }
                    Input {
                        key: Key::Char('C'),
                        ..
                    } => {
                        textarea.delete_line_by_end();
                        textarea.cancel_selection();
                        return Transition::Mode(Mode::Insert);
                    }
                    Input {
                        key: Key::Char('p'),
                        ..
                    } => {
                        textarea.paste();
                        return Transition::Mode(Mode::Normal);
                    }
                    Input {
                        key: Key::Char('u'),
                        ctrl: false,
                        ..
                    } => {
                        textarea.undo();
                        return Transition::Mode(Mode::Normal);
                    }
                    Input {
                        key: Key::Char('r'),
                        ctrl: true,
                        ..
                    } => {
                        textarea.redo();
                        return Transition::Mode(Mode::Normal);
                    }
                    Input {
                        key: Key::Char('x'),
                        ..
                    } if Self::is_before_line_end(textarea)
                        || textarea.lines()[textarea.cursor().0].is_empty() =>
                    {
                        textarea.delete_next_char();
                        return Transition::Mode(Mode::Normal);
                    }
                    Input {
                        key: Key::Char('i'),
                        ..
                    } => {
                        textarea.cancel_selection();
                        return Transition::Mode(Mode::Insert);
                    }
                    Input {
                        key: Key::Char('a'),
                        ..
                    } => {
                        textarea.cancel_selection();
                        if Self::is_before_line_end(textarea) {
                            textarea.move_cursor(CursorMove::Forward);
                        }
                        return Transition::Mode(Mode::Insert);
                    }
                    Input {
                        key: Key::Char('A'),
                        ..
                    } => {
                        textarea.cancel_selection();
                        textarea.move_cursor(CursorMove::End);
                        return Transition::Mode(Mode::Insert);
                    }
                    Input {
                        key: Key::Char('o'),
                        ..
                    } => {
                        textarea.move_cursor(CursorMove::End);
                        textarea.insert_newline();
                        return Transition::Mode(Mode::Insert);
                    }
                    Input {
                        key: Key::Char('O'),
                        ..
                    } => {
                        textarea.move_cursor(CursorMove::Head);
                        textarea.insert_newline();
                        textarea.move_cursor(CursorMove::Up);
                        return Transition::Mode(Mode::Insert);
                    }
                    Input {
                        key: Key::Char('I'),
                        ..
                    } => {
                        textarea.cancel_selection();
                        textarea.move_cursor(CursorMove::Head);
                        return Transition::Mode(Mode::Insert);
                    }
                    Input {
                        key: Key::Char('J'),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Normal => {
                        let row = textarea.cursor().0;
                        if row + 1 < textarea.lines().len() {
                            textarea.move_cursor(CursorMove::End);
                            textarea.delete_next_char(); // delete newline
                            textarea.insert_char(' ');
                        }
                        return Transition::Mode(Mode::Normal);
                    }
                    Input {
                        key: Key::Char('J'),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Visual => {
                        // Join all lines in selection
                        let (start, end) = {
                            let sel = textarea.selection_range();
                            match sel {
                                Some((s, e)) => (s.0, e.0),
                                None => return Transition::Mode(Mode::Normal),
                            }
                        };
                        textarea.cancel_selection();
                        textarea.move_cursor(CursorMove::Jump(start as u16, 0));
                        for _ in start..end {
                            textarea.move_cursor(CursorMove::End);
                            textarea.delete_next_char();
                            textarea.insert_char(' ');
                        }
                        return Transition::Mode(Mode::Normal);
                    }
                    Input {
                        key: Key::Char('S'),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Normal => {
                        textarea.move_cursor(CursorMove::Head);
                        textarea.delete_line_by_end();
                        return Transition::Mode(Mode::Insert);
                    }
                    Input {
                        key: Key::Char('S'),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Visual => {
                        textarea.move_cursor(CursorMove::Forward);
                        textarea.cut();
                        return Transition::Mode(Mode::Insert);
                    }
                    Input {
                        key: Key::Char('r'),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Normal => {
                        return Transition::Mode(Mode::Replace(true));
                    }
                    Input {
                        key: Key::Char('R'),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Normal => {
                        return Transition::Mode(Mode::Replace(false));
                    }
                    Input {
                        key: Key::Char('q'),
                        ..
                    } => return Transition::Quit,
                    Input {
                        key: Key::Char('e'),
                        ctrl: true,
                        ..
                    } => textarea.scroll((1, 0)),
                    Input {
                        key: Key::Char('y'),
                        ctrl: true,
                        ..
                    } => textarea.scroll((-1, 0)),
                    Input {
                        key: Key::Char('d'),
                        ctrl: true,
                        ..
                    } => textarea.scroll(Scrolling::HalfPageDown),
                    Input {
                        key: Key::Char('u'),
                        ctrl: true,
                        ..
                    } => textarea.scroll(Scrolling::HalfPageUp),
                    Input {
                        key: Key::Char('f'),
                        ctrl: true,
                        ..
                    } => textarea.scroll(Scrolling::PageDown),
                    Input {
                        key: Key::Char('b'),
                        ctrl: true,
                        ..
                    } => textarea.scroll(Scrolling::PageUp),
                    Input {
                        key: Key::Char('v'),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Normal => {
                        textarea.start_selection();
                        return Transition::Mode(Mode::Visual);
                    }
                    Input {
                        key: Key::Char('V'),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Normal => {
                        textarea.move_cursor(CursorMove::Head);
                        textarea.start_selection();
                        textarea.move_cursor(CursorMove::End);
                        return Transition::Mode(Mode::Visual);
                    }
                    Input { key: Key::Esc, .. }
                    | Input {
                        key: Key::Char('['),
                        ctrl: true,
                        ..
                    }
                    | Input {
                        key: Key::Char('v'),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Visual => {
                        textarea.cancel_selection();
                        return Transition::Mode(Mode::Normal);
                    }
                    Input {
                        key: Key::Char('g'),
                        ctrl: false,
                        ..
                    } if matches!(
                        self.pending,
                        Input {
                            key: Key::Char('g'),
                            ctrl: false,
                            ..
                        }
                    ) =>
                    {
                        textarea.move_cursor(CursorMove::Top)
                    }
                    Input {
                        key: Key::Char('G'),
                        ctrl: false,
                        ..
                    } => textarea.move_cursor(CursorMove::Bottom),
                    Input {
                        key: Key::Char(c),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Operator(c) => {
                        // Handle yy, dd, cc. (This is not strictly the same behavior as Vim)
                        textarea.move_cursor(CursorMove::Head);
                        textarea.start_selection();
                        let cursor = textarea.cursor();
                        textarea.move_cursor(CursorMove::Down);
                        if cursor == textarea.cursor() {
                            textarea.move_cursor(CursorMove::End); // At the last line, move to end of the line instead
                        }
                    }
                    Input {
                        key: Key::Char(op @ ('y' | 'd' | 'c')),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Normal => {
                        textarea.start_selection();
                        return Transition::Mode(Mode::Operator(op));
                    }
                    Input {
                        key: Key::Char('y'),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Visual => {
                        textarea.move_cursor(CursorMove::Forward); // Vim's text selection is inclusive
                        textarea.copy();
                        return Transition::Mode(Mode::Normal);
                    }
                    Input {
                        key: Key::Char('d'),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Visual => {
                        textarea.move_cursor(CursorMove::Forward); // Vim's text selection is inclusive
                        textarea.cut();
                        return Transition::Mode(Mode::Normal);
                    }
                    Input {
                        key: Key::Char('c'),
                        ctrl: false,
                        ..
                    } if self.mode == Mode::Visual => {
                        textarea.move_cursor(CursorMove::Forward); // Vim's text selection is inclusive
                        textarea.cut();
                        return Transition::Mode(Mode::Insert);
                    }
                    input => return Transition::Pending(input),
                }

                // Handle the pending operator
                match self.mode {
                    Mode::Operator('y') => {
                        textarea.copy();
                        Transition::Mode(Mode::Normal)
                    }
                    Mode::Operator('d') => {
                        textarea.cut();
                        Transition::Mode(Mode::Normal)
                    }
                    Mode::Operator('c') => {
                        textarea.cut();
                        Transition::Mode(Mode::Insert)
                    }
                    _ => Transition::Nop,
                }
            }
            Mode::Insert => match input {
                Input { key: Key::Esc, .. }
                | Input {
                    key: Key::Char('c'),
                    ctrl: true,
                    ..
                }
                | Input {
                    key: Key::Char('['),
                    ctrl: true,
                    ..
                } => Transition::Mode(Mode::Normal),
                input => {
                    textarea.input(input); // Use default key mappings in insert mode
                    Transition::Mode(Mode::Insert)
                }
            },
            Mode::Replace(once) => match input {
                Input { key: Key::Esc, .. }
                | Input {
                    key: Key::Char('['),
                    ctrl: true,
                    ..
                } => Transition::Mode(Mode::Normal),
                Input {
                    key: Key::Char(c),
                    ctrl: false,
                    alt: false,
                    ..
                } => {
                    // Replace the character under the cursor
                    if Self::is_before_line_end(textarea)
                        || textarea.lines()[textarea.cursor().0].len() == textarea.cursor().1
                    {
                        textarea.delete_next_char();
                        textarea.insert_char(c);
                    }
                    if once {
                        Transition::Mode(Mode::Normal)
                    } else {
                        Transition::Mode(Mode::Replace(false))
                    }
                }
                _ => Transition::Mode(if once {
                    Mode::Normal
                } else {
                    Mode::Replace(false)
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a fresh `Vim` (starting in `start`) through `keys`, folding
    /// transitions back exactly as a frontend driver loop would.  Returns the
    /// final mode and the textarea's resulting lines.
    fn run(start: Mode, initial: &str, keys: &[Input]) -> (Mode, Vec<String>) {
        let mut textarea = TextArea::default();
        textarea.insert_str(initial);
        let mut vim = Vim::new(start);
        for key in keys.iter().cloned() {
            vim = match vim.transition(key, &mut textarea) {
                Transition::Mode(m) if vim.mode() != m => Vim::new(m),
                Transition::Nop | Transition::Mode(_) => vim,
                Transition::Pending(p) => vim.with_pending(p),
                Transition::Quit => vim,
            };
        }
        (vim.mode(), textarea.lines().to_vec())
    }

    fn ch(c: char) -> Input {
        Input {
            key: Key::Char(c),
            ..Input::default()
        }
    }

    #[test]
    fn i_enters_insert_and_esc_returns_to_normal() {
        let esc = Input {
            key: Key::Esc,
            ..Input::default()
        };
        // `insert_str` leaves the cursor at the end, so `^` first parks it at
        // the line head; `i` then inserts before it.
        let (mode, lines) = run(Mode::Normal, "ab", &[ch('^'), ch('i'), ch('X'), esc]);
        assert_eq!(mode, Mode::Normal);
        assert_eq!(lines, vec!["Xab".to_string()]);
    }

    #[test]
    fn x_deletes_char_under_cursor() {
        // `^` parks the cursor on the first char; `x` deletes it.
        let (mode, lines) = run(Mode::Normal, "abc", &[ch('^'), ch('x')]);
        assert_eq!(mode, Mode::Normal);
        assert_eq!(lines, vec!["bc".to_string()]);
    }

    #[test]
    fn dd_deletes_the_line() {
        // dd on the only line clears it to empty.
        let (mode, lines) = run(Mode::Normal, "hello", &[ch('d'), ch('d')]);
        assert_eq!(mode, Mode::Normal);
        assert_eq!(lines, vec![String::new()]);
    }

    #[test]
    fn q_in_normal_mode_is_quit() {
        let mut textarea = TextArea::default();
        let vim = Vim::new(Mode::Normal);
        assert!(matches!(
            vim.transition(ch('q'), &mut textarea),
            Transition::Quit
        ));
    }
}
