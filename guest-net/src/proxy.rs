//! The intercepting proxy: turns an [`Accepted`] connection into one judged
//! HTTP request — this is what makes a host grant a *verb* grant.
//!
//! Gate three lives here, split by port. On 443 there is no `CONNECT` to
//! answer; the name comes from SNI, and [`crate::ca::LeafResolver`] is the
//! one place that checks it, by simply declining to mint a leaf for a name
//! that disagrees with the one DNS issued this connection's address for —
//! so the handshake itself fails, and this module never sees a policy
//! branch deciding to fail it after the fact. Repeating that check here
//! would be a second enforcement of the same fact, free to drift out of
//! step with the first, so this module does not. On 80 there is no
//! handshake to fail, so the equivalent check is this module's own, made
//! against the `Host:` header — but it honours the same asymmetry
//! [`Refusal::WrongName`] already carries: a mismatch is dropped, never
//! answered, on either port.
//!
//! ALPN offers `http/1.1` only — every tool this crate exists for (apt,
//! pip, npm, curl) speaks it, and nothing here parses anything else.
//! Redirects are relayed verbatim, never followed: the guest's own client
//! re-dials whatever `Location` names, re-entering DNS, accept, and this
//! module's own checks for that host, which is strictly stronger than
//! resolving it on this side ever was.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use exarch::egress::{Egress, Record};
use rustls::server::Acceptor;
use rustls::{ServerConfig, ServerConnection, StreamOwned};

use crate::ca::{Ca, LeafResolver};
use crate::pipe::ProxyPipe;
use crate::refusal::Refusal;
use crate::stack::{Accepted, Handler};

/// Told once per name a session's user has not already been shown a card for.
///
/// The name, then the plain-English sentence to show. `synod`'s to supply;
/// see [`handler`] for the dedup this crate applies before ever calling it.
pub type Blocked = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// The longest a request line and its headers may run before this proxy
/// gives up on the guest ever finishing them. Not a [`Refusal`] — no policy
/// question is open yet, just a peer not behaving like an HTTP client, so
/// the connection is simply dropped, exactly as a broken frame is in
/// `ral_daemon::packet`.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// Guest request headers relayed upstream verbatim, matched against the
/// wire's own header names case-insensitively — HTTP field names are, and a
/// guest sending `range:` must not slip past the list by spelling. Every
/// other header the guest sends is dropped, hop-by-hop ones foremost
/// (`Connection`, `Keep-Alive`, `Transfer-Encoding`, `TE`, `Trailer`,
/// `Upgrade`, `Proxy-Authorization`, `Proxy-Authenticate` — not named here
/// because none of them ever reach this far: they describe *this* hop, and
/// forwarding one to the next hop is a bug, not a policy choice).
///
/// A relayed header is an exfiltration channel of the same class as the
/// query string `docs/ral-wiki/design/egress.md`'s honest residual already
/// accounts for — naming this list widens nothing that page closed, and
/// the ledger still records the request whether or not a header inside it
/// turns out to have mattered. What naming buys over denying the
/// hop-by-hop set and relaying the rest is that a header HTTP has not
/// invented yet is dropped by default, not relayed by default — and in
/// the meantime `apt` gets `Range` to resume a partial fetch, `PyPI`'s JSON
/// API gets `Accept` to negotiate against, and a host that answers 403 to
/// a client with no `User-Agent` gets one.
const RELAYED_HEADERS: &[&str] = &[
    "accept",
    "accept-encoding",
    "accept-language",
    "range",
    "if-match",
    "if-none-match",
    "if-modified-since",
    "if-unmodified-since",
    "if-range",
    "user-agent",
    "authorization",
    "content-type",
];

/// Build the [`Handler`] `stack::run` spawns a worker thread with for
/// every accepted connection.
///
/// `blocked` is called at most once per distinct name for the life of the
/// returned handler — one `git clone` against an off-list host trips the
/// gate on every object it fetches, and a card per attempt is the same
/// storm the per-domain consent card was retired for. The audit ledger
/// carries no such deduplication: every attempt, carded or not, is
/// `egress.audit`'s.
///
/// # Errors
/// Returns `Err` if the outbound client cannot be built — see
/// [`crate::upstream::client`]; guest networking does not start on this
/// host without one.
///
/// # Panics
/// Panics only if the dedup set's own lock is poisoned, which only follows
/// a prior panic while a worker thread held it.
pub fn handler(egress: Egress, ca: Arc<Ca>, blocked: Blocked) -> Result<Handler, String> {
    let client = crate::upstream::client()?;
    let carded: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
    Ok(Arc::new(move |accepted: Accepted| {
        let Accepted { name, port, pipe } = accepted;
        if let Err(refusal) = serve(pipe, &name, port, &egress, &client, &ca) {
            let mut seen = carded.lock().expect("card dedup set poisoned");
            if seen.insert(name.clone()) {
                drop(seen);
                blocked(&name, &refusal.sentence());
            }
        }
    }))
}

/// Serve one accepted connection to completion: terminate TLS if `port` is
/// 443, read the one HTTP request the guest sends, judge it, fetch it, and
/// relay the answer back — or refuse it, leaving the audit ledger the only
/// record of what happened.
fn serve(
    mut pipe: ProxyPipe,
    name: &str,
    port: u16,
    egress: &Egress,
    client: &reqwest::blocking::Client,
    ca: &Arc<Ca>,
) -> Result<(), Refusal> {
    if port == 443 {
        let mut tls = accept_tls(pipe, name, ca)?;
        handle_request(&mut tls, name, "https", egress, client)
    } else {
        handle_request(&mut pipe, name, "http", egress, client)
    }
}

/// Complete the TLS handshake, minting a leaf for `name` — never for
/// whatever the `ClientHello`'s SNI actually asks for, which is the whole
/// of gate three on this port. A failed handshake (no hello arrives, no
/// SNI, a disagreeing one, or anything else `rustls` refuses) is reported
/// as [`Refusal::WrongName`], whose `page` this crate never delivers —
/// there is no session yet to deliver it on.
fn accept_tls(
    mut pipe: ProxyPipe,
    name: &str,
    ca: &Arc<Ca>,
) -> Result<StreamOwned<ServerConnection, ProxyPipe>, Refusal> {
    let no_hello = || Refusal::WrongName {
        requested: "no TLS hello arrived".to_string(),
        issued_for: name.to_string(),
    };
    let mut acceptor = Acceptor::default();
    let accepted = loop {
        match acceptor.read_tls(&mut pipe) {
            Ok(0) | Err(_) => return Err(no_hello()),
            Ok(_) => {}
        }
        match acceptor.accept() {
            Ok(Some(accepted)) => break accepted,
            Ok(None) => {} // the hello has not fully arrived yet — read more
            Err(_) => return Err(no_hello()),
        }
    };
    let requested = accepted
        .client_hello()
        .server_name()
        .map_or_else(|| "no SNI name".to_string(), str::to_string);

    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("safe default protocol versions")
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(LeafResolver::new(ca.clone(), name.to_string())));
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    accepted
        .into_connection(Arc::new(config))
        .map(|conn| StreamOwned::new(conn, pipe))
        .map_err(|_| Refusal::WrongName {
            requested,
            issued_for: name.to_string(),
        })
}

/// Read the one HTTP request `stream` sends, judge it against `egress`, and
/// either relay it upstream and back or refuse it in place — every path
/// through here ends with exactly one `egress.audit.record` for this
/// request, allowed or not.
///
/// The response is read to completion into `received` below before a byte
/// of it reaches the guest, up to `max_response_bytes` (256 MB by
/// default) — never streamed with a running cut-off. Streaming would let a
/// response that trips [`Refusal::TooBig`] mid-body have already delivered
/// however much of it fit, past the point this function ever judges
/// anything: a truncated file is not a refused one, it is a corrupt one
/// the guest has no way to tell from a good download that happened to end
/// early. Buffering keeps the two outcomes disjoint — either the whole
/// response, once judged, reaches the guest, or none of it does — at the
/// cost of holding a large response in RAM before relaying any of it.
fn handle_request<S: Read + Write>(
    stream: &mut S,
    name: &str,
    scheme: &str,
    egress: &Egress,
    client: &reqwest::blocking::Client,
) -> Result<(), Refusal> {
    let Some(Request {
        method,
        url,
        body,
        headers,
    }) = read_request(stream, name, scheme, egress.policy.max_response_bytes)
    else {
        return Ok(()); // malformed or abandoned: no policy question was ever open
    };

    if !egress.policy.allows(&method, name) {
        return deny(
            stream,
            egress,
            &method,
            &url,
            name,
            Refusal::VerbDenied {
                method: method.clone(),
                host: name.to_string(),
            },
        );
    }
    if !egress.limiter.try_take() {
        return deny(
            stream,
            egress,
            &method,
            &url,
            name,
            Refusal::TooMany {
                host: name.to_string(),
            },
        );
    }

    let Ok(reqwest_method) = reqwest::Method::from_bytes(method.as_bytes()) else {
        return deny(
            stream,
            egress,
            &method,
            &url,
            name,
            Refusal::VerbDenied {
                method: method.clone(),
                host: name.to_string(),
            },
        );
    };
    let mut builder = client.request(reqwest_method, &url);
    for (header, value) in &headers {
        builder = builder.header(*header, value);
    }
    if !body.is_empty() {
        builder = builder.body(body);
    }
    let Ok(mut response) = builder.send() else {
        return deny(
            stream,
            egress,
            &method,
            &url,
            name,
            Refusal::NotTrusted {
                host: name.to_string(),
            },
        );
    };

    let status = response.status();
    let cap = egress.policy.max_response_bytes;
    let mut received = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let Ok(n) = response.read(&mut chunk) else {
            return deny(
                stream,
                egress,
                &method,
                &url,
                name,
                Refusal::NotTrusted {
                    host: name.to_string(),
                },
            );
        };
        if n == 0 {
            break;
        }
        received.extend_from_slice(&chunk[..n]);
        if received.len() as u64 > cap {
            return deny(
                stream,
                egress,
                &method,
                &url,
                name,
                Refusal::TooBig {
                    host: name.to_string(),
                    limit: cap,
                },
            );
        }
    }

    egress.audit.record(Record::Request {
        method: &method,
        url: &url,
        host: name,
        allowed: true,
        status: Some(status.as_u16()),
        bytes: Some(received.len() as u64),
        note: None,
    });
    let _ = write_response(stream, status, response.headers(), &received);
    Ok(())
}

/// Audit `refusal` against the one request it belongs to, deliver its page
/// if it has one, and hand it back as the `Err` [`handle_request`] ends on.
fn deny<S: Write>(
    stream: &mut S,
    egress: &Egress,
    method: &str,
    url: &str,
    host: &str,
    refusal: Refusal,
) -> Result<(), Refusal> {
    egress.audit.record(Record::Request {
        method,
        url,
        host,
        allowed: false,
        status: None,
        bytes: None,
        note: Some(&refusal.sentence()),
    });
    if let Some(page) = refusal.page() {
        let _ = stream.write_all(&page);
    }
    Err(refusal)
}

/// One guest request, reduced to what may cross to the origin: the verb the
/// policy judges, the URL the ledger records, the body, and whichever of
/// [`RELAYED_HEADERS`] the guest actually sent.
struct Request {
    method: String,
    url: String,
    body: Vec<u8>,
    headers: Vec<(&'static str, String)>,
}

/// Read one HTTP/1.1 request off `stream`: the request line, the headers
/// [`RELAYED_HEADERS`] names, and whatever body `Content-Length` names.
/// `None` for anything not worth a refusal — headers that never finish, a
/// request past [`MAX_HEADER_BYTES`], a `Content-Length` past `max_body`
/// (the declared length is a number the guest made up, so it is refused
/// before a byte of the body is buffered for it — `ral_daemon::packet`'s
/// own law, applied to HTTP), or a `Host:` that disagrees with `name`
/// (gate three's port-80 half; see the module doc for why that mismatch is
/// dropped here rather than turned into a [`Refusal`]).
fn read_request<S: Read>(
    stream: &mut S,
    name: &str,
    scheme: &str,
    max_body: u64,
) -> Option<Request> {
    let mut buf = Vec::new();
    let (method, path, host, content_length, header_len, relayed) = loop {
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_HEADER_BYTES {
            return None;
        }
        let mut headers = [httparse::EMPTY_HEADER; 32];
        let mut request = httparse::Request::new(&mut headers);
        match request.parse(&buf) {
            Ok(httparse::Status::Complete(header_len)) => {
                let method = request.method?.to_string();
                let path = request.path?.to_string();
                let mut host = None;
                let mut content_length = 0usize;
                let mut relayed = Vec::new();
                for h in request.headers.iter() {
                    if h.name.eq_ignore_ascii_case("host") {
                        host = std::str::from_utf8(h.value)
                            .ok()
                            .map(|v| v.split(':').next().unwrap_or(v).to_string());
                    } else if h.name.eq_ignore_ascii_case("content-length") {
                        content_length = std::str::from_utf8(h.value)
                            .ok()
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0);
                    } else if let Some(&canonical) =
                        RELAYED_HEADERS.iter().find(|c| h.name.eq_ignore_ascii_case(c))
                        && let Ok(v) = std::str::from_utf8(h.value)
                    {
                        relayed.push((canonical, v.to_string()));
                    }
                }
                break (method, path, host, content_length, header_len, relayed);
            }
            Ok(httparse::Status::Partial) => {}
            Err(_) => return None,
        }
    };

    if !host.is_some_and(|h| h.eq_ignore_ascii_case(name)) {
        return None;
    }
    if content_length as u64 > max_body {
        return None;
    }

    let mut body = buf[header_len..].to_vec();
    let mut chunk = vec![0u8; 64 * 1024];
    while body.len() < content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Some(Request {
        method,
        url: format!("{scheme}://{name}{path}"),
        body,
        headers: relayed,
    })
}

/// Write `status`/`headers`/`body` back to the guest as one HTTP/1.1
/// response. Drops the hop-by-hop headers this relay itself owns
/// (`Connection`, `Transfer-Encoding`) and the origin's own
/// `Content-Length`, recomputed here from `body`'s actual length — never
/// the origin's claim, which a chunked or streamed reply may not state
/// truthfully in advance.
fn write_response<S: Write>(
    stream: &mut S,
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> std::io::Result<()> {
    use std::fmt::Write as _;
    let mut out = format!(
        "HTTP/1.1 {} {}\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    for (name, value) in headers {
        if matches!(
            name.as_str(),
            "connection" | "transfer-encoding" | "content-length"
        ) {
            continue;
        }
        if let Ok(v) = value.to_str() {
            out.push_str(name.as_str());
            out.push_str(": ");
            out.push_str(v);
            out.push_str("\r\n");
        }
    }
    let _ = write!(
        out,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(out.as_bytes())?;
    stream.write_all(body)
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, ClientConnection};

    use super::*;
    use crate::pipe::new_pipe;

    fn test_egress() -> Egress {
        let policy = exarch::net_policy::NetPolicy {
            allow: vec![exarch::net_policy::Rule {
                host: "example.com".to_string(),
                methods: vec!["GET".to_string()],
            }],
            max_response_bytes: 1_048_576,
            rate_per_minute: 1_000,
            search: false,
        };
        let path = std::env::temp_dir().join(format!(
            "guest-net-proxy-test-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Egress {
            policy: Arc::new(policy),
            audit: exarch::egress::AuditLog::for_test(&path),
            limiter: exarch::egress::RateLimiter::new(1_000),
        }
    }

    /// One end of a duplex `Vec<u8>` for feeding a hand-written request
    /// into [`handle_request`] and capturing whatever it writes back.
    struct Recorded {
        input: std::io::Cursor<Vec<u8>>,
        pub output: Vec<u8>,
    }
    impl Read for Recorded {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(out)
        }
    }
    impl Write for Recorded {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct AcceptAnything;
    impl rustls::client::danger::ServerCertVerifier for AcceptAnything {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            vec![rustls::SignatureScheme::ECDSA_NISTP256_SHA256]
        }
    }

    /// The SNI-disagreement check as it actually runs in this crate: a
    /// `ClientHello` naming a different host than the one DNS issued this
    /// connection's address for never gets a leaf, so `accept_tls` reports
    /// [`Refusal::WrongName`] before any HTTP is ever read. `ca`'s own
    /// tests prove [`LeafResolver`] refuses to mint in isolation; this
    /// proves this module's `Acceptor` wiring around it does the same.
    #[test]
    fn a_disagreeing_sni_is_refused_before_any_http_is_read() {
        let ca = Arc::new(Ca::mint().expect("mint a session CA"));
        let (core, pipe) = new_pipe(None);

        let client = std::thread::spawn(move || {
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let config = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("safe default protocol versions")
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAnything))
                .with_no_client_auth();
            let sni = ServerName::try_from("not-example.com".to_string()).expect("a valid name");
            let mut conn =
                ClientConnection::new(Arc::new(config), sni).expect("build a client connection");
            let mut hello = Vec::new();
            conn.write_tls(&mut hello)
                .expect("a fresh connection has a ClientHello to write");
            core.push_from_guest(&hello);
        });

        let result = accept_tls(pipe, "example.com", &ca);
        assert!(matches!(result, Err(Refusal::WrongName { .. })));
        client.join().expect("client thread must not panic");
    }

    #[test]
    fn a_post_to_a_read_only_host_is_refused_in_plain_english() {
        let egress = test_egress();
        let client = reqwest::blocking::Client::new();
        let request = b"POST / HTTP/1.1\r\nHost: example.com\r\nContent-Length: 0\r\n\r\n".to_vec();
        let mut stream = Recorded {
            input: std::io::Cursor::new(request),
            output: Vec::new(),
        };

        let err = handle_request(&mut stream, "example.com", "http", &egress, &client)
            .expect_err("POST is not among example.com's granted methods");
        assert!(matches!(err, Refusal::VerbDenied { .. }));

        let page = String::from_utf8(stream.output).expect("the 403 page is ASCII");
        assert!(page.starts_with("HTTP/1.1 403 Forbidden"));
        for jargon in [
            "VM",
            "sandbox",
            "capability",
            "manifest",
            "desk",
            "transport",
            "card",
        ] {
            assert!(
                !page.to_lowercase().contains(&jargon.to_lowercase()),
                "refusal page must not say '{jargon}', got: {page}"
            );
        }
    }

    /// Reads example.com's rule (`GET` only) as `test_egress` set it up, so
    /// a `GET` here reaches the verb gate and the upstream fetch proper —
    /// exercising this module's own reaction to a fetch failure, which
    /// `upstream`'s four verification tests do not: they test the client
    /// in isolation, never this crate's decision about what a failed fetch
    /// hands back to the guest. Not one byte of whatever the plain-TCP peer
    /// below might have sent reaches the guest either way — what does
    /// arrive is this crate's own 403 page, [`Refusal::NotTrusted`] being
    /// one of the deliverable variants (the inbound TLS session with the
    /// guest is already open; only the *outbound* leg failed).
    #[test]
    fn a_failed_upstream_fetch_delivers_only_the_refusal_page() {
        let egress = test_egress();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            // A plain TCP listener with nothing behind it that speaks
            // TLS — the client's handshake fails before either side sends
            // anything meaningful, which is exactly the property under
            // test.
            if let Ok((mut sock, _)) = listener.accept() {
                let mut sink = [0u8; 4096];
                let _ = sock.read(&mut sink);
            }
        });
        let client = reqwest::blocking::Client::builder()
            .resolve("example.com", addr)
            .build()
            .expect("build a client");
        let request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n".to_vec();
        let mut stream = Recorded {
            input: std::io::Cursor::new(request),
            output: Vec::new(),
        };

        let err = handle_request(&mut stream, "example.com", "https", &egress, &client)
            .expect_err("a plain-TCP peer cannot complete a TLS handshake");
        assert!(matches!(err, Refusal::NotTrusted { .. }));
        let page = String::from_utf8(stream.output).expect("the 403 page is ASCII");
        assert!(page.starts_with("HTTP/1.1 403 Forbidden"), "got: {page}");
    }

    /// [`RELAYED_HEADERS`] both ways, with the guest sending every header
    /// in mixed case so the match itself is proven case-insensitive: a
    /// listed header (`rAnGe`) reaches the upstream request line, and an
    /// unnamed one (`COOKIE`) plus a hop-by-hop one (`Connection`) do not.
    #[test]
    fn only_allowlisted_headers_reach_upstream_matched_case_insensitively() {
        let egress = test_egress();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_upstream = captured.clone();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                if let Ok(n) = sock.read(&mut buf) {
                    captured_upstream.lock().expect("lock").extend_from_slice(&buf[..n]);
                }
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        let client = reqwest::blocking::Client::builder()
            .resolve("example.com", addr)
            .build()
            .expect("build a client");
        let request = b"GET / HTTP/1.1\r\nHost: example.com\r\n\
                         rAnGe: bytes=0-10\r\nCOOKIE: unique-cookie-marker\r\n\
                         Connection: unique-connection-marker\r\n\r\n"
            .to_vec();
        let mut stream = Recorded {
            input: std::io::Cursor::new(request),
            output: Vec::new(),
        };

        handle_request(&mut stream, "example.com", "http", &egress, &client)
            .expect("GET is granted for example.com");

        let sent = String::from_utf8(captured.lock().expect("lock").clone()).expect("request is ASCII");
        assert!(sent.contains("bytes=0-10"), "range was not relayed, got: {sent}");
        assert!(!sent.contains("unique-cookie-marker"), "cookie leaked, got: {sent}");
        assert!(
            !sent.contains("unique-connection-marker"),
            "hop-by-hop header leaked, got: {sent}"
        );
    }
}
