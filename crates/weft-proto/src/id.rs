//! Message IDs (spec §5.1): `msgid = <origin-network>/<ULID>`.
//!
//! ULIDs are minted ONLY by the origin channel actor (single writer =
//! per-channel total order); this module only parses and formats.

use std::fmt;
use std::str::FromStr;

use ulid::Ulid;

use crate::error::ParseError;
use crate::name::NetworkName;

/// Origin-scoped message ID. Ordering is `(origin, ulid)`, so IDs from the
/// same origin sort in timestamp order; cross-origin order is not meaningful.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MsgId {
    origin: NetworkName,
    ulid: Ulid,
}

impl MsgId {
    pub fn new(origin: NetworkName, ulid: Ulid) -> Self {
        Self { origin, ulid }
    }

    pub fn origin(&self) -> &NetworkName {
        &self.origin
    }

    pub fn ulid(&self) -> Ulid {
        self.ulid
    }

    /// Milliseconds since epoch, from the ULID (server-stamped time, §9.6).
    pub fn timestamp_ms(&self) -> u64 {
        self.ulid.timestamp_ms()
    }
}

impl FromStr for MsgId {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let bad = || ParseError::Invalid {
            what: "msgid",
            value: s.to_string(),
        };
        let (origin, ulid) = s.split_once('/').ok_or_else(bad)?;
        Ok(MsgId {
            origin: origin.parse().map_err(|_| bad())?,
            ulid: Ulid::from_string(ulid).map_err(|_| bad())?,
        })
    }
}

impl fmt::Display for MsgId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Ulid's Display is canonical uppercase Crockford base32.
        write!(f, "{}/{}", self.origin, self.ulid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ULID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    #[test]
    fn msgid_round_trips() {
        let id: MsgId = format!("hda.example/{ULID}").parse().unwrap();
        assert_eq!(id.origin().as_str(), "hda.example");
        assert_eq!(id.to_string(), format!("hda.example/{ULID}"));
    }

    #[test]
    fn msgid_rejects_malformed() {
        assert!("no-slash".parse::<MsgId>().is_err());
        assert!("hda.example/NOT-A-ULID".parse::<MsgId>().is_err());
        assert!(format!("bad host!/{ULID}").parse::<MsgId>().is_err());
    }

    #[test]
    fn same_origin_ids_sort_by_time() {
        let older: MsgId = format!("n.example/{}", Ulid::from_parts(1_000, 0))
            .parse()
            .unwrap();
        let newer: MsgId = format!("n.example/{}", Ulid::from_parts(2_000, 0))
            .parse()
            .unwrap();
        assert!(older < newer);
        assert_eq!(older.timestamp_ms(), 1_000);
    }
}
