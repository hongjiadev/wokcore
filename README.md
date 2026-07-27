# WokCore

WokCore is the independently released local Provider gateway for WokRouter, WokCode, and third-party clients.

The repository is in foundation stage. Local runtime management is implemented; end-user Provider forwarding arrives in a reviewed follow-up change.

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
```

`authorize` intentionally requires JSON output because its successful response contains a one-time proxy token. Tokens, Authorization values, credential paths, and secret references are never accepted as command-line arguments. `status` and `doctor` use read-only discovery, filesystem, SQLite, and loopback probes.

Session discovery automatically detects Codex, Claude Code, and Gemini CLI stores for the current platform. External Session files remain read-only: WokCore indexes bounded metadata and usage, while message bodies are paged directly from their source only after an authenticated request. Timestamps are normalized to UTC from supported source offsets without a locale, language, or timezone selection.

Session, usage, log, and diagnostic-export endpoints accept either the management token or a client token with the exact required scope. Diagnostic events are bounded and redacted; ordinary request events remain in the 16 MiB memory ring, while lifecycle events, warnings, and errors are batch-persisted into rotating segments. A diagnostic export is streamed with a 64 MiB hard bound, is create-new in the CLI, and never contains Session bodies, credentials, authorization headers, cookies, raw tokens, or absolute user paths.

## Development

```powershell
cargo +1.97.1 fmt --all -- --check
cargo +1.97.1 clippy --workspace --all-targets --all-features -- -D warnings
cargo +1.97.1 test --workspace --all-features
```

## License

Licensed under either Apache-2.0 or MIT, at your option.
