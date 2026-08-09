//! Replies, both directions (§9.3 ⇄ Matrix rich replies).
//!
//! WEFT carries a reply as `reply-to=<msgid>` — a plain pointer at the root, and
//! nothing else. Matrix carries it as an `m.relates_to.m.in_reply_to.event_id`
//! relation **plus**, historically, a quoted copy of the original prepended to the
//! body as a "fallback" for clients that cannot render the relation. Translating
//! means mapping the pointer through the link table in both directions, and
//! removing the fallback on the way in — a WEFT client renders the root itself, so
//! keeping it would quote every reply twice.

use serde_json::{json, Value};

/// The event id a Matrix message replies to, if any.
///
/// An `m.replace` (edit) also lives under `m.relates_to`, so the reply relation is
/// read from its own `m.in_reply_to` sub-object rather than from `event_id` —
/// which both relation kinds carry, and which would otherwise make every edit look
/// like a reply to the message it edits.
pub fn in_reply_to(content: &Value) -> Option<&str> {
    content["m.relates_to"]["m.in_reply_to"]["event_id"].as_str()
}

/// The `m.relates_to` a reply to `event_id` needs.
///
/// No body fallback is generated: MSC2781 deprecated it, every current client
/// renders the relation, and a WEFT body is authored text — prepending a quote
/// would put words in the author's message.
pub fn relation(event_id: &str) -> Value {
    json!({ "m.in_reply_to": { "event_id": event_id } })
}

/// A reply body with Matrix's quoted fallback removed.
///
/// The fallback is a run of `> `-prefixed lines followed by one blank line
/// (spec: "rich reply fallbacks"). Only that exact shape is stripped, and only
/// from a message we already know is a reply, so a message that merely *starts*
/// with a quote keeps it: a user quoting something by hand means it.
pub fn strip_fallback(body: &str) -> &str {
    let mut rest = body;
    let mut stripped_any = false;

    while let Some(line_end) = rest.find('\n') {
        if !rest.starts_with("> ") && !rest.starts_with(">\n") {
            break;
        }

        rest = &rest[line_end + 1..];
        stripped_any = true;
    }

    // The blank line separating fallback from reply. Without it this was not a
    // fallback block at all, so nothing is stripped.
    match rest.strip_prefix('\n') {
        Some(after) if stripped_any => after,
        _ => body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_only_a_real_reply_relation() {
        let reply = json!({ "m.relates_to": { "m.in_reply_to": { "event_id": "$root" } } });
        assert_eq!(in_reply_to(&reply), Some("$root"));

        // An edit carries `event_id` at the top level of the relation — it is not a
        // reply to the message it replaces.
        let edit = json!({ "m.relates_to": { "rel_type": "m.replace", "event_id": "$root" } });
        assert_eq!(in_reply_to(&edit), None);

        assert_eq!(in_reply_to(&json!({ "body": "hi" })), None);
    }

    #[test]
    fn strips_the_quoted_fallback_only() {
        // Element's shape: quote lines, a blank line, then the actual reply.
        let body = "> <@alice:matrix.org> the original\n> second line\n\nmy answer";
        assert_eq!(strip_fallback(body), "my answer");

        // A hand-written quote with no blank separator is the message itself.
        let quote = "> I said this\nand I stand by it";
        assert_eq!(strip_fallback(quote), quote);

        // Nothing to strip.
        assert_eq!(strip_fallback("plain"), "plain");

        // A reply whose own body starts with a quote keeps it: only the leading
        // fallback block goes.
        let nested = "> <@bob:matrix.org> hi\n\n> quoting on purpose\nyes";
        assert_eq!(strip_fallback(nested), "> quoting on purpose\nyes");
    }

    #[test]
    fn relation_names_the_root() {
        assert_eq!(relation("$abc")["m.in_reply_to"]["event_id"], "$abc");
    }
}
