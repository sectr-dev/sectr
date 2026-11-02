"""openai-agents adapter: the multiple-interruption guard, tracing, input
and transcript conversion, and construction checks. Driven through
`sectr.drive` so the assertions cover the exact SSE frames (drive's
lifecycle rules included). Streaming, pause, approve, deny, policy gating
and multi-turn history are covered by the conformance harness
(`crates/sectr-conformance`), against a real model client over HTTP.
"""

from __future__ import annotations

import asyncio
import json
import logging
from collections.abc import Callable
from typing import Any

import mcp.types as mcp_types
import pytest
from agents import (
    Agent,
    AgentHooks,
    RunConfig,
    RunContextWrapper,
    ToolGuardrailFunctionOutput,
    function_tool,
    tool_input_guardrail,
    tool_output_guardrail,
)
from agents import Handoff, Runner, handoff
from agents.mcp import MCPServer
from agents.testing import ScriptedModel
from openai.types.responses import ResponseFunctionToolCall, ResponseOutputMessage, ResponseReasoningItem
from openai.types.responses.response_reasoning_item import Summary
from pydantic import BaseModel

from sectr.adapters.openai_agents import _shared, _transcript_items, openai_agents
from sectr.context import SessionContext
from sectr.drive import drive
from sectr.events import Envelope
from sectr.tools import _REGISTRY

from subagent_contract import assert_fails_loudly, assert_opaque, run_turn


@pytest.fixture(autouse=True)
def clean_registry(monkeypatch: pytest.MonkeyPatch):
    saved = dict(_REGISTRY)
    monkeypatch.delenv("SECTR_APPROVAL_POLICY", raising=False)
    yield
    _REGISTRY.clear()
    _REGISTRY.update(saved)


@function_tool
async def lookup_order(order_id: str) -> dict:
    return {"order_id": order_id, "status": "shipped"}


@function_tool(needs_approval=True)
async def refund(order_id: str) -> str:
    return f"Refunded {order_id}"


AGENT = Agent(name="support", instructions="help", tools=[lookup_order, refund])


# ---- scripting helpers -----------------------------------------------


def message(text: str, mid: str = "m1") -> ResponseOutputMessage:
    return ResponseOutputMessage(
        id=mid,
        type="message",
        role="assistant",
        status="completed",
        content=[{"type": "output_text", "text": text, "annotations": []}],
    )


def tool_call(cid: str, name: str, args: str) -> ResponseFunctionToolCall:
    return ResponseFunctionToolCall(
        id=f"fc_{cid}", type="function_call", call_id=cid, name=name, arguments=args
    )


def reasoning(text: str, rid: str = "rs_1", encrypted: str = "enc-1") -> ResponseReasoningItem:
    return ResponseReasoningItem(
        id=rid, type="reasoning", summary=[Summary(text=text, type="summary_text")], encrypted_content=encrypted
    )


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


def user_message_transcript(input: str) -> list[Envelope]:  # noqa: A002
    return [envelope(0, "INVOCATION_STARTED", {"reason": {"kind": "user_message", "input": input}})]


def resume_context(transcript: list[Envelope], approved: bool) -> SessionContext:
    return SessionContext(
        transcript=transcript,
        reason={
            "kind": "approval_decision",
            "decision": {"approved": approved},
        },
    )


async def frames(ctx: SessionContext, steps) -> list[dict]:
    handler = openai_agents(AGENT, model=ScriptedModel(steps))
    return [frame async for frame in drive(handler, ctx)]


def types(frames: list[dict]) -> list[str]:
    return [f["type"] for f in frames]


# ---- scenarios ---------------------------------------------------------


def journaled(payload: dict) -> dict:
    """`payload` as the journal gives it back: by value, an object's keys
    sorted (serde_json, Postgres jsonb)."""
    return json.loads(json.dumps(payload, sort_keys=True))



class Session:
    """Turns through `drive`, each turn's frames journaled back as envelopes,
    as the platform would; the model is shared across turns."""

    def __init__(self, agent: Agent, steps: list | None = None) -> None:
        # Without `steps`, each agent runs its own model.
        self.model = ScriptedModel(steps) if steps is not None else None
        self.handler = openai_agents(agent, model=self.model)
        self.transcript: list[Envelope] = []

    async def _turn(self, reason: dict) -> list[dict]:
        self.transcript.append(envelope(len(self.transcript), "INVOCATION_STARTED", {"reason": reason}))
        ctx = SessionContext(transcript=list(self.transcript), reason=reason)
        out = [frame async for frame in drive(self.handler, ctx)]
        for frame in out:
            self.transcript.append(envelope(len(self.transcript), frame["type"], journaled(frame["payload"])))
        return out

    async def send(self, text: str) -> list[dict]:
        return await self._turn({"kind": "user_message", "input": text})

    async def decide(self, approved: bool) -> list[dict]:
        return await self._turn({"kind": "approval_decision", "decision": {"approved": approved}})


def counted_refund(ran: list[str]) -> Any:
    @function_tool(needs_approval=True)
    async def refund(order_id: str) -> str:
        ran.append(order_id)
        return f"Refunded {order_id}"

    return refund


def calls(frames: list[dict]) -> list[tuple[str, str]]:
    return [(f["type"], f["payload"].get("tool_call_id", "")) for f in frames if f["type"].startswith("TOOL_")]


@pytest.mark.parametrize("second", [True, False], ids=["approve_both", "approve_then_deny"])
async def test_two_gated_calls_become_sequential_approvals(second: bool):
    ran: list[str] = []
    session = Session(
        Agent(name="a", tools=[counted_refund(ran)]),
        [
            [tool_call("c1", "refund", '{"order_id": "1"}'), tool_call("c2", "refund", '{"order_id": "2"}')],
            [message("Done.", "m2")],
        ],
    )
    out = await session.send("refund 1 and 2")
    assert types(out) == ["MESSAGE_START", "MESSAGE_END", "TOOL_CALL", "TOOL_CALL", "TOOL_APPROVAL_REQUIRED"]
    assert out[-1]["payload"]["tool_call_id"] == "c1"
    assert ran == []

    # The SDK pauses on c2 without asking the model again; c2's call isn't
    # journaled twice, and no empty message opens for it.
    out = await session.decide(True)
    assert calls(out) == [("TOOL_RESULT", "c1"), ("TOOL_APPROVAL_REQUIRED", "c2")]
    assert types(out) == ["TOOL_RESULT", "TOOL_APPROVAL_REQUIRED"]
    assert ran == ["1"]
    assert len(session.model.calls) == 1

    out = await session.decide(second)
    answer = "Refunded 2" if second else "Tool execution was not approved."
    assert out[0] == {"type": "TOOL_RESULT", "payload": {"tool_call_id": "c2", "result": answer}}
    assert types(out)[-1] == "NO_MORE_ACTIONS"
    assert ran == (["1", "2"] if second else ["1"])
    sent = [(item.get("type") or item["role"], item.get("call_id")) for item in resumed_input(session.model, -1)]
    assert sent == [
        ("user", None),
        ("function_call", "c1"),
        ("function_call", "c2"),
        ("function_call_output", "c1"),
        ("function_call_output", "c2"),
    ]


async def test_a_denied_first_call_still_pauses_on_the_second():
    ran: list[str] = []
    session = Session(
        Agent(name="a", tools=[counted_refund(ran)]),
        [
            [tool_call("c1", "refund", '{"order_id": "1"}'), tool_call("c2", "refund", '{"order_id": "2"}')],
            [message("Done.", "m2")],
        ],
    )
    await session.send("refund 1 and 2")
    out = await session.decide(False)
    assert calls(out) == [("TOOL_RESULT", "c1"), ("TOOL_APPROVAL_REQUIRED", "c2")]
    out = await session.decide(True)
    assert out[0]["payload"] == {"tool_call_id": "c2", "result": "Refunded 2"}
    assert ran == ["2"]


async def test_two_gated_calls_and_a_sibling_each_run_once():
    ran: list[str] = []
    looked: list[str] = []

    @function_tool
    async def lookup_order(order_id: str) -> str:
        looked.append(order_id)
        return "shipped"

    session = Session(
        Agent(name="a", tools=[lookup_order, counted_refund(ran)]),
        [
            [
                tool_call("c1", "lookup_order", '{"order_id": "9"}'),
                tool_call("c2", "refund", '{"order_id": "1"}'),
                tool_call("c3", "refund", '{"order_id": "2"}'),
            ],
            [message("Done.", "m2")],
        ],
    )
    out = await session.send("check 9, refund 1 and 2")
    assert calls(out)[-2:] == [("TOOL_RESULT", "c1"), ("TOOL_APPROVAL_REQUIRED", "c2")]
    out = await session.decide(True)
    assert calls(out) == [("TOOL_RESULT", "c2"), ("TOOL_APPROVAL_REQUIRED", "c3")]
    out = await session.decide(True)
    assert calls(out) == [("TOOL_RESULT", "c3")]
    assert types(out)[-1] == "NO_MORE_ACTIONS"
    assert looked == ["9"] and ran == ["1", "2"]


async def test_tracing_disabled_by_default():
    handler = openai_agents(AGENT, model=ScriptedModel([[message("hi", "m1")]]))
    ctx = SessionContext(transcript=[], reason={"kind": "user_message", "input": "x"})
    # would raise if tracing were enabled without an OpenAI tracing sink
    out = [frame async for frame in drive(handler, ctx)]
    assert types(out) == ["MESSAGE_START", "MESSAGE_DELTA", "MESSAGE_END", "NO_MORE_ACTIONS"]

# ---- conversation-history regression (M1 run finding) -------------------


async def test_transcript_items_accepted_by_provider_converter():
    """The adapter's item vocabulary must be accepted by the SDK's REAL
    chat-completions converter — the exact code path that died live three
    times (dict content part, unknown item name) while every ScriptedModel
    test sailed past it: the fake never runs provider conversion. This
    test converts what `_transcript_items` emits for a rich transcript
    (user input, assistant text, tool round-trip, string user input)
    and asserts the resulting chat-completions message shapes."""
    from agents.models.chatcmpl_converter import Converter

    transcript = [
        envelope(
            0,
            "INVOCATION_STARTED",
            {"reason": {"kind": "user_message", "input": "Check TX-1"}},
        ),
        envelope(1, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        envelope(
            2,
            "MESSAGE_DELTA",
            {"message_id": "m1", "delta": {"type": "text", "text": "Order TX-1 shipped."}},
        ),
        envelope(3, "MESSAGE_END", {"message_id": "m1"}),
        envelope(
            4,
            "TOOL_CALL",
            {"tool_call_id": "c1", "name": "lookup_order", "arguments": {"order_id": "TX-1"}},
        ),
        envelope(
            5,
            "TOOL_RESULT",
            {"tool_call_id": "c1", "result": {"order_id": "TX-1", "status": "shipped"}},
        ),
        envelope(6, "MESSAGE_START", {"role": "assistant", "message_id": "m2"}),
        envelope(
            7,
            "MESSAGE_DELTA",
            {"message_id": "m2", "delta": {"type": "text", "text": "Shipped."}},
        ),
        envelope(8, "MESSAGE_END", {"message_id": "m2"}),
        envelope(9, "INVOCATION_STARTED", {"reason": {"kind": "user_message", "input": "Refund it please"}}),
    ]
    ctx = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": "Refund it please"})
    items = _transcript_items(ctx)

    # Must not raise — this converter is what rejected the live items.
    messages = Converter.items_to_messages(items)

    assert [m["role"] for m in messages] == ["user", "assistant", "tool", "assistant", "user"]
    assert messages[0]["content"] == "Check TX-1"
    # assistant text with its tool call in one message, as the model wrote
    # them, then the tool result, then the post-tool assistant
    # text (bug-3 regression: the item vocabulary must survive this
    # conversion)
    assert messages[1]["content"] == "Order TX-1 shipped."
    assert messages[1]["tool_calls"][0]["function"]["name"] == "lookup_order"
    assert messages[1]["tool_calls"][0]["function"]["arguments"] == '{"order_id": "TX-1"}'
    assert messages[2]["tool_call_id"] == "c1"
    assert messages[2]["content"] == '{"order_id": "TX-1", "status": "shipped"}'
    assert messages[3]["content"] == "Shipped."
    assert messages[4]["content"] == "Refund it please"


# ---- approved tool results on the resume path ------------------------
#
# The adapter runs the approved tool itself and converts its result as the
# SDK's runner does; the conformance `results_gated` cases cover plain
# values. These cover a tool with a declared output type, which they don't.


class Refund(BaseModel):
    order_id: str
    status: str


def approved_refund_transcript() -> list[Envelope]:
    args = {"order_id": "42"}
    return [
        *user_message_transcript("refund 42"),
        envelope(1, "TOOL_CALL", {"tool_call_id": "c1", "name": "refund", "arguments": args}),
        envelope(2, "TOOL_APPROVAL_REQUIRED", {"tool_call_id": "c1", "name": "refund", "arguments": args}),
        envelope(3, "INVOCATION_STARTED", {"reason": {"kind": "approval_decision", "decision": {"approved": True}}}),
    ]


async def resume(agent: Agent, approved: bool = True, transcript: list[Envelope] | None = None, **kwargs: Any):
    """An approval resume of `transcript` (the refund of order 42 by
    default); the model answers "Done.". Returns the frames and the model."""
    model = ScriptedModel([[message("Done.", "m2")]])
    handler = openai_agents(agent, model=model, **kwargs)
    ctx = resume_context(transcript or approved_refund_transcript(), approved)
    return [frame async for frame in drive(handler, ctx)], model


def resumed_input(model: ScriptedModel, call: int = 0) -> list[dict]:
    """What the model was sent on the resume (or its `call`th request), as
    plain dicts."""
    return [item if isinstance(item, dict) else item.model_dump(exclude_none=True) for item in model.calls[call].input]


async def test_an_approved_tool_with_an_output_type_is_validated_and_converted():
    @function_tool(needs_approval=True, output_type=Refund)
    async def refund(order_id: str) -> Refund:
        return Refund(order_id=order_id, status="refunded")

    out, model = await resume(Agent(name="a", tools=[refund]))
    journaled = '{"order_id":"42","status":"refunded"}'
    assert out[0] == {"type": "TOOL_RESULT", "payload": {"tool_call_id": "c1", "result": journaled}}
    assert resumed_input(model)[-1] == {"type": "function_call_output", "call_id": "c1", "output": journaled}


async def test_an_approved_tool_with_an_output_type_that_raises_journals_the_sdk_error():
    """The SDK's error message isn't validated against the output schema,
    as in the runner: the resume doesn't crash, and journals what an ungated
    run of the tool does (the message's wording varies by SDK version)."""

    def build(needs_approval: bool):
        @function_tool(needs_approval=needs_approval, output_type=Refund)
        async def refund(order_id: str) -> Refund:
            raise RuntimeError("payment provider down")

        return Agent(name="a", tools=[refund])

    out, _ = await resume(build(True))
    model = ScriptedModel([[tool_call("c1", "refund", '{"order_id": "42"}')], [message("Done.", "m2")]])
    ctx = SessionContext(transcript=[], reason={"kind": "user_message", "input": "refund 42"})
    ungated = [frame async for frame in drive(openai_agents(build(False), model=model), ctx)]
    assert out[0]["type"] == "TOOL_RESULT"
    assert out[0]["payload"] == next(f for f in ungated if f["type"] == "TOOL_RESULT")["payload"]


# ---- approval resume: the SDK runs (or rejects) the decided call ----------


def sibling_transcript(approved: bool) -> list[Envelope]:
    """`lookup_order` ran on the pause turn; the gated `refund` paused."""
    args = {"order_id": "42"}
    return [
        *user_message_transcript("check and refund 42"),
        envelope(1, "MESSAGE_START", {"role": "assistant", "message_id": "x"}),
        envelope(2, "MESSAGE_DELTA", {"message_id": "x", "delta": {"type": "text", "text": "On it."}}),
        envelope(3, "MESSAGE_END", {"message_id": "x"}),
        envelope(4, "TOOL_CALL", {"tool_call_id": "c1", "name": "lookup_order", "arguments": args}),
        envelope(5, "TOOL_CALL", {"tool_call_id": "c2", "name": "refund", "arguments": args}),
        envelope(6, "TOOL_RESULT", {"tool_call_id": "c1", "result": "shipped"}),
        envelope(7, "TOOL_APPROVAL_REQUIRED", {"tool_call_id": "c2", "name": "refund", "arguments": args}),
        envelope(8, "INVOCATION_STARTED", {"reason": {"kind": "approval_decision", "decision": {"approved": approved}}}),
    ]


@pytest.mark.parametrize("approved", [True, False], ids=["approve", "deny"])
async def test_the_model_sees_the_paused_response_in_order_and_the_sibling_runs_once(approved: bool):
    ran: list[str] = []

    @function_tool
    async def lookup_order(order_id: str) -> str:
        ran.append(order_id)
        return "shipped"

    agent = Agent(name="a", tools=[lookup_order, refund])
    out, model = await resume(agent, approved, sibling_transcript(approved))
    answer = "Refunded 42" if approved else "Tool execution was not approved."
    assert types(out) == ["TOOL_RESULT", "MESSAGE_START", "MESSAGE_DELTA", "MESSAGE_END", "NO_MORE_ACTIONS"]
    assert out[0]["payload"] == {"tool_call_id": "c2", "result": answer}
    assert ran == []
    sent = [(item.get("type") or item["role"], item.get("call_id"), item.get("output")) for item in resumed_input(model)]
    assert sent == [
        ("user", None, None),
        ("message", None, None),
        ("function_call", "c1", None),
        ("function_call", "c2", None),
        ("function_call_output", "c1", "shipped"),
        ("function_call_output", "c2", answer),
    ]


class ToolStarts(AgentHooks):
    def __init__(self) -> None:
        self.started: list[str] = []

    async def on_tool_start(self, context, agent, tool):
        self.started.append(tool.name)


@pytest.mark.parametrize("approved", [True, False], ids=["approve", "deny"])
async def test_agent_hooks_run_for_an_approved_tool_on_resume(approved: bool):
    hooks = ToolStarts()
    out, _ = await resume(Agent(name="a", tools=[refund], hooks=hooks), approved)
    assert types(out)[0] == "TOOL_RESULT"
    assert hooks.started == (["refund"] if approved else [])


@pytest.mark.parametrize("approved", [True, False], ids=["approve", "deny"])
async def test_tool_guardrails_run_for_an_approved_tool_on_resume(approved: bool):
    checked: list[str] = []

    @tool_input_guardrail
    def check_input(data) -> ToolGuardrailFunctionOutput:
        checked.append("input")
        return ToolGuardrailFunctionOutput.allow()

    @tool_output_guardrail
    def redact(data) -> ToolGuardrailFunctionOutput:
        checked.append("output")
        return ToolGuardrailFunctionOutput.reject_content("[redacted]")

    @function_tool(
        needs_approval=True, tool_input_guardrails=[check_input], tool_output_guardrails=[redact]
    )
    async def refund(order_id: str) -> str:
        return f"Refunded {order_id}"

    out, _ = await resume(Agent(name="a", tools=[refund]), approved)
    if approved:
        assert checked == ["input", "output"]
        assert out[0]["payload"]["result"] == "[redacted]"
    else:
        assert checked == []
        assert out[0]["payload"]["result"] == "Tool execution was not approved."


async def test_the_context_reaches_an_approved_tool_on_resume():
    seen: list[object] = []

    @function_tool(needs_approval=True)
    async def refund(ctx: RunContextWrapper[dict], order_id: str) -> str:
        seen.append(ctx.context)
        return f"Refunded {order_id}"

    out, _ = await resume(Agent(name="a", tools=[refund]), context=lambda session: {"user": "u1"})
    assert out[0]["payload"]["result"] == "Refunded 42"
    assert seen == [{"user": "u1"}]


async def test_a_denied_call_gets_the_run_configs_rejection_message():
    """The resume runs with the caller's run_config, not a fresh one."""
    model = ScriptedModel([[message("Done.", "m2")]])
    config = RunConfig(
        model=model,
        tracing_disabled=True,
        tool_error_formatter=lambda args: f"{args.tool_name} was declined",
    )
    handler = openai_agents(Agent(name="a", tools=[refund]), run_config=config)
    out = [frame async for frame in drive(handler, resume_context(approved_refund_transcript(), False))]
    assert out[0]["payload"] == {"tool_call_id": "c1", "result": "refund was declined"}


async def test_an_approved_tool_missing_from_the_agent_fails_the_resume():
    out, _ = await resume(Agent(name="a", tools=[lookup_order]))
    assert "ERROR" in types(out)
    assert "'refund' is not on the agent" in next(f for f in out if f["type"] == "ERROR")["payload"]["message"]


# ---- handoffs ------------------------------------------------------------
#
# The conversation stays with the agent it was handed off to: the transfer
# is journaled, and each turn continues with the agent the journaled
# transfers lead to. Conformance covers one handoff, the next turn and a
# handoff back, on every endpoint (`scenarios/handoff.rs`).


def _agent(name: str, steps: list, **kwargs: Any) -> Agent:
    """An agent running its own scripted model (`agent.model.calls`)."""
    return Agent(name=name, instructions=f"You are {name}.", model=ScriptedModel(steps), **kwargs)


def transfer(cid: str, to: str) -> ResponseFunctionToolCall:
    return tool_call(cid, f"transfer_to_{to}", "{}")


def results(frames: list[dict]) -> dict[str, Any]:
    return {f["payload"]["tool_call_id"]: f["payload"]["result"] for f in frames if f["type"] == "TOOL_RESULT"}


def sent(model: Any, call: int) -> list[tuple]:
    """What the model was sent on its `call`th request, as (kind, call id,
    output or text): turn 1's and a later turn's compare equal."""
    shapes = []
    for item in resumed_input(model, call):
        if item.get("type") == "message" or "role" in item:
            content = item["content"]
            text = content if isinstance(content, str) else "".join(part["text"] for part in content)
            shapes.append((item["role"], None, text))
        else:
            shapes.append((item["type"], item["call_id"], item.get("output")))
    return shapes


async def test_a_handoff_is_journaled_and_the_next_turn_continues_with_its_agent():
    orders = _agent("orders", [[message("Which order?", "o1")], [message("It shipped.", "o2")]])
    triage = _agent("triage", [[transfer("h1", "orders")]], handoffs=[orders])
    session = Session(triage)

    first = await session.send("Where is my order?")
    assert calls(first) == [("TOOL_CALL", "h1"), ("TOOL_RESULT", "h1")]
    assert first[types(first).index("TOOL_CALL")]["payload"]["name"] == "transfer_to_orders"
    assert results(first) == {"h1": '{"assistant": "orders"}'}

    await session.send("42")
    assert len(triage.model.calls) == 1
    assert len(orders.model.calls) == 2
    # The next turn replays what the agent saw, the transfer included.
    assert sent(orders.model, 1)[:3] == sent(orders.model, 0) == [
        ("user", None, "Where is my order?"),
        ("function_call", "h1", None),
        ("function_call_output", "h1", '{"assistant": "orders"}'),
    ]


async def test_a_handoff_object_with_a_tool_name_override_is_followed():
    handed: list[str] = []
    orders = _agent("orders", [[message("Which order?", "o1")], [message("It shipped.", "o2")]])
    triage = _agent(
        "triage",
        [[tool_call("h1", "to_orders_desk", "{}")]],
        handoffs=[handoff(orders, tool_name_override="to_orders_desk", on_handoff=lambda ctx: handed.append("orders"))],
    )
    session = Session(triage)
    await session.send("Where is my order?")
    await session.send("42")
    assert len(orders.model.calls) == 2
    assert handed == ["orders"]  # on the handoff, not on the turn after it


async def test_two_handoffs_in_one_turn_lead_to_the_last_agent():
    orders = _agent("orders", [[message("Which order?", "o1")], [message("It shipped.", "o2")]])
    billing = _agent("billing", [[transfer("h2", "orders")]], handoffs=[orders])
    triage = _agent("triage", [[transfer("h1", "billing")]], handoffs=[billing])
    session = Session(triage)
    first = await session.send("Where is my order?")
    assert results(first) == {"h1": '{"assistant": "billing"}', "h2": '{"assistant": "orders"}'}
    await session.send("42")
    assert (len(triage.model.calls), len(billing.model.calls), len(orders.model.calls)) == (1, 1, 2)


async def test_a_handoff_back_is_followed_across_turns():
    triage = _agent("triage", [[transfer("h1", "orders")], [message("Anything else?", "t1")], [message("Bye.", "t2")]])
    orders = _agent("orders", [[message("Which order?", "o1")], [transfer("h2", "triage")]], handoffs=[triage])
    triage.handoffs = [orders]
    session = Session(triage)
    await session.send("Where is my order?")
    second = await session.send("Never mind.")
    assert results(second) == {"h2": '{"assistant": "triage"}'}
    await session.send("Thanks.")
    assert (len(triage.model.calls), len(orders.model.calls)) == (3, 2)


async def test_of_several_handoffs_in_one_response_the_first_is_followed():
    orders = _agent("orders", [[message("Which order?", "o1")], [message("It shipped.", "o2")]])
    billing = _agent("billing", [])
    triage = _agent("triage", [[transfer("h1", "orders"), transfer("h2", "billing")]], handoffs=[orders, billing])
    session = Session(triage)
    first = await session.send("Where is my order?")
    assert results(first) == {
        "h1": '{"assistant": "orders"}',
        "h2": "Multiple handoffs detected, ignoring this one.",
    }
    await session.send("42")
    assert (len(orders.model.calls), len(billing.model.calls)) == (2, 0)


@pytest.mark.parametrize("by", ["code", "policy"])
async def test_a_gated_tool_on_the_starting_agent_pauses_and_resumes_after_a_handoff_back(
    by: str, monkeypatch: pytest.MonkeyPatch
):
    """The agent handed back to runs as the adapter prepares the starting
    agent (the policy applied), and the resume continues with it."""
    if by == "policy":
        monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["cancel_order"]')
    ran: list[str] = []

    async def cancel_order(order_id: str) -> str:
        """Cancel an order."""
        ran.append(order_id)
        return "Cancelled"

    triage = _agent(
        "triage",
        [[transfer("h1", "orders")], [tool_call("c1", "cancel_order", '{"order_id": "42"}')], [message("Done.", "t1")]],
        tools=[function_tool(cancel_order, needs_approval=by == "code")],
    )
    orders = _agent("orders", [[transfer("h2", "triage")]], handoffs=[triage])
    triage.handoffs = [orders]
    session = Session(triage)
    first = await session.send("Cancel 42.")
    assert types(first)[-1] == "TOOL_APPROVAL_REQUIRED"
    assert ran == []
    out = await session.decide(True)
    assert results(out) == {"c1": "Cancelled"}
    assert ran == ["42"]
    assert (len(triage.model.calls), len(orders.model.calls)) == (3, 1)


async def test_a_handoff_after_a_gated_call_in_its_response_happens_on_the_resume():
    """The SDK hands off once the response's tools ran: the paused
    response's transfer is carried out after the approved call."""
    ran: list[str] = []
    orders = _agent("orders", [[message("Refund done; anything else?", "o1")], [message("Bye.", "o2")]])
    triage = _agent(
        "triage",
        [[tool_call("c1", "refund", '{"order_id": "42"}'), transfer("h1", "orders")]],
        tools=[counted_refund(ran)],
        handoffs=[orders],
    )
    session = Session(triage)
    first = await session.send("Refund 42, then let me talk to orders.")
    assert types(first)[-1] == "TOOL_APPROVAL_REQUIRED"
    out = await session.decide(True)
    assert results(out) == {"c1": "Refunded 42", "h1": '{"assistant": "orders"}'}
    assert ran == ["42"]
    await session.send("No.")
    assert (len(triage.model.calls), len(orders.model.calls)) == (1, 2)


async def test_a_journaled_handoff_the_agent_no_longer_has_fails_the_turn():
    orders = _agent("orders", [[message("Which order?", "o1")]])
    session = Session(_agent("triage", [[transfer("h1", "orders")]], handoffs=[orders]))
    await session.send("Where is my order?")
    # A redeploy removed the handoff.
    triage = _agent("triage", [[message("Hi.", "t1")]])
    session.handler = openai_agents(triage)
    out = await session.send("42")
    assert [f["payload"].get("code") for f in out if f["type"] == "ERROR"] == ["openai_agents_unknown_handoff"]
    assert "transfer_to_orders" in out[types(out).index("ERROR")]["payload"]["message"]
    assert triage.model.calls == ()


async def test_a_tool_result_that_looks_like_a_transfer_isnt_a_handoff():
    @function_tool
    async def whois(name: str) -> str:
        """Who is it?"""
        return '{"assistant": "orders"}'

    triage = _agent(
        "triage",
        [[tool_call("c1", "whois", '{"name": "x"}')], [message("It's orders.", "t1")], [message("Bye.", "t2")]],
        tools=[whois],
    )
    session = Session(triage)
    await session.send("Who is x?")
    out = await session.send("Thanks.")
    assert types(out)[-1] == "NO_MORE_ACTIONS"
    assert len(triage.model.calls) == 3


def _by_hand(agent: Agent) -> Handoff:
    async def invoke(ctx: Any, arguments: str) -> Agent:
        return agent

    return Handoff(
        tool_name="transfer_to_orders",
        tool_description="Hand off to orders.",
        input_json_schema={},
        on_invoke_handoff=invoke,
        agent_name=agent.name,
    )


@pytest.mark.parametrize(
    ("handoffs", "run_config", "match"),
    [
        (lambda o: [handoff(o, input_filter=lambda data: data)], None, "sets input_filter"),
        (lambda o: [handoff(o, nest_handoff_history=True)], None, "sets nest_handoff_history"),
        (lambda o: [o], RunConfig(nest_handoff_history=True), "run_config sets nest_handoff_history"),
        (lambda o: [o], RunConfig(handoff_input_filter=lambda data: data), "run_config sets handoff_input_filter"),
        (lambda o: [_by_hand(o)], None, "Handoff built by hand"),
    ],
    ids=["input_filter", "nest_handoff_history", "run_config_nest", "run_config_filter", "by_hand"],
)
def test_a_handoff_sectr_cant_continue_through_is_rejected(handoffs: Any, run_config: Any, match: str):
    orders = Agent(name="orders")
    with pytest.raises(ValueError, match=match):
        openai_agents(Agent(name="triage", handoffs=handoffs(orders)), run_config=run_config)


def test_a_handoff_two_handoffs_away_with_an_input_filter_is_rejected():
    orders = Agent(name="orders")
    billing = Agent(name="billing", handoffs=[handoff(orders, input_filter=lambda data: data)])
    with pytest.raises(ValueError, match="'transfer_to_orders' on agent 'billing' sets input_filter"):
        openai_agents(Agent(name="triage", handoffs=[billing]))


def test_history_settings_without_handoffs_are_accepted():
    openai_agents(Agent(name="triage"), run_config=RunConfig(nest_handoff_history=True))


def test_a_subagents_hand_built_handoff_is_accepted():
    """Inside an `as_tool()` agent's run, which is opaque, a handoff needn't
    be continued through."""
    specialist = Agent(name="specialist", handoffs=[_by_hand(Agent(name="orders"))])
    openai_agents(Agent(name="triage", tools=[_as_tool(specialist)]))


# ---- known bugs: each asserts the right behaviour, and fails until its issue is fixed


@pytest.mark.xfail(strict=True, reason="a handoff agent's policy-named tool is rejected (subagent_gated_tool), not gated")
async def test_platform_policy_gates_a_handoff_agents_tool(monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["lookup_order"]')
    ran: list[str] = []

    @function_tool
    async def lookup_order(order_id: str) -> str:
        ran.append(order_id)
        return "shipped"

    triage = Agent(name="triage", handoffs=[Agent(name="orders", tools=[lookup_order])])
    model = ScriptedModel(
        [
            [tool_call("h1", "transfer_to_orders", "{}")],
            [tool_call("c1", "lookup_order", '{"order_id": "42"}')],
            [message("It shipped.", "m1")],
        ]
    )
    ctx = SessionContext(transcript=[], reason={"kind": "user_message", "input": "where is 42?"})
    out = [frame async for frame in drive(openai_agents(triage, model=model), ctx)]
    assert types(out)[-1] == "TOOL_APPROVAL_REQUIRED"
    assert ran == []


# ---- run context ----------------------------------------------------------


def whoami_tool(seen: list[object]) -> Any:
    @function_tool
    async def whoami(ctx: RunContextWrapper[dict]) -> str:
        seen.append(ctx.context)
        return "ok"

    return whoami


async def test_a_context_factory_reaches_the_tools():
    seen: list[object] = []
    calls: list[SessionContext] = []

    async def context(session: SessionContext) -> dict:
        calls.append(session)
        return {"user": "u1"}

    model = ScriptedModel(
        [[tool_call("c1", "whoami", "{}")], [tool_call("c2", "whoami", "{}")], [message("done", "m1")]]
    )
    handler = openai_agents(Agent(name="a", tools=[whoami_tool(seen)]), model=model, context=context)
    ctx = SessionContext(transcript=[], reason={"kind": "user_message", "input": "who am I?"})
    [frame async for frame in drive(handler, ctx)]
    assert seen == [{"user": "u1"}, {"user": "u1"}]
    assert calls == [ctx]  # once per invocation, not per tool call


async def test_the_context_reaches_the_run_after_an_approval():
    """The resumed run gets the context: a tool the model calls after the
    approved one reads it (the approved call itself is tested separately)."""
    seen: list[object] = []
    model = ScriptedModel([[tool_call("c2", "whoami", "{}")], [message("done", "m2")]])
    handler = openai_agents(
        Agent(name="a", tools=[refund, whoami_tool(seen)]),
        model=model,
        context=lambda session: {"user": "u1"},
    )
    out = [frame async for frame in drive(handler, resume_context(approved_refund_transcript(), True))]
    assert types(out)[0] == "TOOL_RESULT"
    assert seen == [{"user": "u1"}]


async def test_without_a_context_factory_the_run_context_is_none():
    seen: list[object] = []
    model = ScriptedModel([[tool_call("c1", "whoami", "{}")], [message("done", "m1")]])
    handler = openai_agents(Agent(name="a", tools=[whoami_tool(seen)]), model=model)
    ctx = SessionContext(transcript=[], reason={"kind": "user_message", "input": "who am I?"})
    [frame async for frame in drive(handler, ctx)]
    assert seen == [None]


async def test_the_context_reaches_the_mcp_tool_listing():
    """The adapter lists the agent's MCP tools itself; a server (or its
    tool filter) sees the run context there, as when the SDK lists them."""
    seen: list[object] = []

    class Server(_FakeMcpServer):
        async def list_tools(self, run_context: Any = None, agent: Any = None) -> list[mcp_types.Tool]:
            seen.append(run_context.context)
            return await super().list_tools(run_context, agent)

    agent = Agent(name="a", mcp_servers=[Server(_mcp_body([]))])
    handler = openai_agents(
        agent, model=ScriptedModel([[message("hi", "m1")]]), context=lambda session: {"user": "u1"}
    )
    ctx = SessionContext(transcript=[], reason={"kind": "user_message", "input": "hi"})
    [frame async for frame in drive(handler, ctx)]
    assert seen and all(value == {"user": "u1"} for value in seen)


# ---- structured output (not supported yet) -------------------------------


class _Out(BaseModel):
    answer: str


def test_structured_output_on_a_handoff_agent_is_rejected():
    typed = Agent(name="typed", output_type=_Out)
    with pytest.raises(ValueError, match="'typed' has output_type=_Out"):
        openai_agents(Agent(name="triage", handoffs=[typed]))


def test_text_output_is_accepted():
    plain = Agent(name="plain")
    openai_agents(Agent(name="a", output_type=str, handoffs=[plain]))


# ---- subagents -----------------------------------------------------
#
# Each shape users write subagents in, against the beta contract
# (`tests/subagent_contract.py`): opaque when its tools are ungated, and a
# gated tool inside it (in code, or named by the policy) fails loudly.

INNER_TEXT = "Let me check with the carrier."


def _specialist(tool: Any, name: str = "specialist") -> Agent:
    """A subagent whose model says `INNER_TEXT`, calls `tool` on order 42,
    then answers."""
    return Agent(
        name=name,
        model=ScriptedModel(
            [
                [message(INNER_TEXT, "im1"), tool_call("i1", tool.name, '{"order_id": "42"}')],
                [message("Done.", "im2")],
            ]
        ),
        tools=[tool],
    )


def _outer(tools: list[Any] | None = None, handoffs: list[Any] | None = None, first: Any = None) -> Agent:
    """The agent under test: its model calls `first` (by default
    `ask_specialist` with the user's question), then answers."""
    first = first or tool_call("c1", "ask_specialist", '{"input": "Cancel order 42."}')
    return Agent(
        name="support",
        model=ScriptedModel([[first], [message("All done.", "m1")]]),
        tools=tools or [],
        handoffs=handoffs or [],
    )


@function_tool
async def track_parcel(order_id: str) -> str:
    """Track an order's parcel."""
    return "In transit"


@pytest.fixture(params=["code", "policy"])
def gated(request: pytest.FixtureRequest, monkeypatch: pytest.MonkeyPatch) -> tuple[Any, list[str]]:
    """`cancel_order`, gated in code or named by `SECTR_APPROVAL_POLICY`, and
    the order ids it ran for."""
    ran: list[str] = []
    if request.param == "policy":
        monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["cancel_order"]')

    async def cancel_order(order_id: str) -> str:
        """Cancel an order."""
        ran.append(order_id)
        return "Cancelled"

    return function_tool(cancel_order, needs_approval=request.param == "code"), ran


def _as_tool(agent: Agent, name: str = "ask_specialist") -> Any:
    return agent.as_tool(tool_name=name, tool_description="Ask the specialist.")


def _hand_rolled(agent: Agent) -> Any:
    """A `function_tool` running the subagent itself, which the adapter can't
    see."""

    @function_tool
    async def ask_specialist(input: str) -> str:  # noqa: A002
        result = await Runner.run(agent, input)
        return str(result.final_output)

    return ask_specialist


def _nested(inner: Any) -> Any:
    """`ask_middle`, an `as_tool()` agent whose own tool is `inner`."""
    middle = Agent(
        name="middle",
        model=ScriptedModel(
            [[tool_call("mc1", "ask_specialist", '{"input": "Cancel order 42."}')], [message("Done.", "mm1")]]
        ),
        tools=[inner],
    )
    return _as_tool(middle, "ask_middle")


class _FakeMcpServer(MCPServer):
    """An MCP server serving one tool, in process (no connection needed)."""

    def __init__(self, fn: Callable[..., Any], require_approval: Any = None) -> None:
        super().__init__(require_approval=require_approval)
        self.fn = fn

    @property
    def name(self) -> str:
        return "fake"

    async def connect(self) -> None:
        pass

    async def cleanup(self) -> None:
        pass

    async def list_tools(self, run_context: Any = None, agent: Any = None) -> list[mcp_types.Tool]:
        schema = {"type": "object", "properties": {"order_id": {"type": "string"}}, "required": ["order_id"]}
        return [mcp_types.Tool(name="cancel_order", description="Cancel an order.", inputSchema=schema)]

    async def call_tool(self, tool_name: str, arguments: dict[str, Any] | None, meta: Any = None) -> Any:
        text = await self.fn(**(arguments or {}))
        return mcp_types.CallToolResult(content=[mcp_types.TextContent(type="text", text=text)])

    async def list_prompts(self) -> Any:
        return mcp_types.ListPromptsResult(prompts=[])

    async def get_prompt(self, name: str, arguments: dict[str, Any] | None = None) -> Any:
        raise NotImplementedError


# Opaque: an ungated subagent runs, and only the outer call reaches the journal.


async def test_an_as_tool_subagent_is_opaque():
    frames = await run_turn(openai_agents(_outer([_as_tool(_specialist(track_parcel))])), "Where is 42?")
    assert_opaque(frames, "ask_specialist", INNER_TEXT)


async def test_a_nested_as_tool_subagent_is_opaque():
    inner = _as_tool(_specialist(track_parcel))
    first = tool_call("c1", "ask_middle", '{"input": "Where is 42?"}')
    frames = await run_turn(openai_agents(_outer([_nested(inner)], first=first)), "Where is 42?")
    assert_opaque(frames, "ask_middle", INNER_TEXT)


async def test_a_hand_rolled_subagent_is_opaque():
    frames = await run_turn(openai_agents(_outer([_hand_rolled(_specialist(track_parcel))])), "Where is 42?")
    assert_opaque(frames, "ask_specialist", INNER_TEXT)


# Fail loudly: a gated tool inside the subagent never runs.


def _reaching(specialist: Agent, shape: str) -> Agent:
    """The outer agent, reaching `specialist` (whose model calls the gated
    tool) by `shape`: as an `as_tool()` subagent, one nested inside another,
    or a handoff (an `Agent`, or a `handoff()` object)."""
    if shape == "as_tool":
        return _outer([_as_tool(specialist)])
    if shape == "nested":
        return _outer([_nested(_as_tool(specialist))], first=tool_call("c1", "ask_middle", '{"input": "Cancel order 42."}'))
    transfer = tool_call("h1", "transfer_to_specialist", "{}")
    return _outer(handoffs=[specialist if shape == "handoff" else handoff(specialist)], first=transfer)


SHAPES = ["as_tool", "nested", "handoff", "handoff_object"]


@pytest.mark.parametrize("shape", SHAPES)
async def test_a_gated_tool_in_a_subagent_fails_loudly(gated: tuple[Any, list[str]], shape: str):
    tool, ran = gated
    await assert_fails_loudly(lambda: openai_agents(_reaching(_specialist(tool), shape)), "Cancel 42.", ran)


async def test_a_gated_tool_two_handoffs_away_fails_loudly(gated: tuple[Any, list[str]]):
    tool, ran = gated
    middle = Agent(
        name="middle",
        model=ScriptedModel([[tool_call("h2", "transfer_to_specialist", "{}")]]),
        handoffs=[_specialist(tool)],
    )
    first = tool_call("h1", "transfer_to_middle", "{}")
    await assert_fails_loudly(lambda: openai_agents(_outer(handoffs=[middle], first=first)), "Cancel 42.", ran)


async def test_a_gated_tool_in_a_handoff_agents_subagent_fails_loudly(gated: tuple[Any, list[str]]):
    tool, ran = gated
    middle = Agent(
        name="middle",
        model=ScriptedModel(
            [[tool_call("mc1", "ask_specialist", '{"input": "Cancel order 42."}')], [message("Done.", "mm1")]]
        ),
        tools=[_as_tool(_specialist(tool))],
    )
    first = tool_call("h1", "transfer_to_middle", "{}")
    await assert_fails_loudly(lambda: openai_agents(_outer(handoffs=[middle], first=first)), "Cancel 42.", ran)


async def test_a_gated_tool_on_a_subagents_handoff_agent_fails_loudly(gated: tuple[Any, list[str]]):
    tool, ran = gated
    middle = Agent(
        name="middle",
        model=ScriptedModel([[tool_call("h2", "transfer_to_specialist", "{}")]]),
        handoffs=[_specialist(tool)],
    )
    first = tool_call("c1", "ask_middle", '{"input": "Cancel order 42."}')
    build = lambda: openai_agents(_outer([_as_tool(middle, "ask_middle")], first=first))  # noqa: E731
    await assert_fails_loudly(build, "Cancel 42.", ran)


async def test_a_callable_needs_approval_in_a_subagent_fails_loudly():
    ran: list[str] = []

    async def always(ctx: Any, params: Any, call_id: str) -> bool:
        return True

    async def cancel_order(order_id: str) -> str:
        """Cancel an order."""
        ran.append(order_id)
        return "Cancelled"

    tool = function_tool(cancel_order, needs_approval=always)
    await assert_fails_loudly(lambda: openai_agents(_outer([_as_tool(_specialist(tool))])), "Cancel 42.", ran)


def _mcp_body(ran: list[str]) -> Callable[..., Any]:
    async def cancel_order(order_id: str) -> str:
        ran.append(order_id)
        return "Cancelled"

    return cancel_order


def _mcp_specialist(ran: list[str], require_approval: Any = None) -> Agent:
    """A subagent whose MCP server serves `cancel_order`; its model says
    `INNER_TEXT`, calls it on order 42, then answers."""
    return Agent(
        name="specialist",
        model=ScriptedModel(
            [
                [message(INNER_TEXT, "im1"), tool_call("i1", "cancel_order", '{"order_id": "42"}')],
                [message("Done.", "im2")],
            ]
        ),
        mcp_servers=[_FakeMcpServer(_mcp_body(ran), require_approval=require_approval)],
    )


@pytest.mark.parametrize("shape", SHAPES)
async def test_a_gated_mcp_tool_on_a_subagent_fails_loudly(gated: tuple[Any, list[str]], shape: str):
    tool, ran = gated
    # Gated in code: the server's `require_approval`; else the policy names it.
    approval = {"cancel_order": "always"} if tool.needs_approval is True else None
    await assert_fails_loudly(lambda: openai_agents(_reaching(_mcp_specialist(ran, approval), shape)), "Cancel 42.", ran)


async def _always_approve(ctx: Any, agent: Any, tool: Any) -> bool:
    return True


@pytest.mark.parametrize(
    "require_approval",
    ["always", {"always": {"tool_names": ["cancel_order"]}}, _always_approve],
    ids=["always", "tool_list", "callable"],
)
async def test_every_form_of_mcp_require_approval_on_a_subagent_fails_loudly(require_approval: Any):
    ran: list[str] = []
    build = lambda: openai_agents(_outer([_as_tool(_mcp_specialist(ran, require_approval))]))  # noqa: E731
    await assert_fails_loudly(build, "Cancel 42.", ran)


# Where it fails: a tool gated in code when the agent is built (`sectr dev`
# and a deploy's build report it); one the policy names, which the runner
# only gets when it starts, at the start of each turn, in the journal.


@pytest.mark.parametrize("shape", SHAPES)
def test_a_tool_gated_in_code_in_a_subagent_is_rejected_at_construction(shape: str):
    with pytest.raises(ValueError, match="'refund' on agent 'specialist' needs approval.*subagent_gated_tool"):
        openai_agents(_reaching(_specialist(refund), shape))


@pytest.mark.parametrize("served", ["function", "mcp"])
@pytest.mark.parametrize("shape", SHAPES)
async def test_a_policy_named_tool_in_a_subagent_fails_the_turn_before_the_model_runs(
    shape: str, served: str, monkeypatch: pytest.MonkeyPatch
):
    ran: list[str] = []
    monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["cancel_order"]')
    if served == "mcp":
        specialist = _mcp_specialist(ran)
    else:
        specialist = _specialist(function_tool(_mcp_body(ran), name_override="cancel_order"))
    handler = openai_agents(_reaching(specialist, shape))  # builds: the policy isn't read yet
    frames = await run_turn(handler, "Cancel 42.")
    assert types(frames) == ["ERROR", "NO_MORE_ACTIONS"]
    assert frames[0]["payload"]["code"] == "subagent_gated_tool"
    assert "'cancel_order' on agent 'specialist' is in the deployment's approval policy" in frames[0]["payload"]["message"]
    assert ran == []


def test_building_an_agent_doesnt_read_the_policy(monkeypatch: pytest.MonkeyPatch):
    monkeypatch.setenv("SECTR_APPROVAL_POLICY", "not json")
    openai_agents(_outer([_as_tool(_specialist(track_parcel))]))


# The starting agent inside its own subagent's run is a subagent too.


def test_the_starting_agent_as_its_subagents_tool_is_a_subagent():
    support = Agent(name="support", tools=[refund])
    specialist = Agent(name="specialist", tools=[_as_tool(support, "ask_support")])
    support.tools = [refund, _as_tool(specialist)]
    with pytest.raises(ValueError, match="'refund' on agent 'support'.*subagent_gated_tool"):
        openai_agents(support)


def test_a_handoff_to_the_starting_agent_inside_a_subagent_is_a_subagent():
    support = Agent(name="support", tools=[refund])
    specialist = Agent(name="specialist", handoffs=[support])
    support.tools = [refund, _as_tool(specialist)]
    with pytest.raises(ValueError, match="'refund' on agent 'support'.*subagent_gated_tool"):
        openai_agents(support)


# Not too loud: what the contract allows keeps working.


@pytest.mark.parametrize("by", ["code", "policy"])
async def test_the_subagent_tool_itself_can_be_gated(by: str, monkeypatch: pytest.MonkeyPatch):
    if by == "policy":
        monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["ask_specialist"]')
    subagent = _specialist(track_parcel).as_tool(
        tool_name="ask_specialist", tool_description="Ask the specialist.", needs_approval=by == "code"
    )
    frames = await run_turn(openai_agents(_outer([subagent])), "Where is 42?")
    assert types(frames)[-1] == "TOOL_APPROVAL_REQUIRED"
    assert frames[-1]["payload"]["name"] == "ask_specialist"


@pytest.mark.parametrize(
    "require_approval", [None, "never", {"never": {"tool_names": ["cancel_order"]}}], ids=["none", "never", "tool_list"]
)
async def test_an_ungated_mcp_server_on_a_subagent_is_opaque(require_approval: Any, monkeypatch: pytest.MonkeyPatch):
    # The policy gates a tool, just not the subagent's.
    monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["refund"]')
    ran: list[str] = []
    agent = _outer([_as_tool(_mcp_specialist(ran, require_approval)), refund])
    frames = await run_turn(openai_agents(agent), "Cancel 42.")
    assert_opaque(frames, "ask_specialist", INNER_TEXT)
    assert ran == ["42"]


async def test_a_subagents_mcp_server_is_connected_for_the_invocation():
    server = _CountingMcpServer()
    specialist = Agent(name="specialist", model=ScriptedModel([[message("Fine.", "im1")]]), mcp_servers=[server])
    frames = await run_turn(openai_agents(_outer([_as_tool(specialist)])), "Where is 42?")
    assert_opaque(frames, "ask_specialist", INNER_TEXT)
    assert len(server.connects) == 1
    assert server.cleanups == server.connects


def test_a_handoff_back_to_the_starting_agent_keeps_its_gated_tools():
    """The starting agent isn't a subagent, even when an agent it hands off
    to hands back: its own gated tool pauses and resumes as usual."""
    triage = Agent(name="triage", tools=[refund])
    triage.handoffs = [Agent(name="orders", tools=[lookup_order], handoffs=[triage])]
    openai_agents(triage)


@pytest.mark.xfail(strict=True, reason="the adapter can't see a Runner.run inside a tool")
async def test_a_gated_tool_in_a_hand_rolled_subagent_fails_loudly(gated: tuple[Any, list[str]]):
    tool, ran = gated
    await assert_fails_loudly(lambda: openai_agents(_outer([_hand_rolled(_specialist(tool))])), "Cancel 42.", ran)


# MCP connections: conformance (`mcp/*`) runs one invocation at a time.


class _CountingMcpServer(_FakeMcpServer):
    """Records the task of each connect and cleanup; fails to connect while
    `down`."""

    def __init__(self) -> None:
        super().__init__(_mcp_body([]))
        self.connects: list[asyncio.Task[Any] | None] = []
        self.cleanups: list[asyncio.Task[Any] | None] = []
        self.down = False

    async def connect(self) -> None:
        if self.down:
            raise ConnectionError("server down")
        self.connects.append(asyncio.current_task())

    async def cleanup(self) -> None:
        self.cleanups.append(asyncio.current_task())


async def test_invocations_at_once_share_one_mcp_connection():
    server = _CountingMcpServer()
    first, second = asyncio.Event(), asyncio.Event()

    async def invocation(entered: asyncio.Event, leave: asyncio.Event) -> None:
        async with _shared(server):
            entered.set()
            await leave.wait()

    a = asyncio.create_task(invocation(first, second))
    await first.wait()
    done = asyncio.Event()
    b = asyncio.create_task(invocation(asyncio.Event(), done))
    await asyncio.sleep(0)
    assert len(server.connects) == 1
    second.set()
    await a
    assert server.cleanups == []  # b still uses it
    done.set()
    await b
    # One task owns the session, as the MCP client's task groups require.
    assert server.cleanups == server.connects

    async with _shared(server):
        assert len(server.connects) == 2  # the next invocation reconnects


async def test_a_failed_mcp_connect_fails_the_invocation_and_the_next_retries():
    server = _CountingMcpServer()
    server.down = True
    with pytest.raises(ConnectionError):
        async with _shared(server):
            pass
    server.down = False
    async with _shared(server):
        assert len(server.connects) == 1


# ---- reasoning ------------------------------------------------------------
#
# A response's reasoning items are journaled as framework state, and sent
# back as the SDK keeps them. Conformance covers every endpoint that
# returns reasoning (`scenarios/reasoning.rs`).


REASONING = '{"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc-1"}'


def reasoning_items(model: ScriptedModel, call: int) -> list[dict]:
    return [i for i in resumed_input(model, call) if i.get("type") == "reasoning"]


async def test_reasoning_is_sent_back_on_the_next_turn_as_in_the_tool_loop():
    session = Session(
        AGENT,
        [
            [reasoning("They want 42."), tool_call("c1", "lookup_order", '{"order_id": "42"}')],
            [message("It shipped.", "m1")],
            [reasoning("Be polite.", "rs_2", "enc-2"), message("Bye.", "m2")],
        ],
    )
    first = await session.send("Where is 42?")
    state, answer = [f["payload"].get("framework_state") for f in first if f["type"] == "MESSAGE_END"]
    assert (state["adapter"], state["v"], len(state["data"]["items"])) == ("openai_agents", 1, 1)
    # Each response's own reasoning only: none for one without, and a later
    # one's without the earlier ones'.
    assert answer is None
    second = await session.send("Thanks!")
    (later,) = [f["payload"].get("framework_state") for f in second if f["type"] == "MESSAGE_END"]
    assert "enc-2" in json.dumps(later) and "enc-1" not in json.dumps(later)
    assert session.model is not None
    in_loop = reasoning_items(session.model, 1)
    assert [i["encrypted_content"] for i in in_loop] == ["enc-1"]
    assert reasoning_items(session.model, 2) == in_loop
    # Keys in their original order, which the journal sorts (the SDK
    # forwards input items to the provider as they are).
    assert json.dumps(in_loop) != json.dumps(in_loop, sort_keys=True)
    assert json.dumps(reasoning_items(session.model, 2)) == json.dumps(in_loop)
    # in front of the call it came with
    kinds = [i.get("type") for i in resumed_input(session.model, 2)]
    assert kinds.index("reasoning") + 1 == kinds.index("function_call")


async def test_reasoning_with_no_text_still_gets_its_message():
    session = Session(AGENT, [[reasoning("")], [message("Bye.", "m2")]])
    first = await session.send("Hi")
    assert types(first) == ["MESSAGE_START", "MESSAGE_END", "NO_MORE_ACTIONS"]
    await session.send("Thanks!")
    assert session.model is not None
    assert [i["encrypted_content"] for i in reasoning_items(session.model, 1)] == ["enc-1"]


async def test_an_approval_resume_sends_the_reasoning_back():
    ran: list[str] = []
    agent = Agent(name="support", instructions="help", tools=[counted_refund(ran)])
    session = Session(
        agent, [[reasoning("Refund it."), tool_call("c1", "refund", '{"order_id": "42"}')], [message("Done.")]]
    )
    await session.send("Refund 42.")
    await session.decide(True)
    assert ran == ["42"]
    assert session.model is not None
    assert [i["encrypted_content"] for i in reasoning_items(session.model, 1)] == ["enc-1"]


def test_transcript_items_read_its_framework_state():
    state = {"adapter": "openai_agents", "v": 1, "data": {"items": [REASONING]}}
    transcript = [
        *user_message_transcript("hi"),
        envelope(1, "MESSAGE_START", {"role": "assistant", "message_id": "m"}),
        envelope(2, "MESSAGE_DELTA", {"message_id": "m", "delta": {"type": "text", "text": "ok"}}),
        envelope(3, "MESSAGE_END", {"message_id": "m", "framework_state": state}),
    ]
    ctx = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": "hi"})
    items = _transcript_items(ctx)
    assert [i.get("type", "user") for i in items] == ["user", "reasoning", "message"]
    assert json.dumps(items[1]) == json.dumps(json.loads(REASONING))


@pytest.mark.parametrize(
    "state",
    [
        {"adapter": "langgraph", "v": 1, "data": {"items": [REASONING]}},
        {"adapter": "openai_agents", "v": 2, "data": {"items": [REASONING]}},
        {"adapter": "openai_agents", "v": 1, "data": {"items": [json.loads(REASONING)]}},
        {"adapter": "openai_agents", "v": 1, "data": {"items": ["rs_1"]}},
        {"adapter": "openai_agents", "v": 1, "data": {"items": ['["rs_1"]']}},
        {"adapter": "openai_agents", "v": 1, "data": {"items": ['{"type": "message", "content": "x"}']}},
        {"adapter": "openai_agents", "v": 1, "data": {"items": [REASONING], "extra": 1}},
        {"adapter": "openai_agents", "v": 1, "items": [REASONING]},
    ],
    ids=[
        "foreign",
        "newer_version",
        "not_a_string",
        "not_json",
        "not_an_object",
        "not_reasoning",
        "unknown_field",
        "not_under_data",
    ],
)
def test_framework_state_it_cant_read_drops_the_reasoning(state: dict, caplog: pytest.LogCaptureFixture):
    transcript = [
        *user_message_transcript("hi"),
        envelope(1, "MESSAGE_START", {"role": "assistant", "message_id": "m"}),
        envelope(2, "MESSAGE_DELTA", {"message_id": "m", "delta": {"type": "text", "text": "ok"}}),
        envelope(3, "MESSAGE_END", {"message_id": "m", "framework_state": state}),
    ]
    ctx = SessionContext(transcript=transcript, reason={"kind": "user_message", "input": "hi"})
    with caplog.at_level(logging.WARNING):
        assert [i.get("type", "user") for i in _transcript_items(ctx)] == ["user", "message"]
    # Its own state it can't read warns; another adapter's is ignored quietly.
    assert ("openai_agents framework state" in caplog.text) == (state["adapter"] == "openai_agents")
