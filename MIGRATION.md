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
- License: imported code remains available under `MIT OR Apache-2.0`; retained OpenCodex MIT attribution is listed in `NOTICE.md`.
- Verification:
  - `cargo +1.97.1 clippy -p wokcore-core --all-targets --all-features -- -D warnings`
  - `cargo +1.97.1 test -p wokcore-core --all-features`
