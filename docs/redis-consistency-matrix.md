# Redis consistency and failure matrix

This matrix is normative for the Gateway data plane. A backend error must never be
silently converted into a successful mutation for a fail-closed domain. Optional
observations may fail open only where explicitly listed.

| Domain | Consistency | Outage policy | Recovery and evidence |
|---|---|---|---|
| Budget | Atomic per tenant and period; request-id idempotent reserve/settle | Fail closed with `503 state_backend_unavailable`; never bypass the hard budget | Conservative reservation remains charged until settlement or period expiry; Redis contract runs before and after restart |
| Quota | Atomic tenant concurrency/RPM/TPM admission and lease settlement | Fail closed with `503 state_backend_unavailable` | Expiring leases bound leaked concurrency; dual-instance contract proves no aggregate over-admission |
| Binding | Compare-and-set by scoped generation | Fail closed for reads and writes used by routing | TTL plus tenant/task tombstone generations prevent resurrection; CAS contract runs across repository instances |
| Circuit | Atomic scoped gate and single Half-Open probe | Fail closed for the affected deployment scope | Expiring probe lease and cooldown permit recovery; scope isolation is contract-tested |
| Decision/feedback | Tenant-scoped authoritative Redis records | Fail closed for management reads/writes and retained record writes | Immutable records and idempotent feedback are reconciled by key; cross-instance contract is restart-tested |
| Metrics counters | Process-local, monotonic best effort | Fail open | Scrape gaps are observable externally; metrics never authorize traffic |
| Latency EWMA / picker signal | Process-local advisory value | Fail open to deterministic weighted selection | New samples rebuild the EWMA; no correctness decision depends on it |

The automated gate is `cargo run -p urouter-xtask -- chaos`. It starts a repository-owned,
uniquely named Redis container, runs all ignored Redis contracts, restarts that exact
container, reruns the contracts, and writes `target/p2-chaos-report.json`. Any failed
contract or configured `urouter-soak` SLO threshold exits non-zero.
