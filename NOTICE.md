# Notices

WokCore is licensed under either the MIT License or the Apache License, Version 2.0, at your option.

Architecture and compatibility research referenced:

- OpenCodex — MIT — https://github.com/lidge-jun/opencodex
- CC-Switch — MIT — https://github.com/farion1231/cc-switch
- Cockpit Tools — CC BY-NC-SA 4.0 — https://github.com/jlcodes99/cockpit-tools

Cockpit Tools is a design reference only. No Cockpit Tools source code is included.

The initial WokCore domain types were migrated from WokRouter commit `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`. The source-path mapping and deliberate adaptations are recorded in `MIGRATION.md`.

The initial WokCore protocol substrate and fixtures were migrated from WokRouter commit `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`. Package renames and the exact source mapping are recorded in `MIGRATION.md`.

The initial WokCore configuration storage was migrated from WokRouter commit `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`. Its exact source-path and blob mapping, package rename, removed UI/LAN fields, validation adaptation, and retained atomic-write behavior are recorded in `MIGRATION.md`.

The initial WokCore secret-storage backends were migrated from WokRouter commit `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`. Their exact source-path and blob mapping, WokCore native service identity, blocking keyring boundary, 64 KiB headless input limit, and fail-closed permission adaptations are recorded in `MIGRATION.md`.

The migrated WokRouter domain types, protocol source, fixture files, configuration storage, and secret storage retain the source MIT notice:

Copyright (c) 2026 WokRouter contributors

The accompanying MIT permission notice and warranty disclaimer are preserved in `LICENSE-MIT`. Those migrated files continue to be covered by the source MIT notice and are distributed in WokCore under this repository's `MIT OR Apache-2.0` dual-license terms.
