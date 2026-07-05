//! Capabilities (spec §10.4). A closed enum of the standard set plus
//! `grant:<cap>` (the delegation right for a specific capability). The
//! wire/CBOR form is the lowercase string in the spec, so tokens are
//! netcat-legible and forward-comparable.

use std::fmt;
use std::str::FromStr;

use crate::CryptoError;

/// One capability. `Grant(Box<Capability>)` is `grant:<cap>` — the right to
/// delegate `<cap>` to others (chain rule, §10.4). Boxed so the enum stays
/// small; grant-of-grant (`grant:grant:send`) is representable and legal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Capability {
    Send,
    EditOwn,
    DeleteOwn,
    DeleteAny,
    React,
    Pin,
    Invite,
    Kick,
    Ban,
    /// Deny `send` to an account at a scope (a persistent mute, §6.7).
    Mute,
    Policy,
    View,
    Attach,
    ChanCreate,
    Reports,
    Bridge,
    NsAdmin,
    NsCreate,
    Netblock,
    /// `grant:<cap>` — may delegate the inner capability.
    Grant(Box<Capability>),
}

impl Capability {
    /// The standard set, sans `grant:` (which composes over these).
    pub const STANDARD: [Capability; 19] = [
        Capability::Send,
        Capability::EditOwn,
        Capability::DeleteOwn,
        Capability::DeleteAny,
        Capability::React,
        Capability::Pin,
        Capability::Invite,
        Capability::Kick,
        Capability::Ban,
        Capability::Mute,
        Capability::Policy,
        Capability::View,
        Capability::Attach,
        Capability::ChanCreate,
        Capability::Reports,
        Capability::Bridge,
        Capability::NsAdmin,
        Capability::NsCreate,
        Capability::Netblock,
    ];

    fn base_str(&self) -> Option<&'static str> {
        Some(match self {
            Capability::Send => "send",
            Capability::EditOwn => "edit-own",
            Capability::DeleteOwn => "delete-own",
            Capability::DeleteAny => "delete-any",
            Capability::React => "react",
            Capability::Pin => "pin",
            Capability::Invite => "invite",
            Capability::Kick => "kick",
            Capability::Ban => "ban",
            Capability::Mute => "mute",
            Capability::Policy => "policy",
            Capability::View => "view",
            Capability::Attach => "attach",
            Capability::ChanCreate => "chan-create",
            Capability::Reports => "reports",
            Capability::Bridge => "bridge",
            Capability::NsAdmin => "ns-admin",
            Capability::NsCreate => "ns-create",
            Capability::Netblock => "netblock",
            Capability::Grant(_) => return None,
        })
    }

    /// The capability this token may *delegate*, if it is a `grant:`. So
    /// `grant:send` delegates `send`; `grant:grant:send` delegates
    /// `grant:send`.
    pub fn delegates(&self) -> Option<&Capability> {
        match self {
            Capability::Grant(inner) => Some(inner),
            _ => None,
        }
    }
}

impl FromStr for Capability {
    type Err = CryptoError;

    fn from_str(s: &str) -> Result<Self, CryptoError> {
        if let Some(rest) = s.strip_prefix("grant:") {
            return Ok(Capability::Grant(Box::new(rest.parse()?)));
        }
        Capability::STANDARD
            .iter()
            .find(|cap| cap.base_str() == Some(s))
            .cloned()
            .ok_or(CryptoError::BadCapability)
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Capability::Grant(inner) => write!(f, "grant:{inner}"),
            other => f.write_str(other.base_str().expect("non-grant has a string")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_standard_cap_round_trips() {
        for cap in Capability::STANDARD {
            assert_eq!(cap.to_string().parse::<Capability>().unwrap(), cap);
        }
    }

    #[test]
    fn grant_nests_and_round_trips() {
        let g: Capability = "grant:send".parse().unwrap();
        assert_eq!(g, Capability::Grant(Box::new(Capability::Send)));
        assert_eq!(g.delegates(), Some(&Capability::Send));
        assert_eq!(g.to_string(), "grant:send");

        // grant-of-grant is legal (delegating the delegation right).
        let gg: Capability = "grant:grant:ban".parse().unwrap();
        assert_eq!(gg.to_string(), "grant:grant:ban");
        assert_eq!(gg.delegates().unwrap().to_string(), "grant:ban");
    }

    #[test]
    fn unknown_caps_rejected() {
        assert_eq!(
            "telepathy".parse::<Capability>(),
            Err(CryptoError::BadCapability)
        );
        assert_eq!(
            "grant:telepathy".parse::<Capability>(),
            Err(CryptoError::BadCapability)
        );
        assert!("send ".parse::<Capability>().is_err()); // no trimming
    }
}
