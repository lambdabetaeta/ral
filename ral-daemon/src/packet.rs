//! The wire format on the net vsock link between the guest pump
//! (`ral-daemon --pump`, `crate::pump`) and the host stack (`guest-net`).
//!
//! This module is deliberately *not* gated on `target_os = "linux"`: the
//! host process links this exact file so the two ends of the wire cannot
//! drift into two codecs that happen to agree today. That is also why the
//! module holds no policy and no I/O beyond the socket itself — it only
//! knows how to turn bytes into frames and back.
//!
//! ```text
//! packet ::= <u32 LE length> <IPv4 packet>
//! ```
//!
//! Packet framing begins on the wire immediately: the first bytes either
//! side sends are one frame, with nothing ahead of it.

use std::fmt;
use std::io::{self, Read, Write};

/// The interface MTU carried on the net wire in both directions.
///
/// Chosen so a length prefix can be trusted at face value: a peer that
/// claims more than this is not describing a bigger packet, it is
/// malfunctioning.
pub const MTU: usize = 1500;

/// Something the peer sent that cannot be treated as this wire's data.
///
/// Every variant here is a broken peer, not a recoverable condition: the
/// only correct response to any of them is to kill the link, not to guess
/// at what was meant. In particular a length of `0`, or one past [`MTU`],
/// is refused before any allocation or read happens for it — never a
/// buffer sized off a number the peer made up, which is how a wedged pump
/// starts.
#[derive(Debug)]
pub enum Error {
    /// A frame was declared to have zero length.
    Empty,
    /// A declared length exceeds [`MTU`].
    Oversize { len: u32, limit: usize },
    /// The stream ended, or a read/write failed, mid-message.
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the peer sent a zero-length frame"),
            Self::Oversize { len, limit } => {
                write!(
                    f,
                    "the peer claimed {len} bytes, over the {limit}-byte limit"
                )
            }
            Self::Io(err) => write!(f, "the net link failed: {err}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Read exactly `len` bytes with raw [`Read::read`] calls.
///
/// Never a [`std::io::BufReader`]: a `BufReader` pulls ahead of the byte
/// count it is asked for, so a read for one frame would pull whatever
/// packets follow it into a buffer that lives on the reader the pump
/// discards once the fd is handed to its own two threads (`crate::pump` on
/// the guest side; the analogous split in `guest-net` on the host side).
/// Those packets would then be gone from the fd and never reach the code
/// that owns it next. [`Read::read_exact`] loops over raw `read` the same
/// way a hand-written loop would and buffers nothing beyond the slice it is
/// given, so it carries none of that risk.
fn read_exact_raw(r: &mut impl Read, buf: &mut [u8]) -> Result<(), Error> {
    r.read_exact(buf).map_err(Error::from)
}

fn read_u32(r: &mut impl Read) -> Result<u32, Error> {
    let mut len = [0u8; 4];
    read_exact_raw(r, &mut len)?;
    Ok(u32::from_le_bytes(len))
}

/// Read one length-prefixed IPv4 packet.
///
/// # Errors
/// [`Error::Empty`] or [`Error::Oversize`] on a length outside `1..=MTU`,
/// [`Error::Io`] on a short read (including a clean EOF mid-frame) or any
/// other I/O failure.
pub fn read_frame(r: &mut impl Read) -> Result<Vec<u8>, Error> {
    let len = read_u32(r)?;
    match len {
        0 => Err(Error::Empty),
        n if n as usize > MTU => Err(Error::Oversize { len: n, limit: MTU }),
        n => {
            let mut packet = vec![0u8; n as usize];
            read_exact_raw(r, &mut packet)?;
            Ok(packet)
        }
    }
}

/// Write one length-prefixed IPv4 packet.
///
/// # Errors
/// [`Error::Empty`] or [`Error::Oversize`] if `packet`'s own length is
/// outside `1..=MTU` — the local side refusing to hand the peer a frame it
/// would have to refuse right back — or [`Error::Io`] if the write fails.
///
/// # Panics
/// Never: the `n <= MTU` arm is only reached once `n` has been checked to
/// fit in a `u32` many times over.
pub fn write_frame(w: &mut impl Write, packet: &[u8]) -> Result<(), Error> {
    match packet.len() {
        0 => Err(Error::Empty),
        n if n > MTU => Err(Error::Oversize {
            len: u32::try_from(n).unwrap_or(u32::MAX),
            limit: MTU,
        }),
        n => {
            let len = u32::try_from(n).expect("n <= MTU, which fits in a u32");
            w.write_all(&len.to_le_bytes())?;
            w.write_all(packet)?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips() {
        let packet = vec![1, 2, 3, 4, 5];
        let mut wire = Vec::new();
        write_frame(&mut wire, &packet).unwrap();
        let mut cursor = wire.as_slice();
        assert_eq!(read_frame(&mut cursor).unwrap(), packet);
        assert!(cursor.is_empty());
    }

    #[test]
    fn a_frame_declaring_zero_length_is_refused() {
        let wire = 0u32.to_le_bytes();
        let mut cursor = wire.as_slice();
        assert!(matches!(read_frame(&mut cursor), Err(Error::Empty)));
    }

    #[test]
    fn writing_an_empty_packet_is_refused() {
        let mut wire = Vec::new();
        assert!(matches!(write_frame(&mut wire, &[]), Err(Error::Empty)));
    }

    #[test]
    fn a_frame_over_the_mtu_is_refused_without_allocating() {
        let wire = (u32::try_from(MTU).unwrap() + 1).to_le_bytes();
        let mut cursor = wire.as_slice();
        assert!(
            matches!(read_frame(&mut cursor), Err(Error::Oversize { len, .. }) if len as usize == MTU + 1)
        );
    }

    #[test]
    fn writing_an_oversize_packet_is_refused() {
        let packet = vec![0u8; MTU + 1];
        let mut wire = Vec::new();
        assert!(matches!(
            write_frame(&mut wire, &packet),
            Err(Error::Oversize { .. })
        ));
    }

    #[test]
    fn a_short_read_mid_frame_is_an_io_error() {
        let mut wire = 10u32.to_le_bytes().to_vec();
        wire.extend_from_slice(&[1, 2, 3]); // declares 10 bytes, gives 3
        let mut cursor = wire.as_slice();
        assert!(matches!(read_frame(&mut cursor), Err(Error::Io(_))));
    }

    /// The wire carries no prologue: the very first bytes either side sends
    /// already are one frame, and reading them back needs nothing before it.
    #[test]
    fn the_first_bytes_on_the_wire_are_a_frame() {
        let packet = vec![9, 8, 7];
        let mut wire = Vec::new();
        write_frame(&mut wire, &packet).unwrap();
        let mut cursor = wire.as_slice();
        assert_eq!(read_frame(&mut cursor).unwrap(), packet);
        assert!(cursor.is_empty());
    }
}
