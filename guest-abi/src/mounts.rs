//! Folder maintenance on the third host wire, framed by [`crate::packet`].
//!
//! Decode before effects. Malformed or mismatched replies terminate the host wire;
//! neither framing nor operation uncertainty permits resynchronization or retry.

use std::fmt;

use crate::link::MTU;

pub const MAX_NAME: usize = 255;
pub const MAX_TEXT: usize = 1024;
pub const MAX_REQUEST: usize = 1 + 4 + 1 + 2 + MAX_NAME;
pub const MAX_REPLY: usize = 1 + 2 + MAX_TEXT;

const _: () = {
    assert!(MAX_NAME <= u16::MAX as usize);
    assert!(MAX_TEXT <= u16::MAX as usize);
    assert!(MAX_REQUEST <= MTU);
    assert!(MAX_REPLY <= MTU);
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FolderName(String);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameError {
    Empty,
    Dot,
    Slash,
    Control,
    TooLong { bytes: usize },
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => {
                f.write_str("the folder name is empty; choose one component under /work")
            }
            Self::Dot => f.write_str("the folder name is . or ..; choose a different name"),
            Self::Slash => f.write_str("the folder name contains /; choose one path component"),
            Self::Control => f.write_str("the folder name contains a control character; remove it"),
            Self::TooLong { bytes } => write!(
                f,
                "the folder name is {bytes} UTF-8 bytes; shorten it to at most {MAX_NAME} bytes"
            ),
        }
    }
}

impl std::error::Error for NameError {}

impl FolderName {
    /// # Errors
    /// Refuses empty, dot, slashed, control-containing or overlong names.
    pub fn new(name: impl Into<String>) -> Result<Self, NameError> {
        let name = name.into();
        if name.is_empty() {
            return Err(NameError::Empty);
        }
        if name == "." || name == ".." {
            return Err(NameError::Dot);
        }
        if name.contains('/') {
            return Err(NameError::Slash);
        }
        if name.chars().any(char::is_control) {
            return Err(NameError::Control);
        }
        if name.len() > MAX_NAME {
            return Err(NameError::TooLong { bytes: name.len() });
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FolderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Transport identity, never a product folder identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(pub u32);

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "s{}", self.0)
    }
}

/// One allocator per machine. Consume before exposure, including failed attempts.
#[derive(Debug)]
pub struct KeyAllocator {
    next: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeysExhausted;

impl fmt::Display for KeysExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the machine has exhausted its share keys; start a new conversation")
    }
}

impl std::error::Error for KeysExhausted {}

impl Default for KeyAllocator {
    fn default() -> Self {
        Self { next: Some(0) }
    }
}

impl KeyAllocator {
    /// # Errors
    /// Refuses after the last representable key has been consumed.
    pub fn allocate(&mut self) -> Result<Key, KeysExhausted> {
        let key = self.next.ok_or(KeysExhausted)?;
        self.next = key.checked_add(1);
        Ok(Key(key))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Survivors {
    Spare,
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Mount {
        key: Key,
        name: FolderName,
        read_only: bool,
    },
    Unmount {
        name: FolderName,
        survivors: Survivors,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Done,
    Busy(String),
    /// Topology is known; the change did not complete. Stop may have killed programs.
    Failed(String),
    /// The postcondition is unproven: terminal uncertainty, never a refusal.
    InDoubt(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    Short,
    Trailing,
    Tag(u8),
    Bool(u8),
    Survivors(u8),
    Utf8,
    TooLong { bytes: usize, limit: usize },
    Name(NameError),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Short => f.write_str("the mount message ended before its fields were complete"),
            Self::Trailing => f.write_str("the mount message has trailing bytes"),
            Self::Tag(tag) => write!(f, "the mount message has unknown tag {tag}"),
            Self::Bool(value) => write!(
                f,
                "the mount message has invalid boolean {value}; expected 0 or 1"
            ),
            Self::Survivors(value) => write!(
                f,
                "the mount message has invalid survivor choice {value}; expected 0 or 1"
            ),
            Self::Utf8 => f.write_str("the mount message contains invalid UTF-8"),
            Self::TooLong { bytes, limit } => write!(
                f,
                "the mount message claims {bytes} text bytes; the limit is {limit}"
            ),
            Self::Name(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for DecodeError {}

impl Request {
    /// Payload for [`crate::packet::write_frame`]; no framing is added here.
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        match self {
            Self::Mount {
                key,
                name,
                read_only,
            } => {
                bytes.push(1);
                bytes.extend_from_slice(&key.0.to_le_bytes());
                bytes.push(u8::from(*read_only));
                put_text(&mut bytes, name.as_str());
            }
            Self::Unmount { name, survivors } => {
                bytes.push(2);
                bytes.push(match survivors {
                    Survivors::Spare => 0,
                    Survivors::Stop => 1,
                });
                put_text(&mut bytes, name.as_str());
            }
        }
        bytes
    }

    /// Decode a payload from [`crate::packet::read_frame`] before any mutation.
    /// # Errors
    /// Refuses unknown tags, invalid fields, short input and trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut input = Input(bytes);
        let request = match input.byte()? {
            1 => {
                let key = Key(u32::from_le_bytes(input.array()?));
                let read_only = match input.byte()? {
                    0 => false,
                    1 => true,
                    value => return Err(DecodeError::Bool(value)),
                };
                Self::Mount {
                    key,
                    name: input.name()?,
                    read_only,
                }
            }
            2 => {
                let survivors = match input.byte()? {
                    0 => Survivors::Spare,
                    1 => Survivors::Stop,
                    value => return Err(DecodeError::Survivors(value)),
                };
                Self::Unmount {
                    name: input.name()?,
                    survivors,
                }
            }
            tag => return Err(DecodeError::Tag(tag)),
        };
        input.end()?;
        Ok(request)
    }
}

impl Reply {
    /// Locally generated text is truncated at a UTF-8 boundary, never on receipt.
    pub fn encode(&self) -> Vec<u8> {
        let (tag, text) = match self {
            Self::Done => return vec![1],
            Self::Busy(text) => (2, text),
            Self::Failed(text) => (3, text),
            Self::InDoubt(text) => (4, text),
        };
        let mut bytes = vec![tag];
        put_text(&mut bytes, &text[..text.floor_char_boundary(MAX_TEXT)]);
        bytes
    }

    /// # Errors
    /// Refuses malformed payloads, including oversized text; the host must end the wire.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut input = Input(bytes);
        let reply = match input.byte()? {
            1 => Self::Done,
            2 => Self::Busy(input.text(MAX_TEXT)?.into()),
            3 => Self::Failed(input.text(MAX_TEXT)?.into()),
            4 => Self::InDoubt(input.text(MAX_TEXT)?.into()),
            tag => return Err(DecodeError::Tag(tag)),
        };
        input.end()?;
        Ok(reply)
    }
}

fn put_text(bytes: &mut Vec<u8>, text: &str) {
    let len = u16::try_from(text.len()).expect("names and reply text fit in u16");
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.extend_from_slice(text.as_bytes());
}

struct Input<'a>(&'a [u8]);

impl<'a> Input<'a> {
    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let (bytes, rest) = self.0.split_first_chunk::<N>().ok_or(DecodeError::Short)?;
        self.0 = rest;
        Ok(*bytes)
    }

    fn byte(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }

    fn text(&mut self, limit: usize) -> Result<&'a str, DecodeError> {
        let len = usize::from(u16::from_le_bytes(self.array()?));
        if len > limit {
            return Err(DecodeError::TooLong { bytes: len, limit });
        }
        let (bytes, rest) = self.0.split_at_checked(len).ok_or(DecodeError::Short)?;
        self.0 = rest;
        std::str::from_utf8(bytes).map_err(|_| DecodeError::Utf8)
    }

    fn name(&mut self) -> Result<FolderName, DecodeError> {
        FolderName::new(self.text(MAX_NAME)?).map_err(DecodeError::Name)
    }

    fn end(self) -> Result<(), DecodeError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(DecodeError::Trailing)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{read_frame, write_frame};

    fn name(value: &str) -> FolderName {
        FolderName::new(value).unwrap()
    }

    fn requests() -> Vec<Request> {
        [false, true]
            .into_iter()
            .map(|read_only| Request::Mount {
                key: Key(0x1234_5678),
                name: name("税金 2025"),
                read_only,
            })
            .chain(
                [Survivors::Spare, Survivors::Stop]
                    .into_iter()
                    .map(|survivors| Request::Unmount {
                        name: name("Tax papers"),
                        survivors,
                    }),
            )
            .collect()
    }

    fn replies() -> Vec<Reply> {
        vec![
            Reply::Done,
            Reply::Busy("python3 (412)".into()),
            Reply::Failed("sync refused".into()),
            Reply::InDoubt("thaw unconfirmed".into()),
        ]
    }

    #[test]
    fn folder_name_validates_one_component_without_normalizing() {
        for value in [
            "", ".", "..", "/", "a/b", "\0", "a\nb", "a\tb", "\u{7f}", "\u{85}",
        ] {
            FolderName::new(value).expect_err(value);
        }
        for value in ["Tax papers", "税金", " a ", "...", "a\\b", "é", "e\u{301}"] {
            assert_eq!(name(value).as_str(), value);
            assert_eq!(name(value).to_string(), value);
        }
        assert_ne!(name("é"), name("e\u{301}"));
    }

    #[test]
    fn every_unicode_control_is_refused_on_construction_and_decode() {
        for code in (0..=0x1f).chain(0x7f..=0x9f) {
            let value = format!("a{}b", char::from_u32(code).unwrap());
            assert_eq!(FolderName::new(&value), Err(NameError::Control));
            for mut bytes in [vec![1, 0, 0, 0, 0, 0], vec![2, 0]] {
                put_text(&mut bytes, &value);
                assert_eq!(
                    Request::decode(&bytes),
                    Err(DecodeError::Name(NameError::Control))
                );
            }
        }
    }

    #[test]
    fn folder_name_limit_counts_utf8_bytes() {
        for value in ["a".repeat(MAX_NAME), format!("{}a", "é".repeat(127))] {
            assert_eq!(name(&value).as_str(), value);
        }
        for value in ["a".repeat(MAX_NAME + 1), "é".repeat(128)] {
            assert_eq!(
                FolderName::new(&value),
                Err(NameError::TooLong { bytes: 256 })
            );
        }
    }

    #[test]
    fn keys_have_one_transport_spelling() {
        assert_eq!(Key(0).to_string(), "s0");
        assert_eq!(Key(u32::MAX).to_string(), "s4294967295");
    }

    #[test]
    fn failed_attempts_still_consume_keys() {
        let mut keys = KeyAllocator::default();
        let failed_attempt = keys.allocate().unwrap();
        let next_attempt = keys.allocate().unwrap();
        assert_eq!(failed_attempt, Key(0));
        assert_eq!(next_attempt, Key(1));
    }

    #[test]
    fn the_last_key_is_allocated_once_then_exhaustion_is_permanent() {
        let mut keys = KeyAllocator {
            next: Some(u32::MAX - 1),
        };
        assert_eq!(keys.allocate(), Ok(Key(u32::MAX - 1)));
        assert_eq!(keys.allocate(), Ok(Key(u32::MAX)));
        assert_eq!(keys.allocate(), Err(KeysExhausted));
        assert_eq!(keys.allocate(), Err(KeysExhausted));
        assert!(keys.next.is_none());
    }

    #[test]
    fn requests_and_replies_round_trip_through_packet_frames() {
        let mut wire = Vec::new();
        for request in requests() {
            write_frame(&mut wire, &request.encode()).unwrap();
        }
        for reply in replies() {
            write_frame(&mut wire, &reply.encode()).unwrap();
        }
        let mut input = wire.as_slice();
        for request in requests() {
            assert_eq!(
                Request::decode(&read_frame(&mut input).unwrap()),
                Ok(request)
            );
        }
        for reply in replies() {
            assert_eq!(Reply::decode(&read_frame(&mut input).unwrap()), Ok(reply));
        }
        assert_eq!(input, []);
    }

    #[test]
    fn payload_layout_is_little_endian_with_fixed_tags() {
        assert_eq!(
            Request::Mount {
                key: Key(0x1234_5678),
                read_only: true,
                name: name("é")
            }
            .encode(),
            [1, 0x78, 0x56, 0x34, 0x12, 1, 2, 0, 0xc3, 0xa9]
        );
        assert_eq!(
            Request::Unmount {
                name: name("x"),
                survivors: Survivors::Spare
            }
            .encode(),
            [2, 0, 1, 0, b'x']
        );
        assert_eq!(
            Request::Unmount {
                name: name("x"),
                survivors: Survivors::Stop
            }
            .encode(),
            [2, 1, 1, 0, b'x']
        );
        assert_eq!(Reply::Done.encode(), [1]);
        assert_eq!(Reply::Busy("x".into()).encode(), [2, 1, 0, b'x']);
        assert_eq!(Reply::Failed("x".into()).encode(), [3, 1, 0, b'x']);
        assert_eq!(Reply::InDoubt("x".into()).encode(), [4, 1, 0, b'x']);
        let long = Reply::Busy("a".repeat(256)).encode();
        assert_eq!(&long[..3], &[2, 0, 1]);
    }

    #[test]
    fn all_short_prefixes_and_trailing_bytes_are_rejected() {
        for request in requests() {
            let bytes = request.encode();
            for end in 0..bytes.len() {
                assert_eq!(Request::decode(&bytes[..end]), Err(DecodeError::Short));
            }
            let mut trailing = bytes;
            trailing.push(0);
            assert_eq!(Request::decode(&trailing), Err(DecodeError::Trailing));
        }
        for reply in replies() {
            let bytes = reply.encode();
            for end in 0..bytes.len() {
                assert_eq!(Reply::decode(&bytes[..end]), Err(DecodeError::Short));
            }
            let mut trailing = bytes;
            trailing.push(0);
            assert_eq!(Reply::decode(&trailing), Err(DecodeError::Trailing));
        }
    }

    #[test]
    fn every_unknown_tag_and_discriminant_is_rejected() {
        for tag in 0..=u8::MAX {
            if ![1, 2].contains(&tag) {
                assert_eq!(Request::decode(&[tag]), Err(DecodeError::Tag(tag)));
            }
            if ![1, 2, 3, 4].contains(&tag) {
                assert_eq!(Reply::decode(&[tag]), Err(DecodeError::Tag(tag)));
            }
        }
        for value in 2..=u8::MAX {
            assert_eq!(
                Request::decode(&[1, 0, 0, 0, 0, value, 1, 0, b'x']),
                Err(DecodeError::Bool(value))
            );
            assert_eq!(
                Request::decode(&[2, value, 1, 0, b'x']),
                Err(DecodeError::Survivors(value))
            );
        }
    }

    #[test]
    fn both_request_decoders_use_the_folder_name_constructor() {
        for value in ["", ".", "..", "a/b", "\0", "a\nb", "\u{85}"] {
            let expected = FolderName::new(value).unwrap_err();
            for mut bytes in [vec![1, 0, 0, 0, 0, 0], vec![2, 0]] {
                put_text(&mut bytes, value);
                assert_eq!(Request::decode(&bytes), Err(DecodeError::Name(expected)));
            }
        }
    }

    #[test]
    fn invalid_utf8_and_oversized_received_fields_are_rejected() {
        for bytes in [&[2, 0, 1, 0, 0xff][..], &[1, 0, 0, 0, 0, 0, 1, 0, 0xff]] {
            assert_eq!(Request::decode(bytes), Err(DecodeError::Utf8));
        }
        for mut bytes in [vec![1, 0, 0, 0, 0, 0], vec![2, 0]] {
            bytes.extend_from_slice(&256u16.to_le_bytes());
            assert_eq!(
                Request::decode(&bytes),
                Err(DecodeError::TooLong {
                    bytes: 256,
                    limit: MAX_NAME
                })
            );
        }
        for tag in [2, 3, 4] {
            assert_eq!(Reply::decode(&[tag, 1, 0, 0xff]), Err(DecodeError::Utf8));
            for len in [1025u16, u16::MAX] {
                let mut bytes = vec![tag];
                bytes.extend_from_slice(&len.to_le_bytes());
                let expected = Err(DecodeError::TooLong {
                    bytes: usize::from(len),
                    limit: MAX_TEXT,
                });
                assert_eq!(Reply::decode(&bytes), expected);
                bytes.resize(3 + usize::from(len), b'a');
                assert_eq!(Reply::decode(&bytes), expected);
            }
        }
    }

    #[test]
    fn local_reply_text_is_truncated_only_at_utf8_boundaries() {
        for prefix in 1020..=1025 {
            for suffix in ["", "é", "税", "🦀"] {
                let text = format!("{}{suffix}tail", "a".repeat(prefix));
                let mut expected = String::new();
                for ch in text.chars() {
                    if expected.len() + ch.len_utf8() > MAX_TEXT {
                        break;
                    }
                    expected.push(ch);
                }
                for reply in [
                    Reply::Busy(text.clone()),
                    Reply::Failed(text.clone()),
                    Reply::InDoubt(text.clone()),
                ] {
                    let bytes = reply.encode();
                    assert!(bytes.len() <= MAX_REPLY);
                    let decoded = Reply::decode(&bytes).unwrap();
                    let actual = match decoded {
                        Reply::Busy(text) | Reply::Failed(text) | Reply::InDoubt(text) => text,
                        Reply::Done => panic!("lost reply tag"),
                    };
                    assert_eq!(actual, expected);
                }
            }
        }
    }

    #[test]
    fn maximum_payloads_fit_the_mtu_and_empty_reply_text_is_valid() {
        let request = Request::Mount {
            key: Key(u32::MAX),
            name: name(&"a".repeat(MAX_NAME)),
            read_only: false,
        };
        assert_eq!(request.encode().len(), MAX_REQUEST);
        assert_eq!(Request::decode(&request.encode()), Ok(request));
        for reply in [
            Reply::Busy("a".repeat(MAX_TEXT)),
            Reply::Failed("a".repeat(MAX_TEXT)),
            Reply::InDoubt("a".repeat(MAX_TEXT)),
            Reply::Busy(String::new()),
            Reply::Failed(String::new()),
            Reply::InDoubt(String::new()),
        ] {
            assert!(reply.encode().len() <= MAX_REPLY);
            assert_eq!(Reply::decode(&reply.encode()), Ok(reply));
        }
        assert_eq!(Reply::Busy("a".repeat(MAX_TEXT)).encode().len(), MAX_REPLY);
    }
}
