//! Certificate-blind QUIC client endpoint, feature-gated for test tooling
//! (conformance suite, weft-tui). The M1 server boots on a fresh
//! self-signed certificate — there is nothing to pin until
//! `/.well-known/weft` lands in M2 — so test clients accept any cert.
//! Never ship this pattern in a real client.

use std::io;
use std::sync::Arc;

use quinn::crypto::rustls::QuicClientConfig;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};

#[derive(Debug)]
struct AcceptAnyCert(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// A client endpoint (bound to an ephemeral port) that accepts any server
/// certificate and offers the given ALPN.
pub fn client_endpoint(alpn: &[u8]) -> io::Result<quinn::Endpoint> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(io::Error::other)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![alpn.to_vec()];
    let mut endpoint = quinn::Endpoint::client(([0, 0, 0, 0], 0).into())?;
    let mut config = quinn::ClientConfig::new(Arc::new(
        QuicClientConfig::try_from(tls).map_err(io::Error::other)?,
    ));
    // §3.4: QUIC keepalive substitutes for sending PINGs. Without it the
    // transport idle limit reaps quiet-but-healthy connections.
    config.transport_config(std::sync::Arc::new(crate::quic::transport_config(Some(
        std::time::Duration::from_secs(15),
    ))));
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}
