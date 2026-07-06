-- §6.6 role definitions: named, colored capability-token bundles per scope.
CREATE TABLE weft_roles (
    scope TEXT NOT NULL,
    name  TEXT NOT NULL,
    color TEXT NOT NULL,
    caps  TEXT NOT NULL,  -- comma-separated capability list
    PRIMARY KEY (scope, name)
);
CREATE INDEX weft_roles_scope ON weft_roles (scope);
