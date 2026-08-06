-- Outbound projection opt-ins (matrix.md §17.1): the schemes a **native**
-- namespace is mirrored into (`NS META <ns> bridge:<scheme> :open`), one row
-- per opt-in.
--
-- The flag doubles as the return-path authorization anchor: only a provider
-- whose pinned scheme is listed may attribute foreign users into the
-- namespace's channels (and the home mints). No rows = not projected.
--
-- The parent table is keyed by the mutable vanity name (v0.13), so the FK
-- follows renames and deletion tears the opt-ins down with the namespace.
CREATE TABLE weft_namespace_bridges (
    namespace TEXT NOT NULL REFERENCES weft_namespaces (name)
        ON DELETE CASCADE ON UPDATE CASCADE,
    scheme    TEXT NOT NULL,
    PRIMARY KEY (namespace, scheme)
);
