//! The host→guest handshake of `OpenVMM`'s hybrid vsock relay (`support/hybrid_vsock/src/lib.rs`).

/// `OK ` and a `u32` instance id, with room to quote what came instead.
pub(crate) const REPLY_LIMIT: usize = 64;
const _: () = assert!("OK 4294967295\n".len() <= REPLY_LIMIT);

/// The whole request; the relay reads it into 45 bytes, so nothing may follow until it answers.
pub(crate) fn request(port: u32) -> String {
    format!("CONNECT {port}\n")
}

/// The instance id of an accepting reply, `OK <u32>\n`, read through its newline.
pub(crate) fn accepted(reply: &[u8]) -> Option<u32> {
    let id = reply.strip_prefix(b"OK ")?.strip_suffix(b"\n")?;
    if id.is_empty() {
        return None;
    }
    id.iter().try_fold(0u32, |value, &digit| {
        value
            .checked_mul(10)?
            .checked_add(char::from(digit).to_digit(10)?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ok_and_a_u32_through_the_newline_accepts() {
        assert_eq!(request(1732), "CONNECT 1732\n");
        assert_eq!(accepted(b"OK 7\n"), Some(7));
        assert_eq!(accepted(b"OK 4294967295\n"), Some(u32::MAX));
        for refused in [
            &b"OK 4294967296\n"[..],
            b"OK \n",
            b"OK 7",
            b"OK -1\n",
            b"OK +1\n",
            b"OK 7 \n",
            b"ok 7\n",
            b"ERR\n",
            b"",
        ] {
            assert_eq!(
                accepted(refused),
                None,
                "{:?}",
                String::from_utf8_lossy(refused)
            );
        }
    }
}
