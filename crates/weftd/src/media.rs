//! M-media-0 data plane (§13): the filesystem content-addressed [`FsBlobStore`],
//! the QUIC data-plane stream handler, and the HTTP media endpoints. The server
//! is a **blind Delivery Service** — it stores/serves opaque bytes named by their
//! BLAKE3 hash and never interprets them.
//!
//! Two transfer surfaces share one blob store (decision: both QUIC + HTTP):
//! - **QUIC**: extra bidi streams on the control connection carry a tiny framed
//!   `PUT`/`GET` request (see [`handle_data_stream`]).
//! - **HTTP**: `POST /media` (upload) + `GET /media/<hash>` (fetch, Range) on the
//!   existing axum surface, for browsers and ranged video.
//!
//! Spike scope: uploads consume a one-time `STREAM OFFER` grant; fetches require
//! a valid media bearer (per-blob membership-gating is M-media-1). WS-binary
//! transfer is deferred (QUIC + HTTP satisfy the M0 round-trip).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use quinn::{RecvStream, SendStream};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tracing::debug;
use weft_core::{PublicKey, ServerCtx, MEDIA_MAX_BYTES};
use weft_proto::NetworkName;
use weft_store::{blob_hash, BlobHash, BlobMeta, BlobRecord, BlobStore, StoreError};

/// Peer network signing keys (from `[[peers]]`), used to verify inbound `MIRROR`
/// pull requests (§11.8). A requester proves control of its network key over the
/// data plane exactly as a bridge does over the control plane.
pub(crate) type PeerKeys = Arc<HashMap<NetworkName, PublicKey>>;

/// Max bytes of a single data-plane request (blob ceiling + a header allowance).
const MAX_REQUEST: usize = MEDIA_MAX_BYTES as usize + 4096;

fn backend(e: std::io::Error) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// `weft-media://<origin>/<b3-hash>` — the content-addressed URI (§13).
fn media_uri(ctx: &ServerCtx, hash: &BlobHash) -> String {
    format!("weft-media://{}/{}", ctx.info.network, hash)
}

// ---- filesystem content-addressed store (L3: it needs async fs I/O) ----

/// Blobs on disk, sharded `<root>/<hh>/<hash>.blob` with a `<hash>.meta` sidecar
/// (`mime\nbytes`). Dedup is by path: an existing hash is never rewritten.
pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    /// Open (creating) the blob directory.
    pub async fn open(root: PathBuf) -> std::io::Result<Self> {
        tokio::fs::create_dir_all(&root).await?;
        Ok(Self { root })
    }

    fn paths(&self, hash: &BlobHash) -> (PathBuf, PathBuf) {
        let h = hash.as_str();
        let dir = self.root.join(&h[0..2]);
        (dir.join(format!("{h}.blob")), dir.join(format!("{h}.meta")))
    }
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put(&self, mime: &str, bytes: &[u8]) -> Result<BlobHash, StoreError> {
        let hash = blob_hash(bytes);
        let (blob, meta) = self.paths(&hash);
        // Dedup: identical content is already stored under this name.
        if tokio::fs::try_exists(&blob).await.unwrap_or(false) {
            return Ok(hash);
        }
        if let Some(dir) = blob.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(backend)?;
        }
        // Write to a temp name then rename, so a reader never sees a partial blob.
        let tmp = blob.with_extension("tmp");
        tokio::fs::write(&tmp, bytes).await.map_err(backend)?;
        tokio::fs::rename(&tmp, &blob).await.map_err(backend)?;
        tokio::fs::write(&meta, format!("{mime}\n{}", bytes.len()))
            .await
            .map_err(backend)?;
        Ok(hash)
    }

    async fn get(
        &self,
        hash: &BlobHash,
        range: Option<(u64, u64)>,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let (blob, _) = self.paths(hash);
        let mut file = match tokio::fs::File::open(&blob).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(backend(e)),
        };
        match range {
            None => {
                let mut buf = Vec::new();
                file.read_to_end(&mut buf).await.map_err(backend)?;
                Ok(Some(buf))
            }
            Some((start, end_inclusive)) => {
                let len = file.metadata().await.map_err(backend)?.len();
                let start = start.min(len);
                let end = end_inclusive.saturating_add(1).min(len); // exclusive
                let count = end.saturating_sub(start);
                file.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(backend)?;
                let mut buf = vec![0u8; count as usize];
                file.read_exact(&mut buf).await.map_err(backend)?;
                Ok(Some(buf))
            }
        }
    }

    async fn stat(&self, hash: &BlobHash) -> Result<Option<BlobMeta>, StoreError> {
        let (_, meta) = self.paths(hash);
        match tokio::fs::read_to_string(&meta).await {
            Ok(s) => {
                let (mime, bytes) = s.split_once('\n').unwrap_or((s.as_str(), "0"));
                Ok(Some(BlobMeta {
                    mime: mime.to_string(),
                    bytes: bytes.trim().parse().unwrap_or(0),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(backend(e)),
        }
    }

    async fn delete(&self, hash: &BlobHash) -> Result<(), StoreError> {
        let (blob, meta) = self.paths(hash);
        // Idempotent: absence is success.
        for path in [blob, meta] {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(backend(e)),
            }
        }
        Ok(())
    }
}

// ---- QUIC data plane: one framed PUT/GET per bidi stream ----

/// Parse a `<start>-<end>` (or `<start>-`) range spec into an inclusive range.
fn parse_range_spec(spec: &str) -> Option<(u64, u64)> {
    let (start, end) = spec.split_once('-')?;
    let start = start.parse().ok()?;
    let end = if end.is_empty() {
        u64::MAX // open-ended: clamped to the blob length by the store
    } else {
        end.parse().ok()?
    };
    Some((start, end))
}

async fn respond(send: &mut SendStream, line: &str) {
    let _ = send.write_all(line.as_bytes()).await;
    let _ = send.write_all(b"\n").await;
    let _ = send.finish();
}

/// Handle one data-plane bidi stream. Request forms (first line, then, for PUT,
/// the raw bytes):
/// - `PUT <upload-token>\n<bytes…>` → `OK <weft-media://…>` | `ERR <why>`
/// - `GET <bearer> <hash> [start-end]` → `OK <len>\n<bytes…>` | `ERR <why>`
/// - `MIRROR <requester-net> <hash> <sig>` → `OK <mime> <len>\n<bytes…>` | `ERR`
///   (§11.8 federation pull: `sig` is the requester network's key over
///   `hash‖requester‖origin`; served only to a `[[peers]]`-known network.)
pub(crate) async fn handle_data_stream(
    ctx: Arc<ServerCtx>,
    peer_keys: PeerKeys,
    mut send: SendStream,
    mut recv: RecvStream,
) {
    let req = match recv.read_to_end(MAX_REQUEST).await {
        Ok(r) => r,
        Err(e) => {
            debug!("media data stream read failed: {e}");
            respond(&mut send, "ERR read").await;
            return;
        }
    };
    let nl = req.iter().position(|&b| b == b'\n').unwrap_or(req.len());
    let header = String::from_utf8_lossy(&req[..nl]).into_owned();
    let body: &[u8] = req.get(nl + 1..).unwrap_or(&[]);
    let mut parts = header.split_whitespace();

    match parts.next() {
        Some("PUT") => {
            let token = parts.next().unwrap_or("");
            match ctx.take_upload_token(token) {
                None => respond(&mut send, "ERR token").await,
                Some(grant) if body.len() as u64 > grant.max_bytes => {
                    respond(&mut send, "ERR too-large").await
                }
                Some(grant) => {
                    // §13 hash-moderation gate (stub → false until M-media-5).
                    if ctx.is_blob_blocked(blob_hash(body).as_str()).await {
                        respond(&mut send, "ERR blocked").await;
                        return;
                    }
                    match ctx.blobs.put(&grant.mime, body).await {
                        Ok(hash) => {
                            record_upload(&ctx, &hash, &grant.mime, body).await;
                            respond(&mut send, &format!("OK {}", media_uri(&ctx, &hash))).await;
                        }
                        Err(e) => {
                            debug!("blob put failed: {e}");
                            respond(&mut send, "ERR store").await;
                        }
                    }
                }
            }
        }
        Some("GET") => {
            let bearer = parts.next().unwrap_or("");
            let hash_str = parts.next().unwrap_or("");
            let range = parts.next().and_then(parse_range_spec);
            // Invariant 1: an unauthorized/absent fetch is uniformly "not found".
            let Some(account) = ctx.media_bearer_account(bearer) else {
                respond(&mut send, "ERR nosuch").await;
                return;
            };
            let Some(hash) = BlobHash::parse(hash_str) else {
                respond(&mut send, "ERR nosuch").await;
                return;
            };
            if !ctx.may_fetch(&account, hash.as_str()).await
                || ctx.is_blob_blocked(hash.as_str()).await
            {
                respond(&mut send, "ERR nosuch").await;
                return;
            }
            match ctx.blobs.get(&hash, range).await {
                Ok(Some(data)) => {
                    let _ = send
                        .write_all(format!("OK {}\n", data.len()).as_bytes())
                        .await;
                    let _ = send.write_all(&data).await;
                    let _ = send.finish();
                }
                Ok(None) => respond(&mut send, "ERR nosuch").await,
                Err(e) => {
                    debug!("blob get failed: {e}");
                    respond(&mut send, "ERR store").await;
                }
            }
        }
        Some("BACKFILL") => {
            // §6/§13: pull a pre-serialized, membership-gated HISTORY batch that
            // exceeded the inline threshold. The one-time token is the cap (the
            // body was authorized at mint time); a bad/spent token is uniformly
            // "nosuch" (invariant 1).
            let token = parts.next().unwrap_or("");
            match ctx.take_backfill_token(token) {
                Some(body) => {
                    let _ = send
                        .write_all(format!("OK {}\n", body.len()).as_bytes())
                        .await;
                    let _ = send.write_all(&body).await;
                    let _ = send.finish();
                }
                None => respond(&mut send, "ERR nosuch").await,
            }
        }
        Some("MIRROR") => {
            let requester = parts.next().unwrap_or("");
            let hash_str = parts.next().unwrap_or("");
            let sig_b64 = parts.next().unwrap_or("");
            // Origin authority (§11.8): serve only when a `[[peers]]`-known
            // network proves its key over `hash‖requester‖origin`. Any failure
            // is the uniform "nosuch" (invariant 1: presence never leaks).
            let authorized = requester
                .parse::<NetworkName>()
                .ok()
                .and_then(|net| peer_keys.get(&net).copied())
                .zip(weft_crypto::signature_from_b64(sig_b64).ok())
                .is_some_and(|(key, sig)| {
                    weft_crypto::verify_mirror_request(
                        &key,
                        hash_str,
                        requester,
                        ctx.info.network.as_str(),
                        &sig,
                    )
                });
            let Some(hash) = BlobHash::parse(hash_str).filter(|_| authorized) else {
                respond(&mut send, "ERR nosuch").await;
                return;
            };
            match ctx.blobs.get(&hash, None).await {
                Ok(Some(data)) => {
                    let mime = ctx
                        .media_refs
                        .blob_meta(hash.as_str())
                        .await
                        .ok()
                        .flatten()
                        .map(|m| m.mime)
                        .unwrap_or_else(|| "application/octet-stream".to_string());
                    let _ = send
                        .write_all(format!("OK {} {}\n", mime, data.len()).as_bytes())
                        .await;
                    let _ = send.write_all(&data).await;
                    let _ = send.finish();
                }
                Ok(None) => respond(&mut send, "ERR nosuch").await,
                Err(e) => {
                    debug!("mirror blob get failed: {e}");
                    respond(&mut send, "ERR nosuch").await;
                }
            }
        }
        _ => respond(&mut send, "ERR verb").await,
    }
}

/// Store a blob pulled from a peer via [`crate::dialer`] mirroring (§11.8): it is
/// content-verified by the caller, so here we honor the moderation gate, persist
/// the bytes, and record metadata (dimensions/thumbnail) exactly like an upload.
/// Returns whether the blob is now stored locally.
pub(crate) async fn store_mirrored(
    ctx: &ServerCtx,
    expected: &str,
    mime: &str,
    bytes: &[u8],
) -> bool {
    if blob_hash(bytes).as_str() != expected || ctx.is_blob_blocked(expected).await {
        return false;
    }
    match ctx.blobs.put(mime, bytes).await {
        Ok(hash) => {
            record_upload(ctx, &hash, mime, bytes).await;
            true
        }
        Err(e) => {
            debug!("mirror store failed: {e}");
            false
        }
    }
}

// ---- HTTP data plane: POST /media (upload) + GET /media/<hash> (fetch) ----

#[derive(serde::Deserialize)]
struct TokenQuery {
    /// Upload grant (POST) or fetch bearer (GET).
    #[serde(default)]
    t: String,
}

/// Mount the media HTTP routes with a raised body limit for uploads.
pub(crate) fn router(ctx: Arc<ServerCtx>) -> Router {
    Router::new()
        .route("/media", post(upload))
        .route("/media/:hash", get(download))
        .route("/backfill", get(backfill))
        .layer(DefaultBodyLimit::max(MAX_REQUEST))
        .with_state(ctx)
}

/// §6/§13 pull a large HISTORY batch (web client). `?t=<token>` is the one-time
/// backfill grant minted when the page exceeded the inline threshold; the body
/// is the newline-delimited `Reply` lines the client folds like an inline
/// `BATCH`. A bad/spent token is uniformly "not found" (invariant 1). A failed
/// fetch is retried by re-issuing the HISTORY (resume = new token), so one-time
/// consumption is safe.
async fn backfill(State(ctx): State<Arc<ServerCtx>>, Query(q): Query<TokenQuery>) -> Response {
    match ctx.take_backfill_token(&q.t) {
        Some(body) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                // The token rides the URL (decision #9) — keep it out of referers.
                (header::REFERRER_POLICY, "no-referrer"),
            ],
            body,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no such target").into_response(),
    }
}

async fn upload(
    State(ctx): State<Arc<ServerCtx>>,
    Query(q): Query<TokenQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Authorize by an OFFER grant (carries mime + size) OR the session bearer
    // (browser convenience: one POST, mime from Content-Type, size ≤ config).
    let (mime, max_bytes) = if let Some(grant) = ctx.take_upload_token(&q.t) {
        (grant.mime, grant.max_bytes)
    } else if ctx.media_bearer_account(&q.t).is_some() {
        let mime = headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        (mime, MEDIA_MAX_BYTES)
    } else {
        return (StatusCode::FORBIDDEN, "invalid upload token").into_response();
    };
    if body.len() as u64 > max_bytes {
        return (StatusCode::PAYLOAD_TOO_LARGE, "blob exceeds size limit").into_response();
    }
    // §13 hash-moderation gate (stub → false until M-media-5).
    if ctx.is_blob_blocked(blob_hash(&body).as_str()).await {
        return (StatusCode::FORBIDDEN, "blocked content").into_response();
    }
    match ctx.blobs.put(&mime, &body).await {
        Ok(hash) => {
            let record = record_upload(&ctx, &hash, &mime, &body).await;
            Json(serde_json::json!({
                "hash": hash.to_string(),
                "media": media_uri(&ctx, &hash),
                "mime": record.mime,
                "bytes": record.bytes,
                "width": record.width,
                "height": record.height,
                // Thumbnail as a fetchable weft-media URI (images only).
                "thumb": record.thumb.map(|h| format!("weft-media://{}/{}", ctx.info.network, h)),
            }))
            .into_response()
        }
        Err(e) => {
            debug!("blob put failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "store error").into_response()
        }
    }
}

/// Parse an HTTP `Range: bytes=start-end` header into an inclusive range.
fn http_range(headers: &HeaderMap) -> Option<(u64, u64)> {
    let raw = headers.get(header::RANGE)?.to_str().ok()?;
    parse_range_spec(raw.strip_prefix("bytes=")?)
}

async fn download(
    State(ctx): State<Arc<ServerCtx>>,
    AxumPath(hash): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    // Media-proxy model (§13): weftd serves a content-addressed blob to anyone
    // presenting its hash — the 256-bit BLAKE3 hash is the capability, only
    // obtainable from a message you can already see. This deliberately drops the
    // per-blob channel-membership gate (`may_fetch`), which otherwise 404s media
    // weftd *holds* whenever its blob→channel refs are absent (e.g. wiped by a
    // memory-backend restart, or a fetch that races ahead of ref recording) —
    // the cause of "all images show as broken links". A stale/absent `?t=`
    // bearer no longer matters. The §13 hash-moderation block still applies, and
    // a missing blob still reads as "not found".
    let gated_absent = || (StatusCode::NOT_FOUND, "no such target").into_response();
    let Some(hash) = BlobHash::parse(&hash) else {
        return gated_absent();
    };
    if ctx.is_blob_blocked(hash.as_str()).await {
        return gated_absent();
    }
    let range = http_range(&headers);
    match ctx.blobs.get(&hash, range).await {
        Ok(Some(data)) => {
            let mime = ctx
                .media_refs
                .blob_meta(hash.as_str())
                .await
                .ok()
                .flatten()
                .map(|m| m.mime)
                .unwrap_or_else(|| "application/octet-stream".to_string());
            let status = if range.is_some() {
                StatusCode::PARTIAL_CONTENT
            } else {
                StatusCode::OK
            };
            (
                status,
                [
                    (header::CONTENT_TYPE, mime),
                    (header::ACCEPT_RANGES, "bytes".to_string()),
                    // The bearer rides the URL (decision #9) — keep it out of referers.
                    (header::REFERRER_POLICY, "no-referrer".to_string()),
                ],
                data,
            )
                .into_response()
        }
        Ok(None) => gated_absent(),
        Err(e) => {
            debug!("blob get failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "store error").into_response()
        }
    }
}

/// Wall-clock unix ms — the upload timestamp / GC grace anchor (§13).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Longest edge of a generated thumbnail (§13).
const THUMB_MAX: u32 = 256;

/// Record a freshly-stored blob's metadata (M-media-1b): probe image dimensions
/// and generate a thumbnail (stored as its own blob, referenced alongside the
/// parent by the channel actor). Returns the stored record.
async fn record_upload(ctx: &ServerCtx, hash: &BlobHash, mime: &str, bytes: &[u8]) -> BlobRecord {
    let (width, height, thumb) = if mime.starts_with("image/") {
        image_meta(ctx, bytes).await
    } else {
        (None, None, None)
    };
    let record = BlobRecord {
        hash: hash.to_string(),
        mime: mime.to_string(),
        bytes: bytes.len() as u64,
        width,
        height,
        thumb,
        created_ms: now_ms(),
    };
    let _ = ctx.media_refs.record_blob(record.clone()).await;
    record
}

/// Probe dimensions + generate a ≤`THUMB_MAX` PNG thumbnail (its own blob). The
/// decode/resize/encode is CPU-bound → `spawn_blocking`; a decode failure just
/// yields no dimensions/thumbnail (host stays blind either way).
async fn image_meta(ctx: &ServerCtx, bytes: &[u8]) -> (Option<u32>, Option<u32>, Option<String>) {
    let owned = bytes.to_vec();
    let probe = tokio::task::spawn_blocking(move || {
        let img = image::load_from_memory(&owned).ok()?;
        let (w, h) = (img.width(), img.height());
        let thumb = img.thumbnail(THUMB_MAX, THUMB_MAX);
        let mut buf = std::io::Cursor::new(Vec::new());
        thumb.write_to(&mut buf, image::ImageFormat::Png).ok()?;
        Some((w, h, thumb.width(), thumb.height(), buf.into_inner()))
    })
    .await
    .ok()
    .flatten();
    let Some((w, h, tw, th, thumb_bytes)) = probe else {
        return (None, None, None);
    };
    // Store the thumbnail as its own content-addressed blob + record it.
    let thumb = match ctx.blobs.put("image/png", &thumb_bytes).await {
        Ok(thumb_hash) => {
            let _ = ctx
                .media_refs
                .record_blob(BlobRecord {
                    hash: thumb_hash.to_string(),
                    mime: "image/png".to_string(),
                    bytes: thumb_bytes.len() as u64,
                    width: Some(tw),
                    height: Some(th),
                    thumb: None,
                    created_ms: now_ms(),
                })
                .await;
            Some(thumb_hash.to_string())
        }
        Err(e) => {
            debug!("thumbnail store failed: {e}");
            None
        }
    };
    (Some(w), Some(h), thumb)
}

/// Content-addressed store contract also holds for the fs backend.
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fs_blob_store_satisfies_contract() {
        let dir = std::env::temp_dir().join(format!("weft-blob-test-{}", std::process::id()));
        let store = FsBlobStore::open(dir.clone()).await.unwrap();
        weft_store::blob_store_contract(&store).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
