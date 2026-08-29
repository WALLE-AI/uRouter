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
