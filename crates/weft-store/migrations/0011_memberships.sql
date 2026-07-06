-- §6.3 persistent channel membership (auto-rejoin on auth).
CREATE TABLE weft_memberships (
    account TEXT NOT NULL,
    channel TEXT NOT NULL,
    PRIMARY KEY (account, channel)
);
CREATE INDEX weft_memberships_acct ON weft_memberships (account);
