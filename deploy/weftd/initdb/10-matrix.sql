-- Databases for the `matrix` profile (docker-compose.yml). Runs ONLY on a fresh
-- Postgres data volume; for an existing one, MATRIX.md has the equivalent psql.
--
-- Harmless when the bridge is not deployed: two unused databases.

-- Synapse refuses to start on a database whose collation is not C (or POSTGRES):
-- its ordering assumptions depend on it, and a locale-aware collation silently
-- corrupts state-group comparisons. POSTGRES_DB cannot express this — it
-- inherits the cluster default — so the database is created here instead.
--
-- Change the password to match homeserver.yaml's database.args.password.
CREATE ROLE synapse WITH LOGIN PASSWORD 'change-me-synapse-postgres-password';

CREATE DATABASE synapse
    OWNER synapse
    ENCODING 'UTF8'
    LC_COLLATE 'C'
    LC_CTYPE 'C'
    TEMPLATE template0;

-- The bridge daemon's own store. Its tables are `matrix_`-prefixed, so weftd's
-- database would work too; a separate one is the default so that removing the
-- bridge is `DROP DATABASE weftmatrix` and nothing else.
CREATE DATABASE weftmatrix OWNER weft;
