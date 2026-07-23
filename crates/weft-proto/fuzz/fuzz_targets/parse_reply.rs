#![no_main]
//! Server→client events (§7). A client parses these from whatever its network
//! — or a *bridge peer's* network — sends, so the same no-panic and strict-out
//! properties apply in the other direction.
use libfuzzer_sys::fuzz_target;
use weft_proto::Reply;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(reply) = Reply::parse(text) else {
        return;
    };
    let Ok(out) = reply.serialize() else {
        return;
    };
    let reparsed = Reply::parse(&out).expect("we must accept what we emit");
    assert_eq!(
        reparsed.event, reply.event,
        "event changed across a round trip: {text:?}"
    );
});
