//! Outbound HTTPS: trust policy and transport liveness.
//!
//! Two concerns for the clients exarch builds. First, trust: every client
//! validates the peer against the bundled Mozilla webpki root store rather
//! than the operating system's trust store ([`config`]). This keeps exarch
//! self-contained: it runs unchanged on container images that ship no
//! `ca-certificates` bundle — where reqwest's default
//! `rustls-platform-verifier` path loads zero roots and aborts the client
//! build — and it sidesteps platform-verifier quirks such as macOS rejecting
//! an otherwise-valid certificate with `OSStatus -26276`.
//!
//! Second, liveness: the transport [`client`] arms a per-read idle timeout
//! ([`STREAM_IDLE_TIMEOUT`]) and keep-alive probes so a silent or black-holed
//! connection becomes a retryable transport error rather than a forever-hung
//! stream.

use std::time::Duration;

/// Idle bound for the transport: the longest the socket may go *without
/// reading a single byte* before the connection is judged dead.  reqwest's
/// [`read_timeout`](reqwest::ClientBuilder::read_timeout) arms it per read
/// and resets it on every byte received, so liveness is measured at the
/// byte/SSE-frame level — an SSE keepalive, a `ping` event, or a partial
/// frame that genai consumes without emitting a decoded `ChatStreamEvent`
/// all keep the stream alive.  It is *not* a total cap on the response: a
/// legitimately slow completion that keeps dribbling bytes runs as long as
/// it likes.  Without it a connection that goes silent (TCP open, bytes
/// stopped) blocks the stream forever — `next()` never wakes.  The timeout
/// turns that dead silence into a retryable transport error instead; 180s is
/// generous enough not to trip on a slow high-effort time-to-first-token, and the total
/// idle budget stays bounded across the transient retry budget (~3 attempts,
/// see [`MAX_ATTEMPTS`]).
///
/// [`MAX_ATTEMPTS`]: crate::provider::MAX_ATTEMPTS
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_mins(3);

/// rustls config validating against the bundled Mozilla webpki roots.
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

/// How often to probe an HTTP/2 connection with a PING frame, and how long
/// to wait for the PONG before judging the connection dead.  Without these,
/// a black-holed connection (TCP open, bytes silently dropped by a NAT or
/// LB) sits in reqwest's pool looking healthy, and every retry of a stalled
/// request is dealt onto the *same* dead connection — observed in production
/// as a run burning its whole provider retry budget on back-to-back
/// `STREAM_IDLE_TIMEOUT` stalls while a fresh process succeeded at once.
/// PINGs turn that silence into a transport error in ~45s and evict the
/// connection, so the next attempt dials fresh.
const H2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(30);
const H2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(15);
/// TCP-level keepalive: the same dead-peer detection for HTTP/1.1
/// connections, which h2 PINGs do not cover.
const TCP_KEEP_ALIVE: Duration = Duration::from_secs(30);

/// A `reqwest::Client` bound to [`config`], for the genai transport and model
/// listing — which would otherwise let genai build a client against the system
/// trust store. It sets no *total* request timeout (a legitimately slow
/// completion stays alive) and layers on the liveness bounds documented at
/// [`STREAM_IDLE_TIMEOUT`], [`H2_KEEP_ALIVE_INTERVAL`], and [`TCP_KEEP_ALIVE`].
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
