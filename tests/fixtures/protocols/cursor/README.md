# Cursor fixture provenance

The `.connect.hex` files are source-clear protobuf/Connect transcriptions
from OpenCodex v2.7.35 commit
`97e7326f89bcfbb29a2c73250cb25eb801d066b6`:

https://github.com/lidge-jun/opencodex/blob/97e7326f89bcfbb29a2c73250cb25eb801d066b6/src/adapters/cursor/gen/agent_pb.ts
https://github.com/lidge-jun/opencodex/blob/97e7326f89bcfbb29a2c73250cb25eb801d066b6/tests/cursor-framing.test.ts

The opt-in/default-disabled and native-local-execution-disabled behavior is normalized from:

https://github.com/lidge-jun/opencodex/blob/97e7326f89bcfbb29a2c73250cb25eb801d066b6/src/adapters/cursor/transport.ts
https://github.com/lidge-jun/opencodex/blob/97e7326f89bcfbb29a2c73250cb25eb801d066b6/src/adapters/cursor/exec-policy.ts

The generated schema supplies the exact field numbers, and the framing test
supplies the five-byte Connect envelope. Normalization: request ids, model text,
tool arguments, and usage counts are fixed synthetic values. No token, account
identifier, internal endpoint, command output, captured traffic, or production
body is present.

Independent verification uses Python `google.protobuf==6.33.5`, not the Rust
production codec. The committed probe defines the pinned message subset in a
dynamic `FileDescriptorProto`, verifies the SHA-256 of every fixture, parses
every response payload, and asserts
`SerializeToString(deterministic=True) == payload`; it also independently
encodes the normalized request fixture. Exact invocation:

```text
python tests/fixtures/protocols/cursor/verify_fixtures.py
```

The expected summary identifies, in order, `text_delta("Hello")`,
`thinking_delta("Checking.")`, MCP `weather/call_cursor`,
two cumulative `partial_tool_call` values ending in `{"city":"Paris"}`,
`token_delta(3)`, `turn_ended`, Connect end-stream `{}`, and the separate
`exec_server_message`. The request output was a 97-byte protobuf
payload inside a flags-zero Connect frame.

Fixture file SHA-256:

```text
request/run.connect.hex  602e78062b9e339314ecc5ba8ec38695f82fa6e3311a9fd378e64f453dc6d42e
stream/tool.connect.hex  b9fc35502595b7caa4343d4ddfba05684b89ba3473a4475f26f0a1eca302f592
stream/exec.connect.hex  68a9bb76c85d22d98ff0f36af18b6c619cb179badcd42623a122ec9e621e03e5
```
