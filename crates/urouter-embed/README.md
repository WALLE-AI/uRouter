# urouter-embed

In-process decision facade over the zero-I/O `urouter-core::RouteConfig::decide`
core. Construct `EmbedRouter` from a validated Catalog and Route, optionally add
an `ArtifactController`, and call `decide` with an `EmbedContext`. The host injects
one immutable `CapacitySnapshot` per decision; `CapacitySnapshot::empty()` selects
the documented capacity-neutral mode. It performs no provider I/O.

`decision_only` demonstrates tier-only embedding. `driver_host` demonstrates the
host-owned `Driver` boundary: uRouter selects the tier/model and the host performs
the model call. Both examples depend on `urouter-core`, not `urouter-gateway`,
`urouter-state`, or `urouter-client`.
