//! A blocking ttrpc client (`support/mesh/mesh_rpc/src/message.rs`), one call at a time.

use std::fmt;
use std::io::{self, ErrorKind, Read, Write};
use std::time::{Duration, Instant};

use super::proto::{Value, bytes, fields, uint};

const SERVICE: &str = "vmservice.VM";
const REQUEST: u8 = 1;
const RESPONSE: u8 = 2;
const HEADER: usize = 10;
/// The top length byte is reserved.
const MAX_BODY: usize = 0x00ff_ffff;

#[derive(Clone, Copy)]
pub(crate) enum Method {
    CreateVm,
    ResumeVm,
}

impl Method {
    const fn name(self) -> &'static str {
        match self {
            Self::CreateVm => "CreateVM",
            Self::ResumeVm => "ResumeVM",
        }
    }
}

/// A stream whose blocking reads and writes give up after a timeout.
pub(crate) trait Timed: Read + Write {
    fn set_timeouts(&self, left: Option<Duration>) -> io::Result<()>;
}

impl Timed for std::net::TcpStream {
    fn set_timeouts(&self, left: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(left)?;
        self.set_write_timeout(left)
    }
}

#[cfg(all(test, unix))]
impl Timed for std::os::unix::net::UnixStream {
    fn set_timeouts(&self, left: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(left)?;
        self.set_write_timeout(left)
    }
}

pub(crate) struct Client<S> {
    stream: S,
    /// Client stream ids are odd.
    next: u32,
}

#[derive(Debug)]
pub(crate) enum Failure {
    Io(io::Error),
    TimedOut,
    Status { code: i32, message: String },
    Malformed(&'static str),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(cause) => write!(f, "the RPC connection to OpenVMM failed: {cause}"),
            Self::TimedOut => f.write_str("OpenVMM did not answer before the deadline"),
            Self::Status { code, message } => {
                write!(
                    f,
                    "OpenVMM refused with {} (status {code}): {message}",
                    code_name(*code)
                )
            }
            Self::Malformed(what) => {
                write!(f, "the ttrpc exchange with OpenVMM broke protocol: {what}")
            }
        }
    }
}

/// The `google.rpc.Code` name.
const fn code_name(code: i32) -> &'static str {
    match code {
        0 => "OK",
        1 => "CANCELLED",
        2 => "UNKNOWN",
        3 => "INVALID_ARGUMENT",
        4 => "DEADLINE_EXCEEDED",
        5 => "NOT_FOUND",
        6 => "ALREADY_EXISTS",
        7 => "PERMISSION_DENIED",
        8 => "RESOURCE_EXHAUSTED",
        9 => "FAILED_PRECONDITION",
        10 => "ABORTED",
        11 => "OUT_OF_RANGE",
        12 => "UNIMPLEMENTED",
        13 => "INTERNAL",
        14 => "UNAVAILABLE",
        15 => "DATA_LOSS",
        16 => "UNAUTHENTICATED",
        _ => "an unknown code",
    }
}

impl<S: Timed> Client<S> {
    pub(crate) fn new(stream: S) -> Self {
        Self { stream, next: 1 }
    }

    /// Sends `payload` to `method` and returns the response payload, all before `deadline`.
    pub(crate) fn call(
        &mut self,
        method: Method,
        payload: &[u8],
        deadline: Instant,
    ) -> Result<Vec<u8>, Failure> {
        let id = self.next;
        let frame = frame(id, &request(method, payload, remaining(deadline)?))?;
        self.next = id.wrapping_add(2);
        write_all(&mut self.stream, &frame, deadline)?;
        let mut header = [0; HEADER];
        read_exact(&mut self.stream, &mut header, deadline)?;
        let [l0, l1, l2, l3, i0, i1, i2, i3, kind, _flags] = header;
        if kind != RESPONSE {
            return Err(Failure::Malformed("a reply frame is not of type Response"));
        }
        if u32::from_be_bytes([i0, i1, i2, i3]) != id {
            return Err(Failure::Malformed(
                "a reply frame answers another stream than the request's",
            ));
        }
        let length = u32::from_be_bytes([l0, l1, l2, l3]) as usize;
        if length > MAX_BODY {
            return Err(Failure::Malformed(
                "a reply frame exceeds ttrpc's 16 MiB limit",
            ));
        }
        let mut body = vec![0; length];
        read_exact(&mut self.stream, &mut body, deadline)?;
        response(&body)
    }
}

/// The `Request` message; a zero `timeout_nano` would mean no timeout, so `left` is never zero.
fn request(method: Method, payload: &[u8], left: Duration) -> Vec<u8> {
    [
        bytes(1, SERVICE.as_bytes()),
        bytes(2, method.name().as_bytes()),
        bytes(3, payload),
        uint(4, u64::try_from(left.as_nanos()).unwrap_or(u64::MAX)),
    ]
    .concat()
}

fn frame(id: u32, body: &[u8]) -> Result<Vec<u8>, Failure> {
    let length = u32::try_from(body.len())
        .ok()
        .filter(|&length| length as usize <= MAX_BODY)
        .ok_or(Failure::Malformed(
            "the request exceeds ttrpc's 16 MiB limit",
        ))?;
    Ok([
        &length.to_be_bytes()[..],
        &id.to_be_bytes(),
        &[REQUEST, 0],
        body,
    ]
    .concat())
}

/// The `Response` oneof: field 1 a `google.rpc.Status`, field 2 the payload, the last one present winning.
fn response(body: &[u8]) -> Result<Vec<u8>, Failure> {
    fields(body).try_fold(Ok(Vec::new()), |outcome, field| {
        match field.map_err(Failure::Malformed)? {
            (1, Value::Bytes(status)) => Ok(Err(refusal(status)?)),
            (2, Value::Bytes(payload)) => Ok(Ok(payload.to_vec())),
            (1 | 2, _) => Err(Failure::Malformed(
                "a Response field has the wrong wire type",
            )),
            _ => Ok(outcome),
        }
    })?
}

fn refusal(status: &[u8]) -> Result<Failure, Failure> {
    let (code, message) = fields(status)
        .try_fold((0, String::new()), |(code, message), field| match field? {
            (1, Value::Varint(value)) => {
                // An int32 is the varint's low 32 bits.
                let [a, b, c, d, ..] = value.to_le_bytes();
                Ok((i32::from_le_bytes([a, b, c, d]), message))
            }
            (2, Value::Bytes(text)) => Ok((code, String::from_utf8_lossy(text).into_owned())),
            (1 | 2, _) => Err("a google.rpc.Status field has the wrong wire type"),
            _ => Ok((code, message)),
        })
        .map_err(Failure::Malformed)?;
    Ok(Failure::Status { code, message })
}

/// The time left before `deadline`, never zero.
fn remaining(deadline: Instant) -> Result<Duration, Failure> {
    Some(deadline.saturating_duration_since(Instant::now()))
        .filter(|left| !left.is_zero())
        .ok_or(Failure::TimedOut)
}

/// Runs one blocking `op`, retrying on timeouts and interrupts until `deadline`.
fn until<S: Timed, T>(
    stream: &mut S,
    deadline: Instant,
    mut op: impl FnMut(&mut S) -> io::Result<T>,
) -> Result<T, Failure> {
    loop {
        stream
            .set_timeouts(Some(remaining(deadline)?))
            .map_err(Failure::Io)?;
        match op(stream) {
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            done => return done.map_err(Failure::Io),
        }
    }
}

fn write_all<S: Timed>(stream: &mut S, frame: &[u8], deadline: Instant) -> Result<(), Failure> {
    let mut written = 0;
    while written < frame.len() {
        match until(stream, deadline, |s| s.write(&frame[written..]))? {
            0 => return Err(Failure::Io(ErrorKind::WriteZero.into())),
            n => written += n,
        }
    }
    Ok(())
}

fn read_exact<S: Timed>(
    stream: &mut S,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<(), Failure> {
    let mut filled = 0;
    while filled < buffer.len() {
        match until(stream, deadline, |s| s.read(&mut buffer[filled..]))? {
            0 => {
                return Err(Failure::Io(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "the connection closed before the reply was complete",
                )));
            }
            n => filled += n,
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn request_frames_are_golden() {
        let resume: &[&[u8]] = &[
            &[0, 0, 0, 0x1e], // length 30
            &[0, 0, 0, 1],    // stream id 1
            &[REQUEST, 0],    // type Request, flags 0
            &[0x0a, 0x0c],    // 1: service, 12 bytes
            b"vmservice.VM",
            &[0x12, 0x08],                         // 2: method, 8 bytes
            b"ResumeVM",                           // 3: payload empty, omitted
            &[0x20, 0x80, 0xe4, 0x97, 0xd0, 0x12], // 4: timeout_nano = 5_000_000_000
        ];
        let request_bytes = request(Method::ResumeVm, &[], Duration::from_secs(5));
        assert_eq!(frame(1, &request_bytes).unwrap(), resume.concat());
        let create: &[&[u8]] = &[
            &[0, 0, 0, 0x1e], // length 30
            &[0, 0, 0, 3],    // stream id 3
            &[REQUEST, 0],    // type Request, flags 0
            &[0x0a, 0x0c],    // 1: service, 12 bytes
            b"vmservice.VM",
            &[0x12, 0x08], // 2: method, 8 bytes
            b"CreateVM",
            &[0x1a, 0x02, 0xaa, 0xbb], // 3: payload, 2 bytes
            &[0x20, 0x01],             // 4: timeout_nano = 1
        ];
        let request_bytes = request(Method::CreateVm, &[0xaa, 0xbb], Duration::from_nanos(1));
        assert_eq!(frame(3, &request_bytes).unwrap(), create.concat());
    }

    #[test]
    fn responses_decode_as_a_oneof() {
        assert_eq!(response(&[]).unwrap(), b"");
        assert_eq!(response(&[0x12, 0x00]).unwrap(), b""); // 2: payload, 0 bytes
        let skipped: &[u8] = &[
            0x18, 0x07, // 3: varint, unknown
            0x21, 0, 0, 0, 0, 0, 0, 0, 0, // 4: fixed64, unknown
            0x12, 0x02, 0xaa, 0xbb, // 2: payload, 2 bytes
            0x2d, 0, 0, 0, 0, // 5: fixed32, unknown
            0x32, 0x01, 0xcc, // 6: bytes, unknown
        ];
        assert_eq!(response(skipped).unwrap(), [0xaa, 0xbb]);
        assert!(matches!(
            response(&[0x10, 0x01]),
            Err(Failure::Malformed(_))
        )); // 2: as a varint
        assert!(matches!(
            response(&[0x12, 0x05, 0xaa]),
            Err(Failure::Malformed(_))
        )); // 2: truncated
    }

    #[test]
    fn statuses_decode_with_their_code_and_message() {
        let not_found: &[u8] = &[
            0x0a, 0x09, // 1: Status, 9 bytes
            0x08, 0x05, //   1: code = 5 (NOT_FOUND)
            0x12, 0x03, b'a', b'b', b'c', // 2: message "abc"
            0x1a, 0x00, //   3: details, empty, skipped
        ];
        let refused = response(not_found).unwrap_err();
        assert!(matches!(&refused, Failure::Status { code: 5, message } if message == "abc"));
        assert_eq!(
            refused.to_string(),
            "OpenVMM refused with NOT_FOUND (status 5): abc"
        );
        let negative: &[u8] = &[
            0x0a, 0x0b, // 1: Status, 11 bytes
            0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0x01, // 1: code = -1, sign-extended
        ];
        assert!(matches!(
            response(negative),
            Err(Failure::Status { code: -1, .. })
        ));
        let empty_status: &[u8] = &[0x0a, 0x00]; // 1: Status, 0 bytes: code 0, still a failure
        assert!(matches!(
            response(empty_status),
            Err(Failure::Status { code: 0, .. })
        ));
        let message_as_varint: &[u8] = &[0x0a, 0x02, 0x10, 0x05]; // 1: Status { 2: varint 5 }
        assert!(matches!(
            response(message_as_varint),
            Err(Failure::Malformed(_))
        ));
        let truncated: &[u8] = &[0x0a, 0x02, 0x12, 0x05]; // 1: Status { 2: 5 bytes promised, none given }
        assert!(matches!(response(truncated), Err(Failure::Malformed(_))));
    }

    #[test]
    fn payload_and_status_last_one_wins() {
        let both: &[u8] = &[0x0a, 0x02, 0x08, 0x05, 0x12, 0x01, 0xaa];
        assert_eq!(response(both).unwrap(), [0xaa]);
        let reversed: &[u8] = &[0x12, 0x01, 0xaa, 0x0a, 0x02, 0x08, 0x05];
        assert!(matches!(
            response(reversed),
            Err(Failure::Status { code: 5, .. })
        ));
    }

    #[test]
    fn oversize_requests_are_refused_before_sending() {
        assert!(matches!(
            frame(1, &vec![0; MAX_BODY + 1]),
            Err(Failure::Malformed(_))
        ));
        assert_eq!(
            frame(1, &vec![0; MAX_BODY]).unwrap()[..4],
            [0, 0xff, 0xff, 0xff]
        );
    }

    #[cfg(unix)]
    mod wire {
        use super::*;
        use std::os::unix::net::UnixStream;

        const PATIENCE: Duration = Duration::from_secs(5);

        fn connected() -> (Client<UnixStream>, UnixStream) {
            let (ours, theirs) = UnixStream::pair().unwrap();
            theirs.set_timeouts(Some(PATIENCE)).unwrap();
            (Client::new(ours), theirs)
        }

        fn reply(id: u32, kind: u8, body: &[u8]) -> Vec<u8> {
            let length = u32::try_from(body.len()).unwrap();
            [
                &length.to_be_bytes()[..],
                &id.to_be_bytes(),
                &[kind, 0],
                body,
            ]
            .concat()
        }

        /// The next request's stream id and `timeout_nano`, read from the server's end.
        fn received(server: &mut UnixStream) -> (u32, u64) {
            let mut header = [0; HEADER];
            server.read_exact(&mut header).unwrap();
            let [l0, l1, l2, l3, i0, i1, i2, i3, kind, flags] = header;
            assert_eq!((kind, flags), (REQUEST, 0));
            let mut body = vec![0; u32::from_be_bytes([l0, l1, l2, l3]) as usize];
            server.read_exact(&mut body).unwrap();
            let timeout = fields(&body)
                .find_map(|field| match field.unwrap() {
                    (4, Value::Varint(nanos)) => Some(nanos),
                    _ => None,
                })
                .unwrap();
            (u32::from_be_bytes([i0, i1, i2, i3]), timeout)
        }

        #[test]
        fn calls_use_odd_increasing_ids_and_send_the_time_left() {
            let (mut client, mut server) = connected();
            server.write_all(&reply(1, RESPONSE, &[])).unwrap();
            server
                .write_all(&reply(3, RESPONSE, &[0x12, 0x02, 0xaa, 0xbb]))
                .unwrap();
            let deadline = Instant::now() + PATIENCE;
            assert_eq!(
                client
                    .call(Method::CreateVm, &[0x0a, 0x00], deadline)
                    .unwrap(),
                b""
            );
            assert_eq!(
                client.call(Method::ResumeVm, &[], deadline).unwrap(),
                [0xaa, 0xbb]
            );
            let budget = u64::try_from(PATIENCE.as_nanos()).unwrap();
            for expected in [1, 3] {
                let (id, timeout) = received(&mut server);
                assert_eq!(id, expected);
                assert!(0 < timeout && timeout <= budget, "timeout_nano {timeout}");
            }
        }

        #[test]
        fn a_status_is_a_failure() {
            let (mut client, mut server) = connected();
            server
                .write_all(&reply(1, RESPONSE, &[0x0a, 0x02, 0x08, 0x0d]))
                .unwrap(); // INTERNAL
            let failed = client.call(Method::ResumeVm, &[], Instant::now() + PATIENCE);
            assert!(matches!(failed, Err(Failure::Status { code: 13, .. })));
        }

        #[test]
        fn mismatched_frames_are_malformed() {
            for (id, kind) in [(3, RESPONSE), (1, REQUEST)] {
                let (mut client, mut server) = connected();
                server.write_all(&reply(id, kind, &[])).unwrap();
                let failed = client.call(Method::ResumeVm, &[], Instant::now() + PATIENCE);
                assert!(
                    matches!(failed, Err(Failure::Malformed(_))),
                    "id {id}, type {kind}"
                );
            }
        }

        #[test]
        fn oversize_replies_are_malformed() {
            let (mut client, mut server) = connected();
            server
                .write_all(&[0x01, 0, 0, 0, 0, 0, 0, 1, RESPONSE, 0])
                .unwrap(); // length 16 MiB
            let failed = client.call(Method::ResumeVm, &[], Instant::now() + PATIENCE);
            assert!(matches!(failed, Err(Failure::Malformed(_))));
        }

        #[test]
        fn nothing_is_sent_once_the_deadline_has_passed() {
            let (mut client, server) = connected();
            assert!(matches!(
                client.call(Method::ResumeVm, &[], Instant::now()),
                Err(Failure::TimedOut)
            ));
            server.set_nonblocking(true).unwrap();
            let unsent = (&server).read(&mut [0; 1]).unwrap_err();
            assert_eq!(unsent.kind(), ErrorKind::WouldBlock);
        }

        #[test]
        fn a_reply_stalled_mid_frame_times_out() {
            let whole = reply(1, RESPONSE, &[0x12, 0x00]);
            for sent in [5, HEADER + 1] {
                let (mut client, mut server) = connected();
                server.write_all(&whole[..sent]).unwrap();
                let failed = client.call(
                    Method::ResumeVm,
                    &[],
                    Instant::now() + Duration::from_millis(50),
                );
                assert!(
                    matches!(failed, Err(Failure::TimedOut)),
                    "after {sent} bytes"
                );
            }
        }
    }
}
