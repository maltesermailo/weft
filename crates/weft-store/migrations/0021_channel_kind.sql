-- §16 WEFT-RT: a channel's kind (text | voice). Voice channels are voice-only
-- rooms, advertised separately and invisible to the IRC gateway. Set at
-- creation, immutable after. Existing channels default to text.
ALTER TABLE weft_channels ADD COLUMN kind TEXT NOT NULL DEFAULT 'text';
