//! The portable, host-facing half of the agent hatch protocol.
//!
//! A Linux guest writes this preamble before speaking the ordinary wire
//! protocol; the host may be Unix or Windows, so decoding cannot live in the
//! guest's Unix-only process-spawning module.

/// The agent-port preamble's published magic.  It separates stray dials from
/// hatched children; the secret token in the following eight bytes provides
/// the correlation.
pub const MAGIC: [u8; 8] = *b"ralagent";

/// Read and validate a fresh dial's 16-byte agent-port preamble.
///
/// # Errors
/// Returns a sentence if the stream closes before 16 bytes arrive, or the
/// magic does not match.
pub fn read(stream: &mut impl std::io::Read) -> Result<u64, String> {
    let mut buf = [0u8; 16];
    stream
        .read_exact(&mut buf)
        .map_err(|e| format!("hatch: failed to read the agent-port preamble: {e}"))?;
    if buf[..8] != MAGIC {
        return Err(
            "hatch: this dial's preamble does not carry the expected magic — a stray connection, \
             not a hatched child"
                .to_string(),
        );
    }
    let mut token = [0; 8];
    token.copy_from_slice(&buf[8..]);
    Ok(u64::from_le_bytes(token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_preamble_returns_its_token() {
        let token = 0x0123_4567_89ab_cdef_u64;
        let bytes = MAGIC
            .into_iter()
            .chain(token.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(read(&mut bytes.as_slice()).unwrap(), token);
    }

    #[test]
    fn wrong_magic_is_a_stray_dial() {
        let mut bytes = [0u8; 16].as_slice();
        assert!(read(&mut bytes).unwrap_err().contains("stray connection"));
    }

    #[test]
    fn truncated_preamble_names_the_read() {
        let mut bytes = MAGIC.as_slice();
        assert!(
            read(&mut bytes)
                .unwrap_err()
                .contains("failed to read the agent-port preamble")
        );
    }
}
