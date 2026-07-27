PRAGMA secure_delete = ON;
DROP TABLE thread_affinities;
DROP TABLE quota_windows;
PRAGMA secure_delete = OFF;

CREATE TABLE provider_runtime_metadata(
    provider_id TEXT PRIMARY KEY NOT NULL,
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0)
) STRICT;

CREATE TABLE account_runtime_metadata(
    account_id TEXT PRIMARY KEY NOT NULL,
    provider_id TEXT NOT NULL,
    health_state TEXT NOT NULL CHECK(health_state IN ('healthy', 'cooling_down', 'quarantined')),
    consecutive_failures INTEGER NOT NULL CHECK(consecutive_failures BETWEEN 0 AND 64),
    cooldown_until_ms INTEGER CHECK(cooldown_until_ms >= 0),
    quota_remaining INTEGER CHECK(quota_remaining >= 0),
    quota_resets_at_ms INTEGER CHECK(quota_resets_at_ms >= 0),
    selection_count INTEGER NOT NULL CHECK(selection_count >= 0),
    last_selected_sequence INTEGER NOT NULL CHECK(last_selected_sequence >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0),
    CHECK(
        (health_state = 'cooling_down' AND cooldown_until_ms IS NOT NULL)
        OR (health_state != 'cooling_down' AND cooldown_until_ms IS NULL)
    ),
    CHECK(
        (quota_remaining IS NULL AND quota_resets_at_ms IS NULL)
        OR (quota_remaining IS NOT NULL AND quota_resets_at_ms IS NOT NULL)
    ),
    FOREIGN KEY(provider_id) REFERENCES provider_runtime_metadata(provider_id) ON DELETE CASCADE
) STRICT;

CREATE INDEX account_runtime_metadata_provider
ON account_runtime_metadata(provider_id, account_id);

INSERT INTO schema_migrations(version, applied_at) VALUES (4, datetime('now'));
