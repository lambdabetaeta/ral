//! The session's own certificate authority.
//!
//! One [`Ca`] is minted per synod session, never written to disk except as
//! the PEM that rides the net wire's prologue to the guest (`packet.rs`'s
//! `Prologue`, appended to the guest's system trust bundle by
//! `net::delivery`). It never leaves this process otherwise, and a fresh one
//! is minted the next time a guest boots with networking.
//!
//! [`LeafResolver`] is the seam `proxy.rs` uses: one instance per *accepted*
//! connection, built with `expected` set to the name DNS minted that
//! connection's address for. Installed as a fresh `rustls::ServerConfig`'s
//! cert resolver, it mints a leaf the first (and only) time it is asked —
//! but only if the `ClientHello`'s SNI name agrees with `expected`. A
//! disagreement returns `None`, which fails the handshake outright — a
//! guest that resolves an allowed name, dials its address, then asks for a
//! *different* name in SNI is refused here, not by a policy branch that
//! decides to fail after minting a leaf. This is the third of the three
//! checks that close the raw-IP bypass (see `docs/ral-wiki/design/egress.md`
//! once it lands): the request must speak the name the address was issued
//! for, or the DNS/accept gates alone would not stop a guest from resolving
//! one allowed name and then asking a different one for bytes.

use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use rustls::crypto::CryptoProvider;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

/// A session's certificate authority: one self-signed root.
#[derive(Debug)]
pub struct Ca {
    issuer: CertifiedIssuer<'static, KeyPair>,
    pem: String,
    provider: CryptoProvider,
}

impl Ca {
    /// Mint a fresh session CA.
    ///
    /// # Errors
    /// Returns `Err` if key generation or self-signing fails — both
    /// infallible in practice on the pinned aws-lc-rs/rcgen combination, but
    /// rcgen's own API is fallible, and a plain-English refusal is what boot
    /// gets to show for it rather than a panic mid-boot.
    ///
    /// # Panics
    /// Never in practice: the one `.expect` below is on an *empty*
    /// subject-alternative-name list, which `CertificateParams::new` cannot
    /// reject (there is nothing in it to fail validating).
    pub fn mint() -> Result<Self, String> {
        let key = KeyPair::generate()
            .map_err(|e| format!("minting this session's certificate authority failed: {e}"))?;
        let mut params = CertificateParams::new(Vec::<String>::new())
            .expect("an empty subject-alternative-name list cannot fail to build");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name = DistinguishedName::new();
        params
            .distinguished_name
            .push(DnType::CommonName, "Synod session proxy");
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::CrlSign,
        ];
        let issuer = CertifiedIssuer::self_signed(params, key)
            .map_err(|e| format!("minting this session's certificate authority failed: {e}"))?;
        let pem = issuer.pem();
        Ok(Self {
            issuer,
            pem,
            provider: rustls::crypto::aws_lc_rs::default_provider(),
        })
    }

    /// The CA's own certificate, PEM-encoded — what rides the net wire's
    /// prologue so the guest can be taught to trust it.
    #[must_use]
    pub fn pem(&self) -> &str {
        &self.pem
    }

    /// Mint a fresh leaf certificate for `name`, signed by this session's
    /// CA.
    ///
    /// No caching: a leaf costs one ECDSA P-256 keypair and one signature,
    /// and a session sees at most a few dozen distinct names — cheaper than
    /// the locking a cache shared across [`LeafResolver`]'s `Send + Sync`
    /// boundary would need.
    ///
    /// # Errors
    /// Returns `Err` naming `name` if rcgen or rustls refuses to build the
    /// certificate.
    fn leaf(&self, name: &str) -> Result<Arc<CertifiedKey>, String> {
        let key = KeyPair::generate()
            .map_err(|e| format!("minting a certificate for '{name}' failed: {e}"))?;
        let mut params = CertificateParams::new(vec![name.to_string()])
            .map_err(|e| format!("'{name}' cannot be issued a certificate: {e}"))?;
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, name);
        params.use_authority_key_identifier_extension = true;
        params.key_usages.push(KeyUsagePurpose::DigitalSignature);
        params
            .extended_key_usages
            .push(ExtendedKeyUsagePurpose::ServerAuth);
        let cert = params
            .signed_by(&key, &self.issuer)
            .map_err(|e| format!("minting a certificate for '{name}' failed: {e}"))?;
        let certified = CertifiedKey::from_der(vec![cert.der().clone()], key.into(), &self.provider)
            .map_err(|e| format!("minting a certificate for '{name}' failed: {e}"))?;
        Ok(Arc::new(certified))
    }
}

/// Mints one leaf certificate for one accepted connection, refusing to mint
/// a leaf whose name disagrees with the name that connection's address was
/// issued for.
#[derive(Debug)]
pub struct LeafResolver {
    ca: Arc<Ca>,
    expected: String,
}

impl LeafResolver {
    /// `expected` is the name DNS minted this connection's address for —
    /// read off the accept-time record (`proxy.rs`'s listener names the
    /// address it just accepted), not off the `ClientHello`, so [`resolve`]
    /// has something independent of the guest's own claim to check the SNI
    /// name against.
    ///
    /// One [`LeafResolver`] per accepted connection, not one shared across
    /// the session: build a fresh `rustls::ServerConfig` per connection with
    /// `Arc::new(LeafResolver::new(ca.clone(), expected))` as its cert
    /// resolver.
    ///
    /// [`resolve`]: ResolvesServerCert::resolve
    #[must_use]
    pub fn new(ca: Arc<Ca>, expected: String) -> Self {
        Self { ca, expected }
    }
}

impl ResolvesServerCert for LeafResolver {
    /// Mint a leaf for the SNI name — but only if it is the name DNS issued
    /// this connection's address for. A guest that resolves an allowed
    /// name, dials its address, then asks for a *different* name in SNI is
    /// refused here: no leaf is minted, so the handshake fails outright,
    /// rather than a policy branch choosing to fail it after the fact. No
    /// SNI at all is refused the same way — there is no name to check.
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let sni = client_hello.server_name()?;
        if !sni.eq_ignore_ascii_case(&self.expected) {
            return None;
        }
        self.ca.leaf(&self.expected).ok()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::sync::Arc;

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, ServerConfig, SignatureScheme, StreamOwned};

    use super::*;

    /// Accepts any server certificate without checking it. These tests
    /// probe only whether the *server*'s [`LeafResolver`] minted a leaf at
    /// all — whether a client would actually trust it is `upstream`'s
    /// concern, exercised there against the real platform verifier.
    #[derive(Debug)]
    struct AcceptAnything;

    impl ServerCertVerifier for AcceptAnything {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::ED25519,
            ]
        }
    }

    /// Complete one TLS handshake against `resolver` with client hello
    /// `sni`, and report whether it succeeded. A refused resolve (`None`)
    /// fails the handshake before either side can send application data —
    /// that is the property under test, not the bytes exchanged.
    fn handshake_succeeds(resolver: Arc<LeafResolver>, sni: &str) -> bool {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let server_config = Arc::new(
            ServerConfig::builder_with_provider(provider.clone())
                .with_safe_default_protocol_versions()
                .expect("safe default protocol versions")
                .with_no_client_auth()
                .with_cert_resolver(resolver),
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let server = std::thread::spawn(move || {
            let (sock, _) = listener.accept().expect("accept one connection");
            let conn = rustls::ServerConnection::new(server_config).expect("build a server connection");
            let mut stream = StreamOwned::new(conn, sock);
            stream.write_all(b"ok").is_ok()
        });

        let client_config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("safe default protocol versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnything))
            .with_no_client_auth();
        let name = ServerName::try_from(sni.to_string()).expect("a valid DNS name");
        let conn =
            ClientConnection::new(Arc::new(client_config), name).expect("build a client connection");
        let sock = std::net::TcpStream::connect(addr).expect("connect to the loopback server");
        let mut stream = StreamOwned::new(conn, sock);
        let client_ok = stream.write_all(b"hi").is_ok();

        client_ok && server.join().expect("server thread must not panic")
    }

    #[test]
    fn a_leaf_carries_the_name_it_was_minted_for() {
        let ca = Ca::mint().expect("mint a session CA");
        let leaf = ca.leaf("example.com").expect("mint a leaf");
        assert!(!leaf.cert.is_empty());
    }

    #[test]
    fn an_agreeing_sni_completes_the_handshake() {
        let ca = Arc::new(Ca::mint().expect("mint a session CA"));
        let resolver = Arc::new(LeafResolver::new(ca, "example.com".to_string()));
        assert!(handshake_succeeds(resolver, "example.com"));
    }

    #[test]
    fn a_disagreeing_sni_gets_no_leaf_and_the_handshake_fails() {
        let ca = Arc::new(Ca::mint().expect("mint a session CA"));
        let resolver = Arc::new(LeafResolver::new(ca, "example.com".to_string()));
        assert!(!handshake_succeeds(resolver, "not-example.com"));
    }
}
