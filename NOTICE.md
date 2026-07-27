# Notices

WokCore is licensed under either the MIT License or the Apache License, Version 2.0, at your option.

Architecture and compatibility research referenced:

- OpenCodex — MIT — https://github.com/lidge-jun/opencodex
- CC-Switch — MIT — https://github.com/farion1231/cc-switch
- Cockpit Tools — CC BY-NC-SA 4.0 — https://github.com/jlcodes99/cockpit-tools

Cockpit Tools is a design reference only. No Cockpit Tools source code is included.

The bundled Provider identifiers, labels, adapter/authentication families, default endpoints, and static model seeds in `crates/wokcore-engine/provider-catalog/providers.toml` are adapted from OpenCodex v2.7.35 commit `97e7326f89bcfbb29a2c73250cb25eb801d066b6`. The validation and runtime catalog implementation are original WokCore code.

The adapted Provider catalog retains the OpenCodex MIT notice:

Copyright (c) 2026 opencodex contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

The Codex Session discovery, incremental parser, usage reconstruction, and fork-replay index in `crates/wokcore-sessions` are original WokCore code. CC-Switch informed the read-only Session-first compatibility research; no CC-Switch source code is included.

The initial WokCore domain types were migrated from WokRouter commit `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`. The source-path mapping and deliberate adaptations are recorded in `MIGRATION.md`.

The initial WokCore protocol substrate and fixtures were migrated from WokRouter commit `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`. Package renames and the exact source mapping are recorded in `MIGRATION.md`.

The WokCore data-plane protocol registry is adapted from WokRouter commit `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`. The exact source path, blob, public-codec boundary, bounded canonical validation, and deliberate omissions are recorded in `MIGRATION.md`.

The initial WokCore configuration storage was migrated from WokRouter commit `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`. Its exact source-path and blob mapping, package rename, removed UI/LAN fields, validation adaptation, and retained atomic-write behavior are recorded in `MIGRATION.md`.

The initial WokCore secret-storage backends were migrated from WokRouter commit `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`. Their exact source-path and blob mapping, WokCore native service identity, blocking keyring boundary, 64 KiB headless input limit, fail-closed permission adaptations, and structural offline native-credential test gate are recorded in `MIGRATION.md`.

The initial WokCore durable state storage was migrated from WokRouter commit `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`. Its exact source-path and blob mapping, batch-only request metrics, metadata-only schema, disabled automatic WAL checkpointing, 16 MiB passive threshold primitive, explicit truncate checkpoint, and corruption-preservation behavior are recorded in `MIGRATION.md`.

The initial WokCore platform path discovery was adapted from WokRouter commit `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`. Its exact source path and blob mapping, product-directory rename, pure environment-snapshot resolver, discovery/lock path values, and excluded locale/service modules are recorded in `MIGRATION.md`.

The WokCore secure runtime ownership and discovery implementation adapts the atomic-file and opened-handle permission-verification patterns from WokRouter commit `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`. Exact source paths and blobs, retained behavior, and deliberate omissions are recorded in `MIGRATION.md`.

The WokCore split-scope token, authentication metadata, management bootstrap, and immutable in-memory registry implementation is original WokCore code. Its security boundaries and the distinction from migrated WokRouter source are recorded in `MIGRATION.md`.

The migrated WokRouter domain types, protocol source, and fixture files, configuration storage, secret storage, durable state storage, platform path discovery, and secure runtime filesystem patterns retain the source MIT notice:

Copyright (c) 2026 WokRouter contributors

The accompanying MIT permission notice and warranty disclaimer are preserved in `LICENSE-MIT`. Those migrated files continue to be covered by the source MIT notice and are distributed in WokCore under this repository's `MIT OR Apache-2.0` dual-license terms.
