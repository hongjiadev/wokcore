CREATE TABLE client_token_scopes_v5(
    token_id TEXT NOT NULL,
    scope TEXT NOT NULL CHECK(scope IN (
        'proxy.use',
        'sessions.read',
        'usage.read',
        'diagnostics.read',
        'diagnostics.export',
        'service.read',
        'service.control',
        'providers.read',
        'providers.write',
        'clients.manage'
    )),
    PRIMARY KEY(token_id, scope),
    FOREIGN KEY(token_id) REFERENCES client_tokens(token_id) ON DELETE CASCADE
) STRICT;

INSERT INTO client_token_scopes_v5(token_id, scope)
SELECT token_id, scope FROM client_token_scopes;

DROP TABLE client_token_scopes;
ALTER TABLE client_token_scopes_v5 RENAME TO client_token_scopes;

INSERT INTO schema_migrations(version, applied_at) VALUES (5, datetime('now'));
