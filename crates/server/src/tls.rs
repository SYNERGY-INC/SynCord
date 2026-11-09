//! Encrypted connections. The server makes itself a certificate on first start
//! (kept next to the settings file) and accepts both encrypted (TLS) and plain
//! connections on the same port: the first byte of a connection tells them
//! apart. Apps from 0.5 always encrypt and remember the server's certificate the
//! first time they connect, so nobody can pretend to be the server later.
//! Plain connections remain for older apps and for the "is it running?" page.

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_util::either::Either;
use tracing::{debug, info, warn};

pub const CERT_FILE: &str = "celerity-tls-cert.pem";
pub const KEY_FILE: &str = "celerity-tls-key.pem";
/// Names from before the rename to Celerity: kept, so apps that trust this server still do.
const LEGACY_CERT_FILE: &str = "syncord-tls-cert.pem";
const LEGACY_KEY_FILE: &str = "syncord-tls-key.pem";

/// First byte of a TLS handshake.
const TLS_HANDSHAKE: u8 = 0x16;
/// Connections that don't say anything for this long are dropped.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(15);

/// SHA-256 of a certificate, as the app shows and remembers it (hex, with colons every 2 bytes).
pub fn fingerprint(cert: &[u8]) -> String {
    let hash = Sha256::digest(cert);
    hash.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

/// Load the server's certificate, making one the first time. Returns the TLS settings and the fingerprint.
pub fn load_or_create(dir: &Path) -> Result<(Arc<ServerConfig>, String), String> {
    let (mut cert_path, mut key_path) = (dir.join(CERT_FILE), dir.join(KEY_FILE));
    if !cert_path.exists() && dir.join(LEGACY_CERT_FILE).exists() && dir.join(LEGACY_KEY_FILE).exists() {
        (cert_path, key_path) = (dir.join(LEGACY_CERT_FILE), dir.join(LEGACY_KEY_FILE));
    }
    if !cert_path.exists() || !key_path.exists() {
        let made = rcgen::generate_simple_self_signed(vec!["celerity".to_string(), "localhost".to_string()])
            .map_err(|e| format!("couldn't make a certificate: {e}"))?;
        std::fs::write(&cert_path, made.cert.pem())
            .map_err(|e| format!("couldn't save {}: {e}", cert_path.display()))?;
        write_private(&key_path, made.signing_key.serialize_pem())
            .map_err(|e| format!("couldn't save {}: {e}", key_path.display()))?;
        info!("made a new certificate for encrypted connections ({})", cert_path.display());
    }
    let cert =
        CertificateDer::from_pem_file(&cert_path).map_err(|e| format!("can't read {}: {e}", cert_path.display()))?;
    let key = PrivateKeyDer::from_pem_file(&key_path).map_err(|e| format!("can't read {}: {e}", key_path.display()))?;
    let print = fingerprint(&cert);
    let mut config = ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| format!("the certificate files don't match: {e}"))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok((Arc::new(config), print))
}

/// Write a file only the owner can read (on systems that have permissions like that).
fn write_private(path: &Path, contents: String) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(contents.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

pub type Connection = Either<TlsStream<TcpStream>, TcpStream>;

/// A listener that hands axum both encrypted and plain connections.
pub struct DualListener {
    rx: mpsc::Receiver<(Connection, SocketAddr)>,
    local: SocketAddr,
}

impl DualListener {
    pub fn new(listener: TcpListener, tls: Arc<ServerConfig>) -> io::Result<Self> {
        let local = listener.local_addr()?;
        let (tx, rx) = mpsc::channel(64);
        let acceptor = TlsAcceptor::from(tls);
        tokio::spawn(async move {
            loop {
                let (stream, addr) = match listener.accept().await {
                    Ok(c) => c,
                    Err(e) => {
                        // Usually "too many open files"; wait a moment instead of spinning.
                        warn!("accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let (tx, acceptor) = (tx.clone(), acceptor.clone());
                // Each connection is sorted out on its own, so a slow one can't hold up the rest.
                tokio::spawn(async move {
                    let _ = stream.set_nodelay(true);
                    let mut first = [0u8; 1];
                    let peeked = tokio::time::timeout(FIRST_BYTE_TIMEOUT, stream.peek(&mut first)).await;
                    let conn = match peeked {
                        Ok(Ok(1)) if first[0] == TLS_HANDSHAKE => {
                            match tokio::time::timeout(FIRST_BYTE_TIMEOUT, acceptor.accept(stream)).await {
                                Ok(Ok(tls)) => Either::Left(tls),
                                Ok(Err(e)) => return debug!("TLS handshake with {addr} failed: {e}"),
                                Err(_) => return debug!("TLS handshake with {addr} timed out"),
                            }
                        }
                        Ok(Ok(1)) => Either::Right(stream),
                        _ => return,
                    };
                    let _ = tx.send((conn, addr)).await;
                });
            }
        });
        Ok(Self { rx, local })
    }
}

impl axum::serve::Listener for DualListener {
    type Io = Connection;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(c) => c,
            // The accept loop never ends while the listener exists.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn certificate_is_made_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("celerity-tls-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (_, first) = load_or_create(&dir).unwrap();
        let (_, second) = load_or_create(&dir).unwrap();
        assert_eq!(first, second, "same certificate after a restart");
        assert_eq!(first.len(), 32 * 3 - 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(KEY_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "private key is private");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
