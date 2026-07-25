CREATE TABLE runtime_secret_bindings(
    binding_name TEXT PRIMARY KEY,
    secret_ref TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision = 1),
    created_at TEXT NOT NULL
);
CREATE TABLE client_tokens(
    token_id TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    token_digest BLOB NOT NULL UNIQUE CHECK(length(token_digest) = 32),
    issued_at TEXT NOT NULL,
    revoked_at TEXT
);
INSERT INTO schema_migrations(version, applied_at) VALUES (2, datetime('now'));
