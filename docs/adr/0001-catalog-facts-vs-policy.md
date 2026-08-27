# ADR 0001: Separate catalog facts from routing policy

- Status: Accepted
- Date: 2026-08-25

## Decision

`urouter-ai` stores externally verifiable model and provider facts: identifiers,
wire API, capabilities, compatibility behavior, lifecycle, endpoint metadata,
and pricing. It does not store tiers, workload suitability scores, learned
quality, or user preference weights.

Routing policy will reference canonical model IDs from an immutable catalog
snapshot. This prevents a routine catalog update from silently changing a
trained policy and keeps fact provenance independently auditable.

## Rejected alternatives

- Storing `efficient/balanced/capable` in `ModelSpec`: tiers are route-specific.
- Storing coding or planning scores in the catalog: these are evaluated policy
  inputs with their own dataset and version, not provider facts.
- Letting deployments copy model facts: copies drift and corrupt filters and
  cost records.

## Consequences

The future policy layer needs a separate `ModelProfile` or artifact schema.
Deployments may only override deployment facts and explicitly audited prices.
