//! Adversarial-input regression gate for the §4/§6/§7 parsers.
//!
//! The fuzz targets under `fuzz/` are the campaign; this is the part that runs
//! on stable in CI on every push. It asserts the same two properties over a
//! corpus of inputs chosen to break parsers — so a panic reachable from
//! untrusted bytes fails the build rather than waiting for a fuzz run:
//!
//!  1. **No panic.** These parsers see unauthenticated remote input.
//!  2. **Strict-out (§4).** Whatever we emit, we must be able to parse back
//!     identically — otherwise two peers can disagree about what a line said.
//!
//! Anything found by fuzzing should be pasted in here as a permanent case.

use weft_proto::{Line, Reply, Request};

/// Inputs designed to hit boundaries: empty and whitespace-only, truncated
/// prefixes, unterminated escapes, oversize fields, duplicate and empty tags,
/// unicode edge cases, and structural markers in unexpected positions.
fn corpus() -> Vec<String> {
    let mut v: Vec<String> = vec![
        // Degenerate.
        "",
        " ",
        "\t",
        ":",
        "@",
        "@;",
        "@=",
        " :",
        ": ",
        "::",
        "@ ",
        "@:",
        // Verb-only / truncated.
        "MSG",
        "MSG ",
        "MSG #",
        "MSG #general",
        "MSG #general :",
        "PRIVMSG",
        "@label=",
        "@label",
        "@label= MSG",
        "@=x MSG #a :b",
        "@;;; MSG #a :b",
        // Escapes: dangling, doubled, unknown.
        r"MSG #a :b\",
        r"MSG #a :b\\",
        r"MSG #a :b\x",
        r"MSG #a :\r\n",
        r"@k=v\ MSG #a :b",
        r"@k=v\s\s MSG #a :b",
        r"@k=\ MSG #a :b",
        // Structural markers in odd places.
        "MSG :#a :b",
        "MSG #a:b :c",
        "@a=:b MSG #c :d",
        "MSG #a ::b",
        // Separators.
        "MSG\r\n#a :b",
        "MSG\n",
        "\r\n",
        "MSG #a\t:b",
        "   MSG   #a   :b   ",
        // Unicode / normalization / bidi.
        "MSG #a :\u{0}",
        "MSG #a :\u{feff}",
        "MSG #a :\u{202e}gnip",
        "MSG #ä :b",
        "MSG #a :🧵\u{200d}🧶",
        "@k=\u{1}v MSG #a :b",
        // Numeric fields that must not panic on overflow.
        "ROLE CREATE ns:a #fff send pos=99999999999999999999 :n",
        "ROLE CREATE ns:a #fff send pos=-1 :n",
        "ROLE CREATE ns:a #fff send hoist=maybe :n",
        "HISTORY #a limit=99999999999999999999",
        "HISTORY #a limit=-5",
        // Events (server→client direction).
        "MESSAGE",
        "MESSAGE #a",
        "ERR",
        "ERR :",
        "@msgid= MESSAGE #a b :c",
        "MESSAGE #a b@c :d",
        "BATCH",
        "BATCH + x",
        "BATCH -",
        // Comma-delimited fields (roles, caps, manifests).
        "ROLE REORDER ns:a :",
        "ROLE REORDER ns:a :,,,",
        "ROLE RENAME ns:a :,",
        "ROLE RENAME ns:a :a,",
        "ROLE RENAME ns:a :,b",
        "ROLE RENAME ns:a :a,b,c",
        "GRANT x ns:a ,,,",
        "GRANT x ns:a *",
        // Retention policy: multibyte trailing char must not land the unit
        // split inside a UTF-8 char (fuzz find: parse_reply POLICY).
        "POLICY #a retained:3û",
        "POLICY #a retained:û",
        "POLICY #a retained:12€",
        "CHANNEL POLICY #a retained:5naïve",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();

    // Length boundaries around the §4 8 KiB line cap — under, at, and over.
    for n in [8_000usize, 8_190, 8_192, 8_193, 16_384] {
        v.push(format!("MSG #general :{}", "a".repeat(n)));
        v.push(format!("@label={} MSG #general :hi", "b".repeat(n)));
        v.push(format!("MSG #{} :hi", "c".repeat(n)));
    }
    // Many tags, and deeply repeated separators.
    v.push(format!(
        "@{} MSG #a :b",
        (0..500)
            .map(|i| format!("k{i}=v"))
            .collect::<Vec<_>>()
            .join(";")
    ));
    v.push(format!("MSG {} :b", "#a ".repeat(500)));
    v.push(format!("MSG #a :{}", ":".repeat(5000)));
    v
}

#[test]
fn parsers_never_panic_and_are_strict_out() {
    for input in corpus() {
        // 1. The raw line codec.
        if let Ok(line) = Line::parse(&input) {
            if let Ok(out) = line.serialize() {
                let reparsed = Line::parse(&out)
                    .unwrap_or_else(|e| panic!("emitted a line we reject: {out:?} ({e:?})"));
                assert_eq!(
                    Ok(out.clone()),
                    reparsed.serialize(),
                    "serialize not idempotent for {input:?}"
                );
            }
        }

        // 2. Commands (client→server, pre-auth).
        if let Ok(request) = Request::parse(&input) {
            if let Ok(out) = request.serialize() {
                let reparsed = Request::parse(&out)
                    .unwrap_or_else(|e| panic!("emitted a command we reject: {out:?} ({e:?})"));
                assert_eq!(
                    reparsed.command, request.command,
                    "command drifted across a round trip: {input:?}"
                );
            }
        }

        // 3. Events (server→client, and bridge→us).
        if let Ok(reply) = Reply::parse(&input) {
            if let Ok(out) = reply.serialize() {
                let reparsed = Reply::parse(&out)
                    .unwrap_or_else(|e| panic!("emitted an event we reject: {out:?} ({e:?})"));
                assert_eq!(
                    reparsed.event, reply.event,
                    "event drifted across a round trip: {input:?}"
                );
            }
        }
    }
}

/// A line over the §4 cap must be **rejected**, not truncated: silently cutting
/// it would let a peer smuggle a different message than the one we validated.
#[test]
fn oversize_lines_are_rejected_not_truncated() {
    let huge = format!("MSG #general :{}", "a".repeat(64 * 1024));
    match Line::parse(&huge) {
        Err(_) => {}
        Ok(line) => {
            let out = line.serialize().expect("if we accept it we must emit it");
            assert_eq!(
                out, huge,
                "an accepted oversize line must round-trip whole, never truncated"
            );
        }
    }
}

/// Every prefix of a valid line must parse-or-error, never panic — the shape a
/// partial read off the wire actually takes.
#[test]
fn every_prefix_of_a_valid_line_is_safe() {
    let full = "@label=m1;fmt=md;msgid=hda.example/01ARZ3NDEKTSV4RRFFQ69G5FAV \
                MESSAGE #ns/general ada@hda.example :hello \\r\\n world";
    for i in 0..full.len() {
        let prefix = &full[..i];
        if !prefix.is_char_boundary(i) {
            continue;
        }
        let _ = Line::parse(prefix);
        let _ = Request::parse(prefix);
        let _ = Reply::parse(prefix);
    }
}
