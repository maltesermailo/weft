-- §6.5 explicit role membership: who holds which role at which scope.
CREATE TABLE weft_role_assignments (
    scope   TEXT NOT NULL,
    name    TEXT NOT NULL,
    account TEXT NOT NULL,
    PRIMARY KEY (scope, name, account)
);
CREATE INDEX weft_role_assignments_acct ON weft_role_assignments (scope, account);
