# OpenAI Responses fixture provenance

These fixtures are hand-normalized protocol snapshots. They are not generated
from `ResponsesCodec` output.

## Primary source

- OpenAI API Reference, **Streaming events**, sections `response.created`,
  `response.completed`, returned `Reasoning` item, and the text/reasoning/tool
  lifecycle events:
  <https://platform.openai.com/docs/api-reference/responses-streaming/response/refusal/delta?lang=curl>
- Snapshot reviewed on 2026-07-24.

The reference examples supply the formal Response shape and event field names.
Fixture-specific IDs, timestamps, model names, text, tool arguments, usage, and
trusted request metadata were substituted with deterministic test values.

## Formal Response normalization

The normalized Response snapshot has exactly these 24 keys. No optional
provider fields are silently copied into the fixture.

| Field | Fixture source/defaulting rule |
| --- | --- |
| `id` | Derived from canonical `Created.response_id`. |
| `object` | Schema discriminator, always `"response"`. |
| `created_at` | Copied from `ResponsesEncodeContext.created_at`. |
| `status` | Derived from lifecycle: `"in_progress"` or `"completed"`. |
| `completed_at` | `null` while created; completed response copies the caller-supplied nullable template value. |
| `error` | Copied from the caller-supplied nullable template; `null` in this successful fixture. |
| `incomplete_details` | Copied from the caller-supplied nullable template; `null` in this completed fixture. |
| `instructions` | Copied verbatim from the caller-supplied string-or-array template value. |
| `max_output_tokens` | Copied from the caller-supplied nullable template value. |
| `model` | Copied from `ResponsesEncodeContext.model`. |
| `output` | Empty on creation; assembled from canonical output items on completion. |
| `parallel_tool_calls` | Copied from the caller-supplied boolean; no codec default. |
| `previous_response_id` | Copied from the caller-supplied nullable template value. |
| `reasoning` | Copied verbatim from the caller-supplied reasoning object. |
| `store` | Copied from the caller-supplied boolean; no codec default. |
| `temperature` | Copied from the caller-supplied nullable numeric template value. |
| `text` | Copied verbatim from the caller-supplied text configuration object. |
| `tool_choice` | Copied verbatim from the caller-supplied string-or-object value. |
| `tools` | Copied verbatim from the caller-supplied tool array. |
| `top_p` | Copied from the caller-supplied nullable numeric template value. |
| `truncation` | Copied verbatim from the caller-supplied value. |
| `usage` | `null` while created; assembled from canonical `Usage` on completion. |
| `user` | Copied from the caller-supplied nullable template; explicitly `null` here. |
| `metadata` | Copied from the caller-supplied metadata map. |

Fields whose schemas are unions or nested provider objects remain
`serde_json::Value` in the trusted template so the codec does not narrow or
invent provider metadata. Required booleans, arrays, maps, timestamps, and
nullable scalars use explicit Rust field types. Version-specific optional
Response fields outside this 24-key reference snapshot are intentionally out of
scope rather than synthesized.

## Output-item normalization

- Message, reasoning, and function-call items receive their first-seen
  `output_index`.
- Returned reasoning items explicitly use `status: "in_progress"` in
  `response.output_item.added` and `status: "completed"` in done/final output.
- The stream remains the independently specified 18-frame lifecycle in
  `stream/ordered.json`; adding formal Response metadata does not add, remove,
  or reorder frames.
