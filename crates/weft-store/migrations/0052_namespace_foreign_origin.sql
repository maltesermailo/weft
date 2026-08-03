-- Foreign-bridge framework (§3): mark a namespace as a replica of a foreign
-- space by its origin URI (`<scheme>://<realm>/<space>`). NULL = native
-- namespace. Reuse + marker (not a parallel foreign table); a partial index
-- keeps the `NS JOIN <uri>` known-local lookup fast.
ALTER TABLE weft_namespaces ADD COLUMN origin TEXT;
CREATE INDEX weft_namespaces_origin_idx ON weft_namespaces (origin) WHERE origin IS NOT NULL;
