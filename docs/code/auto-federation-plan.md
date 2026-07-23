# Auto-federation (transparent bridging) — design plan

Status: **design, for approval** (2026-07-07). Decisions taken: build the M5d
dialer first as the foundation; **open** outbound trigger (any user, any
non-blocked domain), abuse handled reactively. This doc turns that into a
concrete flow + phases. It amends spec §11 (federation) — the wire pieces land
in the spec in the same PR as the code (per CLAUDE.md).

## 1. Goal

A user on their home network `H` accesses a namespace on a foreign network `F`
by naming it — `F/gaming` in the quick-switcher, or clicking a
`weft://F/i/<token>` invite — and it "just works": `H` auto-establishes a bridge
to `F` scoped to that namespace, `F`'s channels mirror in, and the user
participates. No operator ceremony, no manual `BRIDGE PROPOSE/ACCEPT`.

This is the transparent-federation experience. It is a **thin layer on top of
the outbound dialer** — which is the actual work.

## 2. What exists vs what's missing

| Piece | State |
|---|---|
| Inbound bridge sessions (accept, ingest, forward, gating) | ✅ M5b/M5c |
| `FederationConfig` `accept_any` / `auto_accept` (inbound policy) | ✅ |
| `NETBLOCK` (name-keyed defederation, invariant 7) | ✅ |
| Signed manifests, scope-authority-signed | ✅ M5a |
| Invite links carry the network — `weft://<net>/i/<token>` | ✅ (client parses it) |
| **Outbound dialer** (dial a foreign weftd over QUIC) | ❌ **M5d, deferred** |
| **Well-known key fetch** (get `F`'s signing key + endpoint) | ❌ M5d |
| `<network>/<namespace>` cross-network addressing | ❌ new |
| Foreign-side "allow federation" toggle | ❌ new |
| Auto-bridge-on-demand trigger | ❌ new |

The bottom four all sit on the dialer. **No auto-federation is possible until
weftd can dial out** — today `BRIDGE ADD/REMOVE` answer `UNSUPPORTED` and
"forwarding over the bridge session is the dialer's job (M5d)."

## 3. Addressing

- **Namespace reference:** `<network>/<namespace>` — e.g. `test.example/gaming`.
  The left of the first `/` is a DNS network name; the right is a namespace on
  that network. (Local namespaces stay bare: `gaming`, channels `#gaming/general`.)
- **Invite link:** `weft://<network>/i/<token>` — already the format. Redeeming
  one whose `<network>` ≠ home network is the invite-driven entry to the same
  flow.
- **Canonical link (optional):** `weft://<network>/<namespace>` for "open this
  foreign namespace," so links are shareable outside the app.

## 4. The auto-bridge flow

User `U` on home `H` wants namespace `N` on foreign `F` (via `F/N` or a foreign
invite). `H` and `F` are the two weftds.

```
U →(access F/N)→ H
                 ├─ netblocked(F)?            → refuse (BLOCKED)
                 ├─ existing bridge H↔F ⊇ N?  → reuse, join N, done
                 └─ auto-bridge:
                     1. resolve F: GET https://F/.well-known/weft
                        → F's network pubkey + QUIC endpoint      [SSRF guard]
                     2. rate/cap check (even in open mode)
                     3. dial F over VERIFIED QUIC (ALPN weft/1)   [M5d dialer]
                     4. AUTH BRIDGE: H proves its network key to F
                     5. H requests namespace N  ── new: BRIDGE REQUEST
                     6. F evaluates: N public? federation=open? H not netblocked?
                        → if ok, F SIGNS N's manifest and PROPOSEs it (F owns
                          N's scope authority — H cannot sign it)
                        → else NO-SUCH-TARGET (anti-enumeration) / BLOCKED
                     7. H auto-accepts (open policy) → bridge live
                     8. N's channels mirror into H; U joins; U's posts
                        originate at H and forward one hop to F (invariant 2)
```

Key subtlety (**why a request, not a propose, from `H`**): a manifest for `N`
must be signed by `N`'s scope authority, which lives on `F`. `H` cannot propose a
bridge *for `F`'s namespace* — it can only **ask**, and `F` decides and offers.
So auto-federation needs a small new **`BRIDGE REQUEST <ns>`** verb (request the
peer to offer a manifest for one of *its* scopes). `F`'s existing
`accept_any`/auto-propose logic answers it.

## 5. The "federation on" toggle

Two independent switches, one per side:

- **Foreign side (`F`, the namespace being reached) — the consent gate.** The
  namespace owner sets `NS META <name> federation :open` (default `closed`). Only
  an `open` **and `public`** namespace will be auto-offered to a requesting peer.
  This is how "when on" is expressed: the ns owner opts their namespace into
  being reachable. `closed`/`unlisted`/`private` namespaces never auto-bridge.
- **Home side (`H`) — the trigger policy.** Per the decision: **open** — any
  member may trigger an outbound bridge request to any non-blocked domain. This
  is a `[federation]` config (`auto_bridge = open|off`), so an operator can turn
  the whole behavior off for their network.

## 6. Security — "open" is the auth model, not the absence of safety

The chosen policy removes the *per-user authorization gate*. It does **not**
remove these, which are mandatory regardless:

1. **SSRF / private-address block (non-negotiable).** `F` must be a public DNS
   name resolving to a public IP. Refuse to dial loopback, RFC-1918, link-local,
   CGNAT, ULA, or cloud metadata addresses. A user naming `F` must never make the
   server hit internal infrastructure.
2. **Foreign consent is structural.** `H` cannot force `F` to bridge — `F` only
   offers when `N` is `public` + `federation=open` + `H` not netblocked *by F*.
   There is no "make them peer" path.
3. **Rate limiting + backoff.** Per-user and global caps on *new* outbound dial
   attempts; exponential backoff per failing domain. Stops dial-storm DoS.
4. **Concurrent-bridge cap.** A ceiling on live outbound bridges.
5. **NETBLOCK both directions.** `H` won't dial a domain it blocked; `F` won't
   offer to a peer it blocked. The reactive abuse tool the "open" model relies on.
6. **Well-known fetch hardening.** TLS-verified, timeout, small response cap, no
   redirects to private hosts.
7. **Visibility.** Every auto-bridge emits `MANIFEST` to affected members (§11.5)
   and appears in the namespace's Federation tab — never silent.

"Open" means step 2 of §4 doesn't consult a cap or allow-list; every item above
still runs.

## 7. Participation model (cross-network)

- `U@H` joining bridged `N` is a **member on H's mirror** of `N`. Their messages
  originate on `H` (msgid `H/<ulid>`) and forward one hop to `F` (invariant 2 —
  origin preserved, never re-minted). `F`'s members see `U@H`.
- Retention/media/typing negotiate to the **strictest** of the two sides (§5.2,
  existing manifest negotiation).
- **`e2ee` namespaces are never bridged** (invariant 8 — the server can't mirror
  ciphertext it can't represent). A foreign `e2ee` namespace is simply
  unreachable this way. Stated explicitly so it's not a surprise.

## 8. Prior art: Matrix (and why WEFT differs)

Matrix is the reference for transparent, open federation — it validates the
shape here and warns about the cost of "open."

**How Matrix does it.** Users are `@alice:home.server`; the shared unit is a
room. To join a remote room: discover the target server
(`/.well-known/matrix/server` + SRV, fetch its Ed25519 key from
`/_matrix/key/v2/server`) → the `make_join` / `send_join` handshake → the
resident server validates the join against the room's auth rules and returns the
**full current room state + auth chain** → the joining server now holds a
**complete replica of the room's event DAG** and receives future events via
signed federation transactions. Authorization is the room's own
`m.room.join_rules` (`public` / `invite` / `knock` / `restricted`), *not* a
server-to-server accept. Federation is **open by default**; blocking is reactive
(`m.room.server_acl` + each homeserver's allow/deny list).

**What WEFT adopts from it:**
- Trigger on **user access/join** (alias / ID / link), not operator ceremony.
- Authorize by the **target's own visibility rule** — WEFT namespace `public` +
  `federation=open` = Matrix's `join_rules: public`.
- **Well-known discovery** of the peer's signing key + endpoint.
- **Open by default** (the chosen policy); NETBLOCK ≈ server ACLs.

**Where WEFT stays different, deliberately:**
- **One-hop relay, not full replication.** Matrix copies the entire room DAG to
  every participating server and merges via state resolution — resilient but
  heavy (the infamous slow join of large rooms; CPU-costly state-res). WEFT keeps
  events ≤ 1 hop from origin, origin-ordered, no merge. Simpler and cheaper, and
  it fits the IRC-simple / netcat-debuggable ethos — worth preserving.
- **Explicit bilateral bridge, not emergent membership.** In Matrix the
  server relationship is emergent (you federate by having a user in the room —
  there is no bridge object). WEFT keeps an explicit, signed, bilateral bridge
  (propose/accept/sever + manifest) for clean defederation and auditability.
  Auto-federation *auto-establishes* that bridge on the Matrix-style trigger
  rather than discarding it.

**The cautionary tale (why §6's guardrails are non-negotiable).** Open federation
made Matrix ubiquitous, but it also brought room/invite spam, abuse, and
resource exhaustion — forcing later bolt-ons: server ACLs, allow-list-only
deployments, and moderation tooling (mjolnir/Draupnir). Matrix learned the
guardrails retroactively; WEFT should ship §6 *with* P3, not after.

## 9. Phases (each independently shippable)

- **P0 — spec.** Amend §11: `BRIDGE REQUEST`, the `federation` ns flag,
  `<network>/<namespace>` addressing, the open-trigger policy + the §6 guardrails.
  Appendix A decision entry. *(No code; your approval gate.)*
- **P1 — M5d dialer (the foundation).** weft-transport verified outbound client
  (partly exists: `client_endpoint`); weftd well-known **fetch client** +
  `[[peers]]` config; outbound `AUTH BRIDGE`; `BRIDGE ADD/REMOVE` over real QUIC;
  **two-live-weftd conformance** (two servers actually federate). Unblocks all.
  - **P1a ✅ (2026-07-07)** — outbound dialer + AUTH BRIDGE handshake
    (`weftd::dialer::dial_bridge`): HELLO negotiation → `AUTH BRIDGE` → sign
    `nonce‖peer-net` → `AUTH PROOF` → WELCOME, over real QUIC. Two-weftd
    conformance: auth succeeds under `accept_any`, rejected when unpinned.
  - **P1b ✅ (2026-07-07)** — `weft_core::run_bridge_client` runs an outbound
    session over the authenticated link: `begin_outbound_bridge` transmits the
    operator's stored `BRIDGE PROPOSE`, the peer ingests + auto-accepts, and the
    ordinary bridge loop handles the `BRIDGE ACCEPT` → mutually-acked manifest.
  - **P1c ✅ (2026-07-07)** — forwarding + ingestion ride the reused bridge loop;
    `[[peers]]` config + `dialer::spawn_dialers` (one maintained dial per peer,
    reconnect + 5s backoff, shutdown-aware). End-to-end conformance: a message on
    H's `#general` forwards one hop to a member on F (two live weftds).
  - **Deferred to P3:** well-known key fetch (pinned `[[peers]]` keys for now).
- **P2 ✅ (2026-07-08) — foreign-side consent.** `BridgeRequest` verb (proto +
  round-trip test); `NamespaceRecord.federation` flag (mem + PG + migration 0015
  + contract test); `NS META <name> federation :open|closed` (ns-admin gate,
  `open` requires `public`); `on_bridge_request_in` auto-offers a signed manifest
  for a `public`+`federation`-open namespace, else `NO-SUCH-TARGET` (uniform,
  anti-enumeration). Shared `store_bridge_proposal` helper (DRY with operator
  `BRIDGE PROPOSE`). Core tests: the `NS META` public-gate, and the
  offer/anti-enumeration path over an authenticated bridge session.
- **P3 ◑ (2026-07-08) — auto-bridge trigger (home side).** Done: the **SSRF
  guard** (`dialer::is_dialable`, invariant 13 — rejects loopback / RFC-1918 /
  CGNAT / link-local / ULA / metadata / v4-mapped-private, unit-tested); the
  **requester orchestration** `run_bridge_requester` (`BRIDGE REQUEST` +
  auto-accept via a `request_accept` session flag, DRY-shared with the proposer
  via `OutboundStart`); `dialer::auto_bridge` (SSRF + dial + request) over
  `run_peer_requester`; the `[federation] auto_bridge = off|open` knob; and a
  two-weftd conformance test (H requests F's reachable ns → live bridge, no
  operator ceremony). **Deferred:** well-known key fetch (arbitrary-domain
  discovery — needs an HTTPS client; caller supplies key+addr for now); the
  client-facing trigger verb + `network/namespace` parse + the ctx→weftd trigger
  channel; rate-limit/concurrent-cap (§6, gates the trigger); and mirror/join
  surfacing (→ P4).
- **P3 trigger ✅ (2026-07-08).** The `FEDERATE <network>/<namespace>` verb
  (proto + round-trip); `on_federate` (self-network/netblock/cooldown gate) hands
  an `AutoBridgeRequest` to weftd over a `ServerCtx` **port** (installed only when
  `auto_bridge = open` — so policy is structural); `dialer::spawn_auto_bridge_consumer`
  drains it → resolves the peer → `auto_bridge` (SSRF + dial + request). Core
  tests: FEDERATE reaches the sink, and the self/off/throttle gates.
- **P3 well-known fetch ✅ (2026-07-08).** `dialer::fetch_signing_key` — a
  minimal hand-rolled HTTPS GET of `/.well-known/weft` over `tokio-rustls` (ring
  provider + Mozilla roots, no aws-lc), TLS-verified, 10s timeout, 64 KiB cap, no
  redirect-following, and **SSRF-guarded before connecting**. The consumer uses
  it for any network not in `[[peers]]`, so **`FEDERATE <any-public-domain>/<ns>`
  now works with no pre-pinning** — genuinely open, arbitrary-domain federation.
  Test: the fetch refuses a private-resolving host. (A full two-weftd HTTPS fetch
  needs a real trusted cert — exercised on a live deployment, not in-tree, since
  the conformance servers use self-signed certs.)
- **P4 ✅ (2026-07-08) — client UX.** The **`federation :open` toggle** (Federation
  tab, reads the `NS-META` flag, gated on `public`); a **"join a foreign
  namespace"** field in Discover; a live **"Connecting to <net>/<ns>…" banner**
  (spinner + dismiss; auto-opens the namespace when its channels surface, else
  clears after a grace window); **foreign-invite auto-routing** (redeeming a
  `weft://<foreign>/<ns>/i/<id>` link routes into `FEDERATE`); and
  **federation-ready invite links** — every namespace invite now embeds the
  namespace (`weft://<net>/<ns>/i/<id>`), so any link works cross-network even
  when minted locally. Remaining: precise "connected/failed" detection needs the
  mirror-surfacing step (H materializing the bridged namespace's channels for the
  requester — a noted follow-up).

P1 is most of the effort and is valuable on its own (real two-server federation).
P2–P4 are comparatively small once dialing exists.

## 10. Decisions (resolved 2026-07-07)

1. **Addressing** — `network/namespace` in the UI; `weft://…` for shareable
   links (`weft://<net>/<ns>` to open a namespace, existing `weft://<net>/i/<tok>`
   for invites).
2. **Request mechanism** — an explicit **`BRIDGE REQUEST <ns>`** verb (bounded:
   asks the peer to offer a manifest for exactly one of its namespaces), not
   reuse of `accept_any`.
3. **Foreign flag** — **per-namespace** `federation :open|closed` (the ns owner
   opts their namespace in), not a network-wide switch.
4. **Membership** — bridged membership **persists**; auto-rejoin on reconnect
   re-triggers the bridge if it was severed.
5. **Teardown** — **sever** the bridge when the last local member leaves (after
   an idle window); re-access re-establishes it.

Drafted into the reviewable **`docs/auto-federation-spec-amendment.md`** (the P0
deliverable). Once approved it folds into spec §11 + Appendix A alongside P1.

---

*Next:* review the spec amendment, then **P1 (the dialer)** — the deferred
milestone that makes two live weftds federate for real. Auto-bridge (P2–P4) is
quick once P1 lands.
