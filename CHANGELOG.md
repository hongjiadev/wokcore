# Changelog

All notable WokCore changes are documented in this file.

## [Unreleased]

### Added

- Internal `wokcore-core` domain types migrated from the recorded WokRouter source revision.
- Internal canonical IR, bounded SSE primitives, Provider protocol codecs, and offline fixtures migrated from the recorded WokRouter source revision.

### Fixed

- Protocol channels, SSE frame aggregation, and Azure/Gemini event aggregation now fail closed at configured memory and event bounds.
- Restored the migrated WokRouter source MIT notice and documented its relationship to WokCore's dual-license terms.

## [0.1.0] - 2026-07-24

### Added

- Independent Rust workspace and `wokcore` executable.
- `wokcore --version`.
- Five-target CI, dependency policy, public-repository hygiene, governance, and migration provenance.
