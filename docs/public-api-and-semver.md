# Public API And Semver Policy

The publishable P6 surface is split by responsibility:

- `urouter-contracts`: versioned FeatureFrame and immutable decision evidence.
- `urouter-protocol`: normalized messages and explicit semantic-loss policy.
- `urouter-client`: replay-aware Gateway address failover.
- `urouter-embed`: in-process facade over the exact Gateway decision core.
- `urouter-artifact`: signed policy schema, bounded infer and rollout controller.
- `urouter-eval`: governed offline dataset and statistical evaluation types.

All crates use workspace semver. Adding optional fields with serde defaults and
adding APIs is minor-compatible. Removing/renaming fields, changing enum wire
names, changing routing semantics for the same revision, or increasing required
feature/artifact schema versions is breaking. Non-exhaustive provider enums must
retain an explicit unsupported-adapter path.

Feature, DecisionRecord, dataset and artifact schema versions evolve separately
from crate versions. Readers must reject unsupported forward versions and may
read documented older versions only without inventing propensity, consent,
revision or semantic-loss evidence.

`urouter-client` retries only requests declared replay-safe. Once streaming body
bytes have started, the client returns that response and never replays it on a
different Gateway. `urouter-embed` performs no provider I/O and must remain tier
parity-compatible with the Gateway core for identical Catalog, Route, artifact
and context inputs.
