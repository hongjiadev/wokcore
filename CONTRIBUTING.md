# Contributing

Open an issue before starting a breaking API or large architectural change.

All contributions must pass formatting, Clippy, tests, dependency policy, and public repository hygiene checks. Internal Rust packages remain `publish = false`; the supported public contract will be the versioned HTTP API.

Windows contributors must run workspace tests through `tests/scripts/run-fixed-test-host.ps1`; it gives every loopback-listening test the stable `target/wokcore-test-host.exe` process identity used by CI.

Do not commit Superpowers specs/plans, AI review/progress artifacts, private roadmaps, local junctions, credentials, prompts, responses, or Session content.

By submitting a contribution, you agree to license it under `MIT OR Apache-2.0`.
