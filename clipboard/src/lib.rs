//! Where a copy lands: the clipboard of the machine the program runs on, or,
//! across ssh or with no display to reach, the host terminal's by OSC 52.

use std::io::{self, Write};

use base64::{Engine, engine::general_purpose::STANDARD};

/// Text bytes that fit the 100 000-byte sequence OSC 52 tools have assumed every
/// terminal accepts since hterm's `osc52.sh`: less 8 bytes of framing, at
/// base64's 3 bytes in 4.
const OSC52_CAP: usize = (100_000 - 8) / 4 * 3;

/// Where copies land, chosen at the first and held for the session.
///
/// On X11 the text lives in its owner's process.  A tmux session started
/// locally keeps its environment when later attached over ssh, so it still
/// copies locally.
pub struct Clipboard {
    /// Whether the program runs across ssh, where the machine's own clipboard
    /// is not the user's.
    remote: bool,
    sink: Option<Sink>,
}

enum Sink {
    Native(arboard::Clipboard),
    Osc52,
}

/// How much of a copy landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Copied {
    Whole,
    /// Only the last bytes, as many as OSC 52 carries.
    Tail(usize),
}

impl Clipboard {
    /// A clipboard for a program that runs `remote`ly (across ssh) or not.
    #[must_use]
    pub fn new(remote: bool) -> Self {
        Self { remote, sink: None }
    }

    /// Copy `text`; `terminal` is whether the host terminal may be asked, the
    /// only way across ssh or where the native clipboard will not open.
    ///
    /// # Errors
    /// The native write or the write to stdout failed, or the terminal was
    /// needed but not allowed.
    pub fn copy(&mut self, text: &str, terminal: bool) -> io::Result<Copied> {
        let remote = self.remote;
        let sink = self.sink.get_or_insert_with(|| {
            if remote {
                return Sink::Osc52;
            }
            arboard::Clipboard::new().map_or(Sink::Osc52, Sink::Native)
        });
        match sink {
            Sink::Native(c) => c
                .set_text(text)
                .map(|()| Copied::Whole)
                .map_err(io::Error::other),
            Sink::Osc52 if !terminal => Err(io::Error::other(
                "no native clipboard, and the terminal is not known to accept OSC 52",
            )),
            Sink::Osc52 => {
                let tail = tail_bytes(text, OSC52_CAP);
                let mut out = io::stdout().lock();
                out.write_all(osc52(tail).as_bytes())?;
                out.flush()?;
                Ok(if tail.len() < text.len() {
                    Copied::Tail(tail.len())
                } else {
                    Copied::Whole
                })
            }
        }
    }
}

/// OSC 52 putting `text` on the system clipboard.  Terminated with BEL, not ST:
/// tmux relays a copy through its `Ms` capability, which emits BEL, and some
/// terminals implementing OSC 52 never learned ST.  A terminal drops an
/// oversized sequence in silence, hence [`OSC52_CAP`]; tmux passes it on only
/// with `set-clipboard on` (and, from 3.3, `allow-passthrough on`).
fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", STANDARD.encode(text))
}

/// The last `cap` bytes of `text`, snapped forward to a char boundary; all of
/// `text` when it already fits.
fn tail_bytes(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let start = text.ceil_char_boundary(text.len() - cap);
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_frames_base64_with_bel() {
        assert_eq!(osc52("hello"), "\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn cap_is_the_largest_that_fits() {
        assert!(osc52(&"x".repeat(OSC52_CAP)).len() <= 100_000);
        assert!(osc52(&"x".repeat(OSC52_CAP + 1)).len() > 100_000);
    }

    #[test]
    fn tail_snaps_to_a_char_boundary() {
        assert_eq!(tail_bytes("abé", 2), "é");
        assert_eq!(tail_bytes("abé", 9), "abé");
    }
}
