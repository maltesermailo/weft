//! Black-box round-trip tests over the public API: every spec example line
//! and a full request/response exchange survive parse → serialize → parse.

use weft_proto::{Command, ErrCode, Event, Line, Reply, Request};

/// A wire line whose canonical serialization is byte-identical to the input.
fn assert_canonical(input: &str) {
    let line = Line::parse(input).unwrap();
    assert_eq!(line.serialize().unwrap(), input);
}

#[test]
fn spec_example_lines_are_canonical() {
    // §3.6
    assert_canonical("HELLO weft/1");
    assert_canonical("@features=media,backfill,voice,irc-gw WELCOME hda.example :Willkommen");
    // §6.1 AUTH KEY exchange
    assert_canonical("AUTH KEY ada B64KEY==");
    assert_canonical("CHALLENGE B64NONCE==");
    assert_canonical("AUTH PROOF B64SIG==");
    assert_canonical("@attestation=B64ATT== WELCOME hda.example");
}

#[test]
fn full_send_ack_exchange() {
    // Client sends a labeled MSG (§9.2: echo with same label = ack).
    let request = Request::parse("@label=m1;fmt=md MSG #general :*hi*").unwrap();
    let Command::Msg { target, body, meta } = &request.command else {
        panic!()
    };
    assert_eq!(target.to_string(), "#general");
    assert_eq!(body.as_deref(), Some("*hi*"));
    assert_eq!(meta.fmt.as_deref(), Some("md"));

    // Server echo carries the label plus the actor-assigned msgid.
    let echo = Reply::parse(
        "@label=m1;fmt=md;msgid=hda.example/01ARZ3NDEKTSV4RRFFQ69G5FAV \
         MESSAGE #general ada@hda.example :*hi*",
    )
    .unwrap();
    assert_eq!(echo.label, request.label);
    let Event::Message(message) = &echo.event else {
        panic!()
    };
    assert_eq!(message.body, "*hi*");
    assert_eq!(message.msgid.origin().as_str(), "hda.example");

    // Both directions re-serialize to the same canonical bytes.
    assert_eq!(
        Request::parse(&request.serialize().unwrap()).unwrap(),
        request
    );
    assert_eq!(Reply::parse(&echo.serialize().unwrap()).unwrap(), echo);
}

#[test]
fn err_reply_echoes_label_on_direct_response() {
    // §3.5: every direct response echoes the label — including ERR.
    let reply = Reply::parse("@label=j7 ERR NO-SUCH-TARGET :no such channel").unwrap();
    assert_eq!(reply.label.as_deref(), Some("j7"));
    let Event::Err(err) = &reply.event else {
        panic!()
    };
    // §8: the single anti-enumeration code.
    assert_eq!(err.code, ErrCode::NoSuchTarget);
}

#[test]
fn noisy_input_parses_but_reserializes_canonically() {
    // Lenient-in: mixed case, duplicate spaces, CRLF. Strict-out: canonical.
    let line = Line::parse("join   #General\r\n").unwrap();
    assert_eq!(line.serialize().unwrap(), "JOIN #General"); // grammar keeps params verbatim
                                                            // ...while the typed layer folds identifier case (§2.3).
    let request = Request::from_line(&line).unwrap();
    let Command::Join { channel, .. } = &request.command else {
        panic!()
    };
    assert_eq!(channel.as_str(), "#general");
}
