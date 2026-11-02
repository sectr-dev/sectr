"""Golden tests: `sectr.events` serialization must validate against
`schema/event.schema.json#/$defs/runnerEvent` — the same contract the Rust
side tests against (shared fixtures in `schema/`)."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from jsonschema import Draft202012Validator, FormatChecker
from referencing import Registry, Resource

from sectr import (
    Error,
    MessageDelta,
    MessageEnd,
    MessageStart,
    NoMoreActions,
    ToolApprovalRequired,
    ToolCall,
    ToolResult,
    TranscriptCompacted,
)
from sectr.adapters._framework_state import FrameworkData, framework_state

SCHEMA_DIR = Path(__file__).resolve().parents[2] / "schema"
EVENT_SCHEMA = json.loads((SCHEMA_DIR / "event.schema.json").read_text())

# Runner-emittable event constructors under test.
CONSTRUCTORS = {
    "MESSAGE_START": lambda: MessageStart(role="assistant", message_id="m1"),
    "MESSAGE_DELTA": lambda: MessageDelta(message_id="m1", delta={"type": "text", "text": "hi"}),
    "MESSAGE_END": lambda: MessageEnd(message_id="m1", finish_reason="stop"),
    "TOOL_CALL": lambda: ToolCall(
        tool_call_id="c1", name="lookup_order", arguments={"order_id": "A-1"}
    ),
    "TOOL_RESULT": lambda: ToolResult(tool_call_id="c1", result={"ok": True}),
    "TOOL_APPROVAL_REQUIRED": lambda: ToolApprovalRequired(
        tool_call_id="c1",
        name="refund",
        arguments={"order_id": "A-1"},
        reason="needs a human",
    ),
    "ERROR": lambda: Error(message="boom", code="x", transient=True),
    "NO_MORE_ACTIONS": lambda: NoMoreActions(),
    "TRANSCRIPT_COMPACTED": lambda: TranscriptCompacted(
        replaces_up_to_seq=3, messages=[{"role": "user", "content": "summary"}]
    ),
}


@pytest.fixture(scope="module")
def runner_event_validator() -> Draft202012Validator:
    invocation = json.loads((SCHEMA_DIR / "invocation.schema.json").read_text())
    registry = Registry().with_resources(
        [
            (EVENT_SCHEMA["$id"], Resource.from_contents(EVENT_SCHEMA)),
            (invocation["$id"], Resource.from_contents(invocation)),
        ]
    )
    return Draft202012Validator(
        {"$ref": f"{EVENT_SCHEMA['$id']}#/$defs/runnerEvent"},
        registry=registry,
        format_checker=FormatChecker(),
    )


def test_every_runner_event_type_has_a_constructor() -> None:
    enum = set(EVENT_SCHEMA["$defs"]["runnerEventType"]["enum"])
    assert enum == set(CONSTRUCTORS), "runnerEventType enum out of sync"


@pytest.mark.parametrize("event_type", sorted(CONSTRUCTORS))
def test_serialization_validates_against_schema(
    event_type: str, runner_event_validator: Draft202012Validator
) -> None:
    frame = CONSTRUCTORS[event_type]().to_runner_event()
    assert frame["type"] == event_type
    errors = list(runner_event_validator.iter_errors(frame))
    assert not errors, "; ".join(e.message for e in errors)


@pytest.mark.parametrize("adapter", ["langgraph", "openai_agents", "pydantic_ai"])
def test_framework_state_validates_against_schema(
    adapter: str, runner_event_validator: Draft202012Validator
) -> None:
    class Data(FrameworkData):
        version = 1
        pieces: tuple[str, ...]

    Data.adapter = adapter
    state = framework_state(Data(pieces=('{"thinking":"hmm","signature":"sig"}',)))
    frame = MessageEnd(message_id="m1", framework_state=state).to_runner_event()
    errors = list(runner_event_validator.iter_errors(frame))
    assert not errors, "; ".join(e.message for e in errors)


def test_optional_fields_are_omitted() -> None:
    frame = MessageEnd(message_id="m1").to_runner_event()
    assert frame["payload"] == {"message_id": "m1"}


def test_required_fields_are_kept_when_none() -> None:
    frame = ToolResult(tool_call_id="c1", result=None).to_runner_event()
    assert frame["payload"] == {"tool_call_id": "c1", "result": None}
    assert ToolCall(tool_call_id="c1", name="n", arguments=None).payload()["arguments"] is None


def test_payload_type_flexibility() -> None:
    # delta: typed {type: text|reasoning, text} only — one wire shape, closed
    # vocabulary (bare strings are invalid per the schema and rejected by the
    # conformance suite)
    for delta in ({"type": "text", "text": "hi"}, {"type": "reasoning", "text": "hmm"}):
        frame = MessageDelta(message_id="m1", delta=delta).to_runner_event()
        assert frame["payload"]["delta"] == delta
