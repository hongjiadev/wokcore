CREATE TABLE session_sources(
    source_key TEXT PRIMARY KEY,
    source_kind TEXT NOT NULL CHECK(source_kind IN ('codex', 'claude', 'gemini')),
    current_generation INTEGER CHECK(current_generation > 0),
    staging_generation INTEGER CHECK(staging_generation > 0),
    retired_generation INTEGER CHECK(retired_generation > 0),
    status TEXT NOT NULL CHECK(status IN ('undiscovered', 'available', 'stale', 'unavailable', 'resource_limited')),
    error_code TEXT,
    last_transition_at TEXT,
    CHECK(current_generation IS NULL OR current_generation != staging_generation),
    CHECK(current_generation IS NULL OR current_generation != retired_generation),
    CHECK(staging_generation IS NULL OR staging_generation != retired_generation)
);
CREATE TABLE session_scan_cursors(
    source_key TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    source_kind TEXT NOT NULL CHECK(source_kind IN ('codex', 'claude', 'gemini')),
    generation_state TEXT NOT NULL CHECK(generation_state IN ('staging', 'current', 'retired')),
    file_identity TEXT NOT NULL,
    observed_size INTEGER NOT NULL CHECK(observed_size >= 0),
    modified_at TEXT NOT NULL,
    complete_byte_offset INTEGER NOT NULL CHECK(complete_byte_offset >= 0),
    stable_record_ordinal INTEGER NOT NULL CHECK(stable_record_ordinal >= 0),
    parser_checkpoint BLOB NOT NULL CHECK(length(parser_checkpoint) <= 65536),
    head_fingerprint BLOB NOT NULL CHECK(length(head_fingerprint) = 32),
    boundary_fingerprint BLOB NOT NULL CHECK(length(boundary_fingerprint) = 32),
    parent_source_key TEXT,
    parent_generation INTEGER CHECK(parent_generation > 0),
    replay_boundary_fingerprint BLOB CHECK(length(replay_boundary_fingerprint) = 32),
    result_code TEXT,
    result_changed_at TEXT,
    PRIMARY KEY(source_key, generation),
    FOREIGN KEY(source_key) REFERENCES session_sources(source_key) ON DELETE CASCADE,
    CHECK((parent_source_key IS NULL) = (parent_generation IS NULL)),
    CHECK((parent_source_key IS NULL) = (replay_boundary_fingerprint IS NULL))
);
CREATE TABLE session_index(
    session_key TEXT NOT NULL,
    source_key TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    source_kind TEXT NOT NULL CHECK(source_kind IN ('codex', 'claude', 'gemini')),
    created_at TEXT NOT NULL,
    last_active_at TEXT NOT NULL,
    message_count INTEGER NOT NULL CHECK(message_count >= 0),
    usage_event_count INTEGER NOT NULL CHECK(usage_event_count >= 0),
    availability TEXT NOT NULL CHECK(availability IN ('available', 'unavailable')),
    PRIMARY KEY(source_key, generation, session_key),
    FOREIGN KEY(source_key, generation) REFERENCES session_scan_cursors(source_key, generation) ON DELETE CASCADE
);
CREATE INDEX session_index_order ON session_index(last_active_at DESC, session_key);
CREATE TABLE session_usage_records(
    usage_id TEXT NOT NULL,
    session_key TEXT NOT NULL,
    source_key TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK(generation > 0),
    source_kind TEXT NOT NULL CHECK(source_kind IN ('codex', 'claude', 'gemini')),
    model TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    input_tokens INTEGER NOT NULL CHECK(input_tokens >= 0),
    output_tokens INTEGER NOT NULL CHECK(output_tokens >= 0),
    cache_read_tokens INTEGER NOT NULL CHECK(cache_read_tokens >= 0),
    cache_write_tokens INTEGER NOT NULL CHECK(cache_write_tokens >= 0),
    reasoning_tokens INTEGER NOT NULL CHECK(reasoning_tokens >= 0),
    record_revision INTEGER NOT NULL CHECK(record_revision > 0),
    PRIMARY KEY(source_key, generation, usage_id),
    FOREIGN KEY(source_key, generation) REFERENCES session_scan_cursors(source_key, generation) ON DELETE CASCADE
);
CREATE INDEX session_usage_order ON session_usage_records(occurred_at, usage_id);
CREATE TABLE codex_replay_signatures(
    parent_source_key TEXT NOT NULL,
    parent_generation INTEGER NOT NULL CHECK(parent_generation > 0),
    token_event_ordinal INTEGER NOT NULL CHECK(token_event_ordinal > 0),
    occurred_at TEXT NOT NULL,
    signature_hash BLOB NOT NULL CHECK(length(signature_hash) = 32),
    PRIMARY KEY(parent_source_key, parent_generation, token_event_ordinal),
    FOREIGN KEY(parent_source_key, parent_generation) REFERENCES session_scan_cursors(source_key, generation) ON DELETE CASCADE
);
CREATE TABLE request_supplemental_metadata(
    request_id TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    trace_id TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    route_fingerprint TEXT NOT NULL,
    provider_fingerprint TEXT NOT NULL,
    account_fingerprint TEXT,
    retry_decision TEXT NOT NULL,
    failover_decision TEXT NOT NULL,
    queue_ms INTEGER NOT NULL CHECK(queue_ms >= 0),
    connect_ms INTEGER NOT NULL CHECK(connect_ms >= 0),
    first_byte_ms INTEGER NOT NULL CHECK(first_byte_ms >= 0),
    total_ms INTEGER NOT NULL CHECK(total_ms >= 0),
    request_bytes INTEGER NOT NULL CHECK(request_bytes >= 0),
    response_bytes INTEGER NOT NULL CHECK(response_bytes >= 0),
    status_code INTEGER,
    error_code TEXT,
    logical_bytes INTEGER NOT NULL CHECK(logical_bytes >= 0 AND logical_bytes <= 2048),
    PRIMARY KEY(request_id, attempt_id)
);
CREATE INDEX request_supplemental_retention ON request_supplemental_metadata(occurred_at, request_id, attempt_id);
CREATE TABLE client_token_scopes(
    token_id TEXT NOT NULL,
    scope TEXT NOT NULL CHECK(scope IN ('proxy.use', 'sessions.read', 'usage.read', 'diagnostics.read', 'diagnostics.export')),
    PRIMARY KEY(token_id, scope),
    FOREIGN KEY(token_id) REFERENCES client_tokens(token_id) ON DELETE CASCADE
);
INSERT INTO client_token_scopes(token_id, scope)
SELECT token_id, 'proxy.use' FROM client_tokens;
INSERT INTO schema_migrations(version, applied_at) VALUES (3, datetime('now'));
