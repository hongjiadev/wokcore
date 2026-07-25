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

- Test-only `syn` and `proc-macro2` parsing scans every Rust source file, walks the complete production module graph from crate roots (including `#[path]` modules outside `src`), and recursively follows test-reachable external modules and `cfg_attr(test, path = ...)` overrides; unresolved, ambiguous, or unsupported module-path shapes fail closed.
- Direct keyring entry operations and `NativeSecretStore` references are structurally limited to the exact native backend definition, its designated `SecretStore` implementation, and the two exact public re-exports. The gate covers item, statement, expression, local, `cfg`/`cfg_attr`, alias, UFCS, and macro-token shapes without treating string literals as calls; it is a source-structure guarantee rather than whole-program call-graph proof.
- The bounded file reader now accepts a `Read` source and capacity hint, reads through `take(limit + 1)`, and has paired exact-64-KiB/one-byte-over tests plus a cursor-position assertion proving that it does not consume beyond the bound.
- The actual invalid-UTF-8 decode path exposes a test-only post-zeroization observer; mutation evidence proves the assertion fails when the production zeroization statement is removed.
- Windows tests replace inherited temporary-file permissions with an explicit protected DACL, verify successful access with only the current-user allow ACE, then verify fail-closed behavior after adding an Everyone allow ACE.
- Native `BadDataFormat` mapping separately proves that neither its byte payload nor backend diagnostic reaches the generic rendered error.

## Runtime import 5: durable state storage

- Source repository: `https://github.com/hongjiadev/wokrouter`
- Source commit: `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`
- Imported paths:
  - `crates/wokrouter-storage/migrations/0001_initial.sql` (blob `05f782b2276cee06aebdaef2b408adf74f07c55a`) → `crates/wokcore-storage/migrations/0001_initial.sql`
  - `crates/wokrouter-storage/src/state/mod.rs` (blob `9e40ad8e4c926d50926f1bc67a48f2a2c6da0d34`) → `crates/wokcore-storage/src/state/mod.rs`
  - `crates/wokrouter-storage/src/state/store.rs` (blob `83a9639a174451d7bb55bc8938aafc2137867ff7`) → `crates/wokcore-storage/src/state/store.rs`
  - state-storage portions of `crates/wokrouter-storage/src/lib.rs` (blob `9617147ff895b56284ed9ec210624ef1044b7fe3`) → `crates/wokcore-storage/src/lib.rs`
  - `crates/wokrouter-storage/tests/state_store.rs` (blob `0fe818645f8078dea6a0a0990c886138f4b3e925`) → `crates/wokcore-storage/tests/state_store.rs`
- Renames:
  - Cargo package `wokrouter-storage` → internal, unpublished `wokcore-storage`
  - Rust crate path `wokrouter_storage` → `wokcore_storage`
  - domain crate path `wokrouter_core` → `wokcore_core`
- Deliberate adaptation:
  - request persistence is batch-only: an empty slice returns without a transaction, while a non-empty slice uses one immediate transaction and one prepared statement, with any row failure rolling back the whole batch;
  - request state retains only identifiers, provider/model metadata, timing/status/error metadata, and input/output token totals; no request or response bodies, tool payloads, stream chunks, credentials, cookies, or session bodies are represented;
  - SQLite enables foreign keys and WAL, uses a 5000 ms busy timeout, and disables automatic WAL checkpointing;
  - the state API exposes WAL byte measurement, a passive checkpoint gated by an injected threshold, the architecture threshold constant of 16 MiB, and an explicit truncate checkpoint, without timers, watchers, background workers, or per-request checkpoints;
  - setup locking, initial migration 1, orphan-secret recovery metadata, and corruption mapping are retained; corrupt database bytes are never deleted, overwritten, or rebuilt, and this import adds no future migration or backup policy.
- License: migrated state-storage source and tests remain available under `MIT OR Apache-2.0`; the direct WokRouter source MIT notice remains retained in `NOTICE.md` and `LICENSE-MIT`.
- Verification:
  - `cargo +1.97.1 fmt --all -- --check`
  - `cargo +1.97.1 clippy -p wokcore-storage --all-targets --all-features -- -D warnings`
  - `cargo +1.97.1 test -p wokcore-storage --test state_store`
  - `cargo +1.97.1 test -p wokcore-storage --all-features`

## Runtime import 6: platform path discovery

- Source repository: `https://github.com/hongjiadev/wokrouter`
- Source commit: `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`
- Imported path:
  - `crates/wokrouter-platform/src/system/paths.rs` (blob `c9d671235de0d5caa57525422dc367aea88a2e1d`) adapted into `crates/wokcore-platform/src/system/paths.rs`
- Renames:
  - Cargo package `wokrouter-platform` to internal, unpublished `wokcore-platform`
  - application directory identity `WokRouter` to `WokCore`
- Deliberate adaptation:
  - production discovery snapshots the operating-system environment and passes it to the same pure resolver used by deterministic tests;
  - `AppPaths` additionally provides discovery and instance-lock path values; resolving does not create directories or files, set permissions, acquire locks, or write discovery contents;
  - Linux and Windows reject relative environment values, use the documented home fallback when available, and otherwise fail closed.
- Excluded source:
  - `crates/wokrouter-platform/src/system/locale.rs`
  - `crates/wokrouter-platform/src/service/**`
- License: the adapted source remains available under `MIT OR Apache-2.0`; the direct WokRouter source MIT notice remains retained in `NOTICE.md` and `LICENSE-MIT`.
- Verification:
  - `cargo +1.97.1 clippy -p wokcore-platform --all-targets --all-features -- -D warnings`
  - `cargo +1.97.1 test -p wokcore-platform --all-features`

## Runtime import 7: secure runtime ownership and discovery

- Source repository: `https://github.com/hongjiadev/wokrouter`
- Source commit: `226a40e08ad6c783e996ceed77b8e6dfe2640fb4`
- Reviewed source patterns:
  - same-directory temporary-file write, file synchronization, Windows `ReplaceFileW`, Unix atomic rename, and Unix parent-directory synchronization from `crates/wokrouter-storage/src/config/store.rs` (blob `8dcc6f163940b9bc380901aabb5f202ba9eedac8`) → `crates/wokcore-platform/src/runtime/discovery.rs`
  - opened-handle file-type, Unix owner/mode, and Windows owner/DACL verification from `crates/wokrouter-storage/src/secrets/permissioned_file.rs` (blob `cf6fa6015e08e62d65bbcf7a49156b38dcb51521`) → `crates/wokcore-platform/src/runtime/permissions.rs`
- Deliberate adaptation:
  - runtime ownership uses a non-blocking `fs4` operating-system lock held by `RuntimeLease`; Unix additionally locks a permanent zero-length `0600` POSIX shared-memory object whose short name contains the effective user ID and a stable hash of the normalized absolute runtime path, so replacing the runtime-directory pathname cannot create another lock domain. The shared-memory object is owner/type/mode verified, opened close-on-exec, and deliberately never unlinked; the lease simultaneously retains this namespace lock plus the verified runtime-directory and `instance.lock` handles;
  - `DiscoveryStore` records the verified runtime-directory file identity and checks every later pathname reopen against it; a renamed/recreated runtime pathname therefore fails discovery operations closed. Raw Unix child opens atomically set close-on-exec, stale lock-file text is never interpreted as ownership, and neither stale text nor an executed unrelated child extends the lease;
  - runtime, lock, temporary, and discovery objects are created with owner-only permissions and verified through opened handles; Windows opens existing lock entries without following reparse points, while symlink/reparse, wrong-type, foreign-owner, and broader-access targets fail closed;
  - discovery is reduced to the five public fields `base_url`, `pid`, `instance_id`, `wokcore_version`, and `api_major`, requires an exact ASCII-digit nonzero loopback port, accepts the SemVer 2.0 prerelease/build grammar, rejects unknown fields, and bounds reads at 16 KiB;
  - discovery publication writes and synchronizes the same-directory temporary entry before one canonical-name commit: Linux/Android use directory-FD-relative `renameat2(RENAME_EXCHANGE)`, Apple targets use `renameatx_np(RENAME_SWAP)`, and Windows uses `ReplaceFileW` with a backup tombstone and `REPLACEFILE_WRITE_THROUGH`. Thus an uncontended reader sees either the complete old record or the complete new record, never a missing canonical name; pre-commit failure retains the old canonical, while interruption after commit retains the old object as one bounded internal staging/tombstone entry;
  - Unix owned removal atomically moves the canonical entry through the verified runtime-directory handle to an identity-encoded internal tombstone, verifies it against the opened discovery, and performs no later pathname delete or restore in that operation; a match removes the canonical name, while a mismatch is retained as a tombstone and returns `UnsafeRuntimePath`;
  - publish preparation garbage-collects the fixed Unix publish-staging name and strict platform-specific `.wokcore-tombstone-*` internal format only after no-follow regular-file and owner-only permission checks; identity-encoded tombstones additionally require the encoded file identity to match. Malformed, wrong-type, or identity-mismatched tombstones fail closed and are retained. These internal names are private WokCore garbage inside the `0700`/current-user-only runtime directory: same-UID adversarial swaps after an internal garbage-name check are outside the cleanup contract, but cleanup never targets canonical `discovery.json`, follows links, or reaches outside the runtime directory. Completed publish/remove lifecycles retain at most one internal entry, and steady reads perform no cleanup or other writes;
  - the final Unix exchange and Windows `ReplaceFileW` necessarily resolve filesystem names rather than compare-and-swap an inode. WokCore verifies the destination immediately before that commit and Windows denies delete sharing until the commit call, but does not claim protection from a malicious same-UID actor swapping canonical or temporary names in the final syscall window; that case is inside the explicitly current-user-only runtime trust boundary. This limitation does not introduce a canonical-name absence window during normal or failed WokCore publication;
  - the securely opened parent-directory handle is synchronized after publication/removal on Windows as well as Unix; configuration revisions, daemon PID liveness inference, fallback ports, legacy control IPC, and WokRouter runtime lifecycle behavior are not imported.
- License: the adapted patterns remain available under `MIT OR Apache-2.0`; the direct WokRouter source MIT notice remains retained in `NOTICE.md` and `LICENSE-MIT`.
- Verification:
  - `cargo +1.97.1 clippy -p wokcore-platform --all-targets --all-features -- -D warnings`
  - `cargo +1.97.1 test -p wokcore-platform --test runtime_ownership`
  - `cargo +1.97.1 test -p wokcore-platform --test discovery`
  - `cargo +1.97.1 test -p wokcore-platform --all-features`
