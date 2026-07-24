# Gemini fixture provenance

`tool_stream.sse` is a deterministic transcription of the public
`models.streamGenerateContent` SSE response shape documented at:

https://ai.google.dev/api/generate-content

Normalization: `responseId`, tool-call id, model text, and token counts are fixed fixture values.
No credential, account identifier, or captured production body is present.
