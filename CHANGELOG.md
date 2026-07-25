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

### Fixed

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
