# Compatibility policy

WokCore and its clients use independent Semantic Versioning. They are not required to share a version number.

In the foundation release, the supported public surface is the `wokcore` executable and its `--version` flag. Internal Rust packages use `publish = false` and are not a supported embedding API.

When the local HTTP API is introduced, its compatibility contract is:

- management paths are versioned under `/wokcore/v1/...`;
- additive response fields and capabilities are backward-compatible;
- clients ignore unknown fields and gate optional behavior on the capability handshake;
- breaking management API changes require a new API major version;
- Provider-compatible data-plane behavior is versioned independently from the management API.

Breaking public API proposals require a public RFC before implementation.
