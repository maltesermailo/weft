-- Verification infrastructure: claims on accounts ("email", "age", ...).
-- The verifiers (SMTP flow, ID provider, admin panel) and the wire
-- protocol for proving a claim are later work; this is the substrate.
CREATE TABLE weft_verifications (
    account     TEXT   NOT NULL REFERENCES weft_accounts (name) ON DELETE CASCADE,
    kind        TEXT   NOT NULL,
    subject     TEXT   NOT NULL,
    verified_at BIGINT,          -- NULL = pending
    PRIMARY KEY (account, kind)
);
