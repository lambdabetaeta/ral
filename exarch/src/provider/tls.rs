//! Outbound HTTPS for every client exarch builds.
//!
//! Trust is anchored in a bundled root store rather than the host's, and the
//! liveness bounds here turn a silent or black-holed connection into a
//! retryable error.

use std::time::Duration;

/// Longest the socket may go without reading a byte before the connection is
/// judged dead. Armed per read and reset by every byte, so an SSE keepalive
/// counts as life and a slow completion runs as long as it likes; only true
/// silence — TCP open, bytes stopped, `next()` never waking — trips it, into a
/// retryable transport error. `retry::idle_timeout` reuses the value as attempt
/// one's bound on *opening* a stream, where three minutes is the room a slow
/// high-effort time-to-first-token needs.
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_mins(3);

/// rustls config validating against the bundled Mozilla webpki roots, never the
/// host's trust store: exarch must run unchanged on container images that ship
/// no `ca-certificates` bundle, where reqwest's default platform verifier finds
/// zero roots and the client build aborts. It also sidesteps verifier quirks
/// like macOS rejecting a valid certificate with `OSStatus -26276`.
pub(crate) fn config() -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .roots
        .extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    rustls::ClientConfig::builder_with_provider(
        rustls::crypto::aws_lc_rs::default_provider().into(),
    )
    .with_safe_default_protocol_versions()
    .expect("rustls safe default protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth()
}

/// PING interval and PONG deadline for HTTP/2. Without them a black-holed
/// connection — TCP open, bytes silently dropped by a NAT or LB — sits in
/// reqwest's pool looking healthy, and every retry is dealt onto that same dead
/// socket, spending the whole budget on back-to-back idle stalls. The probes
/// evict it in ~45s, so the next attempt dials fresh.
const H2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
const H2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(15);
/// The same dead-peer detection for HTTP/1.1, which h2 PINGs do not reach.
const TCP_KEEP_ALIVE: Duration = Duration::from_secs(30);

/// A `reqwest::Client` bound to [`config`], handed to the genai transport and
/// the model listing so neither builds its own against the host trust store.
/// It sets no *total* request timeout: [`STREAM_IDLE_TIMEOUT`] and the
/// keep-alives are what bound a hung request, so a slow one is never cut off.
pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_preconfigured_tls(config())
        .read_timeout(STREAM_IDLE_TIMEOUT)
        .tcp_keepalive(TCP_KEEP_ALIVE)
        .http2_keep_alive_interval(H2_KEEP_ALIVE_INTERVAL)
        .http2_keep_alive_timeout(H2_KEEP_ALIVE_TIMEOUT)
        .http2_keep_alive_while_idle(true)
        .build()
        .expect("preconfigured rustls reqwest client")
}
