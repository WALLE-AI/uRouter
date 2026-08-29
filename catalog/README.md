# uRouter catalog

`catalog.json` is the static MVP catalog consumed by `urouter-ai`.

- OpenAI and Anthropic entries carry official source URLs and verification dates.
- `local-fixture` is test-only and is not a production model claim.
- `local-vllm/qwen3.5-4b` and `local-vllm-qwen38/qwen3.8-27b` are pinned from
  real endpoint smoke tests performed on 2026-08-26.
- `manifest.json` pins both the source file and semantic component hashes.
- Money values are USD per one million tokens, serialized as decimal strings.

Validate and inspect the catalog:

```bash
cargo run -q -p urouter-catalog -- check catalog/catalog.json
cargo run -q -p urouter-catalog -- show catalog/catalog.json openai/gpt-5.6-terra
cargo run -q -p urouter-catalog -- list catalog/catalog.json --capability vision
cargo run -q -p urouter-catalog -- cost catalog/catalog.json \
  anthropic/claude-sonnet-4-6 --usage catalog/fixtures/usage-mixed.json
cargo run -q -p urouter-catalog -- --json check catalog/catalog.json
cargo run -q -p urouter-smoke -- --model local-vllm-qwen38/qwen3.8-27b --json
```

Provider inventories are discovered separately from the production Catalog.
Discovery never publishes a model or changes a route:

```powershell
cargo run -q -p urouter-catalog -- providers check
cargo run -q -p urouter-catalog -- providers list
cargo run -q -p urouter-catalog -- sync discover --instance siliconflow-main
cargo run -q -p urouter-catalog -- sync status --instance siliconflow-main
```

See `catalog/providers/README.md` for instance configuration and credential
environment variables.

After an intentional catalog change, review the semantic diff and regenerate the
manifest output. Do not update a hash merely to make CI pass; the source and
pricing evidence must be reviewed first.

```bash
cargo run -q -p urouter-catalog -- diff old.json catalog/catalog.json
cargo run -q -p urouter-catalog -- --json diff old.json catalog/catalog.json
cargo run -q -p urouter-catalog -- manifest catalog/catalog.json
```
