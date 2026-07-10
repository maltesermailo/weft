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

- **M-media-0 — data-plane transport spike.** `BlobStore` fs CAS + the QUIC
  uni-stream / WS-binary / HTTP transfer paths, generic over `media|backfill`.
  Upload ONE blob (`OFFER`→token→transfer), BLAKE3-hash + store + dedup, fetch it
  back (bearer token, Range). No `MSG`/attach yet. *Green:* a conformance test
  round-trips a blob over QUIC and over HTTP; the same bytes dedupe to one hash.
- **M-media-1 — posting + fetch semantics (single network).** `attach.N=`/
  `attach-meta=` on `MSG`, `weft-media://` refs + `BlobMeta`, membership-gated
  fetch (token → session → blob⇄ref index), refcount→retention GC in maintenance,
  server-generated thumbnails + probed dimensions, `is_blocked` gate (stub).
  *Green:* two clients exchange an image message; a non-member is denied; deleting
  the message eventually GCs the blob.
- **M-media-2 — GUI client (Tauri/web).** Upload (drag/paste, progress bars),
  render images inline + **ranged `<video>`** + file chips; the `weft-media://` →
  `/media` + session-token resolution; wasm + native. *Green:* two browsers
  post + view images and stream a video; weftd enforces membership on each fetch.
- **M-media-3 — federation mirroring (§11.8).** On ingesting a bridged message
  with attachments, fetch the blobs over the bridge data plane, **BLAKE3-verify**,
  store under receiver retention + receiver blocklist, bounded by the manifest
  `media` policy. *Green:* `ada@net1` posts an image; `bob@net2` (bridged) sees it
  from net2's mirror, verified, with net1 never contacted by bob.
- **M-media-4 — backfill over STREAM.** `STREAM OFFER backfill`; `HISTORY`
  switches to STREAM above ~200 events (§6), ULID-cursor resumable; bulk bridge
  backfill (§11.7) rides it. *Green:* a large scrollback transfers as one resumable
  stream instead of hundreds of lines.
- **M-media-5 — hash moderation enablement.** `MEDIA BLOCK <hash>` (mod cap) +
  the blocklist table; the `is_blocked` stub goes live on upload/fetch/mirror; a
  blocked hash is deleted + dead-on-arrival. *Green:* blocking a hash removes it
  and rejects re-upload + mirror.

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
