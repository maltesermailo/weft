# Decisions needed — v0.11 editorial pass

Judgment calls the editorial pass could not make without choosing behavior. None block v0.11 (the text records the status quo honestly); each wants an owner ruling for a future design pass.

## 1. Event key=value tokens: tags or middle params?

The codec today: **commands** carry optional `key=value` as middle params (`HISTORY limit=`, `GRANT expiry=`, `INVITE MINT max-uses=`, `REPORTS LIST status=`, `BRIDGE PROPOSE history=`, `ROLE CREATE hoist=/pos=`); **events** carry them as tags — with one exception, `ROLE`, whose `hoist=`/`pos=` echo the command's middle-param form. v0.11 codifies this asymmetry in the §7 preamble.
**Options:** (a) keep and codify (done — zero wire change); (b) unify events to tags-only (`ROLE` changes shape — a breaking event change); (c) unify commands to tags (breaks every existing client). **Recommendation:** (a); revisit only if a v2 line grammar ever happens.

## 2. Invariants 5–7 and 10: recover or retire?

Cited nowhere in the spec; `[reserved]` in §9.0. The repo (CLAUDE.md "Security invariants") holds definitions that fit the gaps:
- **5** — auth replay-proofing: proofs sign `nonce ‖ network-name`; password compares constant-time; `AUTH-FAILED` uniform (§6.1 states all three, uncited).
- **6** — backpressure: slow client ⇒ `SLOW` + forced HISTORY resync; never unbounded buffering (§9.2 states it, uncited).
- **7** — `NETBLOCK` is name-keyed; all four effects fire together; key rotation never evades (§11.6 states it, uncited).
- **10** — compaction batch purity: batches never carry `EDITED` chains or reaction ping-pong (§12.1 states it, uncited).
**Options:** adopt these four into §9.0 and add the citations at §6.1/§9.2/§11.6/§12.1 (a pure documentation change, my recommendation), or renumber/retire. Adopting keeps spec numbering aligned with the repo's 13-invariant test suite.

## 3. §9.5 cross-network DMs

v0.11 adds the honest note that a two-member cross-network group DM already carries the conversation. **Options:** keep 1:1 DMs deferred with the note (done), or specify 1:1 semantics now (default-`permanent` retention across two networks, symmetric `HISTORY @user` across a bridge, consent + routing — the open §18 #7 design).

## 4. Appendix B location

Currently in the spec file as a non-normative appendix. **Options:** keep in-file (self-contained, my mild preference), or move to a repo `IMPLEMENTATION.md` and slim the spec further.

## 5. Shipped-but-unspecified wire surfaces (found during the codec audit)

The codec parses/serializes these; the spec never mentions them:
- `PROFILE SET` (command; `display=`/`avatar=` tags) and the `PROFILE <user>` event — §10.3 describes the concept and §11's diagram lists PROFILE in the mirror, but neither §6 nor §7 defines the wire form.
- `INVITE LIST` → `INVITE-INFO <scope> <invite-id> <creator>` (`uses=`/`expiry=` tags) — the invites-menu surface.
- `STREAM STORED <token> :<media-uri>` — the upload-complete event.
- `VOICE STATE/CAND/GRANT` events; the `CHANNEL CREATE` voice `kind` param; `CHANNEL-LAYOUT kind=` is now mentioned but the voice-channel model is otherwise §16-stub territory.
- `PRESENCE offline` as a *client-sent* status parses but has no specified semantics (the event side is now documented).
**Options:** a small follow-up amendment documenting each (they are shipped behavior — same class as the §6.8 social addition), or leave until their §16/§18 design passes. Flagged, not silently added, because they were outside this pass's mandate.

## 6. §16 voice under-specification

§16 is a four-line sketch while the shipped surface includes `VOICE OFFER/STATE/DESC/CAND/GRANT`, LiveKit transport (`mode=livekit`), voice-only channels, per-participant call media (§6.8), and the cross-network relay (§11.12). Needs a real WEFT-RT section; v0.11 only documents `VOICE OFFER` (cited by §16's own JOIN description) and leaves the rest to this decision.

## 7. Auto-federation open points (§11.10 TODOs)

(a) Foreign-invite redemption routing — server-automatic vs client-issued `FEDERATE` first; (b) the per-account cooldown duration (normative floor or implementation-chosen); (c) the amendment-draft-only proposals: sever-on-idle, auto-rejoin re-trigger, global dial caps + per-domain backoff, "`e2ee` namespaces are never auto-bridged". The draft (`docs/code/auto-federation-spec-amendment.md`) reads as owner-approved intent — say the word and they graduate from TODO to normative text.

## 8. `INVITE REVOKE-ALL` ack shape

§6.5 says the ack is "an `INVITED` with `invite-id=*`, `max-uses=0`" — but the `INVITED` event *requires* a `token=` tag in the codec, so the ack's literal wire form is under-determined (what token does a bulk-revoke ack carry?). The v0.11 example is deliberately descriptive rather than concrete. Pin the ack's exact shape (or switch it to a different event).
