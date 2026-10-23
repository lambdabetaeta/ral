//! Outbound HTTPS for every client exarch builds.
//!
//! Trust is anchored in a bundled root store rather than the host's, and the
//! liveness bounds here turn a silent or black-holed connection into a
//! retryable error.

use crate::site::{Silent, site};
use std::time::Duration;

/// Longest genai waits for a stream's response headers: room for a slow upstream to open.
pub(crate) const STREAM_HEADER_TIMEOUT: Duration = Duration::from_mins(3);

/// Longest genai waits for the next raw body chunk, comments included. Long
/// enough to ride out an upstream buffering mid-think (100 s observed); a dead
/// peer is the keep-alives' to catch.
pub(crate) const STREAM_READ_TIMEOUT: Duration = Duration::from_mins(10);

/// The backstop under genai's stream bounds for every client sharing this one,
/// held clear so genai's named timeout wins on a stream.
const READ_TIMEOUT: Duration = Duration::from_secs(STREAM_READ_TIMEOUT.as_secs() + 30);

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
/// It sets no *total* request timeout: [`READ_TIMEOUT`] and the keep-alives
/// are what bound a hung request, so a slow one is never cut off.
pub(crate) fn client() -> reqwest::Client {
    site!(Silent::ProviderClient, reqwest::Client::builder())
        .use_preconfigured_tls(config())
        .read_timeout(READ_TIMEOUT)
        .tcp_keepalive(TCP_KEEP_ALIVE)
        .http2_keep_alive_interval(H2_KEEP_ALIVE_INTERVAL)
        .http2_keep_alive_timeout(H2_KEEP_ALIVE_TIMEOUT)
        .http2_keep_alive_while_idle(true)
        .build()
        .expect("preconfigured rustls reqwest client")
}
