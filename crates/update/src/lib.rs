//! Automatic updates, shared by the app and the server.
//!
//! Each GitHub release carries a `latest.json` manifest listing the files for
//! every platform with their SHA-256 and an Ed25519 signature made with the
//! project's private release key. An update is only installed if its
//! signature checks out against [`PUBLIC_KEY`], so a tampered download (or a
//! fake manifest) is refused.

use std::collections::HashMap;

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Where the newest release's manifest lives.
pub const MANIFEST_URL: &str = "https://github.com/bxpo/SynCord/releases/latest/download/latest.json";

/// Public half of the release signing key (the private half is a GitHub secret).
pub const PUBLIC_KEY: [u8; 32] = [
    0xf4, 0x34, 0xdc, 0xde, 0x95, 0xea, 0x6b, 0xc6, 0x07, 0x8f, 0x57, 0xc6, 0xd3, 0x29, 0x71, 0x17, 0xbf, 0x6b, 0xd6,
    0xca, 0x17, 0x54, 0x87, 0x0e, 0x8f, 0x20, 0x8c, 0x0d, 0xca, 0x32, 0x80, 0x35,
];

/// Manifest keys for each downloadable file.
pub mod files {
    pub const WINDOWS_APP: &str = "windows";
    pub const WINDOWS_SERVER: &str = "windows-server";
    /// The Celerity.app disk image.
    pub const MACOS_APP: &str = "macos-app";
    /// For SynCord 0.6 and older, whose updater looks for SynCord.app: the same app in a
    /// disk image under that name.
    pub const MACOS_LEGACY: &str = "macos";
    /// The portable app (`celerity`, plus a `syncord` link for SynCord's updater).
    pub const LINUX_APP: &str = "linux";
    pub const LINUX_DEB: &str = "linux-deb";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub notes: String,
    /// Download page, for updates that have to be installed by hand.
    #[serde(default)]
    pub page: String,
    pub files: HashMap<String, FileEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub url: String,
    pub sha256: String,
    pub size: u64,
    /// Base64 Ed25519 signature of the file's bytes. Unsigned files are never installed.
    #[serde(default)]
    pub signature: Option<String>,
}

/// `"1.2.3"` (optionally with a leading `v`) as numbers.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(version)
}

/// True if `candidate` is a newer version than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

/// Check a download against its manifest entry: size, SHA-256 and signature.
pub fn verify(bytes: &[u8], entry: &FileEntry, key: &[u8; 32]) -> Result<(), String> {
    if bytes.len() as u64 != entry.size {
        return Err(format!("download is {} bytes, expected {}", bytes.len(), entry.size));
    }
    let digest: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    if !digest.eq_ignore_ascii_case(&entry.sha256) {
        return Err("download is corrupted (checksum mismatch)".into());
    }
    let sig = entry.signature.as_deref().ok_or("this release isn't signed, so it won't be installed automatically")?;
    let sig = base64::engine::general_purpose::STANDARD.decode(sig).map_err(|_| "bad signature encoding")?;
    let sig = Signature::from_slice(&sig).map_err(|_| "bad signature")?;
    let key = VerifyingKey::from_bytes(key).map_err(|_| "bad public key")?;
    key.verify_strict(bytes, &sig).map_err(|_| "the signature doesn't match: refusing to install".to_string())
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("Celerity-updater/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default()
}

pub async fn fetch_manifest(http: &reqwest::Client, url: &str) -> Result<Manifest, String> {
    let resp = http.get(url).send().await.map_err(|e| format!("couldn't check for updates: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("couldn't check for updates (error {})", resp.status().as_u16()));
    }
    let body = resp.bytes().await.map_err(|e| format!("couldn't check for updates: {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("the update information is unreadable: {e}"))
}

/// Download a file and refuse it unless it verifies.
pub async fn download(http: &reqwest::Client, entry: &FileEntry, key: &[u8; 32]) -> Result<Vec<u8>, String> {
    let resp = http.get(&entry.url).send().await.map_err(|e| format!("update download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("update download failed (error {})", resp.status().as_u16()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("update download failed: {e}"))?.to_vec();
    verify(&bytes, entry, key)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn signed_entry(bytes: &[u8], key: &SigningKey) -> FileEntry {
        FileEntry {
            name: "f".into(),
            url: "http://x/f".into(),
            sha256: Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect(),
            size: bytes.len() as u64,
            signature: Some(base64::engine::general_purpose::STANDARD.encode(key.sign(bytes).to_bytes())),
        }
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn verification() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let public = key.verifying_key().to_bytes();
        let file = b"new version".to_vec();
        let entry = signed_entry(&file, &key);
        assert!(verify(&file, &entry, &public).is_ok());

        // Tampered file, wrong size, wrong key, missing signature: all refused.
        let mut evil = file.clone();
        evil[0] ^= 1;
        assert!(verify(&evil, &entry, &public).is_err());
        assert!(verify(b"short", &entry, &public).is_err());
        let other = SigningKey::from_bytes(&[8u8; 32]);
        assert!(verify(&file, &signed_entry(&file, &other), &public).is_err());
        let unsigned = FileEntry { signature: None, ..entry.clone() };
        assert!(verify(&file, &unsigned, &public).unwrap_err().contains("isn't signed"));
        // A signature from the right key over different bytes doesn't help an attacker.
        let swapped = FileEntry { signature: signed_entry(b"other", &key).signature, ..entry };
        assert!(verify(&file, &swapped, &public).is_err());
    }

    #[test]
    fn embedded_key_is_valid() {
        assert!(VerifyingKey::from_bytes(&PUBLIC_KEY).is_ok());
    }

    #[test]
    fn manifest_parses() {
        let json = r#"{"version":"0.3.0","files":{"windows":{"name":"a.exe","url":"https://x/a.exe","sha256":"00","size":1}}}"#;
        let m: Manifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.files[files::WINDOWS_APP].signature, None);
    }
}
