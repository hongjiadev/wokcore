# WokCore

WokCore is the independently released local Provider gateway for WokRouter, WokCode, and third-party clients.

The repository is in foundation stage. Local runtime management and the bounded Provider HTTP data plane are implemented; distribution hardening and downstream client cutover are still in progress.

WokCore is an independent program. Its internal Rust packages are not a supported embeddable library API. The supported management contract is the versioned loopback-only HTTP/JSON API in [`openapi/wokcore-v1.json`](openapi/wokcore-v1.json).

## Local control plane

The service binds only an explicitly configured IPv4-loopback listener and accepts only its exact `127.0.0.1:port` authority. Native clients omit `Origin`; browser-origin requests and implicit CORS access are rejected.

Health and the versioned capability handshake are public. Service coordination and client-token issue/revoke operations use the management Bearer token referenced by local discovery and resolved through the configured secret backend. Raw proxy tokens are returned only by a successful authorize response and are never recoverable from SQLite or discovery.

Provider protocol identifiers in the capability response describe implemented codecs only. They do not indicate that a Provider, account, credential, Session, or upstream is configured.

The local CLI surface is:

```text
wokcore serve [--json]
wokcore status [--json]
wokcore stop [--json]
wokcore doctor [--json]
wokcore authorize --client <id> [--scope <scope> ...] --json
wokcore sessions list [--source <codex|claude|gemini>] [--limit <n>] [--json]
wokcore sessions show <session-key> [--cursor <cursor>] [--limit <n>] [--json]
wokcore logs [--request-id <id>] [--level <level>] [--component <component>] [--since <utc>] [--jsonl]
wokcore diagnostics export --output <path>
wokcore providers catalog --json
wokcore providers status --json
wokcore providers models --json
wokcore providers validate --file <path> --json
wokcore providers commit --file <path> --expected-revision <n> --json
wokcore providers reload --json
wokcore providers secret create --provider <id> [--account <id>] --purpose <purpose> --secret-stdin --json
wokcore providers secret replace --secret-ref <ref> --secret-stdin --json
wokcore providers secret delete --secret-ref <ref> --json
```

`authorize` intentionally requires JSON output because its successful response contains a one-time proxy token. Tokens, Authorization values, credential paths, and secret references are never accepted as command-line arguments. `status` and `doctor` use read-only discovery, filesystem, SQLite, and loopback probes.

Session discovery automatically detects Codex, Claude Code, and Gemini CLI stores for the current platform. External Session files remain read-only: WokCore indexes bounded metadata and usage, while message bodies are paged directly from their source only after an authenticated request. Timestamps are normalized to UTC from supported source offsets without a locale, language, or timezone selection.

Session, usage, log, and diagnostic-export endpoints accept either the management token or a client token with the exact required scope. Diagnostic events are bounded and redacted; ordinary request events remain in the 16 MiB memory ring, while lifecycle events, warnings, and errors are batch-persisted into rotating segments. A diagnostic export is streamed with a 64 MiB hard bound, is create-new in the CLI, and never contains Session bodies, credentials, authorization headers, cookies, raw tokens, or absolute user paths.

## Provider management

The management API exposes the frozen 58-Provider catalog, active public models, and the current revisioned Provider/routing configuration. Candidate JSON is validated before publication. A commit requires the caller's expected revision, writes the complete configuration atomically, and publishes one immutable routing snapshot; a conflict is rejected, and a failed explicit reload retains the last valid snapshot.

Provider secrets are accepted only through bounded JSON requests or CLI standard input. Creation is retry-safe for one Provider/account/purpose scope: matching material returns the same opaque `SecretRef`, while different material conflicts and must use replace for intentional rotation. Responses contain a `SecretRef` and operation metadata only. Secret material is never accepted as a CLI argument, written to configuration, or returned by the API. In-use references cannot be deleted, and the runtime management credential is excluded from the Provider secret lifecycle and configuration.

Every Provider management endpoint requires the management token. A proxy-scoped client token cannot list, validate, commit, reload, create, replace, or delete Provider state. This surface performs no OAuth browser flow, Provider discovery, DNS lookup, or upstream request.

## Provider data plane

All data-plane operations require a client token with the exact `proxy.use` scope. The same loopback authority and Origin restrictions as the management API apply. There is no global request semaphore or configured Provider-concurrency ceiling; per-stream channels and retained request/response bodies remain bounded.

| Client contract | Path | Streaming | Production upstream adapter |
| --- | --- | --- | --- |
| OpenAI Responses | `POST /v1/responses` | JSON or SSE | OpenAI Responses, Gemini, Azure |
| OpenAI Chat Completions | `POST /v1/chat/completions` | JSON or SSE | OpenAI Chat, Gemini, Azure |
| Anthropic Messages | `POST /v1/messages` | JSON or SSE | Anthropic, Gemini, Azure |
| Anthropic token count | `POST /v1/messages/count_tokens` | JSON only | Provider-native when supported; otherwise bounded local estimate |
| OpenAI model list | `GET /v1/models` | JSON only | Immutable local routing snapshot; no upstream request |
| OpenAI image generation | `POST /v1/images/generations` | JSON only | OpenAI-compatible image adapters, including Azure URL shaping |
| OpenAI image edit | `POST /v1/images/edits` | JSON only | OpenAI-compatible streamed multipart adapters, including Azure URL shaping |

The bundled catalog describes 58 Provider families, but a catalog capability is not a guarantee that a production wire adapter is enabled. Google image execution currently fails with a typed unsupported-capability result. Provider-specific OAuth browser authorization is not implemented.

JSON requests are capped at 16 MiB. Image edits are streamed through private randomized temporary files with a 20 MiB per-file, 50 MiB aggregate payload, and 51 MiB multipart wire cap; temporary files are removed on success, failure, cancellation, and drop. Pooled transports reject redirects and ambient proxies, validate DNS results against endpoint policy, and bound headers, bodies, connect time, and total time.

Request diagnostics contain only stable request/attempt identifiers, Provider/model metadata, status, timing, and token totals. Successful request events stay in memory. SQLite metrics and account-health metadata flush in batches of 64 or after 250 ms of observed activity; an idle service performs no periodic write, and observability backpressure never blocks a Provider request. Session-derived usage remains the primary detailed usage source.

All repository transport and concurrency tests use injected executors or synthetic loopback Providers. They do not read real credentials or Sessions and do not contact a billable endpoint.

## Development

```powershell
cargo +1.97.1 fmt --all -- --check
cargo +1.97.1 clippy --workspace --all-targets --all-features -- -D warnings
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File tests/scripts/run-fixed-test-host.ps1 `
  -TargetDirectory (Join-Path $PWD "target") `
  -Offline
```

On Windows, the test runner compiles Cargo test artifacts without executing their hash-named files, then runs every artifact sequentially as the fixed `target/wokcore-test-host.exe`. Tests that open loopback listeners therefore keep one stable executable identity. Linux and macOS can run `cargo +1.97.1 test --workspace --all-features` directly.

## License

Licensed under either Apache-2.0 or MIT, at your option.
