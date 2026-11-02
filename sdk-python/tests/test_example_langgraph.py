"""examples/langgraph end to end: the REAL example module (scripted
`SECTR_FAKE=1` model) served through the real runner server, one POST per
invocation with the journal so far as the transcript — what the platform
does. Pins the golden journal sequence for this variant's event mapping:
lookup turn → refund turn → approval → decision → continuation (and the
deny path)."""

from __future__ import annotations

import json
import sys
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest

pytest.importorskip("fastapi")
from fastapi.testclient import TestClient  # noqa: E402

from sectr.server import create_asgi  # noqa: E402
from sectr.tools import _REGISTRY  # noqa: E402

EXAMPLE = Path(__file__).resolve().parents[2] / "examples" / "langgraph"
EXAMPLE_MODULES = ("main", "tools", "fake")


@pytest.fixture()
def client(monkeypatch: pytest.MonkeyPatch) -> Iterator["Journal"]:
    monkeypatch.setenv("SECTR_FAKE", "1")
    monkeypatch.delenv("SECTR_APPROVAL_POLICY", raising=False)
    monkeypatch.syspath_prepend(str(EXAMPLE))
    saved = dict(_REGISTRY)
    for name in EXAMPLE_MODULES:
        sys.modules.pop(name, None)
    import main  # the example's entry module

    with TestClient(create_asgi(main.app)) as http:
        yield Journal(http)
    for name in EXAMPLE_MODULES:
        sys.modules.pop(name, None)
    _REGISTRY.clear()
    _REGISTRY.update(saved)


class Journal:
    """The platform's side of the contract: appends INVOCATION_STARTED and
    the runner's frames to the session journal, posts it back each turn."""

    def __init__(self, http: TestClient) -> None:
        self.http = http
        self.transcript: list[dict[str, Any]] = []

    def _append(self, type: str, payload: dict[str, Any]) -> None:  # noqa: A002
        seq = len(self.transcript)
        self.transcript.append(
            {
                "id": f"id-{seq}",
                "session_id": "s1",
                "deployment_id": "d1",
                "seq": seq,
                "type": type,
                "payload": payload,
                "created_at": "2026-01-01T00:00:00Z",
            }
        )

    def invoke(self, reason: dict[str, Any]) -> list[dict[str, Any]]:
        self._append("INVOCATION_STARTED", {"reason": reason})
        resp = self.http.post(
            "/invocations",
            json={
                "session_id": "s1",
                "invocation_id": f"i{len(self.transcript)}",
                "route": "/chat",
                "transcript": list(self.transcript),
                "reason": reason,
            },
        )
        assert resp.status_code == 200
        frames = [
            json.loads(line.removeprefix("data: "))
            for line in resp.text.splitlines()
            if line.startswith("data: ")
        ]
        for frame in frames:
            self._append(frame["type"], frame["payload"])
        return frames

    def say(self, text: str) -> list[dict[str, Any]]:
        return self.invoke({"kind": "user_message", "input": text})

    def decide(self, approved: bool) -> list[dict[str, Any]]:
        return self.invoke({"kind": "approval_decision", "decision": {"approved": approved}})


def golden(frames: list[dict[str, Any]]) -> list[tuple[Any, ...]]:
    """Frames reduced to their stable content: message ids dropped, text
    deltas merged."""
    out: list[tuple[Any, ...]] = []
    for f in frames:
        t, p = f["type"], f["payload"]
        if t == "MESSAGE_DELTA":
            if out and out[-1][0] == "MESSAGE_DELTA":
                out[-1] = (t, out[-1][1] + p["delta"]["text"])
            else:
                out.append((t, p["delta"]["text"]))
        elif t in ("TOOL_CALL", "TOOL_RESULT", "TOOL_APPROVAL_REQUIRED"):
            out.append((t, p))
        else:
            out.append((t,))
    return out


LOOKUP_TURN = [
    ("MESSAGE_START",),
    ("MESSAGE_DELTA", "Let me check that order."),
    ("MESSAGE_END",),
    ("TOOL_CALL", {"tool_call_id": "c_lookup", "name": "lookup_order", "arguments": {"order_id": "123"}}),
    ("TOOL_RESULT", {"tool_call_id": "c_lookup", "result": '{"order_id": "123", "status": "shipped"}'}),
    ("MESSAGE_START",),
    ("MESSAGE_DELTA", "Good news — order 123 shipped!"),
    ("MESSAGE_END",),
    ("NO_MORE_ACTIONS",),
]

REFUND_TURN = [
    # tool-only response: empty message bracket, then the gated call, then the pause
    ("MESSAGE_START",),
    ("MESSAGE_END",),
    ("TOOL_CALL", {"tool_call_id": "c_refund", "name": "refund", "arguments": {"order_id": "123"}}),
    ("TOOL_APPROVAL_REQUIRED", {"tool_call_id": "c_refund", "name": "refund", "arguments": {"order_id": "123"}}),
]


def test_full_scenario_approved(client: Journal) -> None:
    assert golden(client.say("where is order 123?")) == LOOKUP_TURN
    assert golden(client.say("please refund it")) == REFUND_TURN
    assert golden(client.decide(True)) == [
        ("TOOL_RESULT", {"tool_call_id": "c_refund", "result": "Refunded 123"}),
        ("MESSAGE_START",),
        ("MESSAGE_DELTA", "Refund processed — anything else?"),
        ("MESSAGE_END",),
        ("NO_MORE_ACTIONS",),
    ]


def test_full_scenario_denied(client: Journal) -> None:
    client.say("where is order 123?")
    client.say("please refund it")
    assert golden(client.decide(False)) == [
        ("TOOL_RESULT", {"tool_call_id": "c_refund", "result": "Tool execution was not approved."}),
        ("MESSAGE_START",),
        ("MESSAGE_DELTA", "Refund processed — anything else?"),
        ("MESSAGE_END",),
        ("NO_MORE_ACTIONS",),
    ]
