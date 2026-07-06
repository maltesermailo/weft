//! Namespace root keys (§2.1). Generated and held **only** here — the server
//! receives the *public* key at `NS CREATE` and never the secret; the webview
//! never sees it either. Stored as a 32-byte Ed25519 seed (base64) in the app
//! data dir, one file per `(network, namespace)`, `0600` on unix.
//!
//! This is the owner's crown jewel: everything in a namespace chains from it
//! (moderator tokens, transfer, recovery). File storage matches how weftd
//! persists its own key (`weftd.key`); an OS keychain is the hardening upgrade.

use std::fs;
use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};
use weft_crypto::Keypair;

fn keys_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app data dir: {e}"))?
        .join("ns-keys");
    fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    Ok(dir)
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// A flat, filesystem-safe path per `(network, namespace)`.
fn key_path(app: &AppHandle, network: &str, namespace: &str) -> Result<PathBuf, String> {
    Ok(keys_dir(app)?.join(format!("{}__{}.key", sanitize(network), sanitize(namespace))))
}

/// Device signing keys (§10.2), one per `(host, account)`, in their own dir.
fn device_path(app: &AppHandle, host: &str, account: &str) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app data dir: {e}"))?
        .join("device-keys");
    fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    Ok(dir.join(format!("{}__{}.key", sanitize(host), sanitize(account))))
}

/// Generate + store a device keypair, returning its base64 public key for
/// `AUTH ENROLL`. Re-enrolling replaces the local key (the old pubkey stays
/// enrolled server-side until revoked — a known simplification).
pub fn enroll_device(app: &AppHandle, host: &str, account: &str) -> Result<String, String> {
    let keypair = Keypair::generate();
    write_secret(&device_path(app, host, account)?, &keypair.seed_b64())?;
    Ok(keypair.public().to_b64())
}

/// Load a stored device keypair for `AUTH KEY`/`AUTH PROOF` login.
pub fn load_device(app: &AppHandle, host: &str, account: &str) -> Option<Keypair> {
    let seed = fs::read_to_string(device_path(app, host, account).ok()?).ok()?;
    Keypair::from_seed_b64(seed.trim()).ok()
}

/// Is a device key enrolled locally for this `(host, account)`?
pub fn has_device(app: &AppHandle, host: &str, account: &str) -> bool {
    device_path(app, host, account)
        .map(|p| p.exists())
        .unwrap_or(false)
}

fn write_secret(path: &Path, seed_b64: &str) -> Result<(), String> {
    fs::write(path, seed_b64).map_err(|e| format!("writing key: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Generate a fresh namespace root keypair, persist the secret locally, and
/// return the base64 **public** key for the `@root=` tag. Refuses to clobber
/// an existing key (that would orphan the namespace).
pub fn generate_ns_key(app: &AppHandle, network: &str, namespace: &str) -> Result<String, String> {
    let path = key_path(app, network, namespace)?;
    if path.exists() {
        return Err(format!(
            "a root key for '{namespace}' already exists on this device"
        ));
    }
    let keypair = Keypair::generate();
    write_secret(&path, &keypair.seed_b64())?;
    Ok(keypair.public().to_b64())
}

/// Load a stored namespace root keypair for signing (future TRANSFER/RECOVERY).
/// The secret never leaves this process.
#[allow(dead_code)]
pub fn load_ns_key(app: &AppHandle, network: &str, namespace: &str) -> Result<Keypair, String> {
    let path = key_path(app, network, namespace)?;
    let seed = fs::read_to_string(&path)
        .map_err(|_| format!("no root key for '{namespace}' on this device"))?;
    Keypair::from_seed_b64(seed.trim()).map_err(|e| format!("corrupt root key: {e:?}"))
}
