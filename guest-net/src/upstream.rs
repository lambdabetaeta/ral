//! The outbound half: the HTTP client this process uses to fetch, on the
//! guest's behalf, from the real internet.
//!
//! Trust here is **deliberately the inverse of
//! `exarch::provider::tls::config()`**: that client validates against a
//! bundled set of webpki roots so exarch keeps working on a container image
//! that ships no OS trust store; this one validates against *this
//! computer's own* trust store, via [`rustls_platform_verifier`], because
//! the guest is about to be told these bytes are safe to `pip install` or
//! `apt install` on the strength of them, and the guest never gets to
//! consult a store of its own — this is the one machine, and the one store,
//! standing behind that judgement. The two choices must never be swapped:
//! see the paired paragraph on `exarch::provider::tls::config()`'s own doc
//! comment.
//!
//! There is deliberately no fallback to the bundled webpki roots if the
//! platform store cannot be read: `webpki-roots` is in no line of this
//! crate's `Cargo.toml`, so there is nothing to fall back to even in
//! principle, and [`client`] returns a plain-English refusal instead of
//! silently trusting less than this computer's owner configured it to
//! trust. Guest networking simply does not start.
//!
//! No total response timeout: a 200 MB wheel legitimately takes a while to
//! stream, and `NetPolicy::max_response_bytes` — enforced by whoever reads
//! the body, not by this client — is what actually bounds a transfer. A
//! connect timeout still applies, so a host that never answers the TCP
//! handshake does not hang the fetch forever.
//!
//! Redirects are **returned, not followed**: this client has no policy to
//! check a redirect's target against, and following one here would let an
//! allowed host bounce a request onto one the allowlist never admitted.
//! Whoever reads the 3xx (`proxy.rs`) hands it back to the guest's own HTTP
//! client, which re-enters DNS, accept and verb gates for the new host on
//! its own dial — strictly stronger than following it host-side ever was.

use std::time::Duration;

use rustls_platform_verifier::Verifier;

/// The longest this client waits for a TCP connect to complete before
/// giving up. Independent of the (absent) total-response cap: a slow
/// download that is still receiving bytes is never touched by this bound.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Build the outbound client, trusting this computer's own certificate
/// store.
///
/// # Errors
/// Returns a plain-English `Err` if this computer's trust store cannot be
/// read. There is no fallback trust source to build a working client from
/// in that case — see the module doc.
pub fn client() -> Result<reqwest::blocking::Client, String> {
    let verifier = Verifier::new(std::sync::Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .map_err(|e| {
        format!(
            "this computer's certificate trust store could not be read ({e}), so no site can be \
             checked as genuine — guest networking will not start"
        )
    })?;
    build(verifier, None)
}

/// Build a `reqwest` client against a given `rustls` server-certificate
/// verifier, optionally overriding one hostname's resolution — the trust
/// policy and the resolution override are the only things that vary between
/// [`client`] (this computer's store, real DNS) and the tests below (an
/// extra trusted root, a loopback address); everything else this module
/// cares about (timeouts, redirects) is shared here so the two paths cannot
/// drift apart.
fn build(
    verifier: impl rustls::client::danger::ServerCertVerifier + 'static,
    resolve: Option<(&str, std::net::SocketAddr)>,
) -> Result<reqwest::blocking::Client, String> {
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(verifier))
        .with_no_client_auth();
    let mut builder = reqwest::blocking::Client::builder()
        .use_preconfigured_tls(config)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(None);
    if let Some((host, addr)) = resolve {
        builder = builder.resolve(host, addr);
    }
    builder
        .build()
        .map_err(|e| format!("building the outbound HTTP client failed: {e}"))
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Arc;

    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
        ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
    };
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use rustls_platform_verifier::Verifier;

    use super::*;

    const HOST: &str = "upstream-test.invalid";

    /// A validity window for the tests that are not exercising expiry
    /// itself: short enough (under 825 days) to satisfy the CA/Browser
    /// Forum baseline requirement the platform verifier enforces even for a
    /// locally added trust root, and wide enough to stay valid without
    /// updating this file for a while. Bump both dates forward if this ever
    /// starts failing on `an_expired_certificate_is_refused` reasoning
    /// applied to the *other* three tests instead.
    const VALID: ((i32, u8, u8), (i32, u8, u8)) = ((2026, 1, 1), (2027, 6, 30));

    /// A CA and one leaf for it, both minted fresh, valid from `not_before`
    /// to `not_after` — `rcgen::date_time_ymd` dates rather than relative
    /// arithmetic, so this module needs no direct dependency on the `time`
    /// crate for a type it otherwise never has to name.
    fn signed_leaf(
        name: &str,
        not_before: (i32, u8, u8),
        not_after: (i32, u8, u8),
    ) -> (
        CertificateDer<'static>,
        PrivateKeyDer<'static>,
        CertificateDer<'static>,
    ) {
        let ca_key = KeyPair::generate().expect("generate a CA key");
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("empty SAN list");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
        let ca = CertifiedIssuer::self_signed(ca_params, ca_key).expect("self-sign the test CA");

        let leaf_key = KeyPair::generate().expect("generate a leaf key");
        let mut leaf_params = CertificateParams::new(vec![name.to_string()]).expect("valid SAN");
        leaf_params.distinguished_name = DistinguishedName::new();
        leaf_params.distinguished_name.push(DnType::CommonName, name);
        leaf_params.not_before = rcgen::date_time_ymd(not_before.0, not_before.1, not_before.2);
        leaf_params.not_after = rcgen::date_time_ymd(not_after.0, not_after.1, not_after.2);
        leaf_params
            .extended_key_usages
            .push(ExtendedKeyUsagePurpose::ServerAuth);
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca).expect("sign the leaf");

        (leaf_cert.der().clone(), leaf_key.into(), ca.der().clone())
    }

    /// Serve one TLS connection with `cert`/`key`, replying 200 to whatever
    /// arrives — the response text names the property under test: a client
    /// that correctly refuses the certificate never reads it.
    fn serve_once(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> std::net::SocketAddr {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("safe default protocol versions")
                .with_no_client_auth()
                .with_single_cert(vec![cert], key)
                .expect("a valid cert/key pair"),
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
        let addr = listener.local_addr().expect("local addr");
        std::thread::spawn(move || {
            let Ok((sock, _)) = listener.accept() else {
                return;
            };
            let Ok(conn) = rustls::ServerConnection::new(config) else {
                return;
            };
            let mut stream = rustls::StreamOwned::new(conn, sock);
            let mut buf = [0u8; 1024];
            if stream.read(&mut buf).is_ok() {
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n\
                      not one byte of this",
                );
            }
        });
        addr
    }

    /// A verifier trusting only `roots` — never the real system store, so
    /// each test's outcome depends solely on the one property it varies
    /// (issuer, name, validity).
    fn trusting(roots: impl IntoIterator<Item = CertificateDer<'static>>) -> Verifier {
        Verifier::new_with_extra_roots(roots, Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
            .expect("build a verifier with these trusted roots")
    }

    /// `true` if `GET https://{HOST}/` (resolved to `addr` without touching
    /// real DNS) reads back a whole response — `false` the moment
    /// verification, or anything else about the request, fails.
    fn get(addr: std::net::SocketAddr, verifier: impl rustls::client::danger::ServerCertVerifier + 'static) -> bool {
        build(verifier, Some((HOST, addr)))
            .expect("build the outbound client")
            .get(format!("https://{HOST}/"))
            .send()
            .is_ok()
    }

    #[test]
    fn a_certificate_from_an_unknown_ca_is_refused_and_no_bytes_arrive() {
        let (cert, key, _ca) = signed_leaf(HOST, VALID.0, VALID.1);
        let addr = serve_once(cert, key);
        // Trusting no root at all: the freshly minted CA above is unknown,
        // exactly as it would be to any real trust store.
        let verifier = trusting([]);
        assert!(!get(addr, verifier));
    }

    #[test]
    fn a_certificate_for_the_wrong_name_is_refused_and_no_bytes_arrive() {
        let (cert, key, ca) = signed_leaf("not-upstream-test.invalid", VALID.0, VALID.1);
        let addr = serve_once(cert, key);
        let verifier = trusting([ca]);
        assert!(!get(addr, verifier));
    }

    #[test]
    fn an_expired_certificate_is_refused_and_no_bytes_arrive() {
        let (cert, key, ca) = signed_leaf(HOST, (2020, 1, 1), (2021, 1, 1));
        let addr = serve_once(cert, key);
        let verifier = trusting([ca]);
        assert!(!get(addr, verifier));
    }

    #[test]
    fn a_valid_certificate_from_a_trusted_ca_is_accepted() {
        let (cert, key, ca) = signed_leaf(HOST, VALID.0, VALID.1);
        let addr = serve_once(cert, key);
        let verifier = trusting([ca]);
        assert!(get(addr, verifier));
    }
}
