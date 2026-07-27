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
//! packet   ::= <u32 LE length> <IPv4 packet>
//! prologue ::= <u32 total> <u32 len> <resolv.conf> <u32 len> <CA PEM>
//! ```
//!
//! The prologue rides the same stream, once, before the first packet: it is
//! how the host hands the guest its `resolv.conf` and proxy CA without a
//! third vsock port or a detour through `ral_core`'s own protocol.

use std::fmt;
use std::io::{self, Read, Write};

/// The interface MTU carried on the net wire in both directions.
///
/// Chosen so a length prefix can be trusted at face value: a peer that
/// claims more than this is not describing a bigger packet, it is
/// malfunctioning.
pub const MTU: usize = 1500;

/// The most a prologue may declare.
///
/// The prologue's blobs are a `resolv.conf` and a certificate chain, not
/// packets, so [`MTU`] is the wrong bound for them — but *some* bound is
/// required for the same reason: the length is a number the peer made up,
/// and a peer that says four gigabytes must be refused before the
/// allocation, not after it.
pub const PROLOGUE_MAX: usize = 1 << 20;

/// Something the peer sent that cannot be treated as this wire's data.
///
/// Every variant here is a broken peer, not a recoverable condition: the
/// only correct response to any of them is to kill the link, not to guess
/// at what was meant. In particular a length of `0`, or one past the bound
/// its message type carries ([`MTU`] for a packet, [`PROLOGUE_MAX`] for a
/// prologue), is refused before any allocation or read happens for it —
/// never a buffer sized off a number the peer made up, which is how a
/// wedged pump starts.
#[derive(Debug)]
pub enum Error {
    /// A frame or prologue blob was declared to have zero length.
    Empty,
    /// A declared length exceeds what its message type may carry.
    Oversize { len: u32, limit: usize },
    /// The stream ended, or a read/write failed, mid-message.
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("the peer sent a zero-length frame"),
            Self::Oversize { len, limit } => {
                write!(f, "the peer claimed {len} bytes, over the {limit}-byte limit")
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
/// count it is asked for, so the first read after the prologue would pull
/// whatever packets follow it into a buffer that lives on the reader the
/// pump discards once the fd is handed to its own two threads (`crate::pump`
/// on the guest side; the analogous split in `guest-net` on the host side).
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
        n if n as usize > MTU => Err(Error::Oversize {
            len: n,
            limit: MTU,
        }),
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

/// The `resolv.conf` and proxy CA the host hands the guest once, at the
/// head of the net stream, before the first packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prologue {
    pub resolv_conf: Vec<u8>,
    pub ca_pem: Vec<u8>,
}

impl Prologue {
    /// Encode as `<u32 total> <u32 len> resolv.conf <u32 len> CA PEM`,
    /// `total` covering everything after itself.
    ///
    /// # Panics
    /// If `resolv_conf`, `ca_pem`, or their sum exceeds `u32::MAX` bytes —
    /// not a real boot's resolver config or proxy certificate.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let blob_len = |n: usize| {
            u32::try_from(n)
                .expect("resolv.conf and a CA PEM are not gigabytes")
                .to_le_bytes()
        };
        let mut body = Vec::with_capacity(8 + self.resolv_conf.len() + self.ca_pem.len());
        for blob in [&self.resolv_conf, &self.ca_pem] {
            body.extend_from_slice(&blob_len(blob.len()));
            body.extend_from_slice(blob);
        }
        let mut out = Vec::with_capacity(4 + body.len());
        out.extend_from_slice(&blob_len(body.len()));
        out.extend(body);
        out
    }

    /// Read a prologue written by [`Self::encode`].
    ///
    /// The `resolv.conf` blob may be empty (a networked boot with no
    /// resolver to hand down is not this module's business to refuse); the
    /// CA PEM may be empty too — a caller with no proxy to trust for is
    /// `guest-net`'s decision, not this codec's.
    ///
    /// # Errors
    /// [`Error::Oversize`] on a `total` past [`PROLOGUE_MAX`], refused
    /// before the body is allocated; [`Error::Io`] on a short read,
    /// including a `total` or blob length that runs past what the stream
    /// actually holds.
    pub fn parse(r: &mut impl Read) -> Result<Self, Error> {
        let total = read_u32(r)?;
        if total as usize > PROLOGUE_MAX {
            return Err(Error::Oversize {
                len: total,
                limit: PROLOGUE_MAX,
            });
        }
        let mut body = vec![0u8; total as usize];
        read_exact_raw(r, &mut body)?;
        let mut rest = body.as_slice();
        let resolv_conf = take_blob(&mut rest)?;
        let ca_pem = take_blob(&mut rest)?;
        Ok(Self {
            resolv_conf,
            ca_pem,
        })
    }
}

/// Take one `<u32 len> <bytes>` blob off the front of `rest`, the prologue
/// body already being fully in memory by the time this runs.
fn take_blob(rest: &mut &[u8]) -> Result<Vec<u8>, Error> {
    let eof = || Error::Io(io::Error::from(io::ErrorKind::UnexpectedEof));
    let (len_bytes, after_len) = rest.split_at_checked(4).ok_or_else(eof)?;
    let len = u32::from_le_bytes(len_bytes.try_into().expect("checked to 4 bytes")) as usize;
    let (blob, after_blob) = after_len.split_at_checked(len).ok_or_else(eof)?;
    *rest = after_blob;
    Ok(blob.to_vec())
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
    fn a_prologue_declaring_a_gigabyte_is_refused_without_allocating() {
        let wire = u32::MAX.to_le_bytes();
        let mut cursor = wire.as_slice();
        assert!(matches!(
            Prologue::parse(&mut cursor),
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

    #[test]
    fn a_prologue_round_trips() {
        let prologue = Prologue {
            resolv_conf: b"nameserver 10.0.2.2\n".to_vec(),
            ca_pem: b"-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n".to_vec(),
        };
        let wire = prologue.encode();
        let mut cursor = wire.as_slice();
        assert_eq!(Prologue::parse(&mut cursor).unwrap(), prologue);
        assert!(cursor.is_empty());
    }

    #[test]
    fn a_prologue_with_an_empty_pem_round_trips() {
        let prologue = Prologue {
            resolv_conf: b"nameserver 10.0.2.2\n".to_vec(),
            ca_pem: Vec::new(),
        };
        let wire = prologue.encode();
        let mut cursor = wire.as_slice();
        assert_eq!(Prologue::parse(&mut cursor).unwrap(), prologue);
    }

    #[test]
    fn a_truncated_prologue_is_an_io_error() {
        let full = Prologue {
            resolv_conf: b"nameserver 10.0.2.2\n".to_vec(),
            ca_pem: b"cert bytes".to_vec(),
        }
        .encode();
        let mut cursor = &full[..full.len() - 3];
        assert!(matches!(Prologue::parse(&mut cursor), Err(Error::Io(_))));
    }
}
