-- §9.4 amendment: thread display names. A thread is a view keyed by its root
-- msgid (no separate identity), so a name is channel-scoped metadata keyed by
-- (scope, root). Reply counts and last-activity are derived from weft_events;
-- only the name needs storage.
CREATE TABLE weft_thread_names (
    scope   TEXT   NOT NULL, -- Scope::as_key ('#chan' / 'ns:x/chan')
    root    TEXT   NOT NULL, -- root message ULID (msgid.ulid)
    name    TEXT   NOT NULL,
    set_by  TEXT   NOT NULL, -- account@network that last named it (audit)
    set_at  BIGINT NOT NULL, -- unix ms
    PRIMARY KEY (scope, root)
);
