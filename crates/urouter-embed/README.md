# urouter-embed

In-process decision facade over the exact `urouter-gateway::RouteConfig::decide`
core. Construct `EmbedRouter` from a validated Catalog and Route, optionally add
an `ArtifactController`, and call `decide` with an `EmbedContext`. It performs no
provider I/O.
