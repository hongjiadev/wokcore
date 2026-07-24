# Changelog

All notable WokCore changes are documented in this file.

## [Unreleased]

### Added

- Internal `wokcore-core` domain types migrated from the recorded WokRouter source revision.
- Internal canonical IR, bounded SSE primitives, Provider protocol codecs, and offline fixtures migrated from the recorded WokRouter source revision.
- Internal `wokcore-storage` configuration storage with a loopback-only, non-zero port contract and revision-checked atomic commits.

### Fixed

- Configuration loading now rejects every field outside top-level `revision`/`server` and nested `server.port`, while preserving invalid source files.
- Protocol channels, SSE frame aggregation, and Azure/Gemini event aggregation now fail closed at configured memory and event bounds.
- OpenAI Responses streaming and non-stream aggregation now bound retained output, identifiers, output items, and serialized context and usage values.
- Corrected the migrated WokRouter source MIT notice scope to cover domain types, protocol source, and fixtures while retaining the OpenCodex attribution.

## [0.1.0] - 2026-07-24

### Added

- Independent Rust workspace and `wokcore` executable.
- `wokcore --version`.
- Five-target CI, dependency policy, public-repository hygiene, governance, and migration provenance.
