-- §11 federation: bridge peerings (with signed manifests) + operator netblocks.

-- One row per peer network. The manifest blobs are opaque base64
-- SignedManifest (weft-crypto); weft-core gates forwarding on the
-- acked-vs-current channel intersection (invariant 3).
CREATE TABLE weft_peers (
    peer            TEXT    PRIMARY KEY,          -- remote network name
    scope           TEXT    NOT NULL,             -- original PROPOSE scope
    manifest        TEXT    NOT NULL,             -- current signed manifest (b64)
    version         BIGINT  NOT NULL,
    acked_manifest  TEXT,                         -- last mutually-acked (b64), NULL until ACCEPT
    severed         BOOLEAN NOT NULL DEFAULT FALSE,
    created_ms      BIGINT  NOT NULL,
    updated_ms      BIGINT  NOT NULL
);

-- Name-keyed operator blocklist (§11.6): the block is on the network name,
-- so key rotation never evades it (invariant 7).
CREATE TABLE weft_netblocks (
    network   TEXT   PRIMARY KEY,
    reason    TEXT,
    added_ms  BIGINT NOT NULL,
    actor     TEXT   NOT NULL
);
