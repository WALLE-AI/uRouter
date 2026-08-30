# uRouter Threat Model

## Assets

- provider credentials and management credentials;
- tenant prompts, tool outputs, feedback, and routing metadata;
- budget, quota, binding, circuit, and deletion state;
- Catalog, Route, and RouterArtifact integrity;
- audit records and model-cost evidence.

## Trust Boundaries

```text
untrusted client
  -> trusted ingress/auth proxy
  -> uRouter data plane
  -> provider endpoint

operator
  -> uRouter management plane
  -> Catalog/Route/Artifact authority

uRouter instances
  -> Redis shared state
```

`x-urouter-tenant-id` is trusted only when the Gateway is unreachable except through
an authenticated proxy that strips client-supplied identity headers. Direct exposure
with `--require-tenant-header` does not authenticate a tenant.

## Primary Threats And Controls

| Threat | Required control |
|---|---|
| Forged tenant identity | ingress authentication, header stripping, network isolation |
| Provider key disclosure | environment/secret manager injection, redaction, rotation |
| Management privilege escalation | scoped Bearer roles, tenant allowlist, synchronous audit |
| Prompt/tool data leakage | metadata-only default, redaction profiles, retention and deletion |
| Feedback poisoning | source trust, rate limits, anomaly quarantine, tenant consent |
| Budget exhaustion | tenant reservation ledger, hard caps, idempotent settlement |
| Catalog or Artifact tampering | signed manifest, content hash, reviewed publication |
| Cross-tenant Redis access | namespaced keys, tenant indexes, ACL/TLS, authorization tests |
| Replay and duplicate charging | request/decision/attempt identity and idempotency contract |
| SSRF through custom endpoints | reviewed endpoint allowlist, URL validation, no client URL input |
| Metric cardinality denial | fixed label allowlist; tenant/request values only in protected traces |

## Required Deployment Properties

- TLS terminates at a trusted ingress or the Gateway.
- Data-plane and management-plane access are independently restricted.
- Redis uses ACL, TLS where traffic leaves a trusted host, persistence, and backups.
- Provider credentials are never accepted in request bodies or refresh endpoints.
- Logs, metrics, traces, crash dumps, and test fixtures are part of the data policy.

## Outstanding Acceptance Work

Production acceptance requires an ingress header-spoofing test, management isolation
test, Redis ACL/TLS/restore exercise, credential rotation exercise, and deletion/export
audit. Code completion alone does not close these items.
