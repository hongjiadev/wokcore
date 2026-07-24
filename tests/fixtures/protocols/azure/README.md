# Azure OpenAI fixture provenance

The request endpoint and Chat Completions response/SSE shapes are deterministic transcriptions of
the Azure OpenAI REST reference:

https://learn.microsoft.com/azure/ai-services/openai/reference

Normalization: response ids, timestamps, synthetic deployment labels, tool arguments, and usage
counts are fixed fixture values. No real Azure resource URL, deployment, API key, account, or
production body is captured.
