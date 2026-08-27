# ADR 0002: Use fixed-point money for authoritative costs

- Status: Accepted
- Date: 2026-08-25

## Decision

Authoritative money is stored as signed 128-bit nano-USD. Catalog rates are
nano-USD per one million tokens and serialize as decimal USD strings. Cost
multiplication uses checked arithmetic and deterministic half-up rounding.

Floating-point values may be produced only at metrics or display boundaries.
Actual cost and counterfactual cost estimates use distinct types.

## Rejected alternatives

- `f64`: accumulation and cross-language rounding are not reproducible enough
  for billing evidence or training labels.
- Micro-USD only: low per-token rates can require sub-micro precision.
- Arbitrary precision in the request path: unnecessary for supported price
  ranges and more expensive to integrate across languages.

## Consequences

All arithmetic can fail with a typed overflow error. Cross-language bindings
must consume the same golden vectors and exact integer results.
