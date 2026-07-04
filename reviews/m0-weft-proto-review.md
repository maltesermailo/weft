# M0 Review — weft-proto codec

*Self-review of the initial M0 implementation (spec v0.10). Status at time of review:
49 tests green, `cargo clippy --workspace --all-targets -- -D warnings` clean, `cargo fmt` applied.*

## Scope reviewed

The repo was restructured into the CLAUDE.md workspace layout: `crates/weft-proto`
(the L0 codec — pure, no I/O, no tokio, only `thiserror` + `ulid`) and `crates/weftd`
(placeholder binary for M1).

| Module | Spec | Contents |
|---|---|---|
| `line.rs` | §4 | `Line` grammar (`@tags VERB params :trailing`), tag escaping, all limits (8 KiB / 32 tags / 15 params / 64 B label) |
| `name.rs` | §2.3 | `Account`, `NetworkName`, `UserRef`, `ChannelName`, `Target` (`#chan` / `@user`) |
| `id.rs` | §5.1 | `MsgId` = `origin/ULID`, ordered so same-origin IDs sort by time |
| `policy.rs` | §5.2 | `RetentionPolicy` incl. `strictest()` bridge negotiation |
| `errcode.rs` | §8 | all 16 `ErrCode`s, deliberately no `UNKNOWN-COMMAND` |
| `types.rs` | — | shared wire enums + `MsgMeta` (`fmt=`, `reply-to=`, `thread=`, `attach.N=`) used by both directions |
| `command.rs` / `event.rs` | §6 / §7 | `Request` / `Reply` = payload + `label`, symmetric `parse`/`serialize` APIs |

Key behaviors: lenient-in/strict-out (parser folds case and tolerates space runs;
serializer refuses anything the parser rejects, with a `debug_assert` round-trip check),
unknown verbs → `Command::Unknown` / `Event::Unknown` (never an error, no wire form out),
round-trip tests for every wire type, spec example lines re-serialize byte-identically.

**Spec amendment made alongside** (per the CLAUDE.md rule): §7 said "as v0.8" for the
TYPING/MARKED/PRESENCE/POLICY event payloads despite v0.10 claiming self-containment.
They are now defined concretely in the §7 table and the change is noted in Appendix A.

## Issues found and fixed during review

1. **8 KiB limit asymmetry (real bug).** `Line::parse` checked the length limit *before*
   stripping the terminator, while `serialize` could emit exactly 8192 bytes without one —
   our own maximal output would have been rejected once the transport appended `\r\n`,
   violating strict-out. Fixed: the limit now applies to the line body; regression test
   `max_length_line_survives_terminated_round_trip` covers it.
2. **`clippy::large_enum_variant`** on `Event::Message` (~300 B vs ~97 B second-largest) —
   boxed the `MessageEvent`.
3. **`attach.N` tag ordering.** `BTreeMap` iterates `attach.10` before `attach.2`
   lexically; the codec sorts attachment indices numerically, with a test proving it.
4. **Leftover shared `Args` cursor** with an empty verb name in `Command::from_line` —
   removed in favor of per-branch cursors so error messages always name the real verb.

## Deliberate tradeoffs (open for revisiting)

- **Unknown `ERR` codes are a parse error**, not `Event::Unknown`. §8 says codes are
  stable, so the enum is closed; if a future spec version adds codes, old clients will
  drop the label correlation on those errors. Revisit if forward-compat starts to matter
  more than strictness.
- **Over-long labels (>64 B) fail parse entirely**, so the server answers `MALFORMED`
  *without* an echo. The alternative (accept in, refuse out) would make the mandatory
  label echo unserializable, which is worse.
- **Semantic rules stay out of the codec**: "empty body iff attachments", password ≥12 B,
  TYPING rate limits are session-layer (M1) concerns — the codec would otherwise
  duplicate policy that needs server config.
- **`RetainedFor` keeps `count` + `unit` as written** so `retained:90d` round-trips
  exactly instead of degrading to seconds; `24h` and `1d` still compare as equally strict
  in `strictest()`.

## Security-relevant detail

`Command::Msg` has no `msgid` field at all, so a client-supplied `msgid=` tag on `MSG` is
structurally ignored — msgids can only enter via server events, upholding the invariant
that ULIDs are minted only by the channel actor (§5.1, §9.1).

## Next step

M1: session FSM in `weft-core/src/session.rs`; the quinn acceptor is plumbing around it.
