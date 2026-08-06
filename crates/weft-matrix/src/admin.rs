//! The bridge's admin bot: operator commands typed into Matrix.
//!
//! The bot the appservice already registers (`[matrix] bot`) doubles as an
//! operator console — invite it to a room, or DM it, and issue commands. This
//! exists for the recovery story (owner requirement 2026-08-06): after a
//! database loss, `!weft recover` rebuilds everything derivable, and the
//! `attach` commands cover the residue a machine cannot infer.
//!
//! **Authorization is a config allowlist** (`[matrix] admins`), not a Matrix
//! power level. Power in a room says what you may do to that room; it says
//! nothing about who may re-point this bridge's internal state, and a room
//! admin on a *consumed* space is a stranger to us. An empty allowlist disables
//! the console entirely rather than falling back to something weaker.

/// A parsed operator command. Parsing is separate from execution so the grammar
/// is testable without a homeserver.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// Rebuild everything derivable from Matrix.
    Recover,
    /// What the daemon currently believes it bridges.
    Status,
    /// Re-point a puppet whose marker is missing: `<mxid> <account-ulid> [name]`.
    AttachPuppet {
        mxid: String,
        ulid: String,
        account: Option<String>,
    },
    /// Re-point a DM room: `<account> <mxid>` (run in the room itself).
    AttachDm { account: String, mxid: String },
    /// The command list.
    Help,
}

/// Parse a `!weft …` line. `None` when it is not addressed to us — an ordinary
/// message in a room the bot happens to be in must not be a command.
pub fn parse(body: &str) -> Option<Result<Command, String>> {
    let rest = body.trim().strip_prefix("!weft")?.trim();
    let mut parts = rest.split_whitespace();

    let verb = parts.next().unwrap_or("help");
    Some(match verb {
        "recover" => Ok(Command::Recover),
        "status" => Ok(Command::Status),
        "help" => Ok(Command::Help),
        "attach-puppet" => match (parts.next(), parts.next()) {
            (Some(mxid), Some(ulid)) => Ok(Command::AttachPuppet {
                mxid: mxid.to_string(),
                ulid: ulid.to_string(),
                account: parts.next().map(str::to_string),
            }),
            _ => Err("usage: !weft attach-puppet <mxid> <account-ulid> [account-name]".into()),
        },
        "attach-dm" => match (parts.next(), parts.next()) {
            (Some(account), Some(mxid)) => Ok(Command::AttachDm {
                account: account.to_string(),
                mxid: mxid.to_string(),
            }),
            _ => Err("usage: !weft attach-dm <weft-account> <mxid>  (in the DM room)".into()),
        },
        other => Err(format!("unknown command {other:?} — try !weft help")),
    })
}

pub const HELP: &str = "weft bridge console\n\
     !weft status                                  what this bridge believes it bridges\n\
     !weft recover                                 rebuild state from Matrix (safe to repeat)\n\
     !weft attach-puppet <mxid> <ulid> [name]      re-point a puppet whose marker is missing\n\
     !weft attach-dm <weft-account> <mxid>         re-point this room as a DM\n\
     !weft help                                    this list";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_addressed_lines_are_commands() {
        // An ordinary message in a room the bot sits in is not a command.
        assert!(parse("hello everyone").is_none());
        assert!(parse("the !weft bridge is nice").is_none());

        assert_eq!(parse("!weft recover"), Some(Ok(Command::Recover)));
        assert_eq!(parse("  !weft   status  "), Some(Ok(Command::Status)));
        // A bare mention is a help request, not an error.
        assert_eq!(parse("!weft"), Some(Ok(Command::Help)));
    }

    #[test]
    fn attach_commands_report_their_usage_rather_than_guessing() {
        assert_eq!(
            parse("!weft attach-puppet @weft_01h:test.example 01h ada"),
            Some(Ok(Command::AttachPuppet {
                mxid: "@weft_01h:test.example".into(),
                ulid: "01h".into(),
                account: Some("ada".into()),
            }))
        );
        // The display name is optional — the ULID is the identity.
        assert!(matches!(
            parse("!weft attach-puppet @weft_01h:test.example 01h"),
            Some(Ok(Command::AttachPuppet { account: None, .. }))
        ));

        // Missing arguments must not be filled in with a guess: a wrong
        // attachment silently re-points a real user's puppet.
        let Some(Err(usage)) = parse("!weft attach-puppet @weft_01h:test.example") else {
            panic!("expected a usage error");
        };
        assert!(usage.contains("attach-puppet"), "{usage}");

        let Some(Err(usage)) = parse("!weft attach-dm ada") else {
            panic!("expected a usage error");
        };
        assert!(usage.contains("attach-dm"), "{usage}");

        let Some(Err(unknown)) = parse("!weft frobnicate") else {
            panic!("expected an unknown-command error");
        };
        assert!(unknown.contains("frobnicate"), "{unknown}");
    }
}
