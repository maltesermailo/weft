#![no_main]
//! The §4 line codec against arbitrary bytes.
//!
//! Two properties, both security-relevant:
//!  1. **No panic.** `Line::parse` is the first thing untrusted bytes touch; a
//!     panic here is a remote crash (invariant: L0 has no I/O and must stay
//!     fuzzable in isolation — CLAUDE.md).
//!  2. **Strict-out.** Anything we parse must re-serialize to something our own
//!     parser accepts, and re-parse to the same line. A round-trip that drifts
//!     means two peers can disagree about what a line said.
use libfuzzer_sys::fuzz_target;
use weft_proto::Line;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(line) = Line::parse(text) else {
        return; // rejecting malformed input is the correct outcome
    };
    let Ok(out) = line.serialize() else {
        return; // refusing to serialize is allowed; emitting garbage is not
    };
    let reparsed = Line::parse(&out).expect("we must accept what we emit");
    assert_eq!(
        Ok(out),
        reparsed.serialize(),
        "serialize is not idempotent for {text:?}"
    );
});
