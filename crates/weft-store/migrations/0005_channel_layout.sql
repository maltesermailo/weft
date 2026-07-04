-- Channel layout within a namespace (Discord-style categories + order).
ALTER TABLE weft_channels ADD COLUMN category TEXT;
ALTER TABLE weft_channels ADD COLUMN position BIGINT NOT NULL DEFAULT 0;
