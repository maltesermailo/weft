//! Channel retention policies (spec §5.2):
//! `ephemeral | retained:<n>(d|h|s) | permanent | e2ee`, n > 0.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use crate::error::ParseError;

/// Unit of a `retained:<n>(d|h|s)` duration. Kept as written so the wire
/// form round-trips exactly (`retained:90d` never becomes `retained:7776000s`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetentionUnit {
    Days,
    Hours,
    Seconds,
}

impl RetentionUnit {
    fn suffix(self) -> char {
        match self {
            RetentionUnit::Days => 'd',
            RetentionUnit::Hours => 'h',
            RetentionUnit::Seconds => 's',
        }
    }

    fn secs(self) -> u64 {
        match self {
            RetentionUnit::Days => 86_400,
            RetentionUnit::Hours => 3_600,
            RetentionUnit::Seconds => 1,
        }
    }
}

/// A positive retention duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RetainedFor {
    pub count: u32,
    pub unit: RetentionUnit,
}

impl RetainedFor {
    pub fn as_secs(&self) -> u64 {
        u64::from(self.count) * self.unit.secs()
    }
}

/// Per-channel retention policy (§5.2). `E2ee` makes server-readable
/// plaintext unrepresentable by construction (§14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetentionPolicy {
    Ephemeral,
    Retained(RetainedFor),
    Permanent,
    E2ee,
}

impl RetentionPolicy {
    /// §5.2 strictness rank: `e2ee > ephemeral > retained > permanent`;
    /// among `retained`, shorter is stricter.
    fn rank(&self) -> u8 {
        match self {
            RetentionPolicy::E2ee => 3,
            RetentionPolicy::Ephemeral => 2,
            RetentionPolicy::Retained(_) => 1,
            RetentionPolicy::Permanent => 0,
        }
    }

    /// `Greater` means `self` is stricter than `other`.
    pub fn cmp_strictness(&self, other: &Self) -> Ordering {
        match (self, other) {
            (RetentionPolicy::Retained(a), RetentionPolicy::Retained(b)) => {
                b.as_secs().cmp(&a.as_secs()) // shorter retention = stricter
            }
            _ => self.rank().cmp(&other.rank()),
        }
    }

    /// Strictest-policy negotiation for bridges (§5.2).
    pub fn strictest(a: Self, b: Self) -> Self {
        if a.cmp_strictness(&b) == Ordering::Less {
            b
        } else {
            a
        }
    }
}

impl FromStr for RetentionPolicy {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let bad = || ParseError::Invalid {
            what: "retention policy",
            value: s.to_string(),
        };
        let folded = s.to_ascii_lowercase();
        match folded.as_str() {
            "ephemeral" => Ok(RetentionPolicy::Ephemeral),
            "permanent" => Ok(RetentionPolicy::Permanent),
            "e2ee" => Ok(RetentionPolicy::E2ee),
            _ => {
                let spec = folded.strip_prefix("retained:").ok_or_else(bad)?;
                // Split on the last *char* boundary, not the last byte: a
                // multibyte trailing char (e.g. "3û") would make `split_at`
                // panic on a non-char-boundary index (fuzz: parse_reply).
                let unit_char = spec.chars().next_back().ok_or_else(bad)?;
                let digits = &spec[..spec.len() - unit_char.len_utf8()];
                let unit = match unit_char {
                    'd' => RetentionUnit::Days,
                    'h' => RetentionUnit::Hours,
                    's' => RetentionUnit::Seconds,
                    _ => return Err(bad()),
                };
                let count: u32 = digits.parse().map_err(|_| bad())?;
                if count == 0 {
                    return Err(bad()); // spec: n > 0
                }
                Ok(RetentionPolicy::Retained(RetainedFor { count, unit }))
            }
        }
    }
}

impl fmt::Display for RetentionPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RetentionPolicy::Ephemeral => f.write_str("ephemeral"),
            RetentionPolicy::Permanent => f.write_str("permanent"),
            RetentionPolicy::E2ee => f.write_str("e2ee"),
            RetentionPolicy::Retained(r) => write!(f, "retained:{}{}", r.count, r.unit.suffix()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RetentionPolicy {
        s.parse().unwrap()
    }

    #[test]
    fn policies_round_trip() {
        for s in [
            "ephemeral",
            "retained:90d",
            "retained:12h",
            "retained:30s",
            "permanent",
            "e2ee",
        ] {
            assert_eq!(p(s).to_string(), s);
        }
    }

    #[test]
    fn rejects_malformed_policies() {
        for s in [
            "retained:0d",
            "retained:5",
            "retained:5w",
            "retained:",
            "forever",
            "",
        ] {
            assert!(s.parse::<RetentionPolicy>().is_err(), "accepted {s:?}");
        }
    }

    #[test]
    fn multibyte_trailing_char_does_not_panic() {
        // Regression (fuzz parse_reply): a multibyte trailing char put the
        // old byte-index `split_at` inside a UTF-8 char and panicked. These
        // must all reject cleanly, never panic.
        for s in [
            "retained:3û",
            "retained:û",
            "retained:12€",
            "retained:5\u{0}",
            "retained:naïve",
        ] {
            assert!(s.parse::<RetentionPolicy>().is_err(), "accepted {s:?}");
        }
    }

    #[test]
    fn strictest_negotiation_order() {
        // §5.2: e2ee > ephemeral > retained(shorter) > retained(longer) > permanent
        let order = [
            p("e2ee"),
            p("ephemeral"),
            p("retained:1h"),
            p("retained:90d"),
            p("permanent"),
        ];
        for (i, a) in order.iter().enumerate() {
            for b in &order[i + 1..] {
                assert_eq!(RetentionPolicy::strictest(*a, *b), *a, "{a} vs {b}");
                assert_eq!(RetentionPolicy::strictest(*b, *a), *a, "{b} vs {a}");
            }
        }
        // Equivalent durations in different units are equally strict.
        assert_eq!(
            p("retained:24h").cmp_strictness(&p("retained:1d")),
            Ordering::Equal
        );
    }
}
