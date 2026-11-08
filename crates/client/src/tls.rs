//! Encrypted connections to the server.
//!
//! SynCord servers make their own certificate, which no certificate authority
//! has signed, so the app trusts a server the first time it connects and
//! remembers its certificate's fingerprint (like SSH does). After that, a server
//! showing a different certificate is refused: that would mean it was
//! reinstalled, or someone is pretending to be it. Servers with a proper
//! certificate (for example behind `wss://chat.example.com`) are checked the
//! normal way and don't need remembering.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

/// Error text used when a remembered server shows a different certificate.
pub const IDENTITY_CHANGED: &str = "syncord: server identity changed";

/// SHA-256 of a certificate in the same form the server logs it (hex pairs joined by colons).
pub fn fingerprint(cert: &[u8]) -> String {
    Sha256::digest(cert).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

#[derive(Debug)]
struct Verifier {
    /// The fingerprint remembered for this server, if any.
    pin: Option<String>,
    /// Fingerprint of the certificate the server showed (read after connecting).
    seen: Arc<Mutex<Option<String>>>,
    /// The normal check, for servers with a certificate from a certificate authority.
    webpki: Arc<rustls::client::WebPkiServerVerifier>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let print = fingerprint(end_entity);
        *self.seen.lock().unwrap() = Some(print.clone());
        if self.webpki.verify_server_cert(end_entity, intermediates, server_name, ocsp, now).is_ok() {
            return Ok(ServerCertVerified::assertion());
        }
        match &self.pin {
            Some(pin) if *pin == print => Ok(ServerCertVerified::assertion()),
            Some(_) => Err(rustls::Error::General(IDENTITY_CHANGED.into())),
            // First time: trust it, and the app remembers it.
            None => Ok(ServerCertVerified::assertion()),
        }
    }

    // The handshake signatures are always checked, so only the real server (holding the
    // private key) can use its certificate.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// TLS settings for one server, and where to find the fingerprint it showed.
pub struct Tls {
    pub config: Arc<ClientConfig>,
    pub seen: Arc<Mutex<Option<String>>>,
}

pub fn config(pin: Option<String>) -> Tls {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let webpki = rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .expect("the built-in root certificates are valid");
    let seen = Arc::new(Mutex::new(None));
    let verifier = Verifier { pin, seen: seen.clone(), webpki, algorithms: provider.signature_verification_algorithms };
    let mut config = ClientConfig::builder_with_provider(provider.clone() as Arc<CryptoProvider>)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Tls { config: Arc::new(config), seen }
}

/// An HTTP client (for pictures and files) that trusts the same server certificate.
pub fn http_client(pin: Option<String>) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .use_preconfigured_tls((*config(pin).config).clone())
        .build()
        .unwrap_or_default()
}
