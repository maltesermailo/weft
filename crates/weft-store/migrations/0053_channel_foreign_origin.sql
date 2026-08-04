-- Foreign-bridge framework (§7a.2): the foreign room a channel replicates
-- (`<scheme>://<realm>/<space>/<segment>`). NULL = native channel. Set at
-- materialization; surfaced as `origin=` on CHANNEL-LAYOUT.
ALTER TABLE weft_channels ADD COLUMN origin TEXT;
