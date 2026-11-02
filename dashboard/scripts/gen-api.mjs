/*
  Generates TS types from the schema contracts (drift rule: edit
  `schema/*.yaml|json` first, regenerate, implementations follow).

  `npx openapi-typescript ../schema/sessions-api.yaml` alone does NOT work:
  event.schema.json carries an `$id` (https://sectr.dev/schemas/...), which
  is the JSON-Schema base URI — the resolver then fetches internal `$defs`
  refs over the network instead of resolving the local file (403 here;
  wrong even on a networked machine). Fix: codegen runs against TEMP COPIES
  with the `$id` neutralized; the authoritative contracts are untouched and
  the generated output is committed, so `gen:api` + `git diff` doubles as
  the drift check in CI.
*/
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { execFileSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import openapiTS, { astToString } from "openapi-typescript";

const SCHEMA = join(import.meta.dirname, "../../schema");

// Walk every $ref string in a schema tree; apply fn, mutating in place.
function walkRefs(node, fn) {
  if (Array.isArray(node)) return node.forEach((n) => walkRefs(n, fn));
  if (typeof node !== "object" || node === null) return;
  if (typeof node.$ref === "string") {
    const mapped = fn(node.$ref);
    if (mapped !== null) node.$ref = mapped;
  }
  for (const value of Object.values(node)) walkRefs(value, fn);
}
const OUT = join(import.meta.dirname, "../src/lib/transport/generated/sessions-api.d.ts");

// Temp copies: sessions-api.yaml + event.schema.json with $id stripped.
const tmp = mkdtempSync(join(tmpdir(), "sectr-codegen-"));
const event = JSON.parse(readFileSync(join(SCHEMA, "event.schema.json"), "utf8"));
delete event.$id;
// $defs/envelope self-refs the absolute $id URL (for cross-file $refs);
// locally it must resolve to this document's root.
const selfRef = "https://sectr.dev/schemas/event.schema.json";
walkRefs(event, (ref) => ref === selfRef ? "#" : null);
writeFileSync(join(tmp, "event.schema.json"), JSON.stringify(event));
writeFileSync(
  join(tmp, "sessions-api.yaml"),
  readFileSync(join(SCHEMA, "sessions-api.yaml"), "utf8").replace(
    'info:', '# $id-neutralized event.schema.json copy beside this file makes ./ refs local\ninfo:',
  ),
);
// dev-api composes sessions-api (and event.schema.json) by relative $ref —
// copies beside it in the same tmp dir resolve.
writeFileSync(join(tmp, "dev-api.yaml"), readFileSync(join(SCHEMA, "dev-api.yaml"), "utf8"));

try {
  const ast = await openapiTS(new URL(`file://${join(tmp, "sessions-api.yaml")}`));
  mkdirSync(join(import.meta.dirname, "../src/lib/transport/generated"), { recursive: true });
  writeFileSync(OUT, `/* GENERATED from schema/sessions-api.yaml + schema/event.schema.json — do not edit.\n   Run \`npm run gen:api\` after any contract change. */\n${astToString(ast)}\n`);
  console.log(`wrote ${OUT}`);
  // dev-api.yaml = the core + dev-only extras (/dev/status -> DevStatus);
  // sibling generated file.
  const devOut = join(import.meta.dirname, "../src/lib/transport/generated/dev-api.d.ts");
  const devAst = await openapiTS(new URL(`file://${join(tmp, "dev-api.yaml")}`));
  writeFileSync(devOut, `/* GENERATED from schema/dev-api.yaml (composes sessions-api.yaml) — do not edit.\n   Run \`npm run gen:api\` after any contract change. */\n${astToString(devAst)}\n`);
  console.log(`wrote ${devOut}`);
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
void execFileSync; // (kept only if we later shell out to the CLI instead)
