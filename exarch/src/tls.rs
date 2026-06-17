//! Outbound HTTPS trust policy.
//!
//! Every client exarch builds for an outbound request validates the peer
//! against the bundled Mozilla webpki root store rather than the operating
//! system's trust store. This keeps exarch self-contained: it runs unchanged
//! on container images that ship no `ca-certificates` bundle — where reqwest's
//! default `rustls-platform-verifier` path loads zero roots and aborts the
//! client build — and it sidesteps platform-verifier quirks such as macOS
//! rejecting an otherwise-valid certificate with `OSStatus -26276`.

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
/// trust store. No request timeout: the transport client carries long-lived
/// streaming responses.
pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .use_preconfigured_tls(config())
        .build()
        .expect("preconfigured rustls reqwest client")
}
