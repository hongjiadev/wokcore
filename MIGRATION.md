# Migration provenance

WokCore starts from a clean repository rather than filtered WokRouter history.

The first runtime-code import will be reviewed separately. Its migration commit must record:

- the clean WokRouter source commit;
- every imported source and test path;
- renames into WokCore package boundaries;
- retained MIT attribution;
- verification results before and after extraction.

The private pre-rewrite recovery bundle is stored in WokDocs, not this public repository.

## Runtime import 1: domain types

- Source repository: `https://github.com/hongjiadev/wokrouter`
- Source commit: `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`
- Imported paths:
  - `crates/wokrouter-core/src/id.rs` → `crates/wokcore-core/src/id.rs`
  - `crates/wokrouter-core/src/secret.rs` → `crates/wokcore-core/src/secret.rs`
- Deliberate adaptation:
  - `crates/wokcore-core/src/lib.rs` identifies `WokCore` and omits the WokRouter-only `control_protocol` field.
- License: imported code remains available under `MIT OR Apache-2.0`. The direct WokRouter source copyright and permission notice is retained in `NOTICE.md`, with its terms reproduced in `LICENSE-MIT`; the OpenCodex MIT attribution remains listed in `NOTICE.md`.
- Verification:
  - `cargo +1.97.1 clippy -p wokcore-core --all-targets --all-features -- -D warnings`
  - `cargo +1.97.1 test -p wokcore-core --all-features`

## Runtime import 2: protocol substrate

- Source repository: `https://github.com/hongjiadev/wokrouter`
- Source commit: `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`
- Imported paths:
  - `crates/wokrouter-protocols/src/**` → `crates/wokcore-protocols/src/**`
  - `crates/wokrouter-protocols/tests/**` → `crates/wokcore-protocols/tests/**`
  - `tests/fixtures/protocols/**` → `tests/fixtures/protocols/**`
- Renames:
  - Cargo package `wokrouter-protocols` → `wokcore-protocols`
  - Rust crate path `wokrouter_protocols` → `wokcore_protocols`
- Deliberate adaptation:
  - the internal package is `publish = false`; protocol behavior and fixtures are otherwise retained.
  - `tests/fixtures/protocols/.gitattributes` pins fixture LF endings and permits the legal SSE blank line at EOF without changing fixture wire bytes.
- License: imported code and fixtures remain available under `MIT OR Apache-2.0`; third-party references remain listed in `NOTICE.md`.
- Verification:
  - `python tests/fixtures/protocols/cursor/verify_fixtures.py`
  - `cargo +1.97.1 clippy -p wokcore-protocols --all-targets --all-features -- -D warnings`
  - `cargo +1.97.1 test -p wokcore-protocols --all-features`

## Runtime import 2 review hardening

- Bounded channels reject capacities outside Tokio's supported semaphore range instead of panicking.
- Every SSE `push` bounds the number of decoded frames before extending its return vector; Azure and Gemini derive that per-push limit from the configured event limit.
- Azure and Gemini non-stream decoders aggregate `max_events` across decode and finish output.
- `NOTICE.md` retains `Copyright (c) 2026 WokRouter contributors` for the migrated domain types, protocol source, and fixtures, and explains the source MIT notice alongside WokCore's `MIT OR Apache-2.0` dual-license terms.

## Runtime import 3: configuration storage

- Source repository: `https://github.com/hongjiadev/wokrouter`
- Source commit: `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`
- Imported paths:
  - `crates/wokrouter-storage/src/config/mod.rs` (blob `3af86aa15f068886e81d6a1d4852b6db81140309`) → `crates/wokcore-storage/src/config/mod.rs`
  - `crates/wokrouter-storage/src/config/model.rs` (blob `cb37c701eda4816b19d6b81556b41f3b87a57956`) → `crates/wokcore-storage/src/config/model.rs`
  - `crates/wokrouter-storage/src/config/store.rs` (blob `8dcc6f163940b9bc380901aabb5f202ba9eedac8`) → `crates/wokcore-storage/src/config/store.rs`
  - configuration portions of `crates/wokrouter-storage/src/lib.rs` (blob `9617147ff895b56284ed9ec210624ef1044b7fe3`) → `crates/wokcore-storage/src/lib.rs`
  - `crates/wokrouter-storage/tests/config_store.rs` (blob `33be6f371b22d8f3b68e9807ba36eec83bef272e`) → `crates/wokcore-storage/tests/config_store.rs`, including malformed-input and cross-process revision tests, and the private replacement-cleanup test in `crates/wokcore-storage/src/config/store.rs`
- Renames:
  - Cargo package `wokrouter-storage` → internal, unpublished `wokcore-storage`
  - Rust crate path `wokrouter_storage` → `wokcore_storage`
- Deliberate adaptation:
  - configuration is reduced to `server.port`, defaults to `10101`, and rejects port `0`;
  - WokRouter host, private-LAN, UI locale, and UI timezone fields are removed rather than renamed;
  - candidates are validated before the lock file is opened, so invalid commits create no artifacts;
  - read-only absent loads, revision conflicts, same-directory temporary files, file synchronization, Windows `ReplaceFileW`, and Unix parent-directory synchronization are retained.
- License: migrated configuration source and tests remain available under `MIT OR Apache-2.0`; the direct WokRouter source MIT notice remains retained in `NOTICE.md` and `LICENSE-MIT`.
- Verification:
  - `cargo +1.97.1 clippy -p wokcore-storage --all-targets --all-features -- -D warnings`
  - `cargo +1.97.1 test -p wokcore-storage --all-features`

## Runtime import 3 review hardening

- The persisted configuration uses a private exact wire model rather than combining Serde flattening with unknown-field rejection.
- Unknown top-level keys and unknown `server` keys now fail as invalid configuration without mutating the source file.
- Cross-process tests restore the source implementation's two-process revision race and verify one success, one conflict, and final revision `1`.
- Malformed TOML tests additionally preserve source bytes and modification time and reject temporary-file residue.

## Runtime import 4: secret storage

- Source repository: `https://github.com/hongjiadev/wokrouter`
- Source commit: `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`
- Imported paths:
  - `crates/wokrouter-storage/src/secrets/mod.rs` (blob `d385a6e8f3ede1ad62f2b174424c9a2f070b001c`) → `crates/wokcore-storage/src/secrets/mod.rs`
  - `crates/wokrouter-storage/src/secrets/store.rs` (blob `29c0d52c8c9df7d0c6f8a7ea015389cea5917af3`) → `crates/wokcore-storage/src/secrets/store.rs`
  - `crates/wokrouter-storage/src/secrets/memory.rs` (blob `2018b6483bf74b42a8fbf5e1ed2e45b9787f64bf`) → `crates/wokcore-storage/src/secrets/memory.rs`
  - `crates/wokrouter-storage/src/secrets/native.rs` (blob `2db05c07e26dcc2b8b8e0eed044819c977bcbb58`) → `crates/wokcore-storage/src/secrets/native.rs`
  - `crates/wokrouter-storage/src/secrets/environment.rs` (blob `f9d0e54a366e1d9595ab42f839e9ef19bbc9ddc3`) → `crates/wokcore-storage/src/secrets/environment.rs`
  - `crates/wokrouter-storage/src/secrets/permissioned_file.rs` (blob `cf6fa6015e08e62d65bbcf7a49156b38dcb51521`) → `crates/wokcore-storage/src/secrets/permissioned_file.rs`
  - secret-storage portions of `crates/wokrouter-storage/src/lib.rs` (blob `9617147ff895b56284ed9ec210624ef1044b7fe3`) → `crates/wokcore-storage/src/lib.rs`
  - offline secret contracts from `crates/wokrouter-storage/tests/secret_store.rs` (blob `4292f0ace6d0f6c83b6b6919944a28fafc029d94`) → `crates/wokcore-storage/tests/secret_store.rs`
- Renames:
  - Cargo package `wokrouter-storage` → internal, unpublished `wokcore-storage`
  - Rust crate path `wokrouter_storage` → `wokcore_storage`
  - native credential service identity `dev.wokrouter.credentials` → `dev.wokcore.credentials`
- Deliberate adaptation:
  - every native keyring operation constructs its entry inside `tokio::task::spawn_blocking`; secret material is moved into the blocking closure, and join/backend diagnostics map to non-diagnostic storage errors;
  - environment and permissioned-file stores accept only their explicitly configured `SecretRef`, remain read-only, and never act as an automatic fallback for native storage;
  - `MAX_HEADLESS_SECRET_BYTES` limits environment and file inputs to `64 KiB`; file reads check metadata on the opened handle and use `take(limit + 1)` before decoding;
  - invalid environment/file encodings and oversized buffers are zeroized on rejection;
  - Unix mode checks and Windows owner/DACL checks remain fail closed and operate on the opened file handle, preventing path-swap replacement;
  - no writable plaintext secret store is included, and tests never invoke the real OS credential store.
- License: migrated secret-storage source and tests remain available under `MIT OR Apache-2.0`; the direct WokRouter source MIT notice remains retained in `NOTICE.md` and `LICENSE-MIT`.
- Verification:
  - `cargo +1.97.1 clippy -p wokcore-storage --all-targets --all-features -- -D warnings`
  - `cargo +1.97.1 test -p wokcore-storage --test secret_store`
  - `cargo +1.97.1 test -p wokcore-storage --all-features`

## Runtime import 4 review hardening

- Test-only `syn` and `proc-macro2` parsing structurally rejects native credential entry/store access from every integration test, every `#[cfg(test)]`/test item, and every `*_tests.rs` unit-test file; detector fixtures cover whitespace, UFCS/function paths, aliases, macros, and newly added test files without treating string literals as calls.
- The bounded file reader now accepts a `Read` source and capacity hint, reads through `take(limit + 1)`, and has paired exact-64-KiB/one-byte-over tests plus a cursor-position assertion proving that it does not consume beyond the bound.
- The actual invalid-UTF-8 decode path exposes a test-only post-zeroization observer; mutation evidence proves the assertion fails when the production zeroization statement is removed.
- Windows tests replace inherited temporary-file permissions with an explicit protected DACL, verify successful access with only the current-user allow ACE, then verify fail-closed behavior after adding an Everyone allow ACE.
- Native `BadDataFormat` mapping separately proves that neither its byte payload nor backend diagnostic reaches the generic rendered error.
