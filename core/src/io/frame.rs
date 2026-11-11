//! Length-prefixed JSON frames: the one framing codec on every ral IPC
//! channel — the sealed engine seed and the engine wire in
//! [`crate::protocol::channel`].

use crate::data::json::{self, JsonDecode, JsonEncode};
use crate::fact::{
    FrameEncodeFailure, FrameEncodeFailureTooLarge, FrameFailure, FrameFailurePartial,
    FrameFailureTooLarge, FramePart,
};
use crate::own::OwnDir;
use crate::path::{Visibility, XdgKind, shown};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;

/// Checked before the body is allocated, so no peer names a huge buffer.
const MAX_FRAME_LEN: u32 = 256 * 1024 * 1024;

fn fuse(len: usize) -> Result<u32, FrameEncodeFailureTooLarge> {
    match u32::try_from(len) {
        Ok(len) if len <= MAX_FRAME_LEN => Ok(len),
        _ => Err(FrameEncodeFailureTooLarge {
            actual: len.into(),
            limit: u64::from(MAX_FRAME_LEN).into(),
        }),
    }
}

pub(crate) struct Encoded {
    len: u32,
    body: Vec<u8>,
}

pub(crate) fn encode<T: JsonEncode + ?Sized>(value: &T) -> Result<Encoded, FrameEncodeFailure> {
    let body = json::to_vec(value).map_err(FrameEncodeFailure::Json)?;
    let len = fuse(body.len()).map_err(FrameEncodeFailure::TooLarge)?;
    Ok(Encoded { len, body })
}

/// Length prefix, body, flush: only I/O can fail here.
pub(crate) fn put<W: Write + ?Sized>(w: &mut W, frame: &Encoded) -> io::Result<()> {
    w.write_all(&frame.len.to_le_bytes())?;
    w.write_all(&frame.body)?;
    w.flush()
}

/// Raw-dump evidence is report-only, separate from the frame's portable cause.
#[derive(Debug)]
pub enum FrameDump {
    Written(PathBuf),
    Failed(io::Error),
}

/// Portable framing failure with separate, report-only raw-dump evidence.
#[derive(Debug)]
pub struct FrameReadError {
    pub cause: FrameFailure,
    pub dump: Option<FrameDump>,
}

impl From<FrameFailure> for FrameReadError {
    fn from(cause: FrameFailure) -> Self {
        Self { cause, dump: None }
    }
}

impl fmt::Display for FrameReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.cause.describe())?;
        match &self.dump {
            None => Ok(()),
            Some(FrameDump::Written(path)) => write!(f, " (raw frame written to {})", shown(path)),
            Some(FrameDump::Failed(error)) => write!(f, " (raw frame not kept: {error})"),
        }
    }
}
impl std::error::Error for FrameReadError {}

pub(crate) fn read_frame<R: Read + ?Sized, T: JsonDecode>(
    reader: &mut R,
) -> Result<Option<T>, FrameReadError> {
    read_body(reader)?
        .map(|body| decode_body(&body))
        .transpose()
}

/// Retry interrupted reads and count the bytes before EOF explicitly.
fn read_part<R: Read + ?Sized>(reader: &mut R, bytes: &mut [u8]) -> Result<usize, FrameFailure> {
    let mut got = 0;
    while got < bytes.len() {
        match reader.read(&mut bytes[got..]) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(FrameFailure::Io(super::failure(&error))),
            Ok(0) => break,
            Ok(count) => got += count,
        }
    }
    Ok(got)
}

fn partial(part: FramePart, got: usize, expected: usize) -> FrameReadError {
    FrameFailure::Partial(FrameFailurePartial {
        part,
        got: got.into(),
        expected: expected.into(),
    })
    .into()
}

/// `Ok(None)` is clean EOF between frames, not a truncated prefix or body.
fn read_body<R: Read + ?Sized>(reader: &mut R) -> Result<Option<Vec<u8>>, FrameReadError> {
    let mut prefix = [0; 4];
    match read_part(reader, &mut prefix)? {
        0 => return Ok(None),
        4 => {}
        got => return Err(partial(FramePart::Length, got, 4)),
    }
    let len = fuse(u32::from_le_bytes(prefix) as usize).map_err(|size| {
        FrameReadError::from(FrameFailure::TooLarge(FrameFailureTooLarge {
            actual: size.actual,
            limit: size.limit,
        }))
    })?;
    let mut body = vec![0; len as usize];
    let got = read_part(reader, &mut body)?;
    if got != body.len() {
        return Err(partial(FramePart::Body, got, body.len()));
    }
    Ok(Some(body))
}

fn decode_body<T: JsonDecode>(body: &[u8]) -> Result<T, FrameReadError> {
    json::from_slice(body).map_err(|cause| FrameReadError {
        cause: FrameFailure::Json(cause),
        dump: Some(match dump_frame(body) {
            Ok(path) => FrameDump::Written(path),
            Err(error) => FrameDump::Failed(error),
        }),
    })
}

/// The body kept in the program's state directory under a fresh name, and
/// where it is.  Owner-only, not the umask default: a frame body is
/// unredacted payload.
fn dump_frame(body: &[u8]) -> io::Result<PathBuf> {
    let state = OwnDir::xdg("ral", XdgKind::State)?;
    let (name, mut file) = state.create_fresh("frame-", ".json", Visibility::Secret)?;
    file.write_all(body)?;
    Ok(state.path().join(name))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct TestJson<T>(T);
    impl<T: serde::de::DeserializeOwned> JsonDecode for TestJson<T> {
        fn decode_json(value: json::JsonRef<'_, '_>) -> Result<Self, crate::fact::DecodeFailure> {
            json::decode_structural(value)
        }
    }
    impl<T: Serialize> JsonEncode for TestJson<T> {
        fn encode_json(
            &self,
            format: json::JsonFormat,
        ) -> Result<Vec<u8>, crate::fact::JsonEncodeFailure> {
            Ok(json::serialize_structural(self, format))
        }
    }

    #[cfg(unix)]
    use crate::host::test_support::with_var;

    struct InterruptOnce {
        bytes: std::io::Cursor<Vec<u8>>,
        interrupted: bool,
    }

    impl Read for InterruptOnce {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            self.bytes.read(buf)
        }
    }

    #[test]
    fn read_frame_retries_a_signal_interrupted_length_read() {
        let mut framed = Vec::new();
        put(&mut framed, &encode(&TestJson("hello")).unwrap()).unwrap();
        let mut reader = InterruptOnce {
            bytes: std::io::Cursor::new(framed),
            interrupted: false,
        };
        let value: Option<TestJson<String>> = read_frame(&mut reader).unwrap();
        assert_eq!(value.map(|value| value.0), Some("hello".into()));
    }

    #[test]
    fn read_frame_retries_a_signal_interrupted_body_read() {
        let frame = encode(&TestJson("hello")).unwrap();
        let mut reader = io::Cursor::new(frame.len.to_le_bytes()).chain(InterruptOnce {
            bytes: io::Cursor::new(frame.body),
            interrupted: false,
        });
        let value: Option<TestJson<String>> = read_frame(&mut reader).unwrap();
        assert_eq!(value.map(|value| value.0), Some("hello".into()));
    }

    #[cfg(unix)]
    #[test]
    fn frame_encoding_preserves_typed_path_refusals_without_writing() {
        use std::os::unix::ffi::OsStringExt;
        let path = crate::path::EnginePath::Host(
            std::ffi::OsString::from_vec(b"/bad\xff".to_vec()).into(),
        );
        assert!(matches!(encode(&path), Err(FrameEncodeFailure::Json(_))));
    }

    #[test]
    fn an_oversized_length_is_refused_before_the_body_is_allocated() {
        let mut reader = std::io::Cursor::new(vec![0xFF; 4]);
        let err = read_frame::<_, TestJson<String>>(&mut reader).unwrap_err();
        assert!(matches!(err.cause, FrameFailure::TooLarge(_)));
    }

    /// Both ends judge a body by the one fuse, so an oversized frame fails
    /// at the writer rather than killing the peer that reads it.
    #[test]
    fn the_fuse_is_the_same_at_both_ends() {
        assert_eq!(fuse(MAX_FRAME_LEN as usize).unwrap(), MAX_FRAME_LEN);
        let err = fuse(MAX_FRAME_LEN as usize + 1).unwrap_err();
        assert_eq!(err.actual.to_u64(), u64::from(MAX_FRAME_LEN) + 1);
        assert_eq!(err.limit.to_u64(), u64::from(MAX_FRAME_LEN));
    }

    #[test]
    fn a_truncated_length_is_not_a_clean_hangup() {
        let mut empty = std::io::Cursor::new(Vec::new());
        let value: Option<TestJson<String>> = read_frame(&mut empty).unwrap();
        assert_eq!(value, None, "EOF at a frame boundary is an orderly close");

        let mut partial = std::io::Cursor::new(vec![0x02, 0x00]);
        let err = read_frame::<_, TestJson<String>>(&mut partial).unwrap_err();
        assert_eq!(
            err.cause,
            FrameFailure::Partial(FrameFailurePartial {
                part: FramePart::Length,
                got: 2_usize.into(),
                expected: 4_usize.into(),
            })
        );
    }

    #[test]
    fn dump_failure_does_not_change_the_authoritative_frame_cause() {
        let cause = FrameFailure::Partial(FrameFailurePartial {
            part: FramePart::Body,
            got: 1_usize.into(),
            expected: 2_usize.into(),
        });
        let error = FrameReadError {
            cause: cause.clone(),
            dump: Some(FrameDump::Failed(io::Error::from(
                io::ErrorKind::PermissionDenied,
            ))),
        };
        assert_eq!(error.cause, cause);
        assert!(error.to_string().contains("raw frame not kept"));
    }

    #[test]
    fn partial_body_retains_the_number_of_received_bytes() {
        let mut bytes = 8_u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"abc");
        let error = read_frame::<_, TestJson<String>>(&mut io::Cursor::new(bytes)).unwrap_err();
        assert_eq!(
            error.cause,
            FrameFailure::Partial(FrameFailurePartial {
                part: FramePart::Body,
                got: 3_usize.into(),
                expected: 8_usize.into(),
            })
        );
        assert!(error.dump.is_none());
    }

    #[test]
    fn a_deep_nest_round_trips_on_a_default_stack() {
        const DEPTH: usize = 5000;
        let nest = format!("{}{}", "[".repeat(DEPTH), "]".repeat(DEPTH));
        let mut framed = fuse(nest.len()).unwrap().to_le_bytes().to_vec();
        framed.extend_from_slice(nest.as_bytes());

        let deep: TestJson<serde_json::Value> =
            read_frame(&mut std::io::Cursor::new(framed.clone()))
                .unwrap()
                .expect("a whole frame decodes");
        let mut again = Vec::new();
        put(&mut again, &encode(&deep).unwrap()).unwrap();
        assert_eq!(again, framed);
    }

    #[test]
    fn a_deep_result_crosses_in_a_report_frame() {
        // Building, comparing and dropping a 5000-deep value recurses past a test thread's stack.
        std::thread::Builder::new()
            .stack_size(64 << 20)
            .spawn(deep_result_crosses)
            .unwrap()
            .join()
            .unwrap();
    }

    fn deep_result_crosses() {
        use crate::data::FOValue;
        use crate::protocol::{DispatchId, Ending, Event, Frame, Ran, Report};

        let lists = (0..5000).fold(FOValue::Unit, |inner, _| FOValue::List {
            items: vec![inner],
        });
        let maps = (0..5000).fold(FOValue::Unit, |inner, _| FOValue::Map {
            entries: vec![("k".into(), inner)],
        });
        for value in [lists, maps] {
            let frame = Frame::Event(
                DispatchId(1),
                Event::Report(Report::Ran(Ran {
                    ending: Ending::Ok(value),
                    rendered: String::new(),
                    single_command: false,
                    captured: None,
                    trail: Vec::new(),
                })),
            );
            let mut framed = Vec::new();
            put(&mut framed, &encode(&frame).unwrap()).unwrap();
            let decoded: Option<Frame> = read_frame(&mut std::io::Cursor::new(framed)).unwrap();
            assert!(decoded == Some(frame), "a deep result must cross intact");
        }
    }

    #[cfg(unix)]
    #[test]
    fn decode_failure_dump_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let state = OwnDir::for_test("frame-dump").unwrap();
        let base = crate::path::utf8(state.path()).unwrap();
        let err = with_var("XDG_STATE_HOME", Some(base), || {
            let mut framed = Vec::new();
            put(&mut framed, &encode(&TestJson("not a number")).unwrap()).unwrap();
            let mut reader = std::io::Cursor::new(framed);
            read_frame::<_, TestJson<u32>>(&mut reader).unwrap_err()
        });

        assert!(matches!(err.cause, FrameFailure::Json(_)));
        let Some(FrameDump::Written(path)) = err.dump else {
            panic!("dump was not retained")
        };
        assert!(path.starts_with(state.path()));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "dump must not be group/world-readable");
    }
}
