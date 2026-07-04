//! Error registry (spec §8). Codes are stable; human text is not.
//! There is deliberately no `UNKNOWN-COMMAND`: unknown verbs are ignored
//! and labels make the silence detectable.

use std::fmt;
use std::str::FromStr;

use crate::error::ParseError;

/// Every normative `ERR` code (§8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrCode {
    Malformed,
    Unsupported,
    NotAuthed,
    AuthFailed,
    /// The single anti-enumeration code (§2.2): nonexistent, private,
    /// view-gated, expired/foreign msgid, dead invite — one code.
    NoSuchTarget,
    Conflict,
    Forbidden,
    /// Carries the missing capability as the context param.
    CapRequired,
    Banned,
    Blocked,
    Quota,
    TooLarge,
    Throttled,
    Policy,
    Slow,
    Internal,
}

impl ErrCode {
    /// All registered codes, for exhaustive registry tests.
    pub const ALL: [ErrCode; 16] = [
        ErrCode::Malformed,
        ErrCode::Unsupported,
        ErrCode::NotAuthed,
        ErrCode::AuthFailed,
        ErrCode::NoSuchTarget,
        ErrCode::Conflict,
        ErrCode::Forbidden,
        ErrCode::CapRequired,
        ErrCode::Banned,
        ErrCode::Blocked,
        ErrCode::Quota,
        ErrCode::TooLarge,
        ErrCode::Throttled,
        ErrCode::Policy,
        ErrCode::Slow,
        ErrCode::Internal,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            ErrCode::Malformed => "MALFORMED",
            ErrCode::Unsupported => "UNSUPPORTED",
            ErrCode::NotAuthed => "NOT-AUTHED",
            ErrCode::AuthFailed => "AUTH-FAILED",
            ErrCode::NoSuchTarget => "NO-SUCH-TARGET",
            ErrCode::Conflict => "CONFLICT",
            ErrCode::Forbidden => "FORBIDDEN",
            ErrCode::CapRequired => "CAP-REQUIRED",
            ErrCode::Banned => "BANNED",
            ErrCode::Blocked => "BLOCKED",
            ErrCode::Quota => "QUOTA",
            ErrCode::TooLarge => "TOO-LARGE",
            ErrCode::Throttled => "THROTTLED",
            ErrCode::Policy => "POLICY",
            ErrCode::Slow => "SLOW",
            ErrCode::Internal => "INTERNAL",
        }
    }
}

impl FromStr for ErrCode {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let folded = s.to_ascii_uppercase();
        ErrCode::ALL
            .iter()
            .find(|code| code.as_str() == folded)
            .copied()
            .ok_or_else(|| ParseError::Invalid {
                what: "error code",
                value: s.to_string(),
            })
    }
}

impl fmt::Display for ErrCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_round_trips() {
        for code in ErrCode::ALL {
            assert_eq!(code.as_str().parse::<ErrCode>().unwrap(), code);
        }
    }

    #[test]
    fn unknown_code_rejected() {
        assert!("UNKNOWN-COMMAND".parse::<ErrCode>().is_err()); // §8: deliberately absent
    }
}
