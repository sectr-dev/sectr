"""`sectr init --template pydantic-ai`: the scaffolded app.py imports with no
provider key (the fake path) and survives one approve cycle through the real
runner server. Catches template drift the Rust scaffold tests cannot see."""

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

TEMPLATE = (
    Path(__file__).resolve().parents[2] / "crates" / "sectr-cli" / "templates" / "pydantic-ai"
)
TEMPLATE_MODULES = ("app", "tools", "fake")


@pytest.fixture()
def http(monkeypatch: pytest.MonkeyPatch) -> Iterator[TestClient]:
    monkeypatch.delenv("OPENAI_API_KEY", raising=False)
    monkeypatch.delenv("SECTR_APPROVAL_POLICY", raising=False)
    monkeypatch.syspath_prepend(str(TEMPLATE))
    saved = dict(_REGISTRY)
    for name in TEMPLATE_MODULES:
        sys.modules.pop(name, None)
    import app  # the template's entry module

    with TestClient(create_asgi(app.app)) as client:
        yield client
    for name in TEMPLATE_MODULES:
        sys.modules.pop(name, None)
    _REGISTRY.clear()
    _REGISTRY.update(saved)


def test_template_runs_an_approve_cycle(http: TestClient) -> None:
    transcript: list[dict[str, Any]] = []

    def invoke(reason: dict[str, Any]) -> list[dict[str, Any]]:
        def append(type: str, payload: dict[str, Any]) -> None:  # noqa: A002
            transcript.append(
                {
                    "id": f"id-{len(transcript)}",
                    "session_id": "s1",
                    "deployment_id": "d1",
                    "seq": len(transcript),
                    "type": type,
                    "payload": payload,
                    "created_at": "2026-01-01T00:00:00Z",
                }
            )

        append("INVOCATION_STARTED", {"reason": reason})
        resp = http.post(
            "/invocations",
            json={
                "session_id": "s1",
                "invocation_id": f"i{len(transcript)}",
                "route": "/chat",
                "transcript": list(transcript),
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
            append(frame["type"], frame["payload"])
        return frames

    lookup = invoke({"kind": "user_message", "input": "where is order 123?"})
    assert lookup[-1]["type"] == "NO_MORE_ACTIONS"

    paused = invoke({"kind": "user_message", "input": "please refund it"})
    assert paused[-1]["type"] == "TOOL_APPROVAL_REQUIRED"
    assert paused[-1]["payload"]["name"] == "refund"

    resumed = invoke({"kind": "approval_decision", "decision": {"approved": True}})
    results = [f["payload"] for f in resumed if f["type"] == "TOOL_RESULT"]
    assert results == [{"tool_call_id": "c_refund", "result": "Refunded 123"}]
    assert resumed[-1]["type"] == "NO_MORE_ACTIONS"
    assert "ERROR" not in [f["type"] for f in resumed]
