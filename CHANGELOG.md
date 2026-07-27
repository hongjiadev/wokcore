# Changelog

All notable WokCore changes are documented in this file.

## [Unreleased]

### Added

- Internal `wokcore-core` domain types migrated from the recorded WokRouter source revision.
- Internal canonical IR, bounded SSE primitives, Provider protocol codecs, and offline fixtures migrated from the recorded WokRouter source revision.
- Internal `wokcore-storage` configuration storage with a loopback-only, non-zero port contract and revision-checked atomic commits.
- Internal memory, native credential, environment, and permissioned-file secret stores with explicit read-only headless configuration and a 64 KiB input limit.
- Internal SQLite durable state storage with batch-only request metrics, metadata and token totals only, corruption-preserving initial migration, and orphan-secret recovery metadata.
- Explicit WAL byte measurement, threshold-gated passive checkpointing at the architecture constant of 16 MiB, and idle-time truncate checkpoint primitives with automatic checkpointing disabled.
- Internal `wokcore-platform` path discovery with deterministic environment snapshots, WokCore-owned OS directories, and discovery/instance-lock path values that have no filesystem side effects.
- Secure per-user runtime ownership with a lease-scoped operating-system lock and bounded, token-free, atomically published loopback discovery.
- Ordered SQLite schema 2 authentication metadata, distinct client identifiers, redacted split-scope token primitives, fail-closed management bootstrap, and immutable in-memory proxy-token validation.
- Unlimited lock-free request admission tracking with explicit drain, cancellation, timeout, and graceful stopping lifecycle states.
- Secure IPv4-loopback HTTP control plane with exact authority/origin enforcement, fresh request IDs, management-first authentication, bounded JSON, versioned capabilities, graceful stop, and an OpenAPI 3.1 contract.
- Local `serve`, `status`, `stop`, `doctor`, and JSON-only `authorize` commands with stable exit/result codes, fixed-port fail-closed startup, exact loopback identity verification, injected runtime seams, read-only diagnostics, and ownership-conditional discovery cleanup.
- Ordered SQLite schema 3 Session state with hidden staged generations, atomic promotion, bounded cursor/index/usage/replay batches, exact client-token scopes, and content-free supplemental request metadata with explicit capacity and retention cleanup.
- Read-only Codex Session discovery and bounded JSONL ingestion with byte cursors, cumulative usage reconstruction, durable fork-replay signatures, automatic UTC timestamp normalization, and immutable title metadata fallbacks.
- Automatic read-only Session indexing for Codex, Claude Code, and Gemini CLI, including bounded source scanners, cross-platform discovery, source-health aggregation, and direct paged message reads.
- Bounded diagnostic memory ring, batched durable segments, drop summaries, causal snapshots, retention, privacy scanning, and validated streamed support-package export.
- Authenticated Session list/message, usage, diagnostic-log, and diagnostic-export APIs with exact client scopes, opaque pagination cursors, bounded query workers, response byte limits, and OpenAPI 3.1 schemas.
- Terminal-safe Session/log CLI output, JSON/JSONL modes, repeatable authorization scopes, and create-new diagnostic ZIP export protected from Session-root aliasing.
- Typed request diagnostics correlated by response request ID; ordinary request events stay memory-only while internal failures remain durable warning candidates.

### Fixed

- Session state now enforces monotonic same-generation appends, immutable same-position parser checkpoints, complete current-cursor reloads, lineage-safe resume, externally rebuildable validated page keys, source-derived effective availability, source-driven current-generation paging that ignores hidden-generation volume, transactional generation compare-and-swap updates, typed supplemental drop outcomes, exact cleanup byte budgets, and three-batch interruption coverage.
- Codex Session scans isolate malformed or resource-limited sources, preserve the last promoted generation, detect same-identity rewrites at the committed cursor boundary, resume interrupted candidates across appends and live-to-archive moves, and perform unchanged scans without durable writes.
- Live diagnostic queries and exports now tolerate the writer-owned empty active segment by using the complete in-memory ring copy while continuing to fail closed for any older unreadable segment.
- Internal diagnostic-export temporary directories are excluded from event enumeration and cannot be created through the public diagnostic-file API.
- Windows workspace tests now execute through one fixed `wokcore-test-host.exe` path, preventing loopback-listener tests from presenting a new hash-named program identity after each build.
- Configuration loading now rejects every field outside top-level `revision`/`server` and nested `server.port`, while preserving invalid source files.
- Protocol channels, SSE frame aggregation, and Azure/Gemini event aggregation now fail closed at configured memory and event bounds.
- OpenAI Responses streaming and non-stream aggregation now bound retained output, identifiers, output items, and serialized context and usage values.
- Corrected the migrated WokRouter source MIT notice scope to cover domain types, protocol source, and fixtures while retaining the OpenCodex attribution.
- Secret-storage tests now recursively follow complete production and test module graphs, including external `#[path]` and test `cfg_attr` path overrides, and structurally confine native credential access to the exact production backend boundary and public re-exports; they also prove bounded reads and error-path zeroization, validate exact size boundaries, and exercise explicit Windows protected-DACL success and rejection paths.

## [0.1.0] - 2026-07-24

### Added

- Independent Rust workspace and `wokcore` executable.
- `wokcore --version`.
- Five-target CI, dependency policy, public-repository hygiene, governance, and migration provenance.
