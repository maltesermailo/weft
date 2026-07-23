# Security & supply-chain posture

What is enforced automatically, what was measured, and what is genuinely still
open. Written so a claim here can be checked rather than taken on trust.

For the *adversary-oriented* view — trust boundaries, federation threats, the
DoS risk register, cryptography flags, and the invariant-enforcement table — see
[`threat-model.md`](./threat-model.md). This document is the codebase/tooling
half; that one is the running-deployment half.

Last measured: 2026-07-23.

## Enforced in CI (`.github/workflows/ci.yml`)

Every push and PR must pass:

| Gate | Command |
|---|---|
| Formatting | `cargo fmt --all --check` |
| Lints | `cargo clippy --workspace --exclude client --all-targets -- -D warnings` |
| Tests | `cargo test --workspace --exclude client --all-targets` + `--doc`, with a **live Postgres service** so the store contract suite runs against both backends rather than skipping the PG half |
| MSRV | `cargo check` on the declared `rust-version` (1.75) |
| Advisories · licences · bans · sources | `cargo deny check` |
| Fuzz smoke | `cargo fuzz run` over each `weft-proto` target, 60 s each |
| Client | `pnpm check` + `pnpm build` |

`client` (the Tauri desktop crate) is excluded from the Linux Rust jobs: it needs
GTK/WebKit/ALSA/v4l system libraries. It is covered by the desktop build, not
silently skipped — which is why the exclusion is written out rather than implied.

### Why cargo-deny and not cargo-audit

Both read the RustSec database. `cargo audit` scans `Cargo.lock`, which records
**optional** dependencies regardless of whether any feature enables them, so it
reports crates that are never compiled. `cargo deny` resolves features and
targets.

Concretely: audit reported `rsa` (RUSTSEC-2023-0071, Marvin timing attack, no fix
available) via `sqlx-mysql`. We enable only sqlx's `postgres` feature, so the
MySQL driver is not in the build graph — `cargo deny check advisories` confirms
it is absent. Chasing that report by dropping sqlx's `macros` feature broke
`sqlx::migrate!` and fixed nothing.

## Advisory triage

`deny.toml` ignores 21 advisories, **each with a written reason**; anything not
listed fails CI. They fall into two groups, and every one is desktop-client-only
— none is compiled into `weftd`:

- **Linux windowing stack** (19): the gtk-rs GTK3 bindings, which Tauri v2 pins,
  plus `atty`, `paste`, `proc-macro-error` and the `unic-*` tables. Unmaintained,
  no known vulnerability.
- **quick-xml < 0.41** (2, both 7.5 high, DoS): reached via `xcb` from the
  screen-capture path. `xcb` parses the *local* X11 protocol description, not
  network input, so the vector needs an attacker who can already write local
  files. Needs an upstream `xcb` release.

One item is a real to-do rather than an acceptance: `rustls-pemfile`
(RUSTSEC-2025-0134) is superseded by `rustls-pki-types`' own PEM support — a
mechanical swap in weftd's certificate loading.

## Fuzzing

`crates/weft-proto/fuzz` has three targets over the L0 parsers — the code that
sees unauthenticated remote bytes before anything else, and which CLAUDE.md
requires stay fuzzable in isolation:

- `parse_line` — the §4 line codec
- `parse_request` — client→server commands (pre-auth)
- `parse_reply` — server→client events (also the bridge-peer direction)

Each asserts two properties: **no panic** (a panic here is a remote crash) and
**strict-out** (anything we emit must re-parse identically — otherwise two peers
can disagree about what a line said).

Because fuzzing needs nightly and a corpus, `crates/weft-proto/tests/adversarial.rs`
runs the *same properties* on stable over a fixed corpus of boundary inputs:
empty/truncated lines, dangling and unknown escapes, structural markers in wrong
positions, NUL/BiDi/ZWJ unicode, integer fields at overflow, 8 KiB line-cap
boundaries, 500-tag lines, and **every prefix of a valid line** (the shape a
partial socket read actually takes). Anything a fuzz run finds should be pasted
in there permanently.

Status: all pass. No panic or round-trip drift found.

## `unwrap` / `expect`

Measured across `crates/*/src`, excluding in-file `#[cfg(test)]` modules:

| Category | Count | Assessment |
|---|---:|---|
| `Mutex::lock().expect("… lock")` | 165 | Poisoning. Standard practice, but see below. |
| Provably infallible | ~40 | `ciborium::into_writer` to a `Vec`, `Hmac::new_from_slice` (accepts any length), argon2 default params, `parse` on a string literal, "checked above" after an explicit guard. |
| **Total** | **212** | |

So "lots of unwraps" is overstated as a correctness risk — but the lock pattern
is worth naming honestly: if a thread panics while holding one of these, every
later lock on it panics too, turning one bug into a persistent failure. The
in-memory store is the main user. Not currently a policy; a candidate for
`parking_lot` (no poisoning) or explicit recovery.

## Known performance issues (not yet fixed)

These are real and measured, not hypothetical:

- **`GET /accounts` is N+1.** It issues `account_ulid`, `deletion_scheduled` and
  `is_suspended` **per account** — 3n+2 queries. Fine at 10 accounts, not at
  10 000. Needs a batched `list_accounts_full()` on `AccountStore`.
- **`GET /namespaces/:name/detail` is N+1 over channels** (`channel()` and
  `members()` each). Bounded by channels-in-one-namespace, so much smaller, but
  the same shape.
- **`EventStore::dm_partners` (PG) scans** every `dm:%` scope key and filters in
  Rust rather than matching in SQL.
- Fixed: `GET /admins` was O(grants × accounts) — it resolved each grant's ULID
  by looping every account. Now builds the index once.

The admin API is operator-only and low-traffic, which is why these have not
bitten — but they are the panel's scaling ceiling and should be batched before
any deployment with a large account table.

## Not yet addressed

- **Load / DoS testing.** There is *enforcement* — 8 KiB line cap, `MALFORMED`
  strike limit, `SLOW` backpressure on lagging consumers, idle timeouts, a
  per-account federation cooldown, the SSRF address classifier — and unit tests
  for several. There is no sustained load or connection-exhaustion test.
- **Rate limiting** beyond the `THROTTLED` plumbing (spec §18 open question 2).
- **A scheduled long fuzz campaign** with a persisted corpus. CI runs 60 s per
  target, which is a regression gate, not a campaign.
- **`cargo-semver-checks`** — not meaningful until something is published.
