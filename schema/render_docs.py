#!/usr/bin/env python3
"""Render schema/REFERENCE.md — a human-readable reference generated from the
JSON Schemas, so the readable version can never drift from the real contract.

    uv run schema/render_docs.py            # (re)generate schema/REFERENCE.md
    uv run schema/render_docs.py --check    # exit 1 if it is stale
"""

import json
import sys
from pathlib import Path

SCHEMA_DIR = Path(__file__).resolve().parent
REFERENCE = SCHEMA_DIR / "REFERENCE.md"

RUNNER_EMITTER_NOTE = {
    "MESSAGE_START": "Runner",
    "MESSAGE_DELTA": "Runner",
    "MESSAGE_END": "Runner",
    "TOOL_CALL": "Runner",
    "TOOL_APPROVAL_REQUIRED": "Runner",
    "ERROR": "Runner",
    "TURN_END": "Runner",
    "SESSION_CREATED": "Platform",
    "INVOCATION_STARTED": "Platform",
    "TOOL_RESULT": "Platform",
    "TOOL_APPROVAL_GRANTED": "Platform",
    "TOOL_APPROVAL_DENIED": "Platform",
    "SESSION_ERROR": "Platform",
}


def load(name: str) -> dict:
    return json.loads((SCHEMA_DIR / name).read_text())


def fmt_type(prop: dict, defs: dict) -> str:
    if isinstance(prop, bool) or prop == {}:
        return "any JSON"
    if "$ref" in prop:
        name = prop["$ref"].rsplit("/", 1)[-1]
        target = defs.get(name, {})
        if name == "eventType":
            return "event type (enum below)"
        if target.get("pattern", "").startswith("^[0-9a-f]{8}-[0-9a-f]{4}-7"):
            return "UUIDv7"
        if target.get("format") == "date-time":
            return "RFC 3339 timestamp"
        if "pattern" in target:
            return "UUID"
        if name == "transcript":
            return "array of event envelopes"
        return name
    if "enum" in prop:
        vals = " \\| ".join(f"`{v}`" for v in prop["enum"])
        return f"enum: {vals}"
    if "anyOf" in prop:
        return " or ".join(fmt_type(p, defs) for p in prop["anyOf"])
    t = prop.get("type")
    if t is None:
        return "any JSON"
    if t == "integer":
        t += " ≥ 0" if prop.get("minimum", 0) == 0 else ""
    if t == "array":
        return "array"
    return t


def spec_desc(spec) -> str:
    """Description of a property spec; specs may be `true` (any JSON)."""
    return (spec.get("description") if isinstance(spec, dict) else "") or ""


def payload_table(typ: str, defs: dict) -> str:
    payload = defs[typ]
    lines = ["| field | type | req | description |", "|---|---|---|---|"]
    for field, spec in payload.get("properties", {}).items():
        req = "✔" if field in payload.get("required", []) else ""
        desc = spec_desc(spec).replace("|", "\\|").replace("\n", " ")
        lines.append(f"| `{field}` | {fmt_type(spec, defs)} | {req} | {desc} |")
    return "\n".join(lines)


def event_types_section(event: dict) -> str:
    defs = event["$defs"]
    runner = defs["runnerEventType"]["enum"]
    platform = [t for t in defs["eventType"]["enum"] if t not in runner]
    out = ["## Event types", ""]
    for label, types, note in [
        ("Runner-emitted", runner,
         "Emitted by the user process on the invoke response stream; validated "
         "and journaled by the sidecar."),
        ("Platform-emitted", platform,
         "Written by the server/sidecar only — never by runners."),
    ]:
        out += [f"### {label}", "", f"> {note}", ""]
        for typ in types:
            summary = (defs[typ].get("description") or "").split(". ")[0]
            out += [f"#### `{typ}` — {summary}", "", payload_table(typ, defs), ""]
    return "\n".join(out)


def envelope_section(event: dict) -> str:
    defs = event["$defs"]
    lines = ["## Event envelope", "",
             "Every journal row and every client SSE frame. `payload` is bound "
             "to a schema by the `type` discriminator (see below).", "",
             "| field | type | req | description |", "|---|---|---|---|"]
    for field, spec in event["properties"].items():
        desc = spec_desc(spec).replace("|", "\\|")
        if not desc:
            desc = {
                "seq": "Per-session, monotonic, gap-free within a turn, starts at 0. Assigned by the journal writer, never user code.",
                "payload": "Object; schema per event type (see tables below).",
                "deployment_id": "Deployment the session is pinned to.",
            }.get(field, "")
        lines.append(f"| `{field}` | {fmt_type(spec, defs)} | ✔ | {desc} |")
    return "\n".join(lines)


def invocation_section(inv: dict, event: dict) -> str:
    req = inv["$defs"]["invocationRequest"]
    lines = ["## Runner invoke contract", "",
             "`POST http://localhost:$SECTR_PORT/invocations` — sidecar → user "
             "process. Response: `200`, `text/event-stream`; each frame's "
             "`data:` is a runner event (`{type, payload}` only). Stream end = "
             "invocation end. The TURN ends when `NO_MORE_ACTIONS` is emitted "
             "(SDK driver: generator exhaustion, or after an unhandled error's "
             "ERROR event); an approval pause leaves `TOOL_APPROVAL_REQUIRED` "
             "as the final frame and does NOT end the turn. The transcript "
             "already includes this invocation's `INVOCATION_STARTED` — the "
             "canonical messages view already contains the current user "
             "message.", "",
             "### Request body", "",
             "| field | type | req | description |", "|---|---|---|---|"]
    for field, spec in req["properties"].items():
        desc = spec_desc(spec).replace("|", "\\|").replace("\n", " ")
        if not desc:
            desc = {
                "session_id": "Session being invoked.",
                "invocation_id": "This turn's invocation; matches the INVOCATION_STARTED event.",
                "route": "Agent route, e.g. `/customer-support`.",
            }.get(field, "")
        lines.append(f"| `{field}` | {fmt_type(spec, event['$defs'])} | ✔ | {desc} |")
    lines += ["", "### Invocation reasons (`reason`)", "",
              "| kind | fields | meaning |", "|---|---|---|"]
    meanings = {
        "user_message": "Starts a turn (first or follow-up); `input` is the user's message text.",
        "approval_decision": "Resumes a turn suspended by TOOL_APPROVAL_REQUIRED; `decision` = `{approved, approver?}`.",
        "tool_result": "Post-MVP: resumes with an externally-computed tool output.",
    }
    for branch in event["$defs"]["invocationReason"]["oneOf"]:
        kind = branch["properties"]["kind"]["const"]
        fields = ", ".join(f"`{k}`" for k in branch["properties"] if k != "kind")
        lines.append(f"| `{kind}` | {fields} | {meanings[kind]} |")
    lines += [""]
    return "\n".join(lines)


def example_section(event: dict) -> str:
    ex = json.loads((SCHEMA_DIR / "fixtures/valid/event/TOOL_APPROVAL_REQUIRED.json").read_text())
    lines = ["## Worked example — approval pause", "",
             "The turn below is SUSPENDED by `TOOL_APPROVAL_REQUIRED` (no "
             "NO_MORE_ACTIONS — the gated tool is a pending action); the platform "
             "journals the decision, then re-invokes with "
             "`reason: approval_decision` (fresh runner, full transcript).", "",
             "```json", json.dumps(ex, indent=2), "```", ""]
    return "\n".join(lines)


def sidecar_section(sidecar: dict) -> str:
    """Contract-4 wire protocol: one row per frame kind, who sends it,
    what it carries."""
    meta = {
        "ready": ("sidecar→server", "First frame after dialing; auth is the ingest token on the WS upgrade."),
        "event": ("sidecar→server", "Validated runner events (newEvent) in stream order; ONE batch in flight. Receiving it IS the lease heartbeat (heartbeat-before-append)."),
        "complete": ("sidecar→server", "Turn ended: completed / suspended / failed / ingest_lost. The journal already carries the end; server releases the lease."),
        "assignment": ("server→sidecar", "One invocation: transcript (incl. INVOCATION_STARTED), reason, env (secrets), runner spawn spec, approval tools."),
        "ack": ("server→sidecar", "Batch journaled: per-event assigned seq + duplicate flag (exactly-once reconnect replay)."),
        "abort": ("server→sidecar", "Turn is no longer this sidecar's (reconciler reclaim / fatal ingest error); the sink fails and the turn task exits."),
    }
    lines = ["## Sidecar↔server protocol (Contract 4)", "",
             "One WebSocket per sidecar container, dialed by the sidecar "
             "(`sidecar.schema.json`); JSON frames, no heartbeat frames — "
             "ingest traffic refreshes the lease. Reconnect: replay the "
             "unacked batch, dedupe by event id.", "",
             "| kind | direction | carries |", "|---|---|---|"]
    for branch in sidecar["oneOf"]:
        name = branch["$ref"].split("/")[-1]
        direction, what = meta[name]
        lines.append(f"| `{name}` | {direction} | {what} |")
    lines.append("")
    return "\n".join(lines)


def serve_section(serve: dict) -> str:
    """Serve mode's HTTP surface: one row per message, its route and what
    it carries."""
    meta = {
        "identity": ("`GET /` → 200", "`sectr_sidecar.version` and `mode: serve`; the platform's readiness check."),
        "wakeRequest": ("`POST /wake`", "The turn's ingest token. The sidecar dials its OWN `SECTR_INGEST_URL`, never a URL from the request."),
        "wakeResponse": ("`POST /wake` → 200", "Sent when the invocation ends: `turn_ended`, or `token_rejected` (401 at the dial, nothing ran)."),
        "manifestRequest": ("`POST /manifest`", "A registration token and the app's entry point."),
        "manifestResponse": ("`POST /manifest` → 200", "`manifest` (+ `uses_uv`), or `token_rejected` / `dial_failed` (never imported) / `import_failed`."),
        "error": ("400 / 409 / 413", "`bad_request`, `busy` (one turn per instance, never queued), `too_large` (bodies over 4 KiB)."),
    }
    lines = ["## Sidecar serve mode (HTTP)", "",
             "`sectr-worker --serve` on the customer's own serverless or "
             "container platform (`serve.schema.json`). A wake is held open "
             "for the whole turn, which then runs over Contract 4 as above. "
             "The sidecar never answers 429 or 503 itself.", "",
             "| message | route | carries |", "|---|---|---|"]
    for branch in serve["anyOf"]:
        name = branch["$ref"].split("/")[-1]
        route, what = meta[name]
        lines.append(f"| `{name}` | {route} | {what} |")
    lines.append("")
    return "\n".join(lines)


def render() -> str:
    event = load("event.schema.json")
    inv = load("invocation.schema.json")
    sidecar = load("sidecar.schema.json")
    serve = load("serve.schema.json")
    parts = [
        "# Sectr contracts — human-readable reference",
        "",
        "> **Generated** from `event.schema.json` / `invocation.schema.json` by "
        "`uv run schema/render_docs.py`. Do not edit by hand — the JSON Schemas "
        "are the source of truth. See `schema/README.md` for the rules and "
        "`schema/schema.mmd` for the database diagram.",
        "",
        envelope_section(event),
        "",
        event_types_section(event),
        "",
        invocation_section(inv, event),
        "",
        sidecar_section(sidecar),
        "",
        serve_section(serve),
        "",
        example_section(event),
        "",
        "## Fixture library",
        "",
        "Golden files under `fixtures/` feed conformance checks and the "
        "Rust/Python typed-model tests:",
        "",
        "- `fixtures/valid/{event,runner-event,invocation,sidecar,serve}/` — must validate",
        "- `fixtures/invalid/{event,runner-event,invocation,sidecar,serve}/` — must be rejected",
        "- Check everything: `uv run pytest` (from `schema/`)",
        "",
    ]
    return "\n".join(parts)


def main() -> int:
    content = render()
    if "--check" in sys.argv[1:]:
        if REFERENCE.exists() and REFERENCE.read_text() == content:
            print("schema/REFERENCE.md is up to date")
            return 0
        print("schema/REFERENCE.md is stale — run: uv run schema/render_docs.py")
        return 1
    REFERENCE.write_text(content)
    print(f"wrote {REFERENCE}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
