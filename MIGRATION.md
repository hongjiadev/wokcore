# Migration provenance

WokCore starts from a clean repository rather than filtered WokRouter history.

The first runtime-code import will be reviewed separately. Its migration commit must record:

- the clean WokRouter source commit;
- every imported source and test path;
- renames into WokCore package boundaries;
- retained MIT attribution;
- verification results before and after extraction.

Private pre-rewrite recovery material is excluded from this public repository.

## Session ingestion 1: Codex

- Implementation: original WokCore code in `crates/wokcore-sessions`.
- Inputs:
  - live Codex rollouts under `sessions/YYYY/MM/DD/*.jsonl`;
  - archived Codex rollouts under `archived_sessions/*.jsonl`;
  - optional bounded `session_index.jsonl` titles;
  - optional recognized Codex SQLite title databases opened only through `mode=ro&immutable=1`.
- Deliberate behavior:
  - Session files are opened only through the pinned, component-safe, read-only `wokcore-platform` APIs;
  - newline-complete JSONL records advance a durable byte cursor in bounded batches, while incomplete tails are replayed;
  - cumulative and last-turn token totals produce content-free usage rows with keyed, domain-separated identifiers;
  - fork prefixes are resolved through persisted, content-free signature pages capped at 512 rows per page and 262,144 rows per rollout;
  - malformed, unavailable, or resource-limited sources are isolated without discarding a previously promoted generation;
  - timestamps are normalized to UTC without locale or user-timezone configuration;
  - no prompt, response, tool body, raw Session path, credential, or Provider traffic is stored or emitted.
- Research references: the read-only Session-first approach is compatible with the CC-Switch research reference listed in `NOTICE.md`; no CC-Switch source code was imported.
- Verification uses only synthetic temporary Session roots and synthetic fixtures. No real Session root, credential, Provider, or billable endpoint is accessed.

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
  - runtime ownership uses non-blocking `fs4` operating-system locks held by `RuntimeLease`. Unix securely creates or opens the stable runtime parent as a current-user-owned `0700` directory without following its final symlink and with close-on-exec, then opens the fixed `.wokcore-runtime-namespace.lock` entry relative to that verified directory. The namespace lock is a current-user-owned regular `0600` file opened no-follow and close-on-exec; it is retained permanently rather than deleted, so replacing and recreating the child runtime-directory pathname cannot create a second lock domain. Different runtime parents remain independent. The lease simultaneously retains this stable parent lock plus the verified runtime-directory and `instance.lock` handles;
  - `DiscoveryStore` records the verified runtime-directory file identity and checks every later pathname reopen against it; a renamed/recreated runtime pathname therefore fails discovery operations closed. Raw Unix child opens atomically set close-on-exec, stale lock-file text is never interpreted as ownership, and neither stale text nor an executed unrelated child extends the lease;
  - runtime, lock, temporary, and discovery objects are created with owner-only permissions and verified through opened handles; Windows opens existing lock entries without following reparse points, while symlink/reparse, wrong-type, foreign-owner, and broader-access targets fail closed. Ordinary read-only discovery handles additionally share delete access so a concurrent `ReplaceFileW` can commit while the reader remains anchored to the complete old object; lock, update, and deletion handles retain their stricter no-delete-sharing behavior;
  - discovery is reduced to the five public fields `base_url`, `pid`, `instance_id`, `wokcore_version`, and `api_major`, requires an exact ASCII-digit nonzero loopback port, accepts the SemVer 2.0 prerelease/build grammar, rejects unknown fields, and bounds reads at 16 KiB;
  - discovery publication writes and synchronizes the same-directory `.wokcore-publish-staging` entry before one canonical-name commit: Linux/Android use directory-FD-relative `renameat2(RENAME_EXCHANGE)`, Apple targets use `renameatx_np(RENAME_SWAP)`, and Windows uses `ReplaceFileW` with the fixed `.wokcore-retired-discovery` backup and `REPLACEFILE_WRITE_THROUGH`. Thus an uncontended reader sees either the complete old record or the complete new record, never a missing canonical name; pre-commit failure retains the old canonical, while interruption after commit retains the old object as one bounded fixed internal entry;
  - Unix owned removal atomically moves the canonical entry through the verified runtime-directory handle to `.wokcore-retired-discovery`, verifies it against the opened discovery, and performs no later pathname delete or restore in that operation; a match removes the canonical name, while a mismatch is retained under that fixed name and returns `UnsafeRuntimePath`;
  - publish preparation probes only `.wokcore-publish-staging` and `.wokcore-retired-discovery`, in constant time and without enumerating or collecting runtime-directory entries. Each exact entry is opened no-follow and accepted only as a current-user-owned regular owner-only file before deletion; directories, reparse points, links, broader permissions, and foreign owners fail closed and are retained. Canonical `discovery.json` is never garbage-collected, and no random legacy temporary namespace is scanned because no public version emitted one. These two internal names are private WokCore garbage inside the `0700`/current-user-only runtime directory: same-UID adversarial swaps after an internal garbage-name check are outside the cleanup contract, but cleanup never follows links or reaches outside the runtime directory. Completed publish/remove lifecycles retain at most one internal entry, steady reads perform no cleanup or other writes, and unrelated directory entries do not affect preparation work;
  - the final Unix exchange and Windows `ReplaceFileW` necessarily resolve filesystem names rather than compare-and-swap an inode. WokCore verifies the destination immediately before that commit and the Windows pre-commit update handle denies delete sharing until the commit call, but does not claim protection from a malicious same-UID actor swapping canonical or temporary names in the final syscall window; that case is inside the explicitly current-user-only runtime trust boundary. This limitation does not introduce a canonical-name absence window during normal or failed WokCore publication;
  - the securely opened parent-directory handle is synchronized after publication/removal on Windows as well as Unix; configuration revisions, daemon PID liveness inference, fallback ports, legacy control IPC, and WokRouter runtime lifecycle behavior are not imported.
- License: the adapted patterns remain available under `MIT OR Apache-2.0`; the direct WokRouter source MIT notice remains retained in `NOTICE.md` and `LICENSE-MIT`.
- Verification:
  - `cargo +1.97.1 clippy -p wokcore-platform --all-targets --all-features -- -D warnings`
  - `cargo +1.97.1 test -p wokcore-platform --test runtime_ownership`
  - `cargo +1.97.1 test -p wokcore-platform --test discovery`
  - `cargo +1.97.1 test -p wokcore-platform --all-features`

## Runtime foundation 8: split-scope authentication metadata and token registry

- The runtime authentication implementation is original WokCore code rather than migrated WokRouter source.
- SQLite schema 2 adds only stable runtime secret references, binding revisions, 32-byte client-token digests, client/token identifiers, and issue/revoke timestamps. Schema 1 and schema 2 are applied in order with one immediate transaction per missing migration; incompatible migration histories fail closed.
- `ClientId` is a distinct opaque type that reuses the existing lowercase, path-safe, 128-byte identifier rules without aliasing `AccountId`.
- Management and proxy token material uses 32 bytes from an injected entropy boundary; production provides the operating-system CSPRNG implementation. Raw material is held in `secrecy::SecretString`, has redacted debug output, and is exposed only by a consuming response conversion.
- Management bootstrap writes the secret backend before binding its reference. A binding failure records the unbound reference in existing orphan metadata and returns no secret material.
- The active proxy-token set is an immutable `arc-swap` snapshot. Validation hashes and reads memory only; issue commits digest metadata before returning material, and revoke commits the matching client/token row before replacing the snapshot.
- SQLite metadata calls are serialized only for low-frequency management mutations and run through `spawn_blocking`. Token validation has no SQLite/keyring call, background task, periodic refresh, semaphore, or configured concurrency limit.
- The pre-existing state-store integration tests originally asserted schema version/count 1. Their three version/count literals were updated to 2 as the minimum compatibility change required by the ordered schema bump; no unrelated legacy test behavior was changed.
- Verification:
  - `cargo +1.97.1 test --offline -p wokcore-storage --test runtime_auth_store --locked`
  - `cargo +1.97.1 test --offline -p wokcore-server --test auth_registry --locked`
  - `cargo +1.97.1 clippy --offline -p wokcore-storage -p wokcore-server --all-targets --all-features --locked -- -D warnings`

## Runtime foundation 9: Session and supplemental state schema

- SQLite schema 3 adds opaque Session source/generation pointers, bounded scan cursors and typed parser checkpoints, compact Session index and usage rows, content-free Codex replay signatures, bounded request supplemental metadata, and normalized client-token scopes.
- Every Session-derived row carries its opaque source key and generation. Replacement data is committed into a hidden staging generation in batches capped at 512 rows and 512 KiB; one pointer-flip transaction promotes it and retires the prior successful generation without synchronously deleting that history.
- Current-generation index, usage, cursor, and replay-signature reads use one SQLite snapshot joined to the current-generation pointer. Bounded source enumeration and global current index/usage keyset pages let startup serve persisted state before scanning, while staging and retired rows remain hidden.
- Exported page keys provide validated constructors and component accessors so another crate can round-trip an opaque transport cursor without exposing storage fields or coupling storage to HTTP encoding. Global current index/usage SQL is source-driven and uses the `(source_key, generation, sort key, stable key)` indexes, so staging and retired row volume cannot amplify work before the page limit.
- Same-generation append commits advance cursors and Session counters monotonically; source-state transitions use immediate compare-and-swap transactions bound to the expected generation. Restart resume also binds the persisted parent generation and replay boundary, so stale lineage is rebuilt rather than reused.
- A cursor replay at the same byte offset and stable ordinal cannot mutate its parser checkpoint or structural lineage, while the explicit result-state transition remains supported. Cleanup accounts for the exact same logical cursor bytes as batch admission, including optional parent source, result code, result-transition timestamp, and UTF-8 byte lengths.
- Session-list reads derive effective availability from the joined source status: any non-available source makes retained rows unavailable without rewriting the index generation, and source recovery restores the persisted row's availability.
- Session timestamps use one canonical second-precision UTC representation. File identities, fingerprints, correlation identifiers, scan results, retry/failover decisions, HTTP status, and persistent error codes are represented by constrained domain types and matching schema checks rather than free-form strings.
- Interrupted candidates and failed source observations preserve the last successful aggregate. Staging and retired cleanup removes at most 512 rows and 512 KiB per transaction and accounts for textual fields by encoded UTF-8 bytes.
- Existing schema-2 client tokens receive exactly `proxy.use`. New token metadata stores only exact allow-listed scope rows and does not support wildcards or implicit expansion.
- Supplemental request metadata is best-effort and content-free. Every write path shares one transactional capacity policy and typed outcome with exact inserted and dropped row counts: each row is capped at 2 KiB and the table is bounded to 24 hours, 32,768 rows, and 64 MiB, with cleanup limited to 512 rows and 512 KiB per call.
- The schema stores no Session paths or bodies, request/response content, tool payloads, stream chunks, headers, cookies, credentials, or raw tokens. Offline and live read-only inspection replays committed WAL state in memory without mutating database sidecars.
- Verification:
  - `cargo +1.97.1 test -p wokcore-storage --test session_state_store --locked --offline`
  - `cargo +1.97.1 test -p wokcore-storage --all-features --locked --offline`
  - `cargo +1.97.1 clippy -p wokcore-storage --all-targets --all-features --locked --offline -- -D warnings`

## Runtime foundation 10: Session diagnostics control plane

- WokCore automatically discovers Codex, Claude Code, and Gemini CLI Session roots from an injected platform environment snapshot. Production uses the same resolver with the current operating-system environment; tests use only synthetic temporary roots.
- External Session files are opened read-only through pinned, no-follow platform handles. Scanners operate in bounded slices, preserve incomplete lines, detect replacement/truncation, and update only WokCore-owned SQLite state through the existing single-writer batch path.
- Session bodies are not copied into SQLite or durable diagnostics. The authenticated message endpoint resolves an opaque indexed key and pages directly from the pinned source generation with a complete serialized-response byte budget.
- The Session list, message, usage, and diagnostic-log APIs use a dedicated two-worker query service, a 32-command non-blocking queue, and a five-second deadline. These bounds do not limit Provider proxy or SSE concurrency.
- The memory diagnostic ring is capped at 16 MiB. Durable events use non-blocking bounded admission, batches of at most 128 events or 256 KiB, 4 MiB rotating segments, and seven-day/64 MiB retention. Empty timers perform no writes.
- Control-plane requests emit one typed event correlated with the returned `X-Request-Id`; there are no per-SSE-chunk events. Ordinary successful and client-error request events stay in memory, while internal failures are eligible for batched persistence.
- The streamed diagnostic ZIP export is built in a current-user-only internal directory, validated against its manifest, checksums, and leak scan, then removed after transfer. CLI export is create-new, streams directly to a pinned destination, rejects existing/raced targets, and refuses destinations inside or aliased to discovered Session roots.
- Client authorization supports exact repeatable scopes: `proxy.use`, `sessions.read`, `usage.read`, `diagnostics.read`, and `diagnostics.export`. Existing schema-2 client tokens retain only `proxy.use`.
- Capabilities add `sessions.index.v1`, `sessions.messages.v1`, `usage.session.v1`, `diagnostics.events.v1`, and `diagnostics.export.v1`.
- Verification:
  - `cargo +1.97.1 test -p wokcore-server --all-features --locked --offline`
  - `cargo +1.97.1 test -p wokcore --all-features --locked --offline`
  - `cargo +1.97.1 test --workspace --all-features --locked --offline`

## Runtime foundation 11: frozen Provider catalog

- Source repository: `https://github.com/lidge-jun/opencodex`
- Source tag and commit: `v2.7.35` at `97e7326f89bcfbb29a2c73250cb25eb801d066b6`
- Adapted static source:
  - `src/providers/registry.ts`
  - `src/providers/base-url-choices.ts`
  - the referenced static Kiro, Antigravity, Kimi, Anthropic, OpenAI, Alibaba, Tencent, MiniMax, and Cloudflare model seeds
- Deliberate adaptation:
  - TypeScript registry objects become a strict, bundled TOML data file consumed by the new internal `wokcore-engine` crate;
  - the frozen baseline contains exactly 58 canonical Provider IDs and records an explicit adapter family, authentication kind, endpoint policy, model-source kind, and capability set for every Provider;
  - endpoint validation rejects credentials, fragments, unsafe schemes, remote hosts under loopback policy, local/private literals under public policy, and malformed HTTPS templates;
  - unknown fields, duplicate IDs/models, alias collisions, invalid identifiers, and inconsistent static/live model metadata fail closed with content-free error codes;
  - no OpenCodex executable code, OAuth implementation, native local execution path, credential material, runtime command, or network behavior is imported;
  - WokCore catalog parsing and validation are original Rust code.
- License: adapted Provider catalog facts retain the complete OpenCodex MIT notice in `NOTICE.md`.
- Verification:
  - `cargo +1.97.1 test -p wokcore-engine --test provider_catalog --locked --offline`
  - `cargo +1.97.1 clippy -p wokcore-engine --all-targets --all-features --locked --offline -- -D warnings`

## Runtime foundation 12: revisioned Provider and routing configuration

- Original WokCore implementation; no external configuration code is imported.
- `wokcore-core` defines bounded Provider instances, accounts, tagged authentication references, model aliases, exact client/model rules, and default route targets. Authentication fields accept only validated opaque `SecretRef` values.
- Existing server-only TOML documents load with empty Provider/routing defaults. New commits preserve the existing optimistic revision and atomic replacement contract while serializing strict nested Provider/routing tables.
- Shape validation runs before the configuration lock or file is created. It rejects excessive collections, duplicate identifiers/aliases, dangling account/routes, invalid model IDs, and endpoint URLs containing credentials, queries, fragments, templates, unsupported schemes, or missing hosts.
- `wokcore-engine` adds catalog-aware validation for known Provider IDs, endpoint-override permission, public HTTPS, explicit private-network opt-in, IPv4-mapped IPv6 private addresses, and exact authentication-kind compatibility.
- Provider endpoint debug output exposes only whether an override is present. `SecretRef` remains redacted, and parse/serialization errors use content-free messages rather than echoing rejected values.
- This layer performs no Provider discovery, OAuth, credential resolution, DNS lookup, or network request.
- Verification:
  - fixed-host `wokcore-engine` Provider configuration tests;
  - fixed-host `wokcore-storage` legacy/round-trip/strict-schema/privacy tests;
  - complete Windows workspace suite through `E:/Projects/wokcore/target/wokcore-test-host.exe`;
  - Clippy with `-D warnings` for core, engine, storage, server, and application targets.

## Runtime foundation 13: immutable routing snapshots

- Original WokCore implementation; no external routing or synchronization code is imported.
- A validated configuration is compiled into one immutable runtime snapshot. `ArcSwap` publishes a complete replacement atomically, readers acquire one `Arc` without a writer lock, and a failed rebuild leaves the last valid snapshot active.
- Route precedence is explicit Provider/model, model alias, client/model rule, default route, then a typed no-route result. More-specific client-and-model rules precede general rules while equal-specificity rules retain configuration order.
- Disabled Providers and accounts never enter runtime indices. Candidate iteration borrows the selected immutable Provider/account records without cloning secret references or allocating a per-request candidate collection.
- Reasoning effort values and configured wire mappings preserve exact validated strings. The public model projection is deterministic, sorted, deduplicated, capability-aware, and excludes endpoints and secret references.
- Snapshot construction performs no credential resolution, filesystem write, Provider discovery, DNS lookup, background work, or network request.
- Verification:
  - fixed-host `wokcore-engine` routing snapshot tests;
  - complete Windows workspace suite through `E:/Projects/wokcore/target/wokcore-test-host.exe`;
  - `cargo +1.97.1 clippy -p wokcore-engine --all-targets --offline -- -D warnings`.

## Runtime foundation 14: account runtime state and schema 4

- Original WokCore implementation; no external account-selection or persistence code is imported.
- Account health is held in 64 bounded shards. Weighted least-use selection, success recovery, quota windows, bounded exponential cooldowns, bounded server retry hints, and immediate invalid-credential quarantine operate without a global request lock or proxy concurrency semaphore.
- Thread affinity is memory-only, split across bounded shards, expires automatically, stores a domain-separated keyed SHA-256 digest rather than the caller's thread key, and returns shared account identifiers without per-lookup string allocation.
- SQLite schema 4 removes the unused schema-1 `thread_affinities` and `quota_windows` tables. Secure deletion is enabled for the one-time removal so legacy raw affinity bytes are cleared; the setting is disabled again before normal operation.
- Replacement batches persist only bounded Provider/account identifiers, health codes, cooldown/quota timestamps, selection counters, and update timestamps. They contain no thread key, prompt, response, tool payload, authorization value, cookie, or credential.
- Batches are validated before a transaction, run through the existing bounded single-writer queue, delete stale rows, update only changed rows, and skip the transaction entirely when the complete state is unchanged. No request or stream event performs a durable write.
- Restart loading preserves active quarantine/cooldown/quota state and clears windows whose explicit deadlines have passed. Invalid or inconsistent rows fail closed.
- Verification:
  - eight fixed-host account-selection, health, quota, affinity, concurrent-observation, and restart tests;
  - seven fixed-host schema-4 migration, atomic batch, zero-WAL replay, bounded replacement, privacy, and writer-queue tests;
  - complete Windows workspace suite through `E:/Projects/wokcore/target/wokcore-test-host.exe`;
  - engine/storage Clippy with `-D warnings`, rustfmt, locked offline metadata, and public repository hygiene checks.

## Runtime foundation 15: pre-visible retry and account failover

- Original WokCore implementation; no external retry, execution, or upstream transport code is imported.
- An execution request borrows its immutable account candidates and retains one bounded shared body only for the execution window. It does not allocate a per-request candidate collection or introduce a global semaphore, queue, or artificial Provider concurrency limit.
- Each request has at most two total upstream attempts. Only rate limits, server failures, timeouts, and resets that occur before any response becomes client-visible may retry; any failure after visibility and all credential, request, policy, or unclassified failures terminate immediately.
- Retry selection remains within the request's authentication kind. Account observations feed the existing bounded cooldown/quarantine state so an eligible alternative is selected without crossing credential types.
- Server retry hints are bounded by policy, waits and in-flight attempts are cancellation-safe, and the retained request body is released when execution returns.
- Attempt history has exactly two optional slots and records only stable request, attempt, Provider, account, boundary, failure kind, and status metadata. It never records request bodies, stream content, credentials, headers, cookies, or raw tokens.
- Verification:
  - eight fixed-host execution tests covering attempt count, visibility, cancellation, authentication isolation, health-driven failover, delay bounds, body lifetime, and diagnostic privacy;
  - fixed-host weighted-selection and affinity regressions;
  - complete Windows workspace suite through `E:/Projects/wokcore/target/wokcore-test-host.exe`;
  - workspace Clippy with `-D warnings`, rustfmt, locked offline checks, and public repository hygiene checks.

## Runtime foundation 16: Provider management control plane

- Original WokCore implementation; no external Provider-management or OAuth code is imported.
- The loopback management API exposes the immutable 58-Provider catalog, active public model projection, and complete revisioned Provider/routing status without resolving credentials or contacting an upstream.
- Candidate validation and commit share the runtime snapshot builder. Commit requires an exact expected revision, persists the complete configuration atomically, and publishes the rebuilt immutable snapshot only after durable success. Management mutations finish in owned tasks after caller cancellation, and an indeterminate commit is reconciled against the complete durable revision before publication.
- Explicit reload rebuilds before publication. Invalid durable input marks reload status failed while retaining the prior revision, models, configuration, and active snapshot.
- Provider secret create, stable-reference replace, and unused-reference delete operations use the configured `SecretStore`. Creation derives a stable opaque reference from the Provider/account/purpose scope, makes same-material retries idempotent after cancellation, and rejects different material until an explicit replace. Requests accept bounded secret material, while responses and error bodies expose metadata only.
- The WokCore runtime management credential is protected after authentication bootstrap: it cannot enter Provider configuration and cannot be created, replaced, or deleted through the Provider lifecycle.
- Every Provider endpoint requires the management token. Proxy-token scopes do not grant Provider administration. Methods, JSON content types, body bounds, paths, and revision conflicts fail closed with stable content-free errors.
- The CLI mirrors the management API with required JSON output. Candidate files are bounded to 16 KiB; secret input is bounded to 8 KiB and is accepted only from standard input. Secret material and secret references are never accepted as positional arguments.
- OpenAPI 3.1 defines the exact Provider paths, security schemes, request/response schemas, write-only secret fields, and revision contracts. The implementation performs no Provider discovery, OAuth browser flow, DNS lookup, or upstream request.
- Verification:
  - five fixed-host Provider management service and HTTP contract tests;
  - fixed-host OpenAPI, CLI, production bootstrap, and secret-store safety regressions;
  - complete Windows workspace suite through `E:/Projects/wokcore/target/wokcore-test-host.exe`;
  - workspace Clippy with `-D warnings`, rustfmt, locked offline checks, and public repository hygiene checks.
