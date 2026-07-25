PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS schema_migrations(version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
CREATE TABLE accounts(id TEXT PRIMARY KEY, provider_id TEXT NOT NULL, display_name TEXT NOT NULL, secret_ref TEXT, auth_state TEXT NOT NULL);
CREATE TABLE quota_windows(account_id TEXT NOT NULL, kind TEXT NOT NULL, used REAL NOT NULL, resets_at TEXT, PRIMARY KEY(account_id, kind));
CREATE TABLE thread_affinities(thread_key TEXT PRIMARY KEY, account_id TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE request_metrics(request_id TEXT PRIMARY KEY, provider_id TEXT NOT NULL, model TEXT NOT NULL, started_at TEXT NOT NULL, latency_ms INTEGER NOT NULL, input_tokens INTEGER, output_tokens INTEGER, status_code INTEGER NOT NULL, error_code TEXT);
CREATE TABLE orphan_secrets(secret_ref TEXT PRIMARY KEY, created_at TEXT NOT NULL);
INSERT INTO schema_migrations(version, applied_at) VALUES (1, datetime('now'));
