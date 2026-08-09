//! Blobs across the bridge (matrix.md §12).
//!
//! Both directions are a *copy*, not a reference: neither side can fetch the
//! other's blobs (weftd's `/media` is not a Matrix homeserver, and an `mxc://`
//! means nothing to a WEFT client), so the bridge downloads and re-uploads.
//!
//! The asymmetry worth knowing: weftd's fetch is **content-addressed and
//! unauthenticated** — the 256-bit BLAKE3 hash *is* the capability, obtainable
//! only from a message you can already see — while the upload needs a one-shot
//! grant from `STREAM OFFER`. So WEFT→Matrix needs no credential at all, and
//! Matrix→WEFT needs the control-stream round trip.

use anyhow::Context as _;

/// weftd's HTTP media plane (§13). Separate from the control stream, and
/// separate from the homeserver's — one client, two endpoints.
#[derive(Clone)]
pub struct WeftMedia {
    http: reqwest::Client,
    base: String,
}

/// How long one blob transfer may take. Same reason as `hs::HS_TIMEOUT` — these
/// calls are awaited inline in the dispatch loop, so an untimed one stops the whole
/// bridge — but a blob is megabytes where a state fetch is bytes, so it gets longer.
const MEDIA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

impl WeftMedia {
    pub fn new(base: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(MEDIA_TIMEOUT)
                .build()
                .expect("a reqwest client with only a timeout set always builds"),
            base: base.trim_end_matches('/').to_string(),
        }
    }

    /// Fetch a blob by hash. No credential: weftd serves content-addressed
    /// blobs to anyone holding the hash (§13's media-proxy model).
    pub async fn fetch(&self, hash: &str) -> anyhow::Result<Vec<u8>> {
        let res = self
            .http
            .get(format!("{}/media/{hash}", self.base))
            .send()
            .await
            .context("fetching a WEFT blob")?;
        let status = res.status();
        let bytes = res.bytes().await.context("reading a WEFT blob")?;

        anyhow::ensure!(status.is_success(), "blob fetch failed: {status}");

        Ok(bytes.to_vec())
    }

    /// Post a blob with an upload grant from `STREAM OFFER`. Returns the
    /// `weft-media://<hash>` reference to attach.
    pub async fn upload(&self, token: &str, bytes: Vec<u8>, mime: &str) -> anyhow::Result<String> {
        let res = self
            .http
            .post(format!("{}/media", self.base))
            .query(&[("t", token)])
            .header(reqwest::header::CONTENT_TYPE, mime)
            .body(bytes)
            .send()
            .await
            .context("uploading a WEFT blob")?;
        let status = res.status();
        let v: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);

        anyhow::ensure!(status.is_success(), "blob upload failed: {status} {v}");

        let hash = v["hash"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("upload response without a hash: {v}"))?;

        Ok(format!("weft-media://{hash}"))
    }
}

/// Everything a deferred attachment message needs once its blob has a hash.
pub struct PendingParts {
    pub sender: String,
    pub channel: String,
    pub body: String,
    pub msgid: String,
    pub event_id: String,
    pub room_id: String,
}

/// The `mxc://` of a Matrix message's attachment, if it has one. Encrypted
/// content (`file`) is deliberately ignored: an e2ee room is never bridged
/// (invariant 8), so a `file` here means a client sent encrypted content into a
/// plain room — not ours to decrypt.
pub fn attachment_of(content: &serde_json::Value) -> Option<(String, String, String)> {
    let msgtype = content["msgtype"].as_str()?;
    if !matches!(msgtype, "m.image" | "m.file" | "m.video" | "m.audio") {
        return None;
    }

    let url = content["url"].as_str()?.to_string();
    let mime = content["info"]["mimetype"]
        .as_str()
        .unwrap_or("application/octet-stream")
        .to_string();
    let name = content["body"].as_str().unwrap_or("file").to_string();

    Some((url, mime, name))
}

/// The Matrix `msgtype` a mime belongs to — the inverse of the filter above.
pub fn msgtype_for(mime: &str) -> &'static str {
    match mime.split('/').next().unwrap_or_default() {
        "image" => "m.image",
        "video" => "m.video",
        "audio" => "m.audio",
        _ => "m.file",
    }
}

/// A `weft-media://<hash>` reference's hash.
pub fn weft_hash(reference: &str) -> Option<&str> {
    reference.strip_prefix("weft-media://")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn attachments_are_recognized_by_msgtype_not_by_shape() {
        let image = json!({
            "msgtype": "m.image",
            "body": "cat.png",
            "url": "mxc://kde.org/abc",
            "info": { "mimetype": "image/png" },
        });
        let (url, mime, name) = attachment_of(&image).expect("an attachment");
        assert_eq!(url, "mxc://kde.org/abc");
        assert_eq!(mime, "image/png");
        assert_eq!(name, "cat.png");

        // Plain text is not an attachment…
        assert!(attachment_of(&json!({ "msgtype": "m.text", "body": "hi" })).is_none());
        // …and encrypted content is not ours to decrypt (invariant 8).
        assert!(attachment_of(&json!({
            "msgtype": "m.image",
            "body": "secret.png",
            "file": { "url": "mxc://kde.org/enc" },
        }))
        .is_none());
    }

    #[test]
    fn mimes_map_to_msgtypes_and_references_unwrap() {
        assert_eq!(msgtype_for("image/webp"), "m.image");
        assert_eq!(msgtype_for("video/mp4"), "m.video");
        assert_eq!(msgtype_for("audio/ogg"), "m.audio");
        assert_eq!(msgtype_for("application/pdf"), "m.file");
        assert_eq!(msgtype_for(""), "m.file");

        assert_eq!(weft_hash("weft-media://abc123"), Some("abc123"));
        assert_eq!(weft_hash("mxc://kde.org/abc"), None);
    }
}
