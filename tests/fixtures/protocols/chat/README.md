# OpenAI Chat Completions fixture provenance

These fixtures are hand-normalized protocol snapshots. They are not generated
from `ChatCodec` output.

## Primary source

- OpenAI API Reference, **Chat Completions**, including message parameters,
  function tools, `ChatCompletion`, `ChatCompletionChunk`, finish reasons, and
  `stream_options.include_usage`:
  <https://developers.openai.com/api/reference/resources/chat>
- Snapshot reviewed on 2026-07-24.

Fixture-specific IDs, timestamps, model names, text, tool arguments, usage, and
safe extension fields were replaced with deterministic values. The stream
snapshot keeps partial function arguments as strings and places usage in the
final empty-`choices` chunk immediately before `[DONE]`.
