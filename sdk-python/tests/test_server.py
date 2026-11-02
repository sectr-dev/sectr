"""Tests for sectr.server: the POST /invocations SSE adapter, including the
turn-stream lifecycle rules enforced by sectr.drive."""

from __future__ import annotations

import json
from collections.abc import AsyncIterator

import pytest

fastapi = pytest.importorskip("fastapi")
from fastapi.testclient import TestClient  # noqa: E402

from sectr import (  # noqa: E402
    AgentApp,
    MessageDelta,
    MessageEnd,
    MessageStart,
    NoMoreActions,
    SessionContext,
    ToolApprovalRequired,
    ToolCall,
)
from sectr.server import create_asgi  # noqa: E402


def sse_frames(response_text: str) -> list[dict]:
    return [
        json.loads(line.removeprefix("data: "))
        for line in response_text.splitlines()
        if line.startswith("data: ")
    ]


def make_app(handler) -> AgentApp:
    app = AgentApp(name="test")
    app.add_route("/chat", handler)
    return app


def post(client: TestClient, route: str = "/chat", **overrides):
    body = {
        "session_id": "s1",
        "invocation_id": "i1",
        "route": route,
        "transcript": [],
        "reason": {"kind": "user_message", "input": "hi"},
        **overrides,
    }
    return client.post("/invocations", json=body)


def test_happy_path_streams_and_appends_sentinel() -> None:
    async def handler(ctx: SessionContext) -> AsyncIterator:
        yield MessageStart(role="assistant", message_id="m1")
        yield MessageDelta(message_id="m1", delta="hello")
        yield MessageEnd(message_id="m1")

    with TestClient(create_asgi(make_app(handler))) as client:
        resp = post(client)
    assert resp.status_code == 200
    assert resp.headers["content-type"].startswith("text/event-stream")
    frames = sse_frames(resp.text)
    assert frames[-1] == {"type": "NO_MORE_ACTIONS", "payload": {}}
    assert [f["type"] for f in frames] == [
        "MESSAGE_START",
        "MESSAGE_DELTA",
        "MESSAGE_END",
        "NO_MORE_ACTIONS",
    ]


def test_approval_required_pause_via_exception() -> None:
    async def handler(ctx: SessionContext) -> AsyncIterator:
        yield ToolCall(tool_call_id="c1", name="refund", arguments={"o": 1})
        await ctx.execute("refund", call_id="c1", order_id="1")  # raises

    from sectr.tools import tool

    @tool(needs_approval=True)
    async def refund(order_id: str) -> str:
        return "ok"

    try:
        with TestClient(create_asgi(make_app(handler))) as client:
            resp = post(client)
        frames = sse_frames(resp.text)
    finally:
        pass
    # Suspension: TOOL_APPROVAL_REQUIRED is the FINAL frame — no sentinel.
    assert [f["type"] for f in frames] == ["TOOL_CALL", "TOOL_APPROVAL_REQUIRED"]


def test_yield_after_approval_required_is_a_handler_error() -> None:
    async def handler(ctx: SessionContext) -> AsyncIterator:
        yield ToolApprovalRequired(
            tool_call_id="c1", name="t", arguments={}
        )
        yield MessageDelta(message_id="m1", delta="oops")  # finality violation

    with TestClient(create_asgi(make_app(handler))) as client:
        resp = post(client)
    frames = sse_frames(resp.text)
    assert frames[-2]["type"] == "ERROR"
    assert frames[-1]["type"] == "NO_MORE_ACTIONS"


def test_unhandled_exception_becomes_error_event() -> None:
    async def handler(ctx: SessionContext) -> AsyncIterator:
        yield MessageStart(role="assistant", message_id="m1")
        raise ValueError("boom")

    with TestClient(create_asgi(make_app(handler))) as client:
        resp = post(client)
    frames = sse_frames(resp.text)
    assert frames[-2] == {"type": "ERROR", "payload": {"message": "boom"}}
    assert frames[-1] == {"type": "NO_MORE_ACTIONS", "payload": {}}


def test_approval_decision_without_a_pending_call_is_an_error() -> None:
    called = False

    async def handler(ctx: SessionContext) -> AsyncIterator:
        nonlocal called
        called = True
        yield MessageStart(role="assistant", message_id="m1")

    with TestClient(create_asgi(make_app(handler))) as client:
        resp = post(client, reason={"kind": "approval_decision", "decision": {"approved": True}})
    frames = sse_frames(resp.text)
    assert [f["type"] for f in frames] == ["ERROR", "NO_MORE_ACTIONS"]
    assert frames[0]["payload"]["code"] == "no_pending_tool_call"
    assert not called


def test_unknown_route_and_bad_body() -> None:
    async def handler(ctx: SessionContext) -> AsyncIterator:
        yield NoMoreActions()

    with TestClient(create_asgi(make_app(handler))) as client:
        assert post(client, route="/nope").status_code == 404
        assert post(client, route="/chat", transcript="bad").status_code == 422
        bad = {"session_id": "s1"}
        assert client.post("/invocations", json=bad).status_code == 422


def test_transcript_reaches_the_handler() -> None:
    seen: list = []

    async def handler(ctx: SessionContext) -> AsyncIterator:
        seen.extend(ctx.transcript)
        seen.append(ctx.input)
        yield MessageEnd(message_id="m1")

    transcript = [
        {
            "id": "e1",
            "session_id": "s1",
            "deployment_id": "d1",
            "seq": 0,
            "type": "INVOCATION_STARTED",
            "payload": {"invocation_id": "i0", "route": "/chat", "deployment_id": "d1",
                        "reason": {"kind": "user_message", "input": "hi"}},
            "created_at": "2026-01-01T00:00:00Z",
        }
    ]
    with TestClient(create_asgi(make_app(handler))) as client:
        post(client, transcript=transcript)
    assert len(seen) == 2
    assert seen[0].type == "INVOCATION_STARTED"
    assert seen[1] == "hi"


def test_a_non_string_user_message_input_is_rejected():
    with TestClient(create_asgi(make_app(lambda ctx: iter(())))) as client:
        for bad in [{"message": "hi"}, [{"type": "text", "text": "hi"}], 0, None, "", "  \n"]:
            response = post(client, reason={"kind": "user_message", "input": bad})
            assert response.status_code == 422, bad
