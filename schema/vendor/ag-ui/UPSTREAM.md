# Vendored: AG-UI 1.0 JSON Schema

`schema.json` is `spec/1.0/schema.json` from
<https://github.com/ag-ui-protocol/ag-ui> at commit
`ec28a110b71acba229cada5cf15bf15bb5324825` (2026-10-01), unmodified. License:
MIT (`LICENSE`, the upstream repo license).

The schema is THEIRS; Sectr pins it so that `test_agui_conformance.py` can
validate every AG-UI event/request our fixtures carry against the protocol's
own contract (closed objects: a field the schema does not declare fails).
Upstream's `spec/harness` is a regression suite for the schema itself and for
AG-UI's first-party clients, not an endpoint test — see `schema/agui-mapping.md`
"Conformance".

Refresh: copy the new `spec/<version>/schema.json` over this file, update the
commit above, and rerun `uv run pytest` (schema/) and the Node verifier
(`schema/agui-conformance`).
