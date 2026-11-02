"""pydantic-ai adapter: streaming mapping, native deferred-tool pause and
history-only resume, sequential approvals (`_SectrGate`), policy gating,
system-prompt restoration, output-type handling and the transcript
projection. The basic turn cases, and the plain policy pause, are covered by the
conformance harness instead (`crates/sectr-conformance`), against a real model
client over HTTP.

Everything is driven through `sectr.drive` with a journal simulator: each
turn appends INVOCATION_STARTED, runs the handler against the transcript so
far, and journals the frames back (a faithful round trip through
`SessionContext.messages`).

The scripted model is STRICT: it raises on any conversation holding a tool
call that no tool return answers, the way real providers 400. Per-tool
execution counters prove nothing runs twice.
"""

from __future__ import annotations

import json
import logging
from collections import deque
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from typing import Any, Union

import pytest
from pydantic import BaseModel
from pydantic_ai import Agent, DeferredToolRequests, DeferredToolResults, RunContext
from pydantic_ai.agent import WrapperAgent
from pydantic_ai.capabilities import AbstractCapability, HandleDeferredToolCalls
from pydantic_ai.exceptions import ApprovalRequired, CallDeferred
from pydantic_ai.messages import (
    ModelMessage,
    ModelRequest,
    ModelResponse,
    RetryPromptPart,
    SystemPromptPart,
    TextPart,
    ThinkingPart,
    ToolCallPart,
    ToolReturn,
    ToolReturnPart,
    UserPromptPart,
)
from pydantic_ai.models.function import AgentInfo, DeltaThinkingPart, DeltaToolCall, FunctionModel

from sectr.adapters.pydantic_ai import REJECTION_MESSAGE, _history, _run_output_type, pydantic_ai
from sectr.context import SessionContext
from sectr.drive import drive
from sectr.events import Envelope

from subagent_contract import assert_fails_loudly, assert_opaque, run_turn

# ---- strict scripted model -----------------------------------------------

OUTPUT_TOOL = "<output tool>"  # resolved to the run's output tool name


@dataclass
class Reply:
    text: tuple[str, ...] | str = ()
    reasoning: str = ""
    signature: str | None = None  # the reasoning's
    calls: tuple[tuple[str, str, Any], ...] = ()  # (call id, tool name, args dict | raw str)


def assert_answered(messages: list[ModelMessage]) -> None:
    """What real providers enforce: every tool call is answered by the next
    request, and every tool return answers a call."""
    pending: list[str] = []
    for m in messages:
        if isinstance(m, ModelRequest):
            returned = [
                p.tool_call_id for p in m.parts if isinstance(p, (ToolReturnPart, RetryPromptPart))
                if p.tool_call_id
            ]
            for rid in returned:
                if rid not in pending:
                    raise ValueError(f"strict model: tool return {rid!r} answers no call")
                pending.remove(rid)
            if pending:
                raise ValueError(f"strict model: tool calls {pending} unanswered")
        else:
            if pending:
                raise ValueError(f"strict model: tool calls {pending} unanswered")
            pending = [p.tool_call_id for p in m.parts if isinstance(p, ToolCallPart)]
    if pending:
        raise ValueError(f"strict model: tool calls {pending} unanswered at end of conversation")


class Script:
    """A strict FunctionModel driven by a queue of `Reply`s."""

    def __init__(self, *replies: Reply) -> None:
        self.replies = deque(replies)
        self.seen: list[list[ModelMessage]] = []
        self.infos: list[AgentInfo] = []
        self.model = FunctionModel(function=self._call, stream_function=self._stream)

    def _reply(self, messages: list[ModelMessage], info: AgentInfo) -> Reply:
        assert_answered(messages)
        self.seen.append(list(messages))
        self.infos.append(info)
        if not self.replies:
            raise AssertionError("scripted model called more times than scripted")
        return self.replies.popleft()

    def _call(self, messages: list[ModelMessage], info: AgentInfo) -> ModelResponse:
        """A non-streamed request (e.g. a delegate agent's `run()`)."""
        reply = self._reply(messages, info)
        text = reply.text if isinstance(reply.text, str) else "".join(reply.text)
        parts: list[Any] = [TextPart(content=text)] if text else []
        for call_id, name, args in reply.calls:
            parts.append(ToolCallPart(tool_name=name, args=args, tool_call_id=call_id))
        return ModelResponse(parts=parts)

    async def _stream(self, messages: list[ModelMessage], info: AgentInfo) -> AsyncIterator[Any]:
        reply = self._reply(messages, info)
        if reply.reasoning:
            yield {0: DeltaThinkingPart(content=reply.reasoning, signature=reply.signature)}
        for chunk in (reply.text,) if isinstance(reply.text, str) else reply.text:
            if chunk:
                yield chunk
        for index, (call_id, name, args) in enumerate(reply.calls):
            if name == OUTPUT_TOOL:
                name = info.output_tools[0].name
            raw = args if isinstance(args, str) else json.dumps(args)
            yield {10 + index: DeltaToolCall(name=name, json_args=raw, tool_call_id=call_id)}

    @property
    def last(self) -> list[ModelMessage]:
        return self.seen[-1]


# ---- tools with execution counters -----------------------------------------


@dataclass
class Counters:
    runs: dict[str, int] = field(default_factory=dict)
    seen_deps: list[Any] = field(default_factory=list)

    def hit(self, name: str) -> None:
        self.runs[name] = self.runs.get(name, 0) + 1


def make_agent(script: Script, counters: Counters, **kwargs: Any) -> Agent[Any, Any]:
    def lookup_order(order_id: str) -> dict[str, str]:
        """Look up an order."""
        counters.hit("lookup_order")
        return {"order_id": order_id, "status": "shipped"}

    def refund(order_id: str) -> str:
        """Refund an order."""
        counters.hit("refund")
        return f"Refunded {order_id}"

    from pydantic_ai import Tool

    return Agent(
        script.model,
        tools=[lookup_order, Tool(refund, requires_approval=True)],
        **kwargs,
    )


# ---- journal simulator ------------------------------------------------------


def journaled(payload: dict[str, Any]) -> dict[str, Any]:
    """`payload` as the journal gives it back: by value, an object's keys
    sorted (serde_json, Postgres jsonb)."""
    return json.loads(json.dumps(payload, sort_keys=True))


class Session:
    def __init__(self, agent: Agent[Any, Any], **adapter_kwargs: Any) -> None:
        self.handler = pydantic_ai(agent, **adapter_kwargs)
        self.transcript: list[Envelope] = []

    def _append(self, type: str, payload: dict[str, Any]) -> None:  # noqa: A002
        self.transcript.append(
            Envelope(
                id=f"id-{len(self.transcript)}",
                session_id="s",
                deployment_id="d",
                seq=len(self.transcript),
                type=type,
                payload=journaled(payload),
                created_at="2026-01-01T00:00:00Z",
            )
        )

    async def _run(self, reason: dict[str, Any]) -> list[dict[str, Any]]:
        self._append("INVOCATION_STARTED", {"reason": reason})
        ctx = SessionContext(transcript=list(self.transcript), reason=reason)
        frames = [f async for f in drive(self.handler, ctx)]
        for frame in frames:
            self._append(frame["type"], frame["payload"])
        return frames

    async def send(self, text: str) -> list[dict[str, Any]]:
        return await self._run({"kind": "user_message", "input": text})

    async def decide(self, approved: bool) -> list[dict[str, Any]]:
        return await self._run({"kind": "approval_decision", "decision": {"approved": approved}})

    def journal_types(self, type: str) -> list[dict[str, Any]]:  # noqa: A002
        return [e.payload for e in self.transcript if e.type == type]


def types(frames: list[dict[str, Any]]) -> list[str]:
    return [f["type"] for f in frames]


def shape(frames: list[dict[str, Any]]) -> list[tuple[Any, ...]]:
    """Frames with message ids dropped and consecutive same-kind deltas merged."""
    out: list[tuple[Any, ...]] = []
    for f in frames:
        t, p = f["type"], f["payload"]
        if t == "MESSAGE_START":
            out.append((t, p["role"]))
        elif t == "MESSAGE_DELTA":
            entry = (t, p["delta"]["type"], p["delta"]["text"])
            if out and out[-1][:2] == entry[:2]:
                out[-1] = (t, entry[1], out[-1][2] + entry[2])
            else:
                out.append(entry)
        elif t in ("MESSAGE_END", "NO_MORE_ACTIONS"):
            out.append((t,))
        else:
            out.append((t, p))
    return out


def call(call_id: str, name: str, **arguments: Any) -> dict[str, Any]:
    return {"tool_call_id": call_id, "name": name, "arguments": arguments}


def result(call_id: str, value: Any) -> dict[str, Any]:
    return {"tool_call_id": call_id, "result": value}


def user_prompts(messages: list[ModelMessage]) -> list[str]:
    return [
        str(p.content)
        for m in messages
        if isinstance(m, ModelRequest)
        for p in m.parts
        if isinstance(p, UserPromptPart)
    ]


def system_prompts(messages: list[ModelMessage]) -> list[str]:
    return [
        p.content
        for m in messages
        if isinstance(m, ModelRequest)
        for p in m.parts
        if isinstance(p, SystemPromptPart)
    ]


# ---- 7. conditional approval -------------------------------------------------------


async def test_conditional_approval_two_calls_sequential() -> None:
    counters = Counters()
    script = Script(
        Reply(calls=(("a1", "big_spend", {"amount": 500}), ("a2", "big_spend", {"amount": 900}))),
        Reply(text="ok"),
    )
    agent: Agent[Any, Any] = Agent(script.model)

    @agent.tool
    def big_spend(ctx: RunContext[None], amount: int) -> str:
        """Spend money; large amounts need approval."""
        if amount > 100 and not ctx.tool_call_approved:
            raise ApprovalRequired()
        counters.hit("big_spend")
        return f"spent {amount}"

    session = Session(agent)
    first = await session.send("spend")
    assert shape(first)[-1] == ("TOOL_APPROVAL_REQUIRED", call("a1", "big_spend", amount=500))
    second = await session.decide(True)
    assert shape(second) == [
        ("TOOL_RESULT", result("a1", "spent 500")),
        ("TOOL_APPROVAL_REQUIRED", call("a2", "big_spend", amount=900)),
    ]
    third = await session.decide(False)
    assert shape(third)[0] == ("TOOL_RESULT", result("a2", REJECTION_MESSAGE))
    assert types(third)[-1] == "NO_MORE_ACTIONS"
    assert counters.runs == {"big_spend": 1}


# ---- 8. SECTR_APPROVAL_POLICY --------------------------------------------------------


async def test_policy_holds_under_an_auto_approver(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setenv("SECTR_APPROVAL_POLICY", json.dumps(["lookup_order"]))

    def approve_all(ctx: RunContext[Any], requests: DeferredToolRequests) -> DeferredToolResults:
        return requests.build_results(approve_all=True)

    script = Script(
        Reply(calls=(("c1", "lookup_order", {"order_id": "7"}),)),
        Reply(text="Done."),
    )
    counters = Counters()
    session = Session(
        make_agent(script, counters, capabilities=[HandleDeferredToolCalls(handler=approve_all)])
    )
    paused = await session.send("look it up")
    assert types(paused)[-1] == "TOOL_APPROVAL_REQUIRED"
    assert counters.runs == {}
    resumed = await session.decide(True)
    assert types(resumed)[-1] == "NO_MORE_ACTIONS"
    assert counters.runs == {"lookup_order": 1}


# ---- 9. system prompts -----------------------------------------------------------------


async def test_system_prompt_and_instructions_on_every_model_call() -> None:
    script = Script(
        Reply(text="one"),
        Reply(calls=(("r1", "refund", {"order_id": "1"}),)),
        Reply(text="three"),
    )
    agent = make_agent(script, Counters(), system_prompt="STATIC SYSTEM", instructions="THE INSTRUCTIONS")

    @agent.system_prompt
    def dynamic() -> str:
        return "DYNAMIC SYSTEM"

    session = Session(agent)
    await session.send("hi")  # turn 1
    await session.send("refund")  # turn 2 (pauses)
    await session.decide(True)  # resume
    assert len(script.seen) == 3
    for messages in script.seen:
        assert system_prompts(messages) == ["STATIC SYSTEM", "DYNAMIC SYSTEM"]
        requests = [m for m in messages if isinstance(m, ModelRequest)]
        assert requests[-1].instructions == "THE INSTRUCTIONS"


# ---- 13. structured output (not supported yet) ---------------------------------------------


class Answer(BaseModel):
    city: str


@pytest.mark.parametrize(
    "output_type",
    [Answer, [Answer, DeferredToolRequests], [str, Answer], str | Answer | DeferredToolRequests],
    ids=["model", "with_deferred", "union_with_str", "union_type"],
)
def test_structured_output_is_rejected_at_construction(output_type: Any) -> None:
    agent: Agent[Any, Any] = Agent(Script().model, output_type=output_type)
    with pytest.raises(ValueError, match="structured output is not supported on Sectr yet"):
        pydantic_ai(agent)


def test_text_output_is_accepted() -> None:
    pydantic_ai(Agent(Script().model))
    pydantic_ai(Agent(Script().model, output_type=str))


@pytest.mark.parametrize(
    "output_type",
    [str | DeferredToolRequests, Union[str, DeferredToolRequests], [str | DeferredToolRequests]],
    ids=["union", "typing_union", "union_in_list"],
)
@pytest.mark.parametrize("validator", [False, True], ids=["plain", "with_validator"])
def test_union_with_deferred_requests_is_accepted(validator: bool, output_type: Any) -> None:
    """A union with `DeferredToolRequests` is text output, like the list
    form."""
    agent: Agent[Any, Any] = Agent(Script().model, output_type=output_type)
    if validator:

        @agent.output_validator
        def check(value: Any) -> Any:
            return value

    pydantic_ai(agent)


# ---- 14. output validators --------------------------------------------------------------


def test_output_validator_without_deferred_requests_is_rejected() -> None:
    agent: Agent[Any, Any] = Agent(Script().model)

    @agent.output_validator
    def check(value: str) -> str:
        return value

    with pytest.raises(ValueError, match="DeferredToolRequests"):
        pydantic_ai(agent)


async def test_output_validator_added_after_construction_fails_the_turn() -> None:
    agent: Agent[Any, Any] = Agent(Script(Reply(text="x")).model)
    handler = pydantic_ai(agent)

    @agent.output_validator
    def check(value: str) -> str:
        return value

    ctx = SessionContext(
        transcript=[], reason={"kind": "user_message", "input": "hi"}
    )
    with pytest.raises(ValueError, match="DeferredToolRequests"):
        [event async for event in handler(ctx)]


async def test_output_validator_with_deferred_requests_declared_works() -> None:
    script = Script(Reply(text="fine"))
    agent: Agent[Any, Any] = Agent(script.model, output_type=[str, DeferredToolRequests])

    @agent.output_validator
    def check(value: Any) -> Any:
        return value

    assert _run_output_type(agent) is None  # no run-level override
    frames = await Session(agent).send("hi")
    assert types(frames)[-1] == "NO_MORE_ACTIONS" and "ERROR" not in types(frames)


# ---- 15. external call ----------------------------------------------------------------------


async def test_external_deferred_call_fails_loudly() -> None:
    script = Script(Reply(calls=(("x1", "remote", {}),)))
    agent: Agent[Any, Any] = Agent(script.model)

    @agent.tool_plain
    def remote() -> str:
        """Runs elsewhere."""
        raise CallDeferred()

    frames = await Session(agent).send("go")
    assert types(frames)[-2:] == ["ERROR", "NO_MORE_ACTIONS"]
    assert frames[-2]["payload"]["code"] == "pydantic_ai_external_call"


# ---- 16. malformed tool args ----------------------------------------------------------------


async def test_malformed_args_keep_the_raw_string_and_the_model_retries() -> None:
    script = Script(
        Reply(calls=(("c1", "lookup_order", "{not json"),)),
        Reply(calls=(("c2", "lookup_order", {"order_id": "5"}),)),
        Reply(text="Got it."),
    )
    counters = Counters()
    session = Session(make_agent(script, counters))
    frames = await session.send("lookup")
    calls = [f["payload"] for f in frames if f["type"] == "TOOL_CALL"]
    assert calls[0]["arguments"] == "{not json"
    results = {f["payload"]["tool_call_id"]: f["payload"]["result"] for f in frames if f["type"] == "TOOL_RESULT"}
    assert isinstance(results["c1"], str) and "Fix the errors and try again." in results["c1"]
    assert results["c2"] == '{"order_id":"5","status":"shipped"}'
    assert counters.runs == {"lookup_order": 1}
    # the journaled transcript (raw args + retry text) is still a valid history
    nxt = Script(Reply(text="again"))
    nxt_agent = make_agent(nxt, Counters())
    handler = pydantic_ai(nxt_agent)
    ctx = SessionContext(
        transcript=session.transcript
        + [
            Envelope("x", "s", "d", 99, "INVOCATION_STARTED", {"reason": {"kind": "user_message", "input": "more"}}, "t")
        ],
        reason={"kind": "user_message", "input": "more"},
    )
    out = [f async for f in drive(handler, ctx)]
    assert types(out)[-1] == "NO_MORE_ACTIONS" and "ERROR" not in types(out)


# ---- 17. deps factory -----------------------------------------------------------------------


async def test_deps_factory_called_once_per_invocation_and_reaches_resumed_tool() -> None:
    script = Script(
        Reply(calls=(("r1", "refund", {"order_id": "1"}),)),
        Reply(text="Done."),
    )
    counters = Counters()
    agent: Agent[Any, Any] = Agent(script.model, deps_type=dict)

    @agent.tool(requires_approval=True)
    def refund(ctx: RunContext[dict[str, Any]], order_id: str) -> str:
        """Refund."""
        counters.seen_deps.append(ctx.deps)
        return f"Refunded {order_id}"

    factory_calls: list[SessionContext] = []

    async def deps(ctx: SessionContext) -> dict[str, Any]:
        factory_calls.append(ctx)
        return {"invocation": len(factory_calls)}

    session = Session(agent, deps=deps)
    await session.send("refund 1")
    assert len(factory_calls) == 1
    await session.decide(True)
    assert len(factory_calls) == 2
    assert counters.seen_deps == [{"invocation": 2}]  # the resumed tool got the resume turn's deps


# ---- 19. constructor ---------------------------------------------------------------------------


def test_constructor_rejects_wrappers_and_sets_framework_marker() -> None:
    agent: Agent[Any, Any] = Agent(Script().model)
    with pytest.raises(TypeError, match="pydantic_ai.Agent"):
        pydantic_ai(WrapperAgent(agent))  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        pydantic_ai("not an agent")  # type: ignore[arg-type]
    assert pydantic_ai(agent).sectr_framework == "pydantic-ai"  # type: ignore[attr-defined]


# ---- 20. approval reason -------------------------------------------------------------------------


async def test_approval_required_metadata_reason_is_surfaced() -> None:
    script = Script(Reply(calls=(("w1", "wire", {"amount": 5000}),)), Reply(text="sent"))
    agent: Agent[Any, Any] = Agent(script.model)

    @agent.tool
    def wire(ctx: RunContext[None], amount: int) -> str:
        """Wire money."""
        if not ctx.tool_call_approved:
            raise ApprovalRequired(metadata={"reason": "Large transfer", "other": 1})
        return "wired"

    session = Session(agent)
    frames = await session.send("wire 5000")
    final = frames[-1]
    assert final["type"] == "TOOL_APPROVAL_REQUIRED"
    assert final["payload"]["reason"] == "Large transfer"
    assert "other" not in json.dumps(final["payload"])
    resumed = await session.decide(True)
    assert shape(resumed)[0] == ("TOOL_RESULT", result("w1", "wired"))


# ---- tool results --------------------------------------------------------------------
#
# The adapter stores each result as its `model_response_str()`; the
# conformance `results` and `mcp` cases cover plain values. These cover what
# they can't reach.


async def test_a_tool_return_keeps_its_content_and_metadata() -> None:
    script = Script(Reply(calls=(("c1", "label", {}),)), Reply(text="Done."))
    agent: Agent[Any, Any] = Agent(script.model)

    @agent.tool_plain
    def label() -> ToolReturn:
        """Print a label."""
        return ToolReturn(return_value={"status": "printed", "id": "7"}, content="See the label.", metadata={"m": 1})

    frames = await Session(agent).send("print it")
    journaled = '{"status":"printed","id":"7"}'
    assert result("c1", journaled) in [f["payload"] for f in frames if f["type"] == "TOOL_RESULT"]
    request = script.last[-1]
    assert isinstance(request, ModelRequest)
    tool_return = next(p for p in request.parts if isinstance(p, ToolReturnPart))
    assert (tool_return.content, tool_return.metadata) == (journaled, {"m": 1})
    assert user_prompts([request]) == ["See the label."]


@dataclass
class _Redact(AbstractCapability[Any]):
    """A user's own after-hook, on the agent: records what it receives."""

    seen: list[Any] = field(default_factory=list)

    async def after_tool_execute(
        self, ctx: Any, *, call: Any, tool_def: Any, args: Any, result: Any
    ) -> Any:
        self.seen.append(result)
        return {**result, "status": "redacted"}


async def test_the_users_after_tool_execute_sees_the_raw_value() -> None:
    script = Script(Reply(calls=(("c1", "lookup_order", {"order_id": "5"}),)), Reply(text="Done."))
    redact = _Redact()
    frames = await Session(make_agent(script, Counters(), capabilities=[redact])).send("lookup")
    assert redact.seen == [{"order_id": "5", "status": "shipped"}]
    # and what it returns is what gets stringified and journaled
    results = [f["payload"] for f in frames if f["type"] == "TOOL_RESULT"]
    assert results == [result("c1", '{"order_id":"5","status":"redacted"}')]


# ---- transcript projection ---------------------------------------------------------------------


def test_history_groups_requests_and_drops_reasoning() -> None:
    def env(seq: int, type: str, payload: dict[str, Any]) -> Envelope:  # noqa: A002
        return Envelope(f"id-{seq}", "s", "d", seq, type, payload, "2026-01-01T00:00:00Z")

    reason = {"kind": "user_message", "input": "hi"}
    transcript = [
        env(0, "INVOCATION_STARTED", {"reason": reason}),
        env(1, "MESSAGE_START", {"role": "assistant", "message_id": "m"}),
        env(2, "MESSAGE_DELTA", {"message_id": "m", "delta": {"type": "reasoning", "text": "hmm"}}),
        env(3, "MESSAGE_DELTA", {"message_id": "m", "delta": {"type": "text", "text": "ok"}}),
        env(4, "MESSAGE_END", {"message_id": "m"}),
        env(5, "TOOL_CALL", {"tool_call_id": "c", "name": "lookup_order", "arguments": {"order_id": "1"}}),
        env(6, "TOOL_RESULT", {"tool_call_id": "c", "result": '{"ok":true}'}),
    ]
    history = _history(SessionContext(transcript=transcript, reason=reason))
    assert [type(m).__name__ for m in history] == ["ModelRequest", "ModelResponse", "ModelRequest"]
    response = history[1]
    assert isinstance(response, ModelResponse)
    assert [type(p).__name__ for p in response.parts] == ["TextPart", "ToolCallPart"]
    tool_return = history[2].parts[0]  # type: ignore[union-attr]
    assert isinstance(tool_return, ToolReturnPart)
    # the journaled string replays unchanged
    assert (tool_return.tool_name, tool_return.content) == ("lookup_order", '{"ok":true}')


def thinking(messages: list[ModelMessage]) -> list[ThinkingPart]:
    return [
        p for m in messages if isinstance(m, ModelResponse) for p in m.parts if isinstance(p, ThinkingPart)
    ]


async def test_reasoning_is_sent_back_on_the_next_turn_as_in_the_tool_loop() -> None:
    script = Script(
        Reply(reasoning="They want 42.", signature="sig-1", calls=(("c1", "lookup_order", {"order_id": "42"}),)),
        Reply(text="It shipped."),
        Reply(text="Bye.", reasoning="Be polite.", signature="sig-2"),
    )
    session = Session(make_agent(script, Counters()))
    first = await session.send("Where is 42?")
    state, answer = [f["payload"].get("framework_state") for f in first if f["type"] == "MESSAGE_END"]
    assert (state["adapter"], state["v"], len(state["data"]["thinking"])) == ("pydantic_ai", 1, 1)
    # Each response's own reasoning only: none for one without, and a later
    # one's without the earlier ones'.
    assert answer is None
    second = await session.send("Thanks!")
    (later,) = [f["payload"].get("framework_state") for f in second if f["type"] == "MESSAGE_END"]
    assert "sig-2" in json.dumps(later) and "sig-1" not in json.dumps(later)
    in_loop, next_turn = script.seen[1], script.seen[2]
    assert [p.signature for p in thinking(in_loop)] == ["sig-1"]
    assert thinking(next_turn) == thinking(in_loop)
    # ahead of the call, as in the response
    assert [type(p) for p in next_turn[1].parts] == [type(p) for p in in_loop[1].parts]  # type: ignore[union-attr]


async def test_an_approval_resume_sends_the_reasoning_back() -> None:
    script = Script(
        Reply(reasoning="Refund it.", signature="sig-1", calls=(("c1", "refund", {"order_id": "42"}),)),
        Reply(text="Done."),
    )
    session = Session(make_agent(script, Counters()))
    await session.send("Refund 42.")
    await session.decide(True)
    assert [(p.content, p.signature) for p in thinking(script.last)] == [("Refund it.", "sig-1")]


@pytest.mark.parametrize(
    "state",
    [
        {"adapter": "langgraph", "v": 1, "data": {"thinking": []}},
        {"adapter": "pydantic_ai", "v": 2, "data": {"thinking": []}},
        {"adapter": "pydantic_ai", "v": 1, "data": {"thinking": "not parts"}},
        {"adapter": "pydantic_ai", "v": 1, "data": {"thinking": [{"part_kind": "text", "content": "x"}]}},
        {"adapter": "pydantic_ai", "v": 1, "data": {"thinking": [], "extra": 1}},
    ],
    ids=["foreign", "newer_version", "garbled", "not_thinking", "unknown_field"],
)
def test_framework_state_it_cant_read_drops_the_reasoning(
    state: dict[str, Any], caplog: pytest.LogCaptureFixture
) -> None:
    def env(seq: int, type: str, payload: dict[str, Any]) -> Envelope:  # noqa: A002
        return Envelope(f"id-{seq}", "s", "d", seq, type, payload, "2026-01-01T00:00:00Z")

    reason = {"kind": "user_message", "input": "hi"}
    transcript = [
        env(0, "INVOCATION_STARTED", {"reason": reason}),
        env(1, "MESSAGE_START", {"role": "assistant", "message_id": "m"}),
        env(2, "MESSAGE_DELTA", {"message_id": "m", "delta": {"type": "text", "text": "ok"}}),
        env(3, "MESSAGE_END", {"message_id": "m", "framework_state": state}),
    ]
    with caplog.at_level(logging.WARNING):
        history = _history(SessionContext(transcript=transcript, reason=reason))
    assert [type(p) for p in history[1].parts] == [TextPart]  # type: ignore[union-attr]
    # Its own state it can't read warns; another adapter's is ignored quietly.
    assert ("pydantic_ai framework state" in caplog.text) == (state["adapter"] == "pydantic_ai")


def test_history_appends_a_missing_current_input() -> None:
    reason = {"kind": "user_message", "input": "only in reason"}
    history = _history(SessionContext(transcript=[], reason=reason))
    assert len(history) == 1 and isinstance(history[0], ModelRequest)
    assert user_prompts(history) == ["only in reason"]


# ---- subagents -----------------------------------------------------
#
# Each shape users write subagents in (agent delegation: a tool awaiting
# another agent's run), against the beta contract
# (`tests/subagent_contract.py`): opaque when its tools are ungated, and a
# gated tool inside it (in code, or named by the policy) fails loudly.

INNER_TEXT = "Let me check with the carrier."


def _specialist(tool: Any, **kwargs: Any) -> Agent[Any, Any]:
    """A delegate whose model says `INNER_TEXT`, calls its tool on order 42,
    then answers. `tool` is registered as a tool, unless a toolset is given."""
    name = getattr(tool, "name", None) or getattr(tool, "__name__", "cancel_order")
    script = Script(
        Reply(text=INNER_TEXT, calls=(("i1", name, {"order_id": "42"}),)),
        Reply(text="Done."),
    )
    tools = [] if "toolsets" in kwargs else [tool]
    return Agent(script.model, name="specialist", tools=tools, **kwargs)


def _delegating(delegate: Agent[Any, Any], name: str = "ask_specialist") -> Any:
    async def ask(ctx: RunContext[Any], input: str) -> str:  # noqa: A002
        """Ask the specialist."""
        result = await delegate.run(input, usage=ctx.usage)
        return str(result.output)

    ask.__name__ = name
    return ask


def _outer(tool: Any, first: str = "ask_specialist") -> Agent[Any, Any]:
    script = Script(
        Reply(calls=(("c1", first, {"input": "Cancel order 42."}),)),
        Reply(text="All done."),
    )
    return Agent(script.model, name="support", tools=[tool])


def _nested(delegate: Agent[Any, Any]) -> Agent[Any, Any]:
    """`ask_middle`'s agent: a delegate that delegates to `delegate`."""
    script = Script(
        Reply(calls=(("mc1", "ask_specialist", {"input": "Cancel order 42."}),)),
        Reply(text="Done."),
    )
    return Agent(script.model, name="middle", tools=[_delegating(delegate)])


def track_parcel(order_id: str) -> str:
    """Track an order's parcel."""
    return "In transit"


GATED = [
    "code",
    pytest.param("policy", marks=pytest.mark.xfail(strict=True, reason="the policy doesn't reach a delegate")),
]


@pytest.fixture(params=GATED)
def gated(request: pytest.FixtureRequest, monkeypatch: pytest.MonkeyPatch) -> tuple[Any, list[str]]:
    """`cancel_order`, gated in code or named by `SECTR_APPROVAL_POLICY`, and
    the order ids it ran for."""
    from pydantic_ai import Tool

    ran: list[str] = []
    if request.param == "policy":
        monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["cancel_order"]')

    def cancel_order(order_id: str) -> str:
        """Cancel an order."""
        ran.append(order_id)
        return "Cancelled"

    return Tool(cancel_order, requires_approval=request.param == "code"), ran


async def test_a_delegate_is_opaque() -> None:
    frames = await run_turn(pydantic_ai(_outer(_delegating(_specialist(track_parcel)))), "Where is 42?")
    assert_opaque(frames, "ask_specialist", INNER_TEXT)


async def test_a_nested_delegate_is_opaque() -> None:
    middle = _nested(_specialist(track_parcel))
    agent = _outer(_delegating(middle, "ask_middle"), first="ask_middle")
    frames = await run_turn(pydantic_ai(agent), "Where is 42?")
    assert_opaque(frames, "ask_middle", INNER_TEXT)


async def test_a_gated_tool_in_a_delegate_fails_loudly(gated: tuple[Any, list[str]]) -> None:
    tool, ran = gated
    await assert_fails_loudly(lambda: pydantic_ai(_outer(_delegating(_specialist(tool)))), "Cancel 42.", ran)


async def test_a_gated_tool_in_a_delegate_allowing_deferred_requests_fails_loudly(
    gated: tuple[Any, list[str]], request: pytest.FixtureRequest
) -> None:
    """The delegate returns its `DeferredToolRequests` to the delegating tool
    instead of raising. A documented gap: the tool doesn't run, but the turn
    completes, and the adapter can't see the delegate's run without
    patching Pydantic AI."""
    if request.node.callspec.params["gated"] == "code":
        request.applymarker(pytest.mark.xfail(strict=True, reason="documented gap: the delegate hands its requests back"))
    tool, ran = gated
    delegate = _specialist(tool, output_type=[str, DeferredToolRequests])
    await assert_fails_loudly(lambda: pydantic_ai(_outer(_delegating(delegate))), "Cancel 42.", ran)


async def test_a_gated_tool_in_a_nested_delegate_fails_loudly(gated: tuple[Any, list[str]]) -> None:
    tool, ran = gated
    agent = _outer(_delegating(_nested(_specialist(tool)), "ask_middle"), first="ask_middle")
    await assert_fails_loudly(lambda: pydantic_ai(agent), "Cancel 42.", ran)


async def test_a_gated_toolset_on_a_delegate_fails_loudly(gated: tuple[Any, list[str]]) -> None:
    """The delegate's tool comes from a toolset, gated with
    `.approval_required()` (or named by the policy)."""
    from pydantic_ai.toolsets import FunctionToolset

    tool, ran = gated
    toolset: Any = FunctionToolset([tool.function])
    if tool.requires_approval:
        toolset = toolset.approval_required()
    delegate = _specialist(tool, toolsets=[toolset])
    await assert_fails_loudly(lambda: pydantic_ai(_outer(_delegating(delegate))), "Cancel 42.", ran)


async def test_a_delegates_gated_tool_error_names_the_delegating_tool() -> None:
    """The `ERROR` points at the tool to gate instead; the turn's own call
    to it is journaled before it."""
    from pydantic_ai import Tool

    def cancel_order(order_id: str) -> str:
        """Cancel an order."""
        return "Cancelled"

    agent = _outer(_delegating(_specialist(Tool(cancel_order, requires_approval=True))))
    frames = await run_turn(pydantic_ai(agent), "Cancel 42.")
    types = [f["type"] for f in frames]
    assert types[-3:] == ["TOOL_CALL", "ERROR", "NO_MORE_ACTIONS"]
    error = frames[-2]["payload"]
    assert error["code"] == "subagent_gated_tool"
    assert "'ask_specialist'" in error["message"] and "not supported on Sectr yet" in error["message"]


async def test_another_user_error_in_a_tool_isnt_a_subagent_error() -> None:
    from pydantic_ai.exceptions import UserError

    def broken(order_id: str) -> str:
        """Break."""
        raise UserError("misconfigured")

    script = Script(Reply(calls=(("c1", "broken", {"order_id": "42"}),)))
    frames = await run_turn(pydantic_ai(Agent(script.model, tools=[broken])), "go")
    errors = [f["payload"] for f in frames if f["type"] == "ERROR"]
    assert errors == [{"message": "misconfigured"}]
