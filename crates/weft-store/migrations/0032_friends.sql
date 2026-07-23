-- Social graph: friends + pending requests (social layer). Federation-able —
-- each side is a full `account@network` UserRef, so a relationship may cross
-- networks. Symmetric: one row per unordered pair, keyed by the two sides in
-- lexicographic order, with `requested_by` recording who sent the request.
CREATE TABLE weft_friends (
    low          TEXT    NOT NULL, -- lexicographically smaller UserRef (account@net)
    high         TEXT    NOT NULL, -- larger UserRef
    requested_by TEXT    NOT NULL, -- 'low' or 'high' — which side asked
    accepted     BOOLEAN NOT NULL, -- false = pending request, true = mutual friends
    since_ms     BIGINT  NOT NULL, -- request time (or accept time once accepted)
    PRIMARY KEY (low, high)
);

-- Both directions are queried ("who are my friends / who asked me").
CREATE INDEX weft_friends_low ON weft_friends (low);
CREATE INDEX weft_friends_high ON weft_friends (high);
