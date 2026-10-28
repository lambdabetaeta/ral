//! Reading a stream whose size is not trusted.

use std::io::{self, Read};

/// All of `reader` if it holds at most `limit` bytes, else `None`.  Never
/// consumes more than `limit + 1` bytes, so an oversized stream costs no more.
///
/// # Errors
/// The read's.
pub fn read_bounded(reader: impl Read, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    Ok((bytes.len() <= limit).then_some(bytes))
}

#[cfg(test)]
mod tests {
    use super::read_bounded;

    #[test]
    fn reads_up_to_the_limit_and_refuses_one_byte_more() {
        assert_eq!(read_bounded(&b"abc"[..], 3).unwrap(), Some(b"abc".to_vec()));
        assert_eq!(read_bounded(&b""[..], 0).unwrap(), Some(Vec::new()));
        assert_eq!(read_bounded(&b"abcd"[..], 3).unwrap(), None);
    }

    #[test]
    fn an_oversized_stream_is_read_only_one_byte_past_the_limit() {
        let mut stream = std::io::Cursor::new(vec![0; 100]);
        assert_eq!(read_bounded(&mut stream, 10).unwrap(), None);
        assert_eq!(stream.position(), 11);
    }
}
