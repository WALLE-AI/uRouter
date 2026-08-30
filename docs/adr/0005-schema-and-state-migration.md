# ADR 0005: Schema And Shared-State Migration

Status: Accepted for execution

## Context

Gateway instances can run different binaries during a rolling deployment. Redis
bindings, circuits, records, feedback, Catalog snapshots, and management API bodies
cannot be changed atomically with every process.

## Decision

Every persisted or external schema has an explicit integer version and a registry in
the requirements matrix. A version change documents:

1. old-reader/new-writer compatibility;
2. new-reader/old-writer compatibility;
3. dual-read or dual-write duration;
4. backfill and deletion behavior;
5. rollback behavior after new data has been written.

Redis keys use a namespace version, for example `prefix:v2:<kind>:<id>`. Migration is
copy-on-read or an explicit bounded job; key renames in place are forbidden. During a
dual-write window, one version is authoritative and reconciliation metrics are
required. Tombstone/deletion generations apply to every version so migration cannot
resurrect deleted tenant data.

Decision records are immutable events. A new reader may project old events into a
new in-memory representation, but must not invent candidate sets, propensity, or
revision values. Such records are marked incomplete and excluded from training.

Catalog, Route, Deployment, Feature, Policy, and Artifact revisions are captured once
at request admission. One request never observes a mixture of revisions.

## Rollout Gate

- mixed-version contract tests pass;
- a downgrade test reads state written by the candidate version;
- reconciliation reaches zero unexplained differences;
- the rollback command and data-retention effect are documented.
