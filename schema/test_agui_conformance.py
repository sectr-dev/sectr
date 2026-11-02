"""AG-UI endpoint conformance: our fixtures against the UPSTREAM
AG-UI 1.0 schema.

The upstream schema (vendored unmodified in vendor/ag-ui/, closed objects:
a field it does not declare fails) is the protocol's own contract, so these
tests pin what our endpoint emits against it — and catch exactly the class of
mistake an earlier draft made (`toolName` vs `toolCallName`, a missing
`messageId` on TOOL_CALL_RESULT, a reasoning message without its role).

  fixtures/valid/agui/*.json    {journal, request, expected, ...}: the journal
                                is a valid event stream, the request a valid
                                RunAgentInput, every `expected` event a valid
                                AG-UI event (checked against its OWN anchor).
                                `"kind": "connect"`: the request is
                                the connect's `{threadId}` and the stream
                                re-streams the latest run (no `input` echo,
                                always a snapshot).
  fixtures/invalid/agui/*.json  {validate, reason, expect_error_contains,
                                document}: must be rejected, FOR THAT REASON

The Rust golden test (sectr-common `agui::tests`) proves `expected` is what
the implementation produces from `journal`; this suite proves the golden
streams are protocol-valid; `agui-conformance/` replays them through the
upstream client's stream verifier + reducer.

Run:  uv run pytest        (from schema/)
"""

import json
import re
from pathlib import Path

import jsonschema
import pytest
import yaml
from referencing import Registry, Resource

SCHEMA_DIR = Path(__file__).resolve().parent
FIXTURES = SCHEMA_DIR / "fixtures"
AGUI_SCHEMA = json.loads((SCHEMA_DIR / "vendor" / "ag-ui" / "schema.json").read_text())
AGUI_ID = AGUI_SCHEMA["$id"]
EVENT_SCHEMA = json.loads((SCHEMA_DIR / "event.schema.json").read_text())
MAPPING = (SCHEMA_DIR / "agui-mapping.md").read_text()
PUBLIC_API = yaml.safe_load((SCHEMA_DIR / "public-api.yaml").read_text())

PROTOCOL_VERSION = "1.0"


def _registry() -> Registry:
    return Registry().with_resources(
        [
            (AGUI_ID, Resource.from_contents(AGUI_SCHEMA)),
            (EVENT_SCHEMA["$id"], Resource.from_contents(EVENT_SCHEMA)),
        ]
    )


def _anchor_validator(anchor: str) -> jsonschema.Draft202012Validator:
    return jsonschema.Draft202012Validator(
        {"$ref": f"{AGUI_ID}#{anchor}"}, registry=_registry()
    )


# type const → anchor, derived from the schema (not hand-maintained).
EVENT_ANCHORS = {
    d["properties"]["type"]["const"]: name
    for name, d in AGUI_SCHEMA["$defs"].items()
    if name.endswith("Event")
    and isinstance(d.get("properties", {}).get("type"), dict)
    and "const" in d["properties"]["type"]
}

VALID = sorted((FIXTURES / "valid" / "agui").glob("*.json"))
INVALID = sorted((FIXTURES / "invalid" / "agui").glob("*.json"))


def test_vendored_schema_is_the_pinned_protocol_version() -> None:
    assert AGUI_ID == f"https://ag-ui.com/spec/{PROTOCOL_VERSION}/schema.json"
    assert f'`protocolVersion: "{PROTOCOL_VERSION}"`' in MAPPING


def test_there_are_fixtures() -> None:
    assert len(VALID) >= 6 and len(INVALID) >= 6


@pytest.mark.parametrize("path", VALID, ids=lambda p: p.stem)
def test_valid_fixture_streams_are_protocol_valid(path: Path) -> None:
    doc = json.loads(path.read_text())

    # The journal is a valid session event stream (our own contract).
    event_validator = jsonschema.Draft202012Validator(
        EVENT_SCHEMA, registry=_registry(), format_checker=jsonschema.FormatChecker()
    )
    for envelope in doc["journal"]:
        errors = list(event_validator.iter_errors(envelope))
        assert not errors, f"journal seq {envelope['seq']}: {errors[0].message}"
    assert [e["seq"] for e in doc["journal"]] == list(range(len(doc["journal"])))

    connect = doc.get("kind") == "connect"
    if connect:
        # A connect request is just the thread (`GET ?threadId=`).
        assert doc["request"].keys() == {"threadId"}
    else:
        # The request is a valid RunAgentInput; the echo in RUN_STARTED too.
        input_validator = _anchor_validator("RunAgentInput")
        errors = list(input_validator.iter_errors(doc["request"]))
        assert not errors, errors[0].message

    # Every expected event validates against ITS OWN anchor (a root-level
    # union check would blur which branch failed).
    assert doc["expected"], "fixture has no expected stream (bless it)"
    for i, event in enumerate(doc["expected"]):
        anchor = EVENT_ANCHORS.get(event["type"])
        assert anchor, f"event {i}: unknown AG-UI event type {event['type']!r}"
        errors = list(_anchor_validator(anchor).iter_errors(event))
        assert not errors, f"event {i} ({event['type']}): {errors[0].message}"
        # and the union root agrees
        assert not list(
            jsonschema.Draft202012Validator(
                {"$ref": AGUI_ID}, registry=_registry()
            ).iter_errors(event)
        ), f"event {i} ({event['type']}) fails the AG-UI event union"

    # Run shape: opens with RUN_STARTED (declaring the version, echoing the
    # request — a connect has no request to echo, so `input` is omitted),
    # ends with exactly one terminal event.
    first, last = doc["expected"][0], doc["expected"][-1]
    assert first["type"] == "RUN_STARTED"
    assert first["protocolVersion"] == PROTOCOL_VERSION
    assert first["threadId"] == doc["request"]["threadId"]
    if connect:
        assert "input" not in first
    else:
        assert first["input"] == doc["request"]
    terminal = [e for e in doc["expected"] if e["type"] in ("RUN_FINISHED", "RUN_ERROR")]
    assert terminal == [last]
    # History reconciliation: a snapshot follows RUN_STARTED on existing
    # threads — and always on a connect (a reloaded client holds nothing).
    has_snapshot = doc["expected"][1]["type"] == "MESSAGES_SNAPSHOT"
    assert has_snapshot == (connect or not doc["created"])


@pytest.mark.parametrize("path", INVALID, ids=lambda p: p.stem)
def test_invalid_fixture_rejected_for_the_stated_reason(path: Path) -> None:
    fixture = json.loads(path.read_text())
    anchor = (
        None if fixture["validate"] == "event" else fixture["validate"]
    )
    validator = (
        jsonschema.Draft202012Validator({"$ref": AGUI_ID}, registry=_registry())
        if anchor is None
        else _anchor_validator(anchor)
    )
    errors = list(validator.iter_errors(fixture["document"]))
    assert errors, "invalid fixture was accepted"
    text = " | ".join(_flatten(e) for e in errors)
    assert fixture["expect_error_contains"] in text, (
        f"rejected, but not for the stated reason {fixture['reason']!r}: {text[:400]}"
    )


def _flatten(error: jsonschema.ValidationError) -> str:
    parts = [error.message]
    for sub in error.context or []:
        parts.append(_flatten(sub))
    return " ".join(parts)


# ---------------------------------------------------------------------------
# agui-mapping.md (normative table) ↔ the event vocabulary
# ---------------------------------------------------------------------------


def _mapping_rows() -> list[tuple[str, str]]:
    section = MAPPING.split("## Journal → AG-UI mapping", 1)[1].split("\n## ", 1)[0]
    rows = []
    for line in section.splitlines():
        if not line.startswith("| ") or line.startswith(("| ---", "| Journal event")):
            continue
        journal, agui = (c.strip() for c in line.strip().strip("|").split(" | ", 1))
        rows.append((journal, agui))
    assert rows, "mapping table not found"
    return rows


def test_mapping_table_covers_every_journal_event_type() -> None:
    journal_column = " ".join(j for j, _ in _mapping_rows())
    for typ in EVENT_SCHEMA["$defs"]["eventType"]["enum"]:
        assert re.search(rf"`{typ}[`\s]", journal_column), (
            f"agui-mapping.md does not say what {typ} maps to (surfaced or not)"
        )


def test_mapping_table_names_only_real_ag_ui_1_0_events() -> None:
    """Every UPPER_SNAKE name in the AG-UI column is an event of the pinned
    schema (catches pre-1.0 slips like THINKING_CONTENT)."""
    skipped = {"not surfaced"}
    for _, agui in _mapping_rows():
        if any(k in agui for k in skipped):
            continue
        for name in re.findall(r"`([A-Z]+(?:_[A-Z]+)+)`", agui):
            assert name in EVENT_ANCHORS, f"{name} is not an AG-UI 1.0 event"
        # inline names without backticks (REASONING_START · …) too
        for name in re.findall(r"\b([A-Z]{2,}(?:_[A-Z]+)+)\b", agui):
            if name.startswith(("RUN_", "TEXT_", "TOOL_CALL", "REASONING_", "MESSAGES_")):
                assert name in EVENT_ANCHORS, f"{name} is not an AG-UI 1.0 event"


def test_mapping_doc_has_no_pre_1_0_reasoning_names_outside_corrections() -> None:
    body, _, corrections = MAPPING.partition("## Corrections to earlier notes")
    assert "THINKING_" not in body
    assert "THINKING_CONTENT" in corrections


# ---------------------------------------------------------------------------
# public-api.yaml's AG-UI entry: the HTTP surface, kept honest
# against the golden streams and the upstream schema.
# ---------------------------------------------------------------------------

AGUI_PATH = PUBLIC_API["paths"]["/ag-ui/{app}/{route}"]


def test_public_api_event_enum_is_what_the_golden_streams_emit() -> None:
    documented = PUBLIC_API["components"]["schemas"]["AgUiEvent"]["properties"]["type"]["enum"]
    emitted = {e["type"] for path in VALID for e in json.loads(path.read_text())["expected"]}
    assert set(documented) == emitted
    assert set(documented) <= set(AGUI_SCHEMA["$defs"]["EventType"]["enum"])


def test_public_api_run_input_mirrors_upstream() -> None:
    ours = PUBLIC_API["components"]["schemas"]["RunAgentInput"]
    theirs = AGUI_SCHEMA["$defs"]["RunAgentInput"]
    assert ours["required"] == theirs["required"]
    assert set(ours["properties"]) == set(theirs["properties"])


def test_public_api_ag_ui_auth_and_errors() -> None:
    """Data-plane credentials only (platform keys are a 403), and the error
    codes agui-mapping.md lists ("Auth, CORS, errors"). Connect never 429s."""
    for method in ("post", "get"):
        assert AGUI_PATH[method]["security"] == [{"userToken": []}, {"invocationKey": []}]
        assert "text/event-stream" in AGUI_PATH[method]["responses"]["200"]["content"]
    assert set(AGUI_PATH["post"]["responses"]) == {"200", "400", "401", "403", "404", "409", "429"}
    assert set(AGUI_PATH["get"]["responses"]) == {"200", "400", "401", "403", "404", "409"}
