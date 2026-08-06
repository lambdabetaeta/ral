//! The 16-byte hello that opens every connection on [`crate::AGENT_PORT`].
//!
//! This sits *below* the frame protocol, on purpose: a stray dial — anything
//! that fails to speak this preamble — is closed before a single protocol
//! byte is parsed. The layout is the 8-byte magic `b"ralagent"` followed by
//! a `u64` token, little-endian — 16 bytes exactly.
//!
//! The magic sorts the confused from the hostile and stops neither: it is
//! published with the binary, so anyone can write it. **The token is the
//! defense** — minted from the OS entropy source, single-use, dead with its
//! pending hatch — and this module has no opinion on how one is minted; it
//! only encodes and decodes the sixteen bytes.

use std::fmt;
use std::io::{self, Read, Write};

/// The magic byte string, sized so it and a `u64` token exactly fill
/// [`LEN`].
pub const MAGIC: [u8; 8] = *b"ralagent";

/// The whole preamble's length in bytes: 8 bytes of magic, 8 of token.
pub const LEN: usize = 16;

/// A decoded preamble: nothing but the token, since the magic — once
/// checked — carries no further information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preamble {
    pub token: u64,
}

/// Why a preamble did not decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadMagic;

impl fmt::Display for BadMagic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "this dial did not speak the agent preamble — its first 8 bytes were not the agent \
             magic. Was something other than a hatched engine dialing this port?",
        )
    }
}

impl std::error::Error for BadMagic {}

impl Preamble {
    /// Encode this preamble's token into the 16 bytes a fresh agent-port
    /// connection opens with.
    #[must_use]
    pub fn encode(token: u64) -> [u8; LEN] {
        let mut bytes = [0u8; LEN];
        bytes[..8].copy_from_slice(&MAGIC);
        bytes[8..].copy_from_slice(&token.to_le_bytes());
        bytes
    }

    /// Decode a preamble from exactly [`LEN`] bytes.
    ///
    /// # Errors
    /// Returns [`BadMagic`] if the first 8 bytes are not [`MAGIC`] — a dial
    /// this is not.
    pub fn decode(bytes: &[u8; LEN]) -> Result<Self, BadMagic> {
        if bytes[..8] != MAGIC {
            return Err(BadMagic);
        }
        let token = u64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        Ok(Self { token })
    }
}

/// Write `token`'s preamble to `writer`.
///
/// # Errors
/// Returns whatever writing the 16 bytes returns.
pub fn write(writer: &mut impl Write, token: u64) -> io::Result<()> {
    writer.write_all(&Preamble::encode(token))
}

/// Read and decode a preamble from `reader`.
///
/// # Errors
/// Returns an [`io::Error`] if the 16 bytes cannot be read, or one of kind
/// [`io::ErrorKind::InvalidData`] carrying [`BadMagic`]'s sentence if they
/// are not the agent preamble.
pub fn read(reader: &mut impl Read) -> io::Result<Preamble> {
    let mut bytes = [0u8; LEN];
    reader.read_exact(&mut bytes)?;
    Preamble::decode(&bytes).map_err(|cause| io::Error::new(io::ErrorKind::InvalidData, cause))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A preamble written and read back over a real socketpair carries the
    /// token unchanged — the whole point of the contract.
    #[cfg(unix)]
    #[test]
    fn a_preamble_round_trips_over_a_socketpair() {
        let (mut a, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
        write(&mut a, 0x1234_5678_9abc_def0).unwrap();
        let preamble = read(&mut b).unwrap();
        assert_eq!(preamble.token, 0x1234_5678_9abc_def0);
    }

    /// The encoded form is exactly 16 bytes: 8 of magic, then the token
    /// little-endian.
    #[test]
    fn encoding_lays_out_magic_then_token_little_endian() {
        let bytes = Preamble::encode(1);
        assert_eq!(bytes.len(), LEN);
        assert_eq!(&bytes[..8], &MAGIC);
        assert_eq!(&bytes[8..], &1u64.to_le_bytes());
    }

    /// Anything whose first 8 bytes are not the magic is refused, not
    /// silently reinterpreted.
    #[test]
    fn a_stray_dial_is_refused_by_the_magic() {
        let mut bytes = Preamble::encode(42);
        bytes[0] = !bytes[0];
        assert_eq!(Preamble::decode(&bytes), Err(BadMagic));
    }

    /// The same refusal, read off a stream rather than decoded from bytes
    /// already in hand — the shape the host actually meets a stray dial in.
    #[cfg(unix)]
    #[test]
    fn a_stray_dial_over_a_socketpair_is_refused_honestly() {
        let (mut a, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
        a.write_all(b"not-an-agent-dial").unwrap();
        drop(a);
        let error = read(&mut b).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(
            error
                .to_string()
                .contains("did not speak the agent preamble")
        );
    }
}
