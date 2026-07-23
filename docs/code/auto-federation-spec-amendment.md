# Proposed spec amendment — Auto-federation (P0)

Status: **proposed, for review** (2026-07-07). Folds into
`docs/weft-protocol-spec.md` (§2/§6/§11 + Appendix A) when **P1** lands — kept
here until then so the normative spec isn't changed ahead of code. Written in the
spec's register. Design rationale: `docs/auto-federation-plan.md`.

Reflects the resolved decisions: `network/namespace` (UI) + `weft://` (links);
explicit `BRIDGE REQUEST` verb; per-namespace `federation` flag; persistent
membership (auto-rejoin); sever-on-idle; **open** trigger policy.

---

## A. Addressing — insert into §2.1 (identifiers)

> **Cross-network reference.** `<network>/<namespace>` (e.g. `test.example/gaming`)
> — the left segment is a DNS network name, the right a namespace on it. Local
> references stay bare (`gaming`, `#gaming/general`). Link forms:
> `weft://<network>/<namespace>` (open a namespace) and
> `weft://<network>/<namespace>/i/<token>` (invite — the namespace is embedded so
> a foreign redeemer auto-federates to it; top-level-channel invites omit it).
> Clients display `network/namespace`;
> `weft://` is the shareable/clickable form. The network segment MUST be a
> resolvable **public** DNS name (§11.10).

## B. Namespace `federation` flag — §6.2 (NS commands)

Add to the `NS META` row (or a sibling): `NS META <name> federation :open|closed`
— cap `ns-admin`; `open` requires `public` visibility (else `FORBIDDEN`); emits
`NS-META` with a `federation=` field. Prose:

> A namespace is **auto-federation-reachable** iff `visibility = public ∧
> federation = open`. Default `closed`. Only such namespaces are offered in
> response to a peer's `BRIDGE REQUEST` (§11.10); everything else answers
> `NO-SUCH-TARGET` (anti-enumeration unchanged). `e2ee` channels within a
> reachable namespace are still never offered (invariant 8 / §14).

## C. `BRIDGE REQUEST` verb — §6.6 (Federation & operator)

Add a row:

| Command | Syntax | Cap | → Result / notes |
|---|---|---|---|
| `BRIDGE REQUEST` | `BRIDGE REQUEST <ns>` | bridge session | Ask the peer to offer a manifest for one of **its** namespaces. Peer replies `BRIDGE PROPOSE` (its signed manifest) iff `<ns>` is auto-federation-reachable (§6.2) and the requester isn't netblocked; else `NO-SUCH-TARGET` / `BLOCKED`. Bridge-session-only. |

Rationale note: a manifest for `<ns>` must be signed by that namespace's scope
authority, which lives on the peer — so the requesting side **asks**, the owning
side **offers**. Bounded: one namespace per request, no `accept_any` blanket.

## C.2 `FEDERATE` verb — §6.6 (the client-facing trigger)

Add a row: `FEDERATE <network>/<namespace>` — cap `membership` + `auto_bridge`
open; → async (`MANIFEST` when live); errors `UNSUPPORTED` / `BLOCKED` /
`THROTTLED`.

**Why a distinct verb (not `NS JOIN <net>/<ns>`).** `NS JOIN` is a *membership*
action against a namespace your server already carries; `FEDERATE` is a
*request to your home network to go get one it doesn't have yet* — a different
operation with a different failure surface (SSRF, netblock, dial failure,
policy-off). Overloading `NS JOIN` would blur "join what exists" with "make it
exist," and hide the outbound-dial side effects behind an innocuous verb. A
separate verb keeps the on-demand dial explicit and greppable.

**Why client→home, not over the bridge session.** The two federation-request
verbs are deliberately different directions: `BRIDGE REQUEST` is peer→peer (H's
server asks F's server, §C), while `FEDERATE` is user→home (a client asks *its
own* server to initiate). `FEDERATE` is what *causes* the `BRIDGE REQUEST` to be
sent, one hop earlier in the chain.

**Layering (why the trigger needs a port, not just a handler).** The command is
parsed + policy-gated in weft-core (L2), but weft-core has no transport and
cannot dial. So the handler hands an `AutoBridgeRequest {network, namespace}` to
weftd (L3) — which owns the dialer — over an in-process port (a
`ServerCtx`-held sender, installed by weftd only when `auto_bridge = open`). This
mirrors the existing ports/adapters seams (`ControlStream`, `EventStore`): L2
states the intent, L3 performs the I/O. It also means the open/off policy is
expressed structurally — no sink installed ⇒ `FEDERATE` answers `UNSUPPORTED`,
with no separate flag to keep in sync.

**Why a per-account cooldown.** Under the open trigger policy (§6) any member can
initiate a dial; the cooldown is the minimal in-core rate-limit that stops a
single account from issuing a dial-storm. It composes with the transport-level
SSRF guard and NETBLOCK — belt, braces, and a third belt.

## D. New §11.10 — Auto-federation (on-demand bridging)

> Federation MAY be established **on demand**: a local user referencing a foreign
> namespace (`<network>/<namespace>`, or a `weft://<network>/…` link/invite whose
> network ≠ home) triggers the home network to auto-establish the bridge — no
> operator ceremony. Governed by config `auto_bridge = open | off` (default
> `off`); `off` disables all outbound auto-establishment (inbound §11.2
> unaffected).
>
> **Trigger — open policy.** Any local account may trigger; there is no per-user
> capability or allow-list. Authorization to *enter* is the target namespace's
> own rule (`public ∧ federation=open`, §6.2) — the join-rule model. Abuse is
> handled reactively via `NETBLOCK` (§11.6).
>
> **Flow** (home `H`, foreign `F`, namespace `N`):
> 1. If `NETBLOCK(F)` → `ERR BLOCKED`. If a live bridge `H↔F` already covers `N`
>    → reuse it (join, done).
> 2. Resolve `F`: fetch `F`'s `/.well-known/weft` → network signing key + QUIC
>    endpoint.
> 3. Dial `F` over **verified** QUIC (ALPN `weft/1`); `AUTH BRIDGE` proving `H`'s
>    network key (§11.2).
> 4. `BRIDGE REQUEST <N>` (§6.6). `F` offers **only** iff `N` is auto-federation-
>    reachable ∧ `H` not netblocked by `F`; it signs `N`'s manifest (scope
>    authority = `F`, §11.1) and replies `BRIDGE PROPOSE`. Else `NO-SUCH-TARGET`.
> 5. `H` auto-accepts (`BRIDGE ACCEPT`); the bridge goes live, `N`'s channels
>    mirror into `H` (§11.4), the user joins. `MANIFEST` to affected members
>    (§11.5).
>
> **Membership — persistent.** A local member of a bridged foreign namespace is
> an ordinary membership record; on reconnect, **auto-rejoin** re-triggers the
> flow if the bridge was severed. The user's events originate on `H` and forward
> one hop to `F` (§11.4 — origin preserved, never re-minted).
>
> **Teardown — sever-on-idle.** When the last local member of a bridged foreign
> namespace leaves, `H` SHOULD **sever** the ns-scoped bridge after an idle grace
> window; re-access re-establishes it. Bounds the outbound-bridge set to active
> interest.
>
> **Security (normative — these hold even under `open`; `open` removes only the
> per-user auth gate):**
> - **Private-address block (MUST).** Refuse to dial loopback, RFC-1918,
>   link-local, CGNAT (100.64/10), ULA (fc00::/7), or cloud-metadata addresses.
>   Resolve `F` via public DNS only — naming `F` MUST NOT reach internal hosts.
> - **Consent is structural (MUST).** `F` offers only auto-federation-reachable
>   namespaces; no path lets `H` compel a bridge.
> - **Rate + cap (SHOULD).** Rate-limit new outbound dials per-account and
>   globally; exponential backoff per failing domain; cap concurrent outbound
>   bridges.
> - **Well-known fetch (MUST).** TLS-verified, bounded timeout + response size,
>   no redirect to a private host.
> - **Visibility (MUST).** Auto-established bridges emit `MANIFEST` to members
>   (§11.5) and appear on the federation surface — never silent.
> - **e2ee (MUST NOT).** `e2ee` namespaces are never auto-bridged (invariant 8).

## E. Config — weftd `[federation]`

```toml
[federation]
# Outbound on-demand bridging (§11.10). "off" = only manual/pinned peering.
auto_bridge = "off"   # "off" | "open"
```

## F. Appendix A — decision-history entry

> **v0.11 (proposed) — Auto-federation (§11.10).** On-demand bridge
> establishment triggered by a user referencing a foreign namespace
> (`network/namespace` / `weft://`). New `BRIDGE REQUEST` verb (§6.6),
> per-namespace `federation` flag (§6.2), `<network>/<namespace>` addressing
> (§2.1). **Open** trigger policy (Matrix-model, chosen deliberately) with
> *mandatory* SSRF/consent/rate guardrails that ship with the trigger, not after
> — the lesson Matrix learned retroactively. One-hop relay and the explicit
> bilateral bridge are retained (WEFT does not adopt Matrix's full replication or
> emergent membership).

---

## Security invariant to add (CLAUDE.md §"Security invariants")

> **13. Auto-federation SSRF (§11.10):** the outbound dialer MUST refuse any
> non-public target (loopback / RFC-1918 / link-local / CGNAT / ULA / metadata);
> a user-supplied network name can never make the server reach internal
> infrastructure. Implement as a test over the address-classification function,
> not just the dial path.
