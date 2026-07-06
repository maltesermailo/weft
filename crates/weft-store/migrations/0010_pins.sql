-- §6.4 pinned messages, per channel. Content is fetched from weft_events.
CREATE TABLE weft_pins (
    channel TEXT NOT NULL,
    msgid   TEXT NOT NULL,
    ulid    TEXT NOT NULL,
    PRIMARY KEY (channel, msgid)
);
CREATE INDEX weft_pins_order ON weft_pins (channel, ulid);
