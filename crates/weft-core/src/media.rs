//! Media data-plane token registry (§13, M-media-0). The control plane mints
//! tokens that gate the *bytes* (which ride the data plane, handled in weftd):
//!
//! - an **upload grant** is one-time, minted by `STREAM OFFER`, bound to the
//!   offered mime + size ceiling; the transfer consumes it.
//! - a **bearer** authorizes fetches for an account (membership-gating lands in
//!   M-media-1; for the spike a valid bearer = allowed to fetch).
//!
//! The registry never sees blob bytes — it only says who may move them.
//!
//! Spike limitation: tokens are held in memory with no TTL/eviction yet;
//! M-media-1 adds expiry + a sweep (and per-blob fetch scoping).

use std::collections::HashMap;
use std::sync::Mutex;

use rand::RngCore;
use weft_proto::Account;

/// Hard ceiling on a single blob (§13 RECOMMENDED 500 MiB video); weftd config
/// may lower it per deployment (M-media-1).
pub const MEDIA_MAX_BYTES: u64 = 500 * 1024 * 1024;

/// Parse a `weft-media://<origin>/<b3-hash>` reference into `(origin, hash)`.
/// `None` if malformed. Used to validate `attach.N=` and gate fetches (§13).
pub fn parse_media_uri(uri: &str) -> Option<(&str, &str)> {
    let rest = uri.strip_prefix("weft-media://")?;
    let (origin, hash) = rest.split_once('/')?;
    (!origin.is_empty() && !hash.is_empty() && !hash.contains('/')).then_some((origin, hash))
}

/// A one-time authorization to upload exactly one blob.
#[derive(Debug, Clone)]
pub struct UploadGrant {
    pub account: Account,
    pub mime: String,
    /// The offered size; the transfer must not exceed it.
    pub max_bytes: u64,
}

/// An unguessable token (192 random bits, hex) — used for both grants + bearers.
fn random_token() -> String {
    let mut bytes = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().fold(String::with_capacity(48), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

#[derive(Default)]
pub(crate) struct MediaRegistry {
    uploads: Mutex<HashMap<String, UploadGrant>>,
    bearers: Mutex<HashMap<String, Account>>,
    /// §6/§13 one-time backfill grants: a serialized `BATCH` (newline-delimited
    /// `Reply` lines) minted when a HISTORY page exceeds the stream threshold,
    /// pulled once over the data plane (`BACKFILL <token>`) then dropped. The
    /// body is already membership-gated at mint time, so the token alone (192
    /// unguessable bits, one-time) is the capability — like an upload grant.
    backfills: Mutex<HashMap<String, Vec<u8>>>,
}

impl MediaRegistry {
    pub(crate) fn mint_upload(&self, grant: UploadGrant) -> String {
        let token = random_token();
        self.uploads
            .lock()
            .expect("media lock")
            .insert(token.clone(), grant);
        token
    }

    /// Consume an upload grant (one-time) if it exists.
    pub(crate) fn take_upload(&self, token: &str) -> Option<UploadGrant> {
        self.uploads.lock().expect("media lock").remove(token)
    }

    pub(crate) fn mint_bearer(&self, account: Account) -> String {
        let token = random_token();
        self.bearers
            .lock()
            .expect("media lock")
            .insert(token.clone(), account);
        token
    }

    pub(crate) fn bearer_account(&self, token: &str) -> Option<Account> {
        self.bearers.lock().expect("media lock").get(token).cloned()
    }

    pub(crate) fn mint_backfill(&self, body: Vec<u8>) -> String {
        let token = random_token();
        self.backfills
            .lock()
            .expect("media lock")
            .insert(token.clone(), body);
        token
    }

    /// Consume a backfill grant (one-time) if it exists.
    pub(crate) fn take_backfill(&self, token: &str) -> Option<Vec<u8>> {
        self.backfills.lock().expect("media lock").remove(token)
    }
}
