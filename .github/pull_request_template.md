## Catalog change checklist

- [ ] Every changed fact links to an authoritative source and updates `checked_at`.
- [ ] Pricing changes include or update a golden cost fixture.
- [ ] Capability and compatibility fields use `unknown` when evidence is incomplete.
- [ ] Custom providers explicitly define every compatibility field.
- [ ] No endpoint credential, authorization value, or private network address is committed.
- [ ] `urouter-catalog diff` was reviewed for pricing, capability, compatibility, and lifecycle changes.
- [ ] Any pricing change has an explicit reviewer and an authoritative effective date/source.
- [ ] Local endpoint capabilities are backed by a saved `urouter-smoke --json` report.
- [ ] `catalog/manifest.json` was regenerated and `urouter-catalog check` passes.
