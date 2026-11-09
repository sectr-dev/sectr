"""Sectr contract conformance suite.

Validates that the contracts in schema/ are internally consistent and that
all fixtures behave as labelled:

  fixtures/valid/{event,runner-event,invocation}/*.json   MUST validate
  fixtures/invalid/{event,runner-event,invocation}/*.json MUST be rejected

plus:
  - serialization round-trip: every valid fixture re-serializes to an
    identical document that still validates (these are golden fixtures for
    the Rust/Python typed-model tests, so byte-stable JSON matters)
  - the `eventType` enum, the per-type payload $defs, the data-model.sql CHECK
    list, and the valid event fixtures all cover the same set of event types
  - schema/REFERENCE.md (generated human-readable reference) is not stale

Run:  uv run pytest        (from schema/)
"""

import json
import re
import sys
from pathlib import Path

import jsonschema
import pytest
import yaml
from referencing import Registry, Resource

sys.path.insert(0, str(Path(__file__).resolve().parent))
import render_docs  # noqa: E402  (stdlib only; used for the REFERENCE drift check)

SCHEMA_DIR = Path(__file__).resolve().parent
FIXTURES = SCHEMA_DIR / "fixtures"
JOURNAL_SQL = (SCHEMA_DIR / "data-model.sql").read_text()

EVENT_SCHEMA = json.loads((SCHEMA_DIR / "event.schema.json").read_text())
INVOCATION_SCHEMA = json.loads((SCHEMA_DIR / "invocation.schema.json").read_text())
SIDECAR_SCHEMA = json.loads((SCHEMA_DIR / "sidecar.schema.json").read_text())
SERVE_SCHEMA = json.loads((SCHEMA_DIR / "serve.schema.json").read_text())

EVENT_TYPES = set(EVENT_SCHEMA["$defs"]["eventType"]["enum"])
RUNNER_TYPES = set(EVENT_SCHEMA["$defs"]["runnerEventType"]["enum"])


@pytest.fixture(scope="session")
def validators() -> dict[str, jsonschema.Draft202012Validator]:
    registry = Registry().with_resources(
        [
            (EVENT_SCHEMA["$id"], Resource.from_contents(EVENT_SCHEMA)),
            (INVOCATION_SCHEMA["$id"], Resource.from_contents(INVOCATION_SCHEMA)),
            (SIDECAR_SCHEMA["$id"], Resource.from_contents(SIDECAR_SCHEMA)),
            (SERVE_SCHEMA["$id"], Resource.from_contents(SERVE_SCHEMA)),
        ]
    )
    format_checker = jsonschema.FormatChecker()  # date-time needs rfc3339-validator
    return {
        "event": jsonschema.Draft202012Validator(
            EVENT_SCHEMA, registry=registry, format_checker=format_checker
        ),
        "runner-event": jsonschema.Draft202012Validator(
            {"$ref": f"{EVENT_SCHEMA['$id']}#/$defs/runnerEvent"},
            registry=registry,
            format_checker=format_checker,
        ),
        "invocation": jsonschema.Draft202012Validator(
            INVOCATION_SCHEMA, registry=registry, format_checker=format_checker
        ),
        "sidecar": jsonschema.Draft202012Validator(
            SIDECAR_SCHEMA, registry=registry, format_checker=format_checker
        ),
        "serve": jsonschema.Draft202012Validator(
            SERVE_SCHEMA, registry=registry, format_checker=format_checker
        ),
    }


def _fixtures(group: str, kind: str) -> list[Path]:
    return sorted((FIXTURES / group / kind).glob("*.json"))


# ---------------------------------------------------------------------------
# OpenAPI contracts: sessions-api (core) + the composed public/dev docs.
# ---------------------------------------------------------------------------

SESSIONS_API = yaml.safe_load((SCHEMA_DIR / "sessions-api.yaml").read_text())
PUBLIC_API = yaml.safe_load((SCHEMA_DIR / "public-api.yaml").read_text())
DEV_API = yaml.safe_load((SCHEMA_DIR / "dev-api.yaml").read_text())

# The session-interaction paths every composed doc must expose verbatim.
CORE_SESSION_PATHS = sorted(
    p for p in SESSIONS_API["paths"] if p.startswith("/session") or p == "/approvals"
)

_CORE_TEST_DOCS = {"core": SESSIONS_API, "public": PUBLIC_API, "dev": DEV_API}
_API_DOCS = {
    "sessions-api": SESSIONS_API,
    "public-api": PUBLIC_API,
    "dev-api": DEV_API,
}


def _resolve_ref(doc: dict, ref: str, source_dir: Path) -> dict:
    """Resolve a JSON $ref ('file.yaml#/json/pointer' or '#/pointer')
    against the given source directory. Returns the target node."""
    file_part, _, pointer = ref.partition("#")
    if file_part:
        target = yaml.safe_load((source_dir / file_part).read_text())
        base = (SCHEMA_DIR / file_part).resolve().parent
    else:
        target = doc
        base = source_dir
    node = target
    for part in [p for p in pointer.split("/") if p]:
        node = node[part.replace("~1", "/").replace("~0", "~")]  # type: ignore[index]
        if isinstance(node, dict) and "$ref" in node and file_part:
            # follow chained external refs (same-dir composition only)
            return _resolve_ref(node, node["$ref"], base)
    return node  # type: ignore[return-value]


def _all_refs(node: object) -> list[str]:
    if isinstance(node, dict):
        if isinstance(node.get("$ref"), str):
            yield node["$ref"]
        for value in node.values():
            yield from _all_refs(value)
    elif isinstance(node, list):
        for item in node:
            yield from _all_refs(item)


@pytest.mark.parametrize("name", ["sessions-api", "public-api", "dev-api"])
def test_openapi_contract_is_valid_structure(name: str) -> None:
    doc = _API_DOCS[name]
    assert doc["openapi"] == "3.1.0"
    assert doc["info"]["title"] and doc["info"]["version"]
    assert isinstance(doc["paths"], dict) and doc["paths"]
    if "components" in doc and "schemas" in doc["components"]:
        assert doc["components"]["schemas"]


_RESPONSE_KEYS = {"description", "headers", "content", "links", "$ref"}


@pytest.mark.parametrize("name", ["sessions-api", "public-api", "dev-api"])
def test_openapi_responses_have_only_response_fields(name: str) -> None:
    """An unquoted comma in a flow-mapping description
    (`{ description: missing, unknown key }`) silently splits it into extra
    keys; the bundled spec then fails OpenAPI validation."""
    for path, item in _API_DOCS[name]["paths"].items():
        for method, op in item.items():
            if not isinstance(op, dict) or "responses" not in op:
                continue
            for status, response in op["responses"].items():
                extra = set(response) - _RESPONSE_KEYS
                assert not extra, f"{name}: {method.upper()} {path} {status}: {extra}"


@pytest.mark.parametrize("name", ["public-api", "dev-api"])
def test_composed_doc_exposes_the_session_core(name: str) -> None:
    doc = _API_DOCS[name]
    # every core session path must be present in the composed doc, and must
    # $ref the core (verbatim composition — no redefinition)
    for path in CORE_SESSION_PATHS:
        assert path in doc["paths"], f"{name}: missing core path {path}"
        entry = doc["paths"][path]
        if "$ref" in entry:
            assert entry["$ref"].startswith("sessions-api.yaml#"), (
                f"{name}: {path} must $ref sessions-api.yaml, got {entry['$ref']}"
            )


def test_composed_docs_refs_resolve() -> None:
    """Every external/internal $ref in both composed docs (and the core)
    must point at an existing node — catches composition typos."""
    for name, doc in _CORE_TEST_DOCS.items():
        refs = list(_all_refs(doc))
        assert refs, f"{name}: no refs found (walk broken?)"
        for ref in refs:
            try:
                _resolve_ref(doc, ref, SCHEMA_DIR)
            except (KeyError, FileNotFoundError, TypeError) as exc:
                pytest.fail(f"{name}: broken $ref {ref!r}: {exc}")


# The data plane: every credential class may call these (scoped to its app,
# and a user token to its own sessions). Re-declared inline in public-api,
# since a path-item $ref can't carry security.
_DATA_PLANE_SECURITY = [{"invocationKey": []}, {"userToken": []}, {"platformApiKey": []}]
_DATA_PLANE_OPS = [
    ("/sessions", "post"),
    ("/sessions", "get"),
    ("/sessions/{sessionId}", "get"),
    ("/sessions/{sessionId}/events", "get"),
    ("/sessions/{sessionId}/input", "post"),
    ("/sessions/{sessionId}/approval", "post"),
]


def _as_core(node: object) -> object:
    """A public-api node as the core would write it: its refs into
    sessions-api.yaml made local."""
    if isinstance(node, dict):
        return {
            key: (
                value.replace("sessions-api.yaml#", "#", 1)
                if key == "$ref" and isinstance(value, str)
                else _as_core(value)
            )
            for key, value in node.items()
        }
    if isinstance(node, list):
        return [_as_core(item) for item in node]
    return node


@pytest.mark.parametrize(("path", "method"), _DATA_PLANE_OPS)
def test_public_data_plane_op_is_the_core_op_plus_security(path: str, method: str) -> None:
    """Each inline copy is the core's operation plus its `security`; only
    the description may differ (it can say how auth applies). Anything
    else that differs is drift between the two documents."""
    op = PUBLIC_API["paths"][path][method]
    assert op["security"] == _DATA_PLANE_SECURITY
    core = SESSIONS_API["paths"][path][method]
    ignored = {"security", "description"}
    assert {k: v for k, v in _as_core(op).items() if k not in ignored} == {
        k: v for k, v in core.items() if k not in ignored
    }


def test_public_approvals_inbox_is_platform_only() -> None:
    """GET /approvals stays a $ref, so the document's platform-key default
    applies to it."""
    assert PUBLIC_API["security"] == [{"platformApiKey": []}]
    assert "$ref" in PUBLIC_API["paths"]["/approvals"]


def test_dev_api_has_no_auth() -> None:
    assert DEV_API["security"] == []
    assert "securitySchemes" not in DEV_API.get("components", {})
    # core is auth-agnostic: no schemes, no op-level security
    assert "securitySchemes" not in SESSIONS_API["components"]
    for path_item in SESSIONS_API["paths"].values():
        for method in ("get", "post", "put", "delete"):
            if method in path_item:
                assert "security" not in path_item[method]


# ---------------------------------------------------------------------------
# 1. Valid fixtures must validate, and round-trip byte-identically.
# ---------------------------------------------------------------------------


ALL_KINDS = ("event", "runner-event", "invocation", "sidecar", "serve")


@pytest.mark.parametrize(
    "path",
    [p for kind in ALL_KINDS for p in _fixtures("valid", kind)],
    ids=lambda p: f"{p.parent.name}/{p.stem}",
)
def test_valid_fixture_validates_and_round_trips(
    path: Path, validators: dict[str, jsonschema.Draft202012Validator]
) -> None:
    validator = validators[path.parent.name]
    doc = json.loads(path.read_text())
    errors = list(validator.iter_errors(doc))
    assert not errors, "; ".join(e.message for e in errors[:3])

    round_tripped = json.loads(json.dumps(doc))
    assert round_tripped == doc, "round-trip changed the document"
    assert not list(validator.iter_errors(round_tripped)), (
        "round-tripped fixture no longer validates"
    )


# ---------------------------------------------------------------------------
# 2. Invalid fixtures must be rejected.
# ---------------------------------------------------------------------------


@pytest.mark.parametrize(
    "path",
    [p for kind in ALL_KINDS for p in _fixtures("invalid", kind)],
    ids=lambda p: f"{p.parent.name}/{p.stem}",
)
def test_invalid_fixture_rejected(
    path: Path, validators: dict[str, jsonschema.Draft202012Validator]
) -> None:
    validator = validators[path.parent.name]
    doc = json.loads(path.read_text())
    assert list(validator.iter_errors(doc)), "invalid fixture was accepted"


# ---------------------------------------------------------------------------
# 3. Cross-artifact consistency: schema ↔ DDL ↔ fixtures.
# ---------------------------------------------------------------------------


def test_runner_types_subset_of_event_types() -> None:
    assert RUNNER_TYPES <= EVENT_TYPES


def test_sidecar_new_event_mirrors_typed_event_runner_branches() -> None:
    """$defs/newEvent's (type const → payload $def) pairing must mirror
    event.schema.json's typedEvent, restricted to runner-emitted types —
    one copy of truth for payload schemas, a checked mirror for the
    ingest path."""
    def fragment(ref: str) -> str:
        return ref.split("#", 1)[1] if "#" in ref else ref

    typed = {}
    for branch in EVENT_SCHEMA["$defs"]["typedEvent"]["oneOf"]:
        props = branch["properties"]
        typed[props["type"]["const"]] = fragment(props["payload"]["$ref"])
    mirror = {}
    for branch in SIDECAR_SCHEMA["$defs"]["newEvent"]["oneOf"]:
        # newEvent branches $ref payload defs directly; the type const is
        # recovered from the matching typedEvent branch (same payload def).
        ref = fragment(branch["$ref"])
        consts = [c for c, r in typed.items() if r == ref]
        assert len(consts) == 1, f"sidecar newEvent $ref {ref} matches no unique typedEvent branch"
        mirror[consts[0]] = ref
    assert set(mirror) == RUNNER_TYPES, (
        f"newEvent oneOf != runner types: {sorted(set(mirror) ^ RUNNER_TYPES)}"
    )


def test_sidecar_message_kinds_match_contract_4() -> None:
    """The frame kinds in sidecar.schema.json are exactly Contract 4's
    decided message set: NO heartbeat kind — ingest
    traffic IS the lease heartbeat."""
    kinds = set()
    for branch in SIDECAR_SCHEMA["oneOf"]:
        def_name = branch["$ref"].split("/", 1)[1].split("/", 1)[1]
        kinds.add(SIDECAR_SCHEMA["$defs"][def_name]["properties"]["kind"]["const"])
    assert kinds == {
        "ready", "event", "complete",  # sidecar → server
        "assignment", "ack", "abort",   # server → sidecar
    }


def test_every_event_type_has_a_payload_def() -> None:
    missing = EVENT_TYPES - set(EVENT_SCHEMA["$defs"])
    assert not missing, f"event types without a payload $def: {sorted(missing)}"


def test_every_event_type_in_journal_check_and_has_fixture() -> None:
    for typ in EVENT_TYPES:
        assert typ in JOURNAL_SQL, f"{typ!r} missing from data-model.sql events CHECK list"
        assert (FIXTURES / "valid" / "event" / f"{typ}.json").exists(), (
            f"{typ!r} has no canonical valid event fixture"
        )


def test_every_runner_type_has_fixture() -> None:
    for typ in RUNNER_TYPES:
        assert (FIXTURES / "valid" / "runner-event" / f"{typ}.json").exists(), (
            f"runner event type {typ!r} has no valid runner-event fixture"
        )


def test_journal_tables_present() -> None:
    for table in [
        "orgs", "users", "apps", "api_keys", "deployments",
        "secrets", "sessions", "events", "leases",
    ]:
        assert re.search(rf"CREATE TABLE {table} \(", JOURNAL_SQL), (
            f"data-model.sql is missing table {table!r}"
        )


# ---------------------------------------------------------------------------
# 4. REFERENCE (generated human-readable reference) must not be stale.
# ---------------------------------------------------------------------------


def test_readme_not_stale() -> None:
    current = render_docs.REFERENCE.read_text() if render_docs.REFERENCE.exists() else None
    assert render_docs.render() == current, (
        "schema/REFERENCE.md is stale — run: uv run schema/render_docs.py"
    )

