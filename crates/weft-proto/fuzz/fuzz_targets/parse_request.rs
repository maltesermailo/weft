#![no_main]
//! Client→server commands (§6) against arbitrary text.
//!
//! Every byte a client can send reaches this parser before authentication, so a
//! panic is an unauthenticated remote crash. Unknown verbs must become
//! `Command::Unknown`, never an error and never a panic (§8).
use libfuzzer_sys::fuzz_target;
use weft_proto::Request;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(request) = Request::parse(text) else {
        return;
    };
    // Strict-out (§4): we must never emit a line our own parser rejects.
    let Ok(out) = request.serialize() else {
        return; // a refusal to serialize is allowed; emitting garbage is not
    };
    let reparsed = Request::parse(&out).expect("we must accept what we emit");
    assert_eq!(
        reparsed.command, request.command,
        "command changed across a round trip: {text:?}"
    );
});
