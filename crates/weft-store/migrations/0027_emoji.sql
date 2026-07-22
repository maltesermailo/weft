-- §9.4 custom (per-namespace) emoji: a shortcode → media reference. The image
-- bytes live in the blob store; this only maps `:name:` to a weft-media URI.
CREATE TABLE weft_emoji (
    namespace TEXT NOT NULL,
    name      TEXT NOT NULL,
    media     TEXT NOT NULL,
    PRIMARY KEY (namespace, name)
);
