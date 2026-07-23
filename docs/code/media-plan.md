# Media — implementation plan

Status: **design, for approval** (2026-07-10). Realizes native, content-addressed
media (spec §13) and its federated mirroring (§11.8), and in doing so builds the
thing WEFT has deliberately not built yet: a **binary data plane** (everything so
far is control-plane text). Prioritized **ahead of E2EE** (see
`docs/e2ee-mls-plan.md`, parked).

## North star (fixed by the spec — do not relitigate)

- Media is **content-addressed by BLAKE3** — `weft-media://<origin>/<b3-hash>` +
  `{mime, bytes, w, h, duration?}`; **dedup by construction** (§13).
- Upload rides `STREAM OFFER media <mime> <bytes>` → `STREAM ACCEPT <token>` →
  data-plane transfer; posting via `attach.N=` (≤10) + `attach-meta=` on `MSG`.
- **Fetching is home-network only** — clients only ever talk to their home
  network; hotlinking leaks reader IPs and breaks on origin outage (§13, §11.8).
- **Hash-level moderation:** a blocked BLAKE3 hash is dead on arrival — upload,
  fetch, and mirror all reject it; re-uploads can't evade (content = identity).
- **Mirroring (§11.8):** referenced blobs are fetched over the bridge data plane,
  **BLAKE3-verified** (substitution detectable), bounded by the manifest `media`
  policy (`mirror | mirror-max:<B> | none`), and obey **receiver** retention +
  **receiver** hash blocklist.
- **E2EE forward-compat:** for e2ee channels the client encrypts pre-upload, no
  server thumbnails, host-blindness extends to attachments. Not built now, but the
  blob/fetch design must not preclude it.

## Decisions (locked 2026-07-10)

| # | Area | Decision |
|---|------|----------|
| 1 | First milestone | **Data-plane spike first** — one blob: upload → BLAKE3 → store → fetch. No posting/attach semantics yet; proves the new transport + content addressing. |
| 2 | Transport | **Both QUIC uni-streams + HTTP.** QUIC data-plane for native clients (spec-literal, efficient); an HTTP endpoint (Range) for browsers + ranged video. One blob store behind both. |
| 3 | Storage | **Filesystem CAS behind a `BlobStore` trait**, metadata rows in the existing store (mem/PG). Dedup by hashed path; S3 impl can slot in later. |
| 4 | Thumbnails | **Server-generated** derived blobs (decode + resize via the `image` crate; video frame later). Dimensions/duration probed server-side. |
| 5 | Upload auth | **`STREAM OFFER`/`ACCEPT` token**, checks `attach` cap + size config before any byte moves; the token authorizes the transfer over **either** transport. |
| 6 | Fetch access | **Membership/cap-gated** — a fetch is allowed only if the requester is a member of (or holds `view` on) a channel that references the blob. |
| 7 | Lifecycle | **Refcount to message retention** — a blob lives while ≥1 non-purged message references it; GC'd after a grace window at 0 refs. A blocklist hit deletes immediately. |
| 8 | Moderation | **Design in, enable later** — every blob path calls `is_blocked(hash)` (stub → false); the blocklist verb/table + enforcement land in a later milestone (but mirroring already honors it). |
| 9 | Fetch mechanism | **Per-session bearer token** — the client's session holds a token; media HTTP requests carry it (`?t=`/header), server maps token → session → membership per fetch. *(Caveat accepted: token rides the URL; see Risks.)* |
| 10 | Federation mirroring | **In scope from the start** — designed into the store/fetch from M0; cross-network blob fetch + BLAKE3-verify enabled as an early milestone (not deferred). |
| 11 | Client rendering | **Images + video + files** — inline images, in-player ranged video, file chips, drag/paste upload, progress. |
| 12 | Backfill mode | **Both media + backfill** — the data plane is generic over `STREAM OFFER media|backfill`; `HISTORY` switches to STREAM above ~200 events (§6) and bulk bridge backfill (§11.7) rides the same path. |

"From the start" (#10, #12) means **designed-in from the first line and committed
milestones** — not enabled before the single-network transfer works. Sequencing is
in the milestones below.

## Target architecture

```
weft-proto  (L0)   STREAM OFFER/ACCEPT verbs (media|backfill), attach.N=/attach-meta=
                   MSG tags, weft-media:// URI + BlobMeta{mime,bytes,w,h,dur},
                   ErrCode::TooLarge. Round-trip tests first. NO transport code.

weft-transport (L2)  THE NEW DATA PLANE:
                   · QUIC: accept/open uni-streams beside control stream 0,
                     framed {token header || bytes}; native up + down.
                   · WS fallback: binary frames carrying the same framing.
                   Generic over payload (media blob | backfill stream). weft-core
                   drives it via a trait, same as ControlStream.

weft-store  (L1)   BlobStore trait (fs CAS default): put(bytes)->b3hash, get(hash,
                   range), delete; BlobMeta rows; blob⇄message refcount index;
                   media-blocklist table (enforced later). mem + PG contract.

weft-core   (L2)   STREAM OFFER cap/size check -> ACCEPT token; ingest -> BLAKE3
                   hash + store + dedup; membership-gated fetch (token->session->
                   ref-index); server thumbnails; refcount GC in maintenance; the
                   §11.8 mirror-on-ingest path. is_blocked(hash) gate everywhere.

weftd       (L3)   HTTP media endpoint on the axum surface: GET /media/<hash>
                   (Range, bearer token) + the QUIC uni-stream acceptor wiring;
                   [media] config (blob dir, size limits, quotas).

clients            upload (OFFER->transfer, drag/paste, progress); render images
                   inline, ranged <video>, file chips; resolve weft-media:// ->
                   /media URL + session token. weft-client-core + -wasm + tauri.
```

## Wire additions (proto-first — codec + round-trip tests before consumers)

- `STREAM OFFER media <mime> <bytes>` / `STREAM OFFER backfill <target> <cursor>` →
  `STREAM ACCEPT <token>` (one-time, binds session + declared size/mime/mode).
- Data-plane transfer: the token-prefixed uni-stream (QUIC) or binary WS frames
  carry the bytes up; fetch pulls them down (or via HTTP GET).
- `MSG … attach.1=<weft-media://…> attach.2=… attach-meta=<b64 json>` (≤10);
  empty body legal iff attachments (already in §6).
- `weft-media://<origin>/<b3-hash>` URI + `BlobMeta` struct.
- A control-plane **fetch-URL/token request** so the client can turn a
  `weft-media://` ref into a `/media/<hash>?t=<session-token>` URL after the
  server confirms membership.
- `ErrCode::TooLarge` (size/cap), `NO-SUCH-TARGET` for a gated/absent blob
  (invariant 1 — a blocked or unauthorized blob is indistinguishable from absent).

## Security invariants to add AS TESTS

- **Content addressing:** stored hash == BLAKE3(bytes); a substituted byte fails
  verification (esp. on mirror ingest).
- **Membership-gated fetch:** a non-member fetching a valid hash gets
  `NO-SUCH-TARGET`, same code + timing as a nonexistent hash (invariant 1).
- **Hash moderation hook:** `is_blocked` is consulted on upload, fetch, AND mirror
  (stubbed now, but the call sites are tested to exist).
- **Refcount GC:** a blob with 0 live references is collected after grace; a blob
  still referenced by a `permanent` channel is never collected.
- **Home-network-only:** clients never dial a foreign media origin; cross-network
  blobs are served only from the local mirror.
- **Mirror bounds:** a `media=none` manifest mirrors nothing; `mirror-max:<B>`
  refuses over-size blobs — never silently.

## Milestones (each independently shippable)

- **M-media-0 ✅ (2026-07-10) — data-plane transport spike.** Proto: `STREAM
  OFFER <media|backfill> <mime> <bytes>` (`StreamMode` via `wire_enum!`) +
  `STREAM ACCEPT`/`STREAM STORED` events, round-trip tested. `weft-store`:
  `BlobStore` trait + `blob_hash` (BLAKE3) + `MemBlobStore` + a shared
  `blob_store_contract`. `weft-core`: `ServerCtx.blobs` + a media-token registry
  (`media.rs` — one-time upload grants from `STREAM OFFER`, fetch bearers) + the
  `on_stream_offer` handler (size-checks, mints, replies `ACCEPT`). `weftd`:
  `FsBlobStore` (sharded fs CAS, temp-then-rename, meta sidecar), the **QUIC
  data plane** (extra bidi streams accepted after the control stream, framed
  `PUT <token>` / `GET <bearer> <hash> [range]`), the **HTTP** `/media` (POST
  upload + GET fetch with `Range`, `no-referrer`), and `[media] dir` config.
  *Green:* conformance round-trips a blob over **QUIC and HTTP**, identical bytes
  dedupe to one hash, ranged fetch + one-time-token + gated-fetch checks hold;
  full workspace green (24 conformance, 100 core, 21 store).
  **Scoping calls (deliberate, deferred):** (1) **WS-binary** transfer deferred —
  QUIC + HTTP satisfy the round-trip; it mirrors the QUIC framing when a WS-only
  native client needs it. (2) **Membership-gating** is stubbed — a valid bearer
  fetches any blob (no channels reference blobs yet); per-blob gating is M-media-1.
  (3) Blob **meta** is a CAS sidecar (`mime\nbytes`); dimensions/refcount move to
  the SQL store in M-media-1. (4) `STREAM STORED` (control event) is codec-only —
  M0 returns the URI on the transfer response (bidi/HTTP); the fire-and-forget
  control-event delivery lands with the M-media-1 posting flow. (5) Bearer
  issuance is an internal `ctx` API (tests mint via `ctx`); client-facing issuance
  is M-media-2.
- **M-media-1 ◑ (2026-07-10) — posting + fetch semantics (single network).**
  Semantic core shipped: `on_msg` now **accepts + validates** `attach.N=`
  (well-formed, same-network `weft-media://` refs; foreign → `POLICY`, empty body
  legal with attachments); a new **`MediaStore`** (reference index + orphan
  tracking; mem + PG migration 0018 + shared contract) records blob⇄message refs
  at the **channel actor** (the single-writer msgid mint point) and drops them on
  DELETE; **membership-gated fetch** (`ServerCtx::may_fetch` — bearer→account, a
  scope referencing the blob must have the account as member/DM-participant; a
  gated blob is uniformly "not found", invariant 1) replaces M0's bearer-only
  stub on both QUIC + HTTP; **refcount→retention GC** (`gc_orphan_blobs` +
  `drop_refs_before` in the maintenance pass, 1 h grace so uploaded-not-yet-posted
  blobs survive; `BlobStore::delete` added, fs + mem); and the **`is_blocked`
  seam** (`ServerCtx::is_blob_blocked` stub → false) is now called on every
  upload. *Green:* two conformance tests (QUIC + HTTP) — upload → post attachment
  → member fetch (incl. range) → **non-member denied** → **DELETE → GC → gone**;
  full workspace green, clippy clean.
- **M-media-1b ✅ (2026-07-10) — thumbnails, dimensions, attach cap.** Blob
  metadata moved into the store: `MediaStore` now records a full **`BlobRecord`**
  (mime, size, width, height, thumbnail hash, created) with a `blob_meta` query
  (mem + PG, migration 0018 → a `weft_blobs` table). On image upload weftd
  **probes dimensions** and **generates a ≤256px PNG thumbnail** (`image` crate,
  off-executor via `spawn_blocking`) stored as its own blob; the HTTP upload
  response returns `{width, height, thumb-uri, …}`. The **thumbnail is a
  first-class blob auto-referenced alongside its parent** by the channel actor —
  so it inherits the parent's membership gating *and* refcount lifetime (deleting
  the message orphans both; one GC pass collects both), no special cascade. And
  the **`attach` cap gate**: attachments to a **restricted** channel require
  `attach` (open channels stay free, mirroring the posting gate) via
  `Session::can_attach`. *Green:* two new conformance tests — a real PNG's
  dimensions + thumbnail (member-fetchable, non-member-gated, GC'd with parent),
  and the restricted-channel attach-cap denial (send-but-no-attach → CAP-REQUIRED).
  Full workspace green, clippy clean.
  **Still deferred:** **DM attachments** (channel scope only); the CAS mime
  sidecar is now vestigial (blob_meta is authoritative) but left in place; video
  duration probing.
- **M-media-2 ✅ (2026-07-10, web) — GUI client.** Wire: a new **`MEDIA TOKEN`
  event** delivers a per-session fetch **bearer** at auth (weft-proto + weft-core
  `welcome_authed`); the HTTP upload endpoint now also accepts that bearer (mime
  from `Content-Type`), so the **browser uploads in one authed POST** — no OFFER
  handshake. `weft-client-core`: `ClientEvent::Message` carries `attachments`,
  `build_msg` takes them, and a `MediaToken` event flows to JS. Frontend
  (`weft.ts`): stores the bearer, `upload(file)` (single `fetch` POST →
  `{media,thumb,width,height}`), and `mediaUrl(ref)` resolving `weft-media://…` →
  `/media/<hash>?t=<bearer>`. UI: an **attach button** + a pending-attachment tray
  in the Composer (image previews via the thumbnail), and a new **`Attachment`**
  component that renders each ref by **probing its `Content-Type`** (1-byte ranged
  GET) → inline **image**, ranged **`<video controls>`**, or a **file chip**.
  *Green:* conformance covers the bearer delivery + bearer-authorized upload +
  post + gated fetch round-trip (27 conformance tests, full workspace, clippy,
  svelte-check, and the embedded web build all pass). The browser `<video>` does
  ranged fetches natively against weftd's `Range` support.
  **Deferred:** the **desktop (Tauri)** media path (needs the QUIC data-plane
  upload + a non-`window.location` origin for `mediaUrl` — the web slice is the
  green); drag-and-drop / paste-to-upload + progress bars; and thumbnails at the
  *recipient* render (needs `attach-meta` in the codec so recipients get dims +
  thumb without a Content-Type probe).
- **M-media-3 — federation mirroring (§11.8). ✅** On ingesting a bridged message
  with a foreign `weft-media://` attachment, the receiver records the reference
  locally (so its members are gated + can fetch) and pulls the blob back over the
  live bridge connection to the origin, **BLAKE3-verified**, then stores it under
  the receiver's own retention + blocklist gate. **Design:** a self-authenticating
  signed `MIRROR <requester-net> <hash> <sig>` on the data plane (`sign_mirror_request`
  over `hash‖requester‖origin`, weft-crypto `mirror.rs`) — the origin serves iff a
  `[[peers]]`-known network proves its key, so no origin↔member correlation is
  needed. **Wiring:** weft-core `MirrorRequest` port (`ServerCtx::set_mirror_sink`)
  eagerly emitted from `on_ingest`; weftd `PeerLinks` registry of live outbound
  bridge connections; `spawn_mirror_consumer` drains the port and pulls; the origin
  side is a `MIRROR` verb in `handle_data_stream`. Failures are the uniform
  `ERR nosuch` (invariant 1). *Green:* two live weftds — F posts an image on a
  bridged `#general`; H ingests the message with F's URI intact, mirrors the blob,
  and a member on H fetches it **from H**, never touching F
  (`federated_media_mirrors_over_the_bridge`). *Deferred:* the manifest `media`-mode
  gate (mirror always-on for now, matching M5c's media-negotiation stub) and the
  `mirror-max` bandwidth bound (§18 #5).
- **M-media-4 — backfill over STREAM. ✅** A served `HISTORY` page over
  `HISTORY_STREAM_THRESHOLD` (200 events) is serialized once, held under a
  one-time token, and answered `STREAM ACCEPT <token>` instead of an inline
  `BATCH` (shared `emit_batch` upgrade → both direct HISTORY and §11.7 bridge
  backfill get it). The requester pulls the batch off the **generic data plane**:
  a `BACKFILL <token>` QUIC data-stream verb + an HTTP `GET /backfill?t=<token>`
  (web client), body = newline-delimited `Reply` lines folded like an inline
  batch. **Bridge flow (full scope), lazy:** federated scrollback is pulled
  **only on client demand** — never eagerly on bridge-up. When a local client's
  `HISTORY` for a forwardable channel runs short (out of local scrollback),
  `on_history` signals a ctx port (`request_channel_backfill`) that every
  outbound bridge session drains (`on_backfill_demand`); the responsible bridge
  (forwardability-gated) sends a bulk `HISTORY` for that `(channel, before)`
  window (deduped); the peer streams it if large; weftd's
  `spawn_backfill_consumer` opens a `BACKFILL` stream over the bridge and feeds
  each line through a new ctx-level `ingest_bridged` (origin-authority +
  manifest-gated, invariants 2/3) — symmetric requests never dup a side's own
  history; pre-bridge scrollback needs `history=full` (so §11.10 auto-federation
  offers full). Client (web): a `backfill` event → `fetch('/backfill')`
  → `feed_line` replays each line through the FSM. *Green:* core unit
  (large page → `STREAM ACCEPT` → token resolves to the full parseable batch,
  one-time), conformance (large scrollback over **QUIC + HTTP**; two-live-weftd
  **federated backfill streams over the bridge** — asserts H holds *none* of F's
  scrollback until a client asks, then a client HISTORY lazily pulls F's 201-msg
  scrollback into H's store), full workspace + clippy `-D warnings` +
  svelte-check + wasm build green. *Deferred:* desktop (Tauri) backfill pull
  (web is the green; desktop paging stays under the threshold); replica
  reaction/edit fidelity (compacted form is lossy, as for any bridged batch).
- **M-media-5 — hash moderation enablement. ✅** A new `media-block` capability
  (`*`-scope — content is network-global) gates `MEDIA BLOCK <hash> [:reason]` /
  `MEDIA UNBLOCK <hash>` / `MEDIA BLOCKS`, each → a `MEDIA-BLOCKED` event. A
  `MediaBlocklistStore` (mem + PG, migration 0020, shared contract) holds the
  hashes; `MEDIA BLOCK` records the hash **and** deletes the blob's bytes + its
  derived thumbnail + forgets the blob records (`ServerCtx::block_media_hash`).
  The `is_blocked` stub is now live (`ServerCtx::is_blob_blocked` → the store),
  checked on every **upload** (QUIC `PUT` + HTTP `POST`), **fetch** (QUIC `GET` +
  HTTP `GET`, uniform not-found), and **mirror** (`store_mirrored`) path — so a
  blocked hash is dead on arrival and re-uploads of identical bytes can't evade
  it (content = identity). *Green:* core cap-gate/flip/unblock unit test; store
  blocklist contract (mem+PG); conformance `media_block_deletes_and_rejects_reupload`
  (upload → post → member fetch OK → `MEDIA BLOCK` → fetch 404 + re-upload 403);
  full workspace + clippy `-D warnings` green. *Deferred:* a client operator UI
  for the verb (a `*`-scope operator action like `NETBLOCK`); cross-network shared
  blocklists (spec §18 #4).

## Hard parts / risks (call out early)

- **The data plane is brand new.** QUIC uni-streams beside stream 0, a binary WS
  framing, AND an HTTP endpoint — three transfer surfaces to build and keep
  consistent. This is the bulk of M-media-0 and the highest-risk piece.
- **Bearer-token-in-URL (#9).** Accepted, but it leaks via `Referer`, server logs,
  and browser history, and is coarse (one token = every blob the session sees).
  Mitigations to build in: short token TTL + rotation, `Referrer-Policy: no-referrer`
  on media responses, and never logging the query string. *(Signed per-blob URLs
  remain a clean future upgrade if this bites.)*
- **Server thumbnails (#4).** The `image` crate + format coverage + CPU/DoS on
  decode (bound dimensions/complexity); video frame extraction is a later add.
- **Refcount correctness (#7).** Edits/deletes/purge/compaction all change
  references; the count must stay right across every path or blobs leak or vanish.
- **Mirror bandwidth/quota (#10).** `history:full` + `media:mirror` bridges can
  pull large volumes; needs the `mirror-max` bound + backfill quotas (spec §18 #5).

## Spec amendments (same-PR, per CLAUDE.md)

§13 gains the concrete data plane (**HTTP is a first-class reference transfer
path**, not only uni-streams), the membership-gated **bearer-token** fetch flow,
and the `STREAM OFFER … / ACCEPT <token>` + `attach-meta=` details. §11.8 gets the
concrete mirror-on-ingest + BLAKE3-verify + receiver-policy rules. Appendix A gets
a decision-history entry. Spec wins over this plan — so these land as amendments.
