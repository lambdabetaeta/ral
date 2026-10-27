//! Net vsock frames are `<u32 LE length> <IPv4 packet>`, with no prologue.
//! Lengths outside `1..=MTU` are refused before payload allocation or I/O.

use crate::link::MTU;

use std::fmt;
use std::io::{self, Read, Write};

/// Any framing or I/O error terminates the link; recovery cannot guess a frame boundary.
#[derive(Debug)]
pub enum Error {
    Empty,
    Oversize { len: u32, limit: usize },
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

fn read_u32(r: &mut impl Read) -> Result<u32, Error> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    Ok(u32::from_le_bytes(len))
}

/// No `BufReader`: read-ahead bytes would be lost when the descriptor changes owners.
/// # Errors
/// Refuses lengths outside `1..=MTU`, short reads or other I/O failures.
pub fn read_frame(r: &mut impl Read) -> Result<Vec<u8>, Error> {
    let len = read_u32(r)?;
    match len {
        0 => Err(Error::Empty),
        n if n as usize > MTU => Err(Error::Oversize { len: n, limit: MTU }),
        n => {
            let mut packet = vec![0u8; n as usize];
            r.read_exact(&mut packet)?;
            Ok(packet)
        }
    }
}

/// # Errors
/// Refuses lengths outside `1..=MTU` or I/O failures.
/// # Panics
/// Panics if an admitted packet length does not fit in a `u32`.
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
        assert_eq!(cursor, []);
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
        wire.extend_from_slice(&[1, 2, 3]);
        let mut cursor = wire.as_slice();
        assert!(matches!(read_frame(&mut cursor), Err(Error::Io(_))));
    }
}
