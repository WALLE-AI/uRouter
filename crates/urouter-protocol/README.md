# urouter-protocol

Normalized message IR and explicit semantic-loss reports for OpenAI Chat,
OpenAI Responses and Anthropic Messages. Callers choose `LossPolicy::Reject` or
`LossPolicy::AllowDocumented`; unsupported semantics are never silently dropped.
