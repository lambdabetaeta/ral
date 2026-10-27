use super::*;
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::{self, Receiver};

const TEST_POLICY: Policy = Policy {
    flow: Duration::from_secs(3),
    request: Duration::from_millis(200),
    response: Duration::from_secs(1),
    poll: Duration::from_millis(5),
    ..POLICY
};

struct Flow {
    addr: SocketAddr,
    cancel: Arc<AtomicBool>,
    result: Receiver<CallbackResult>,
    worker: Option<std::thread::JoinHandle<()>>,
}

fn disconnected(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
    )
}

impl Flow {
    fn new(policy: Policy) -> Self {
        Self::start(bind(&["127.0.0.1:0"]).unwrap(), policy)
    }

    fn start(listener: TcpListener, policy: Policy) -> Self {
        let addr = listener.local_addr().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        let (send, result) = mpsc::channel();
        let worker = Some(std::thread::spawn(move || {
            let _ = send.send(receive(&listener, "expected", &flag, policy));
        }));
        Self {
            addr,
            cancel,
            result,
            worker,
        }
    }

    fn connect(&self) -> TcpStream {
        let stream = TcpStream::connect(self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
    }

    fn request(&self, request: &[u8]) -> Vec<u8> {
        let mut stream = self.connect();
        stream.write_all(request).unwrap();
        let mut response = Vec::new();
        // Socket closure can report a reset or an abort, depending on the host.
        match stream.read_to_end(&mut response) {
            Ok(_) => {}
            Err(e) if disconnected(&e) => {}
            Err(e) => panic!("callback response failed: {e}"),
        }
        response
    }

    fn callback(&self, query: &str) -> Vec<u8> {
        self.request(format!("GET {CALLBACK_PATH}?{query} HTTP/1.1\r\n").as_bytes())
    }

    fn finish(&self) -> CallbackResult {
        self.result
            .recv_timeout(Duration::from_secs(1))
            .expect("callback worker did not finish")
    }
}

impl Drop for Flow {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn callback_retains_code_and_optional_issued_client() {
    for client in [None, Some("oaiapp_issued")] {
        let flow = Flow::new(TEST_POLICY);
        let extra = client
            .map(|id| format!("&client_id={id}"))
            .unwrap_or_default();
        let page = flow.callback(&format!("state=expected&code=auth-code{extra}"));
        assert!(page.starts_with(b"HTTP/1.1 200"));
        let received = flow.finish().unwrap();
        assert_eq!(received.code, "auth-code");
        assert_eq!(received.client_id.as_deref(), client);
    }
}

#[test]
fn unrelated_and_ambiguous_traffic_cannot_settle_a_flow() {
    let flow = Flow::new(Policy {
        request: Duration::from_secs(2),
        ..TEST_POLICY
    });
    for request in [
        "GET /favicon.ico HTTP/1.1\r\n",
        "not an HTTP request\r\n",
        "POST /auth/callback?state=expected&code=bad HTTP/1.1\r\n",
        "GET /other?state=expected&code=bad HTTP/1.1\r\n",
        "GET /auth/callback?code=bad HTTP/1.1\r\n",
        "GET /auth/callback?state=wrong&error=denied HTTP/1.1\r\n",
        "GET /auth/callback?state=expected&state=wrong&code=bad HTTP/1.1\r\n",
        "GET /auth/callback?state=expected&code=a&code=b HTTP/1.1\r\n",
        "GET /auth/callback?state=expected&error=a&error=b HTTP/1.1\r\n",
        "GET /auth/callback?state=expected&code=a&client_id=a&client_id=b HTTP/1.1\r\n",
        "GET http://localhost/auth/callback?state=expected&code=bad HTTP/1.1\r\n",
        "GET //localhost/auth/callback?state=expected&code=bad HTTP/1.1\r\n",
        "GET /auth/callback?state=expected&code=bad#fragment HTTP/1.1\r\n",
        "GET /auth/callback?state=expected&code=bad HTTP/2\r\n",
        "GET /auth/callback?state=expected&code=bad HTTP/1.1\n",
    ] {
        let page = flow.request(request.as_bytes());
        assert!(page.starts_with(b"HTTP/1.1 400") || page.starts_with(b"HTTP/1.1 404"));
        assert!(matches!(
            flow.result.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }
    flow.request(b"GET /auth/callback?state=expected&code=good HTTP/1.0\r\n");
    assert_eq!(flow.finish().unwrap().code, "good");
}

#[test]
fn matching_state_denial_and_invalid_results_are_terminal() {
    for query in [
        "state=expected&error=denied&error_description=No%0Athanks",
        "state=expected",
        "state=expected&code=",
        "state=expected&code=a&error=denied",
    ] {
        let flow = Flow::new(TEST_POLICY);
        flow.callback(query);
        let error = flow.finish().err().unwrap();
        assert!(!error.chars().any(char::is_control));
        assert!(error.len() < 1100);
    }
    let flow = Flow::new(TEST_POLICY);
    flow.callback(&format!("state=expected&error={}", "a".repeat(2000)));
    assert!(flow.finish().err().unwrap().len() < 1100);
}

#[test]
fn idle_preconnections_do_not_block_a_callback() {
    let flow = Flow::new(Policy {
        request: Duration::from_secs(2),
        ..TEST_POLICY
    });
    let mut idle: Vec<_> = (0..POLICY.connections - 1)
        .map(|_| flow.connect())
        .collect();
    let start = Instant::now();
    flow.callback("state=expected&code=good");
    assert_eq!(flow.finish().unwrap().code, "good");
    assert!(start.elapsed() < Duration::from_secs(1));
    for stream in &mut idle {
        assert_closed(stream);
    }
}

#[test]
fn fragmented_callback_works_within_its_deadline() {
    let flow = Flow::new(TEST_POLICY);
    let mut stream = flow.connect();
    stream
        .write_all(b"GET /auth/callback?state=expected&co")
        .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    stream.write_all(b"de=good HTTP/1.1\r\n").unwrap();
    assert_eq!(flow.finish().unwrap().code, "good");
}

fn assert_closed(stream: &mut TcpStream) {
    let mut byte = [0];
    match stream.read(&mut byte) {
        Ok(0) => {}
        Err(e) if disconnected(&e) => {}
        other => panic!("connection was not closed: {other:?}"),
    }
}

#[test]
fn connection_cap_and_expiry_allow_later_traffic() {
    let listener = bind(&["127.0.0.1:0"]).unwrap();
    let addr = listener.local_addr().unwrap();
    let mut idle: Vec<_> = (0..2)
        .map(|_| {
            let stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
        })
        .collect();
    let mut excess = TcpStream::connect(addr).unwrap();
    excess
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let flow = Flow::start(
        listener,
        Policy {
            connections: 2,
            request: Duration::from_secs(2),
            flow: Duration::from_secs(5),
            ..TEST_POLICY
        },
    );
    assert_closed(&mut excess);
    for stream in &mut idle {
        assert_closed(stream);
    }
    flow.callback("state=expected&code=after-expiry");
    assert_eq!(flow.finish().unwrap().code, "after-expiry");
}

#[test]
fn slow_trickle_does_not_extend_the_request_deadline() {
    let flow = Flow::new(TEST_POLICY);
    let mut stream = flow.connect();
    stream.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_millis(600);
    loop {
        assert!(
            Instant::now() < deadline,
            "trickling kept the connection alive"
        );
        let _ = stream.write_all(b"G");
        match stream.read(&mut [0]) {
            Ok(0) => break,
            Err(e) if disconnected(&e) => break,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            other => panic!("unexpected trickle response: {other:?}"),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    flow.callback("state=expected&code=after-trickle");
    assert_eq!(flow.finish().unwrap().code, "after-trickle");
}

#[test]
fn invalid_utf8_oversize_and_eof_close_only_their_connection() {
    let flow = Flow::new(Policy {
        line_bytes: 128,
        request: Duration::from_secs(2),
        ..TEST_POLICY
    });
    assert!(flow.request(b"\xff\r\n").starts_with(b"HTTP/1.1 400"));
    let mut oversized = flow.connect();
    oversized.write_all(&[b'x'; 128]).unwrap();
    assert_closed(&mut oversized);
    let mut incomplete = flow.connect();
    incomplete.write_all(b"GET /auth").unwrap();
    incomplete.shutdown(std::net::Shutdown::Write).unwrap();
    assert_closed(&mut incomplete);
    flow.callback("state=expected&code=good");
    assert_eq!(flow.finish().unwrap().code, "good");
}

#[test]
fn cancellation_closes_idle_and_partial_connections_promptly() {
    let flow = Flow::new(TEST_POLICY);
    let mut idle = flow.connect();
    let mut partial = flow.connect();
    partial.write_all(b"GET /auth/callback?").unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let start = Instant::now();
    flow.cancel.store(true, Ordering::Relaxed);
    assert!(flow.finish().is_err());
    assert!(start.elapsed() < Duration::from_millis(200));
    assert_closed(&mut idle);
    assert_closed(&mut partial);
}

#[test]
fn overall_deadline_is_not_reset_by_stray_traffic() {
    let flow = Flow::new(Policy {
        flow: Duration::from_millis(150),
        ..TEST_POLICY
    });
    let mut idle = flow.connect();
    let deadline = Instant::now() + Duration::from_millis(600);
    loop {
        assert!(
            Instant::now() < deadline,
            "stray traffic kept the flow alive"
        );
        match flow.result.try_recv() {
            Ok(result) => {
                assert!(result.is_err());
                break;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => panic!("callback worker disappeared"),
        }
        if let Ok(mut stream) = TcpStream::connect(flow.addr) {
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let _ = stream.write_all(b"GET /favicon.ico HTTP/1.1\r\n");
            let _ = stream.read_to_end(&mut Vec::new());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_closed(&mut idle);
}

#[test]
fn blocked_response_expires_without_waiting_for_the_peer() {
    struct Blocked(usize);

    impl Read for Blocked {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("a response must not read");
        }
    }

    impl Write for Blocked {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            self.0 += 1;
            Err(ErrorKind::WouldBlock.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            panic!("a response must not flush");
        }
    }

    let start = Instant::now();
    let deadline = start + Duration::from_millis(50);
    let mut connection = Connection {
        stream: Blocked(0),
        deadline,
        phase: Phase::Writing {
            response: b"acknowledgement".to_vec(),
            sent: 0,
        },
    };

    let before_deadline = deadline.checked_sub(Duration::from_nanos(1)).unwrap();
    for now in [start, before_deadline] {
        assert!(matches!(
            connection.advance(now, POLICY.line_bytes),
            Progress::Pending
        ));
    }
    assert_eq!(connection.stream.0, 2);
    assert!(matches!(
        connection.advance(deadline, POLICY.line_bytes),
        Progress::Closed
    ));
    assert_eq!(connection.stream.0, 2, "expiry performs no further I/O");
    let Phase::Writing { sent, .. } = connection.phase else {
        panic!("expected response")
    };
    assert_eq!(sent, 0);
}

#[test]
fn acknowledgement_expiry_preserves_an_already_validated_callback() {
    let listener = bind(&["127.0.0.1:0"]).unwrap();
    let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    peer.write_all(b"GET /auth/callback?state=expected&code=good HTTP/1.1\r\n")
        .unwrap();
    let received = receive(
        &listener,
        "expected",
        &AtomicBool::new(false),
        Policy {
            flow: Duration::from_millis(100),
            response: Duration::from_secs(1),
            poll: Duration::from_secs(1),
            ..TEST_POLICY
        },
    )
    .unwrap();
    assert_eq!(received.code, "good");
}

#[test]
fn binding_tries_addresses_in_order_and_cancellation_precedes_accept() {
    let listener = bind(&["not-an-address", "127.0.0.1:0"]).unwrap();
    let mut queued = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    queued
        .write_all(b"GET /auth/callback?state=state&code=bad HTTP/1.1\r\n")
        .unwrap();
    let error = accept_callback(&listener, "state", &Arc::new(AtomicBool::new(true)))
        .err()
        .unwrap();
    assert!(error.contains("cancelled"));
    assert!(bind(&[]).is_err());
}

#[cfg(target_os = "windows")]
#[test]
fn windows_launch_target_preserves_the_entire_url() {
    let url = "https://example.test/auth?state=a&code=b";
    let wide = windows_shell_target(url);
    assert_eq!(wide.last(), Some(&0));
    assert_eq!(String::from_utf16(&wide[..wide.len() - 1]).unwrap(), url);
}
