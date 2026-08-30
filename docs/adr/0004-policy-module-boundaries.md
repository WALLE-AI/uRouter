# ADR 0004: Policy Module Boundaries

Status: Accepted for execution

## Context

Routing, capacity, retry, persistence, and HTTP execution currently meet in the
Gateway. The target design names many crates, but creating all of them before their
interfaces stabilize would turn internal details into public dependency contracts.

## Decision

Introduce boundaries incrementally:

```text
urouter-contracts <- urouter-ai, urouter-feature, urouter-policy
urouter-policy    <- quality, capacity, reliability modules
urouter-state     <- memory and Redis adapters
urouter-runtime   <- provider transport and execution
urouter-gateway   <- composition root only
```

`urouter-contracts`, `urouter-feature`, and `urouter-policy` are pure. They may not
read the clock, environment, filesystem, network, or Redis. Time, health, quota,
catalog, and configuration are explicit immutable inputs.

Quality, capacity, and reliability remain modules inside `urouter-policy` until an
independent release or dependency boundary justifies another crate. No empty crate is
created to match a diagram.

P1 returns a bounded `RoutingPlan`; it does not introduce internal model calls.
`Step::CallModel` is deferred until Judge/Escalation work and must have recursion,
depth, cancellation, and budget limits.

## Consequences

- Gateway behavior can migrate one pure function at a time under parity tests.
- Offline evaluation reuses exactly the online policy code.
- Fewer public crates reduce version and cyclic-dependency pressure.
- The physical crate tree can differ from the conceptual design without weakening
  the zero-I/O invariant.
