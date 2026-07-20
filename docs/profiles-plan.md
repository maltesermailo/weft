# Profiles (display name + avatar) — implementation plan

Status: **design, in progress** (2026-07-20). Realizes §10.3 *Display identity*:
a **signed profile blob (nick, avatar)** that travels with the user, verified on
remote networks. Today all avatars are initials; this adds real profile pictures
+ display names, signed by the home network so they federate.

## North star (fixed by the spec — §10.3)

- A profile = **display name (nick) + avatar**, per account.
- **Signed by the home network key** (like manifests/attestations, §11.3) so a
  remote can verify a federated user's profile offline against the home network's
  well-known key. The signature covers the avatar's **BLAKE3 hash**, so a mirrored
  avatar blob can't be substituted.
- The avatar image is an ordinary **content-addressed media blob** (§13):
  uploaded once, referenced by hash, **mirrored over the bridge** (§11.8) like
  message attachments, BLAKE3-verified on receipt.
- Remotes **MAY override display, MUST show canonical `user@network`** (the
  handle is never hidden; a display name is additive).

## Decisions (locked 2026-07-20)

| # | Area | Decision |
|---|------|----------|
| 1 | Scope | **Avatar + display name** — the full §10.3 profile. |
| 2 | Federation | **Signed + portable now** — home-network-signed `SignedProfile`; the blob + avatar cross the bridge and are verified/mirrored remotely. |
| 3 | Avatar visibility | **Any authed session may fetch an avatar blob** — a relaxed media gate (avatars are semi-public, low-sensitivity). |
| 4 | Signer | **The home network key** signs the profile (works for password *and* key accounts; remotes already trust it via well-known/peering). |
| 5 | Distribution | Profiles ride the **directory** (like presence): a `PROFILE` event to co-members on change, and the roster/MEMBER path carries `display=`/`avatar=` so a joiner learns everyone's profile. |

## Target architecture

```
weft-crypto  (L0)  SignedProfile: {account, display, avatar-b3hash, updated}
                   network-key-signed CBOR (domain tag weft-profile/1),
                   sign/verify/signed_by/to_b64/from_b64. Models manifest.rs.

weft-store   (L1)  ProfileStore: get/set a per-account ProfileRecord
                   {display, avatar_hash, signed_blob, updated}. mem + PG,
                   migration. Avatar blob lives in the existing BlobStore.

weft-proto   (L0)  PROFILE SET [display=] [avatar=]  (own profile)
                   PROFILE <account> [display=] [avatar=]  (event)
                   PROFILES <account,...>  (query) → PROFILE per account
                   `display=`/`avatar=` tags on MEMBER + the roster.

weft-core    (L2)  on_profile_set: validate own account → sign (network key) →
                   store → broadcast PROFILE to co-members (directory) + ref the
                   avatar blob (avatar-fetchable). Deliver profiles on JOIN /
                   MEMBERS. Federation: attach the SignedProfile to a bridged
                   member + mirror the avatar (§11.8); verify sig + BLAKE3.

weftd        (L3)  Avatar upload reuses the media HTTP/QUIC path; the fetch gate
                   grows an "avatar blob → any authed session" allowance (#3).

client             Upload avatar (reuse upload()), PROFILE SET; render avatars
                   (resolve weft-media URI → mediaUrl) + display names wherever
                   initials show today (member list, messages, DMs, voice, cards).
```

## Milestones (each independently shippable)

- **M-prof-0 ✅ (2026-07-20) — crypto.** `SignedProfile` (network-key-signed,
  domain-tag `weft-profile/1`, avatar-hash-bound; sign/verify/signed_by/to_b64/
  from_b64). 5 unit tests, modeled on `manifest.rs`.
- **M-prof-1 ✅ (2026-07-20) — store.** `ProfileStore` (get/set/batch/`avatar_exists`)
  + `ProfileRecord`, migration 0022, mem + PG, shared contract test.
- **M-prof-2 ✅ (2026-07-20) — proto.** `PROFILE SET` (partial update via
  present-vs-absent `@display=`/`@avatar=` tags), `PROFILE <account>` event,
  `PROFILES <account>...` query. Round-trip tested. *(MEMBER tags deferred —
  distribution rides `PROFILE` events instead, see M-prof-3.)*
- **M-prof-3 ✅ (2026-07-20) — core (local network).** `on_profile_set`
  (partial-update → store → labeled ack + `announce_as` broadcast to co-members),
  `on_profiles_query`, the **relaxed avatar fetch gate** (`may_fetch` allows any
  authed session for an avatar blob) and **GC protection** (orphan GC skips avatar
  hashes — avatars persist though unreferenced by messages). 3 networkless core
  tests. *Signing is home-network-only-ready but only applied at federation
  (M-prof-5); local clients trust the server, so local PROFILE events are plain.*
- **M-prof-4 ✅ (2026-07-20) — client (web).** weft-client-core `ClientEvent::Profile`
  + `build_profile_set`/`build_profiles_query` (native + wasm dispatch + Tauri
  commands). weft.ts `profileSet`/`profilesQuery` wrappers + `avatarUrl`/`mediaHash`
  helpers. Container: a `profiles` store fed by `profile` events + on-demand
  `profilesQuery` (own on connect, co-members on join), and `avatarUrl`/
  `displayName` AppCtx helpers. A reusable **`Avatar.svelte`** (image-or-initials)
  swapped into MemberList, MessageItem, VoiceBar, DmList, PinsModal, UserFooter;
  display names shown alongside (handle preserved). A **Profile editor** in User
  Settings — upload an avatar (reuses `upload()`) + set a display name → `PROFILE
  SET`. *Green:* svelte-check 0/0, wasm + full web build, workspace build, clippy
  clean. *(Federated senders keep initials until M-prof-5; desktop avatar upload
  waits on the desktop media path.)*
- **M-prof-5 ✅ (2026-07-20) — federation.** A local user's `PROFILE` forwards
  over the bridge (`on_bridge_event`) as a **home-network-signed** line
  (`sig=<SignedProfile>`); `on_bridge_line` routes `PROFILE` to ingestion;
  `ingest_profile` verifies the signature against the peer's key + that it covers
  exactly the account/display/avatar, stores it keyed by `user@network`, and
  **mirrors the avatar** over the existing §11.8 path (the mirror consumer ignores
  the channel). `Event::Profile` now carries a **`UserRef`** (qualified handle) so
  federated profiles are representable; `on_profiles_query` serves them; the client
  keys federated profiles by handle and renders foreign senders' avatars/names.
  *Green:* a two-live-weftd conformance — bob on F sets a name + avatar, the signed
  profile crosses the bridge, H verifies + stores + mirrors, and ada on H queries
  it + fetches the avatar bytes **from H**. clippy clean; svelte-check + web build
  + full conformance (35) green.

**Profiles are complete (M-prof-0 → M-prof-5):** signed, federated display names
+ avatars, rendered everywhere, home-network *and* across bridges.

## Spec amendment (same-PR, per CLAUDE.md)

§10.3 gains the concrete `SignedProfile` (network-key-signed, avatar-hash-bound),
the `PROFILE`/`PROFILES` verbs + event, the `display=`/`avatar=` member tags, the
avatar-blob fetch allowance, and the §11.8 avatar-mirror note. Appendix A entry.
