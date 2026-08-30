# ADR 0006: Request Identity And Idempotency

Status: Accepted for execution

## Context

A client retry, Gateway retry, tier fallback, and provider retry are different events.
Without separate identities, costs can be charged twice and feedback can attach to
the wrong execution.

## Decision

Use four identifiers:

| Identifier | Scope | Created by |
|---|---|---|
| `request_id` | one logical client operation | Gateway, or validated client idempotency key |
| `decision_id` | one immutable routing decision | policy host |
| `attempt_id` | one provider execution attempt | runtime |
| `provider_request_id` | provider-side operation | provider response when available |

The Gateway accepts an optional bounded `Idempotency-Key`. It is tenant-scoped and
hashed before persistence. Reuse with a different canonical request hash is rejected.
The initial P0 contract records identities but does not promise response replay.

Budget reservation is keyed by `request_id`; settlement entries are keyed by
`attempt_id`. Feedback references `decision_id` plus turn/signal identity and remains
idempotent. Logs and traces may contain these opaque identifiers but never the raw
tenant idempotency key.

## Failure Rules

- a retry creates a new `attempt_id`, never a new charge for the same attempt;
- a new policy decision creates a new `decision_id` linked to the same `request_id`;
- first-token partial failure is not automatically replayed;
- missing provider usage is settled with an explicitly marked conservative estimate.
