-- Synapse's database. Runs ONLY on a fresh Postgres data volume; for an existing
-- one, README.md has the equivalent psql.
--
-- The daemon's own database is NOT here: `POSTGRES_DB: weftmatrix` in
-- docker-compose.yml already creates it, and creating it twice would fail this
-- script and abort the whole init.
--
-- Synapse refuses to start on a database whose collation is not C (or POSIX): its
-- ordering assumptions depend on it, and a locale-aware collation silently corrupts
-- state-group comparisons. POSTGRES_DB cannot express a collation — it inherits the
-- cluster default — which is the only reason this file exists.
--
-- Change the password to match homeserver.yaml's `database.args.password`. Synapse
-- has no environment expansion, so unlike the daemon's it cannot come from `.env`.
CREATE ROLE synapse WITH LOGIN PASSWORD 'change-me-synapse-postgres-password';

CREATE DATABASE synapse
    OWNER synapse
    ENCODING 'UTF8'
    LC_COLLATE 'C'
    LC_CTYPE 'C'
    TEMPLATE template0;
