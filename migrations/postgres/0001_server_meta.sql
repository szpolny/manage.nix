-- Key/value settings owned by the server (instance id, schema notes, ...).
CREATE TABLE server_meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
