-- M4: user accounts and their login sessions, and indexes for machines'
-- deployment history.

CREATE TABLE users (
    id                  TEXT PRIMARY KEY NOT NULL,
    organization_id     TEXT NOT NULL REFERENCES organizations (id),
    username            TEXT NOT NULL,
    -- An Argon2id PHC string.
    password_hash       TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    password_changed_at TEXT NOT NULL,
    UNIQUE (organization_id, username)
);

CREATE TABLE user_sessions (
    id         TEXT PRIMARY KEY NOT NULL,
    user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- SHA-256 (hex) of the session cookie; the cookie itself is not stored.
    token_hash TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);

CREATE INDEX user_sessions_user ON user_sessions (user_id);
CREATE INDEX user_sessions_expires ON user_sessions (expires_at);

CREATE INDEX deployment_machines_machine ON deployment_machines (machine_id);
CREATE INDEX deployment_machines_store_path ON deployment_machines (store_path);
