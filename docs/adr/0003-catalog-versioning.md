# ADR 0003: Version immutable catalog snapshots by semantic content

- Status: Accepted
- Date: 2026-08-25

## Decision

A loaded catalog becomes an immutable `CatalogSnapshot`. Canonical serialization
produces separate content, pricing, capability, and compatibility hashes.
Timestamps are metadata and do not affect those hashes.

Every execution record identifies the exact snapshot, canonical model,
provider, API, and price source used for actual cost calculation.

## Rejected alternatives

- A single manually incremented version: it cannot prove content identity.
- One opaque hash only: consumers cannot distinguish a price update from an
  execution-compatibility change.
- Lazy file reads from request handling: a single decision could observe mixed
  catalog versions.

## Consequences

Dynamic refresh, when implemented, must build a complete new snapshot and swap
it atomically. Historical cost replay requires retaining the referenced input
catalog or an equivalent signed artifact.
