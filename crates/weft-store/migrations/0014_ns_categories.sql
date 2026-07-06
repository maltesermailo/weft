-- Ordered channel categories per namespace (empty categories persist here).
ALTER TABLE weft_namespaces ADD COLUMN categories TEXT NOT NULL DEFAULT '';
