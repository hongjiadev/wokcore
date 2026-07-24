# WokCore

WokCore is the independently released local Provider gateway for WokRouter, WokCode, and third-party clients.

The repository is in foundation stage. The only implemented CLI surface is currently `wokcore --version`; Provider and management HTTP APIs will arrive in reviewed follow-up changes.

WokCore is an independent program. Its internal Rust packages are not a supported embeddable library API. The future public product contract is a versioned local HTTP/JSON + SSE API.

## Development

```powershell
cargo +1.97.1 fmt --all -- --check
cargo +1.97.1 clippy --workspace --all-targets --all-features -- -D warnings
cargo +1.97.1 test --workspace --all-features
```

## License

Licensed under either Apache-2.0 or MIT, at your option.
