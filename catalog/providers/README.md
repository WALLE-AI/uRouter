# Provider inventory inputs

This directory contains provider instance definitions and discovery evidence. It
does not directly control production routing.

Validate and list instances without reading credentials:

```powershell
cargo run -q -p urouter-catalog -- providers check
cargo run -q -p urouter-catalog -- providers list
```

Discover one provider. Credentials and workspace IDs are read from environment
variables and are never written to snapshots:

```powershell
cargo run -q -p urouter-catalog -- sync discover --instance siliconflow-main

$env:BAILIAN_WORKSPACE_ID = "your-workspace-id"
cargo run -q -p urouter-catalog -- sync discover --instance bailian-cn-beijing-main

cargo run -q -p urouter-catalog -- sync discover --instance ark-cn-beijing-main
```

Compare the last-good snapshot with the published Catalog:

```powershell
cargo run -q -p urouter-catalog -- sync status --instance siliconflow-main
```

Discovery snapshots are stored under `catalog/providers/state/<instance>/`.
Each content hash is immutable, while `latest.json` points to the last complete
successful fetch. Discovery never edits `catalog/catalog.json`,
`catalog/manifest.json`, or a gateway route.

Each HTTP discovery instance may set `min_request_interval_ms`. Bailian applies
it between pagination calls; capability probes are deliberately sequential.

## Review And Candidate

Unknown provider fields are never inferred. Create a review document conforming
to `catalog/schema/provider-review.schema.json`; each model needs non-empty
evidence for `pricing`, `capabilities`, `compat`, `lifecycle`, and `source`.
Then build an immutable quarantined candidate:

```powershell
cargo run -q -p urouter-catalog -- sync candidate `
  --instance siliconflow-main `
  --reviews catalog/providers/reviews/siliconflow-main.json
```

The output conforms to `catalog/schema/provider-candidate.schema.json`. Missing
reviews, evidence, conflicting canonical IDs/aliases, provider mismatches and
retired models keep `complete=false` and cannot be published.

Capability probes require an explicit maximum spend and per-request estimate.
They retain status, latency, time and response hashes, never response content:

```powershell
cargo run -q -p urouter-catalog -- sync probe `
  --instance siliconflow-main `
  --model Pro/deepseek-ai/DeepSeek-R1 `
  --budget-nano-usd 40000000 `
  --estimated-request-nano-usd 10000000 `
  --max-output-tokens 128
```

This command makes paid model calls. Run it only after the budget and credential
have been deliberately approved.

## Publish And Roll Back

Bootstrap the current Catalog/Route control revision without changing either
input:

```powershell
cargo run -q -p urouter-catalog -- sync control-manifest
```

Production should sign the manifest with a secret supplied only through an
environment variable:

```powershell
cargo run -q -p urouter-catalog -- sync control-manifest `
  --signing-key-env UROUTER_CONTROL_SIGNING_KEY
```

Publish accepts only an integrity-valid `complete=true` candidate. It stages the
Catalog, Catalog manifest and control manifest, commits the control manifest
last, and retains `.previous` files:

```powershell
cargo run -q -p urouter-catalog -- sync publish `
  --candidate catalog/providers/candidates/siliconflow-main/<hash>.json `
  --signing-key-env UROUTER_CONTROL_SIGNING_KEY

cargo run -q -p urouter-catalog -- sync rollback
```

Start the Gateway with the same signing key and periodic reload. A request holds
one immutable Catalog/Route snapshot for its entire lifetime. Invalid or partial
files are rejected and the configured `last_good` or `fail_closed` readiness
policy applies:

```powershell
cargo run -q -p urouter-gateway -- `
  --bind 127.0.0.1:8787 `
  --control-manifest gateway/control-manifest.json `
  --control-signing-key-env UROUTER_CONTROL_SIGNING_KEY `
  --control-reload-seconds 5 `
  --control-failure-policy last_good
```

`GET /v1/catalog` returns the active revision and an `ETag`. Admin-only refresh
and rollback operations are `POST /v1/catalog/refresh` and
`POST /v1/catalog/rollback`; they never accept a credential in the request.

## Credentials And Retirement

Catalog providers support `api_key_env`, `ambient_env`, and
`oauth_bearer_file`. The bearer file is owned and atomically rotated by an
external OAuth/workload-identity agent; the Gateway rereads it per request. It
does not implement an OAuth authorization-code or client-credentials exchange.
Errors and logs expose only the configured credential scope, never the secret or
file content.

Retire a deployment in two phases:

1. Set `accept_new_requests=false` and a Unix-second
   `binding_grace_until_unix` in the Route. New work is excluded while existing
   task/session bindings continue until the deadline.
2. After the grace period, migrate or close remaining bindings and mark the
   Catalog model `retired`. Catalog admission then excludes it globally.

Marking the Catalog model retired before the binding grace period would bypass
the grace mechanism and is therefore invalid operational ordering.
