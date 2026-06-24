//! Outbound HTTPS trust policy.
//!
//! Every client exarch builds for an outbound request validates the peer
//! against the bundled Mozilla webpki root store rather than the operating
//! system's trust store. This keeps exarch self-contained: it runs unchanged
//! on container images that ship no `ca-certificates` bundle — where reqwest's
//! default `rustls-platform-verifier` path loads zero roots and aborts the
//! client build — and it sidesteps platform-verifier quirks such as macOS
//! rejecting an otherwise-valid certificate with `OSStatus -26276`.

use std::time::Duration;

/// Idle bound for the transport: the longest the socket may go *without
/// reading a single byte* before the connection is judged dead.  reqwest's
/// [`read_timeout`](reqwest::ClientBuilder::read_timeout) arms it per read
/// and resets it on every byte received, so liveness is measured at the
/// byte/SSE-frame level — an SSE keepalive, a `ping` event, or a partial
/// frame that genai consumes without emitting a decoded `ChatStreamEvent`
/// all keep the stream alive.  It is *not* a total cap on the response: a
/// legitimately slow completion that keeps dribbling bytes runs as long as
/// it likes, bounded only by the harness wall.  Without it a connection
/// that goes silent (TCP open, bytes stopped) blocks the stream forever;
/// under the terminal-bench harness that hang is reaped only by the 900s
/// wall, which kills the run and emits an empty result.json.  120s is
/// generous enough not to trip on a slow time-to-first-token, yet — even
/// after the transient retry budget (~3 attempts, see [`MAX_ATTEMPTS`]) —
/// stays well under that wall, so a truly stalled stream surfaces as a
/// retryable transport error instead of hanging.
///
/// [`MAX_ATTEMPTS`]: crate::provider::MAX_ATTEMPTS
pub(crate) const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

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

/// A `reqwest::Client` bound to [`config`], for the genai transport and model
/// listing — which would otherwise let genai build a client against the system
/// trust store. No *total* request timeout, so a long, legitimately slow
/// completion stays alive; the only liveness bound is [`STREAM_IDLE_TIMEOUT`]
/// as a per-read timeout, which resets on every byte and so detects a stalled
/// socket at the byte/SSE level rather than at the decoded-event level.
pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_preconfigured_tls(config())
        .read_timeout(STREAM_IDLE_TIMEOUT)
        .build()
        .expect("preconfigured rustls reqwest client")
}
