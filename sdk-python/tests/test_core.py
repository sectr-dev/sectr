"""Core SDK behavior: tool registry, approval gating (code ∪ platform
policy), SessionContext.execute / pending_tool_call, and AgentApp route
registration rules."""

from __future__ import annotations

import pytest

from sectr import AgentApp, ApprovalRequired, NoMoreActions, SessionContext, ToolCall, ToolResult, tool
from sectr.events import Envelope
from sectr.tools import _REGISTRY, is_gated


@pytest.fixture(autouse=True)
def clean_registry(monkeypatch: pytest.MonkeyPatch):
    saved = dict(_REGISTRY)
    monkeypatch.delenv("SECTR_APPROVAL_POLICY", raising=False)
    yield
    _REGISTRY.clear()
    _REGISTRY.update(saved)


@tool
async def lookup_order(order_id: str) -> dict:
    return {"order_id": order_id, "status": "shipped"}


@tool(needs_approval=True)
async def refund(order_id: str) -> str:
    return f"Refunded {order_id}"


def envelope(seq: int, type: str, payload: dict) -> Envelope:  # noqa: A002
    return Envelope(
        id=f"id-{seq}",
        session_id="s",
        deployment_id="d",
        seq=seq,
        type=type,
        payload=payload,
        created_at="2026-01-01T00:00:00Z",
    )


# ---- gating ----------------------------------------------------------


def test_gated_by_code_marking() -> None:
    assert is_gated("refund")
    assert not is_gated("lookup_order")


def test_gated_by_platform_policy(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["lookup_order"]')
    assert is_gated("lookup_order")  # safety net: unmarked tool gated by config
    assert is_gated("refund")  # code marking still applies


def test_malformed_policy_fails_loudly(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SECTR_APPROVAL_POLICY", '{"refund": true}')
    with pytest.raises(ValueError):
        is_gated("lookup_order")


# ---- SessionContext.execute -------------------------------------------


async def test_execute_runs_tool() -> None:
    ctx = SessionContext(transcript=[], reason={"kind": "user_message", "input": {}})
    assert await ctx.execute("lookup_order", order_id="A-1") == {
        "order_id": "A-1",
        "status": "shipped",
    }


async def test_execute_raises_approval_required_when_gated() -> None:
    ctx = SessionContext(transcript=[], reason={"kind": "user_message", "input": {}})
    with pytest.raises(ApprovalRequired) as excinfo:
        await ctx.execute("refund", call_id="c1", order_id="A-1")
    assert excinfo.value.name == "refund"
    assert excinfo.value.call_id == "c1"


async def test_execute_on_unknown_tool_is_a_clear_error() -> None:
    ctx = SessionContext(transcript=[], reason={"kind": "user_message", "input": {}})
    with pytest.raises(KeyError):
        await ctx.execute("nope")


async def test_request_approval_always_raises() -> None:
    ctx = SessionContext(transcript=[], reason={"kind": "user_message", "input": {}})
    with pytest.raises(ApprovalRequired):
        await ctx.request_approval("deploy_prod", {}, reason="irreversible")


# ---- pending_tool_call (approval-resume helper) ------------------------


def test_pending_tool_call_scans_transcript() -> None:
    transcript = [
        envelope(0, "TOOL_CALL", {"tool_call_id": "c1", "name": "lookup_order", "arguments": {}}),
        envelope(1, "TOOL_RESULT", {"tool_call_id": "c1", "result": {}}),
        envelope(2, "TOOL_CALL", {"tool_call_id": "c2", "name": "refund", "arguments": {}}),
        envelope(3, "TOOL_APPROVAL_REQUIRED", {"tool_call_id": "c2", "name": "refund", "arguments": {}}),
    ]
    ctx = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": {}})
    pending = ctx.pending_tool_call
    assert pending is not None
    assert pending["tool_call_id"] == "c2"


def test_pending_tool_call_none_when_all_resolved() -> None:
    transcript = [
        envelope(0, "TOOL_CALL", {"tool_call_id": "c1", "name": "lookup_order", "arguments": {}}),
        envelope(1, "TOOL_RESULT", {"tool_call_id": "c1", "result": {}}),
    ]
    assert SessionContext(transcript=transcript, reason={"kind": "user_message", "input": {}}).pending_tool_call is None


def test_a_later_user_turn_abandons_the_pending_call() -> None:
    """The fold answers a call followed by a new user turn as interrupted,
    so no approval decision may resume it afterwards."""
    transcript = [
        envelope(0, "TOOL_CALL", {"tool_call_id": "c1", "name": "lookup_order", "arguments": {}}),
        envelope(1, "INVOCATION_STARTED", {"reason": {"kind": "user_message", "input": "hi"}}),
    ]
    assert SessionContext(transcript=transcript, reason={"kind": "user_message", "input": "hi"}).pending_tool_call is None


# ---- AgentApp route registration ---------------------------------------


def test_add_route_and_decorator() -> None:
    app = AgentApp()

    @app.agent_route("/chat")
    async def chat(ctx: SessionContext):  # pragma: no cover - registration only
        yield ToolCall(tool_call_id="c", name="n", arguments={})

    app.add_route("/other", chat)
    assert [r.path for r in app.routes] == ["/chat", "/other"]


def test_route_validation() -> None:
    app = AgentApp()

    async def handler(ctx: SessionContext):  # pragma: no cover
        yield NoMoreActions()

    with pytest.raises(ValueError):
        app.add_route("chat", handler)  # must start with /
    app.add_route("/chat", handler)
    with pytest.raises(ValueError):
        app.add_route("/chat", handler)  # duplicate


@pytest.mark.asyncio
async def test_gated_tool_executes_on_approval_decision_turn() -> None:
    """The approval-resume contract — ctx.execute must NOT
    re-raise ApprovalRequired on an approval_decision turn (found missing:
    the resume re-suspended forever)."""
    from sectr.tools import tool

    @tool(needs_approval=True)
    async def gated(x: str) -> str:
        return f"ran {x}"

    transcript = [
        envelope(0, "SESSION_CREATED", {"app_id": "a", "route": "/r"}),
        envelope(1, "INVOCATION_STARTED", {"invocation_id": "i1", "route": "/r", "deployment_id": "d", "reason": {"kind": "user_message", "input": "go"}}),
        envelope(2, "TOOL_CALL", {"tool_call_id": "c1", "name": "gated", "arguments": {"x": "1"}}),
        envelope(3, "TOOL_APPROVAL_REQUIRED", {"tool_call_id": "c1", "name": "gated", "arguments": {"x": "1"}}),
        envelope(4, "TOOL_APPROVAL_GRANTED", {"tool_call_id": "c1"}),
        envelope(5, "INVOCATION_STARTED", {"invocation_id": "i2", "route": "/r", "deployment_id": "d", "reason": {"kind": "approval_decision", "decision": {"approved": True}}}),
    ]
    ctx = SessionContext(transcript=transcript, reason={"kind": "approval_decision", "decision": {"approved": True}})
    assert await ctx.execute("gated", call_id="c1", x="1") == "ran 1"
