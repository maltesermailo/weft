-- §2.4 namespace recovery ladder: quorum, pending recovery, root-history.
ALTER TABLE weft_namespaces ADD COLUMN recovery_m       INTEGER;
ALTER TABLE weft_namespaces ADD COLUMN recovery_keys    TEXT;     -- comma-joined b64 pubkeys
ALTER TABLE weft_namespaces ADD COLUMN pending_root_key TEXT;
ALTER TABLE weft_namespaces ADD COLUMN pending_owner    TEXT;
ALTER TABLE weft_namespaces ADD COLUMN pending_eta_ms   BIGINT;
ALTER TABLE weft_namespaces ADD COLUMN pending_rung     SMALLINT;

CREATE TABLE weft_root_history (
    namespace          TEXT   NOT NULL,
    at_ms              BIGINT NOT NULL,
    root_key           TEXT   NOT NULL,
    owner              TEXT   NOT NULL,
    operator_initiated BOOLEAN NOT NULL,
    PRIMARY KEY (namespace, at_ms)
);
