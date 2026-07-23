# WEFT — Roadmap

The single index of everything shipped, in progress, and planned — unifying the
per-feature plans under [`docs/`](./docs) and the client tiers under
[`client/`](./client). **Detail lives in the linked source doc; this page is the
map + status.** Normative behavior is [`docs/protocol/weft-protocol-spec.md`](docs/protocol/weft-protocol-spec.md);
server milestones + conventions are in [`CLAUDE.md`](./CLAUDE.md).

**Status:** ✅ shipped · ◑ partial · ⏳ in progress · ⬜ planned · 🚫 deferred (not building)

**Sources indexed here**

| Area | Detail doc |
| --- | --- |
| Protocol (normative) | [docs/protocol/weft-protocol-spec.md](docs/protocol/weft-protocol-spec.md) |
| Protocol flows (conceptual) | [docs/protocol/weft-protocol-flows.md](docs/protocol/weft-protocol-flows.md) · [weft-federation-flows.md](docs/protocol/weft-federation-flows.md) |
| Server architecture | [docs/architecture/weftd-server-architecture.md](docs/architecture/weftd-server-architecture.md) |
| Security posture & threat model | [docs/security/security-posture.md](docs/security/security-posture.md) · [threat-model.md](docs/security/threat-model.md) |
| Milestones M0–M7 + conventions | [CLAUDE.md](./CLAUDE.md) |
| Client (Discord-parity) tiers | [client/ROADMAP.md](./client/ROADMAP.md) · [client/PLAN.md](./client/PLAN.md) |
| Media (§13) | [docs/code/media-plan.md](docs/code/media-plan.md) |
| Web client + embedding | [docs/code/web-client-plan.md](docs/code/web-client-plan.md) |
| Web admin panel | [docs/code/web-admin-panel-plan.md](docs/code/web-admin-panel-plan.md) |
| Profiles (§10.3) | [docs/code/profiles-plan.md](docs/code/profiles-plan.md) |
| Auto-federation (§11.10) | [docs/code/auto-federation-plan.md](docs/code/auto-federation-plan.md) · [amendment](docs/code/auto-federation-spec-amendment.md) |
| Identity, caps & federated roles | [docs/code/identity-caps-federated-roles-plan.md](docs/code/identity-caps-federated-roles-plan.md) |
| Voice — WEFT-RT SFU (§16) | [docs/code/voice-plan.md](docs/code/voice-plan.md) |
| Voice — LiveKit pivot | [docs/code/voice-livekit-plan.md](docs/code/voice-livekit-plan.md) |
| E2EE via MLS (§5.2, §14) | [docs/code/e2ee-mls-plan.md](docs/code/e2ee-mls-plan.md) |
| Modular monolith & scaling | [docs/code/modular-monolith-plan.md](docs/code/modular-monolith-plan.md) |
| VPS / deploy testing | [deploy/](./deploy) |

---

## Tier 0 — Foundation (shipped)

The protocol core, persistence, capabilities, federation, and gateways. Full
detail + test counts in [`CLAUDE.md`](./CLAUDE.md) (§ Milestones).

- ✅ **M0** codec — `weft-proto` wire grammar, verbs, events, errors
- ✅ **M1** echo server — QUIC + WS transport, session FSM, channel actors
- ✅ **M2** identity — `weft-crypto` (Ed25519, attestations, argon2), REGISTER/AUTH, well-known
- ✅ **M3a/b** persistence — `weft-store` traits + memory + **PostgreSQL**, retention/compaction, HISTORY/BATCH, MARK
- ✅ **M4a/b/c** capabilities, namespaces, moderation — scoped tokens, GRANT/REVOKE, CHANNEL/NS CRUD, invites, recovery ladder, REPORT + holds
- ✅ **M5a–d** federation — signed manifest peering, bridge sessions, ingestion/forwarding, NETBLOCK, backfill, outbound QUIC dialer + two-live-weftd conformance
- ✅ **M6** WEFT-IRC gateway — RFC 2812 front-end as a `ControlStream` projection
- ✅ **M7** moderation — mute/ban/kick, restricted posting, deny-list + `send`-cap surfaces

### Shipped feature plans

- ✅ **Media** (§13) — content-addressed BLAKE3 blobs, STREAM data plane, hash moderation, mirroring → [media-plan.md](docs/code/media-plan.md)
- ✅ **Web client + weftd embedding** — WASM client-core, same-origin `/ws`, embedded SPA → [web-client-plan.md](docs/code/web-client-plan.md)
- ✅ **Web admin panel** — `weft-admin` crate, embedded at `/admin` (operator API + SPA) → [web-admin-panel-plan.md](docs/code/web-admin-panel-plan.md)
- ✅ **Profiles** (§10.3) — signed display name + avatar, mirrored over the bridge → [profiles-plan.md](docs/code/profiles-plan.md)
- ✅ **Account verification** (§10.5) — email/birthday claims, `Mailer` port, lettre SMTP
- ✅ **Auto-federation** (§11.10) — `FEDERATE`, SSRF-guarded dialer, well-known key fetch, per-ns consent → [auto-federation-plan.md](docs/code/auto-federation-plan.md)
- ✅ **Operators in Postgres + `weftd admin` CLI** (§11.3) — operator flag on accounts, CLI bootstrap, config `[operators]` deprecated *(this session)*
- ◑ **Identity, caps & federated roles** — caps keyed by account ULID + §11.11 federated-role recognition shipped; full federation-session authority is the open remainder → [identity-caps-federated-roles-plan.md](docs/code/identity-caps-federated-roles-plan.md)

---

## Tier 1 — Client first-hour gaps

Discord-parity essentials; full detail in [client/ROADMAP.md](./client/ROADMAP.md#tier-1--first-hour-gaps-users-hit-these-immediately).

- ✅ **Fenced code blocks** — ` ``` `/`~~~` with a language label (syntax highlighting still ⬜)
- ✅ **Paste & drag-drop upload** · ✅ **Spoilers** (`||text||`) · ✅ **Audio player + image lightbox**
- ✅ **NEW-messages divider + day separators** · ✅ **Unread counts** (server-authoritative, see Tier 2)
- ✅ **Per-namespace notification prefs** (modal: All / @mentions / Nothing)
- ⬜ **Link previews / embeds** — needs a **server-side unfurl proxy** (`GET /unfurl?url=`, SSRF-guarded, cached); client-side fetch leaks IPs — *the only Tier 1 item left, needs sign-off*

## Tier 2 — Client Discord-parity features

Detail in [client/ROADMAP.md](./client/ROADMAP.md#tier-2--the-features-people-name-when-comparing-to-discord).

- ✅ **Server-controlled unread counts** — `UNREAD` verb → `UNREAD-COUNTS` (mem+PG), pushed on login + cross-device MARK *(this session)*
- ✅ **Message search** — `SEARCH <#chan> :<query>` → BATCH; `EventStore::search` (substring; `tsvector` ⬜) *(this session)*
- ✅ **Threads** — `HISTORY thread=<root>` filter + `EventStore::thread_roots`, thread side panel, "N replies", replies hidden from timeline *(this session)*
- ✅ **Custom / per-namespace emoji** — `EMOJI ADD/REMOVE/LIST` + `EmojiStore` (mem+PG), unified picker, `:name:` render, settings upload tab *(this session; federation ⬜)*
- ◑ **Voice depth** — join/leave/mute/deafen/speaking today; screen share, video, per-user volume, device selection, push-to-talk ride LiveKit → see Tier 4
- 🚫 **Group DMs** — needs multi-party DM protocol design (spec §18) — flag, don't build
- ⬜ **Custom status text · per-server nicknames · bios** — small profile store/proto additions

## Tier 3 — Client polish & platform

Detail in [client/ROADMAP.md](./client/ROADMAP.md#tier-3--polish--platform).

- ⬜ Collapsible categories (per-user) · ⬜ slash-command autocomplete · ⬜ jump-to-date
- ◑ Accessibility (focus-trap, reduced-motion, keyboard nav) · ⬜ settings depth (keybinds, audio devices, font scale, i18n)
- ⬜ Mobile / responsive layout · ⬜ credential hardening (OS keychain, off localStorage)
- ◑ Federation surfacing — trust marks + bridged badges exist; ⬜ foreign-invite auto-routing, live connecting/failed state

---

## Tier 4 — Bigger bets (planned)

Larger efforts, each with its own plan doc. None started unless noted.

- ◑ **Voice — LiveKit M-lk-1/2/3** — media plane pivoted weft-rt SFU → LiveKit; **M-lk-0 shipped**, M-lk-1 (rooms/tokens), M-lk-2 (server mute/kick), M-lk-3 (screenshare/video) remain → [voice-livekit-plan.md](docs/code/voice-livekit-plan.md)
- ⬜ **E2EE via OpenMLS** (§5.2, §14) — `e2ee` retention mode, server as blind DS, always-on engine → [e2ee-mls-plan.md](docs/code/e2ee-mls-plan.md)
- ⬜ **Modular monolith & scaling** — one binary, opt-in `roles = [...]` to split hot paths; no wire change → [modular-monolith-plan.md](docs/code/modular-monolith-plan.md)
- ⬜ **Full federated-role authority** — bridge-tunneled federation session under homeserver authority (trust F) → [identity-caps-federated-roles-plan.md](docs/code/identity-caps-federated-roles-plan.md)
- ⬜ **Emoji federation** — origin-namespace `:name:` resolution + cross-bridge `EMOJI LIST` + §11.8 media mirroring (home-network-only today)
- ⬜ **Link-preview unfurl proxy** — the Tier 1 blocker; server HTTP surface decision
- ⬜ **M6+ media polish** — threads-filter UI refinements, WEFT-RT voice-video, retention-hold surfacing

## Componentization (client, in progress)

- ⏳ **Client monolith → fine-grained components** — `+page.svelte` orchestrator split into components via Svelte context (`AppCtx`); ongoing, tracked in code + memory.

---

## Deliberately not building

From [`CLAUDE.md`](./CLAUDE.md) (§ *Deliberately deferred — do not add*). Flag if a
task appears to need one; don't add the dependency.

- 🚫 SQLite backend (Postgres is the chosen engine — decision reversed 2026-07)
- 🚫 Biscuit tokens · 🚫 SRV discovery · 🚫 cross-network DMs
- 🚫 Per-message rate-limiter beyond `THROTTLED` plumbing · 🚫 shared blocklists
- 🚫 Group DMs and other open questions — decisions live in **spec §18** and belong to Jannik, not a coding session

---

*Open questions and any protocol-shape decisions live in spec [§18](docs/protocol/weft-protocol-spec.md). When a plan doc and this index disagree, the plan doc (and ultimately the spec) win — update both in the same PR.*
