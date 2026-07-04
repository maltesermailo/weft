-- M4b: user-owned namespaces (§2.1, §2.2).
CREATE TABLE weft_namespaces (
    name        TEXT PRIMARY KEY,
    owner       TEXT NOT NULL,
    root_key    TEXT NOT NULL,   -- b64 ed25519 root pubkey (client-generated)
    visibility  TEXT NOT NULL,   -- public | unlisted | private
    title       TEXT,
    description TEXT,
    icon        TEXT
);
CREATE INDEX weft_namespaces_owner ON weft_namespaces (owner);
CREATE INDEX weft_namespaces_public ON weft_namespaces (name) WHERE visibility = 'public';
