/*
 * gen-spec — bundle schema/public-api.yaml into a single self-contained
 * OpenAPI document at public/spec/public-api.yaml.
 *
 * The source spec composes schema/sessions-api.yaml via external $refs
 * (path-item and method-level), and sessions-api.yaml refs
 * schema/event.schema.json. Those files used to be served verbatim for
 * Scalar to dereference at runtime — but Scalar's browser runtime cannot
 * resolve external sibling documents ("Failed to resolve external
 * reference ... missing a loader for this type of reference"), so every
 * $ref'd operation was missing from the rendered reference. We therefore
 * dereference the composition at GENERATION time into one file:
 *
 *   - sessions-api.yaml's paths (via $refs) and components/schemas +
 *     components/responses are inlined; its auth-agnostic root `security: []` is deliberately NOT
 *     copied — the composing document's root security applies to the
 *     inlined operations, exactly as the $ref'd composition specified.
 *   - event.schema.json is inlined as components/schemas/EventEnvelope
 *     (its $id is stripped; internal refs are rewritten to the new base).
 *
 * `check:spec` regenerates and `git diff --exit-code` fails when the
 * bundle moves without a re-gen — same discipline as gen:cli.
 *
 * NOTE: the bundled YAML is a derived artifact (comments stripped). The
 * authoritative, commented specs remain schema/*.yaml.
 */
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { readFileSync } from "node:fs";
import yaml from "js-yaml";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const SCHEMA = join(ROOT, "..", "schema");
const OUT = join(ROOT, "public", "spec");

const SESSIONS_FILE = "sessions-api.yaml";
const SESSIONS_PREFIX = `${SESSIONS_FILE}#`;
const EVENT_FILE = "event.schema.json";
/** Name under components/schemas for the inlined event.schema.json. */
const EVENT_SCHEMA_NAME = "EventEnvelope";

/** Walk an object tree; invoke fn(node) for every object node. */
function walkObjects(node, fn) {
    if (Array.isArray(node)) {
        node.forEach((item) => walkObjects(item, fn));
    } else if (node && typeof node === "object") {
        fn(node);
        for (const value of Object.values(node)) walkObjects(value, fn);
    }
}

/** JSON-pointer get. `pointer` is the raw $ref pointer (with ~1/~0 escapes). */
function getAtPointer(doc, pointer) {
    let node = doc;
    for (const token of pointer.split("/").slice(1)) {
        const key = token.replaceAll("~1", "/").replaceAll("~0", "~");
        if (node == null || typeof node !== "object" || !(key in node)) {
            throw new Error(`cannot resolve ${pointer} (no "${key}")`);
        }
        node = node[key];
    }
    return node;
}

/**
 * Deep-clone `node`, resolving every `"$ref": "<prefix>#<pointer>"`:
 *   - a ref into the core's components becomes the local "#" form: they
 *     are merged into ours, so the bundle keeps named schemas and
 *     responses instead of a copy at every use;
 *   - any other pure { $ref } object is replaced by its dereferenced
 *     target (path-item and method-level refs);
 *   - a $ref with sibling keywords keeps them and the ref is rewritten to
 *     the local "#" form (schema refs — targets live in merged components).
 */
function dereferenceRefs(node, prefix, resolve) {
    if (Array.isArray(node)) return node.map((item) => dereferenceRefs(item, prefix, resolve));
    if (!node || typeof node !== "object") return node;

    const keys = Object.keys(node);
    if (keys.length === 1 && typeof node.$ref === "string" && node.$ref.startsWith(prefix)) {
        const pointer = node.$ref.slice(prefix.length);
        if (pointer.startsWith("/components/")) return { $ref: `#${pointer}` };
        return resolve(node.$ref); // pure $ref: substitute the target
    }

    const out = {};
    for (const [key, value] of Object.entries(node)) {
        if (
            key === "$ref" &&
            typeof value === "string" &&
            value.startsWith(prefix)
        ) {
            out.$ref = `#${value.slice(prefix.length)}`; // keep siblings, localize
        } else {
            out[key] = dereferenceRefs(value, prefix, resolve);
        }
    }
    return out;
}

/**
 * Bundle the public API spec. Returns the composed, self-contained
 * OpenAPI document as a plain object.
 */
export function bundlePublicApi() {
    const pub = yaml.load(readFileSync(join(SCHEMA, "public-api.yaml"), "utf8"));
    const ses = yaml.load(readFileSync(join(SCHEMA, SESSIONS_FILE), "utf8"));
    const event = JSON.parse(readFileSync(join(SCHEMA, EVENT_FILE), "utf8"));

    // ── 1. Inline event.schema.json as components/schemas/EventEnvelope ──
    // Strip $id first: once embedded, its "#/…" refs must resolve against
    // the composing document, not the embedded document's own $id.
    const eventId = event.$id;
    delete event.$id;
    walkObjects(event, (node) => {
        if (typeof node.$ref !== "string") return;
        if (node.$ref.startsWith("#/")) {
            node.$ref = `#/components/schemas/${EVENT_SCHEMA_NAME}${node.$ref.slice(1)}`;
        } else if (node.$ref === eventId) {
            node.$ref = `#/components/schemas/${EVENT_SCHEMA_NAME}`;
        } else {
            throw new Error(`unexpected ref in ${EVENT_FILE}: ${node.$ref}`);
        }
    });
    pub.components.schemas[EVENT_SCHEMA_NAME] = event;

    // ── 2. Rewrite both documents' refs to the inlined event schema ──────
    // (public-api's inline copy of the events operation refs it directly.
    // Sibling keywords — e.g. the description beside PendingApproval's
    // event ref — are preserved: only the ref string changes.)
    let eventRefs = 0;
    walkObjects([ses, pub.paths], (node) => {
        if (typeof node.$ref === "string" && node.$ref.endsWith(EVENT_FILE)) {
            node.$ref = `#/components/schemas/${EVENT_SCHEMA_NAME}`;
            eventRefs++;
        }
    });
    if (eventRefs === 0) {
        throw new Error(`expected ${SESSIONS_FILE} to reference ${EVENT_FILE}`);
    }

    // ── 3. Merge sessions-api components (schemas, shared responses such
    // as TooManyRequests) into public-api's: the inlined paths keep their
    // internal "#/components/…" refs, which must resolve in the bundle ──
    for (const kind of ["schemas", "responses"]) {
        const theirs = ses.components[kind] ?? {};
        const ours = (pub.components[kind] ??= {});
        const collide = Object.keys(theirs).filter((k) => k in ours);
        if (collide.length) throw new Error(`${kind} name collisions: ${collide.join(", ")}`);
        Object.assign(ours, theirs);
    }

    // ── 4. Dereference sessions-api $refs in the public document ─────────
    let dereferenced = 0;
    const bundled = dereferenceRefs(pub, SESSIONS_PREFIX, (ref) => {
        dereferenced++;
        return getAtPointer(ses, ref.slice(SESSIONS_PREFIX.length));
    });
    if (dereferenced === 0) throw new Error("no sessions-api refs found — composition changed?");

    // ── 5. Validate: only local refs remain, and every local ref resolves ─
    walkObjects(bundled, (node) => {
        if (typeof node.$ref !== "string") return;
        if (!node.$ref.startsWith("#/")) {
            throw new Error(`unresolved external $ref: ${node.$ref}`);
        }
        getAtPointer(bundled, node.$ref.slice(1)); // throws if dangling
    });

    return bundled;
}

function main() {
    mkdirSync(OUT, { recursive: true });
    const bundled = bundlePublicApi();
    const header = [
        "# GENERATED FILE — do not edit.",
        "# Bundled from schema/public-api.yaml (+ sessions-api.yaml paths/components,",
        `# event.schema.json inlined as components/schemas/${EVENT_SCHEMA_NAME}) by scripts/gen-spec.mjs.`,
        "# Source of truth: schema/*.yaml — regenerate with `npm run gen`.",
        "",
    ].join("\n");
    writeFileSync(
        join(OUT, "public-api.yaml"),
        header + yaml.dump(bundled, { lineWidth: 100, noRefs: true }),
    );
    console.log(
        `bundled schema/public-api.yaml (+ ${SESSIONS_FILE}, ${EVENT_FILE}) → public/spec/public-api.yaml`,
    );
}

// Run directly (`node scripts/gen-spec.mjs`); skipped when imported for tests.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
    main();
}
