"""langgraph adapter: streaming mapping, `as_langgraph_tool` goldens, pause,
adapter-executed resume (approved + denied), the conformance matrix
(W*/F*/S*), and the transcript projection. Behavior the
conformance harness covers on `create_agent` (`crates/sectr-conformance`: text
and tool turns, pause, approve, deny,
policy, reasoning, raising tools, two gated calls in one response or in a
row, a gated call in a later turn, journal prefixes, results rebuilt in a
later turn) isn't
repeated here; the tests that run every graph shape (`build_graph`) stay.

Everything is driven through `sectr.drive` and asserts exact frames. Multi-
step flows run through `Session`, which journals each turn's frames back
into envelopes (a faithful round trip through `SessionContext.messages`).

The scripted chat model is STRICT: it raises on any conversation holding an
unanswered tool call, the way real providers 400. A lenient classifier fake
re-emits the call on a dangling conversation and hides resume bugs — see
`test_strict_model_rejects_rerunning_from_entry_on_a_dangling_call`.
"""

from __future__ import annotations

import asyncio
import importlib
import json
import logging
import sys
import textwrap
from collections import deque
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Annotated, Any, cast

import pytest
from langchain.agents import create_agent
from langchain.agents.middleware import AgentMiddleware, HumanInTheLoopMiddleware
from langchain_core.callbacks import BaseCallbackHandler
from langchain_core.language_models.chat_models import BaseChatModel
from langchain_core.messages import (
    AIMessage,
    AIMessageChunk,
    BaseMessage,
    HumanMessage,
    ToolMessage,
)
from langchain_core.outputs import ChatGeneration, ChatGenerationChunk, ChatResult
from langchain_core.runnables import RunnableConfig
from langchain_core.tools import BaseTool, InjectedToolCallId, StructuredTool
from langchain_core.tools import tool as langchain_tool
from langgraph.checkpoint.memory import InMemorySaver
from langgraph.graph import END, MessagesState, StateGraph
from langgraph.prebuilt import InjectedState, InjectedStore, ToolNode, ToolRuntime, create_react_agent
from langgraph.types import Command, interrupt
from pydantic import BaseModel

from sectr.adapters.langgraph import (
    REJECTION_MESSAGE,
    _transcript_messages,
    as_langgraph_tool,
    langgraph,
)
from sectr.context import SessionContext
from sectr.drive import drive
from sectr.events import Envelope
from sectr.inspect import build_manifest, scan_interrupt_imports
from sectr.tools import ApprovalRequired, Tool

from subagent_contract import SUBAGENT_GATED_TOOL, assert_fails_loudly, assert_opaque, run_turn

pytestmark = pytest.mark.filterwarnings("ignore::DeprecationWarning")


# ---- strict scripted chat model ------------------------------------------


@dataclass
class Reply:
    text: str = ""
    calls: tuple[tuple[str, str, dict[str, Any]], ...] = ()
    reasoning: str = ""
    signature: str = ""  # the reasoning's
    # the reasoning as a content block (as ChatAnthropic streams it), not in
    # `additional_kwargs` (as ChatOpenRouter does)
    blocks: bool = False


def assert_conversation_answered(messages: list[BaseMessage]) -> None:
    """What real providers enforce: every tool call is answered by tool
    messages immediately following it, and every tool message answers a call."""
    pending: list[str] = []
    for m in messages:
        if isinstance(m, ToolMessage):
            if m.tool_call_id not in pending:
                raise ValueError(f"strict model: tool message {m.tool_call_id!r} answers no call")
            pending.remove(m.tool_call_id)
            continue
        if pending:
            raise ValueError(
                f"strict model: tool calls {pending} unanswered before {type(m).__name__}"
            )
        if isinstance(m, AIMessage) and m.tool_calls:
            pending = [c["id"] for c in m.tool_calls]
    if pending:
        raise ValueError(f"strict model: tool calls {pending} unanswered at end of conversation")


class _ScriptedBase(BaseChatModel):
    replies: Any  # deque[Reply]
    seen: list[list[BaseMessage]] = field(default_factory=list)

    @property
    def _llm_type(self) -> str:
        return "scripted"

    def bind_tools(self, tools: Any, **kwargs: Any) -> Any:
        return self

    def _next(self, messages: list[BaseMessage]) -> Reply:
        assert_conversation_answered(messages)
        self.seen.append(list(messages))
        if not self.replies:
            raise AssertionError("scripted model called more times than scripted")
        return self.replies.popleft()

    def _generate(self, messages: Any, stop: Any = None, run_manager: Any = None, **kw: Any) -> ChatResult:
        reply = self._next(messages)
        message = AIMessage(
            content=reply.text,
            tool_calls=[{"id": i, "name": n, "args": a, "type": "tool_call"} for i, n, a in reply.calls],
        )
        return ChatResult(generations=[ChatGeneration(message=message)])


class ScriptedChatModel(_ScriptedBase):
    """Streams word-sized text chunks and fragmented tool-call arguments."""

    async def _astream(self, messages: Any, stop: Any = None, run_manager: Any = None, **kw: Any):  # type: ignore[override]
        reply = self._next(messages)
        if reply.reasoning and reply.blocks:
            block = {"type": "thinking", "thinking": reply.reasoning, "signature": reply.signature, "index": 0}
            yield ChatGenerationChunk(message=AIMessageChunk(content=[block]))
        elif reply.reasoning:
            kwargs: dict[str, Any] = {"reasoning_content": reply.reasoning}
            if reply.signature:
                kwargs["reasoning_details"] = [
                    {"type": "reasoning.text", "text": reply.reasoning, "signature": reply.signature, "index": 0}
                ]
            yield ChatGenerationChunk(message=AIMessageChunk(content="", additional_kwargs=kwargs))
        words = reply.text.split(" ") if reply.text else []
        for i, word in enumerate(words):
            yield ChatGenerationChunk(
                message=AIMessageChunk(content=word + (" " if i < len(words) - 1 else ""))
            )
        for index, (call_id, name, args) in enumerate(reply.calls):
            raw = json.dumps(args)
            half = len(raw) // 2
            yield ChatGenerationChunk(
                message=AIMessageChunk(
                    content="",
                    tool_call_chunks=[{"name": name, "args": raw[:half], "id": call_id, "index": index}],
                )
            )
            yield ChatGenerationChunk(
                message=AIMessageChunk(
                    content="",
                    tool_call_chunks=[{"name": None, "args": raw[half:], "id": None, "index": index}],
                )
            )
        yield ChatGenerationChunk(message=AIMessageChunk(content="", chunk_position="last"))


class WholeCallChunksModel(_ScriptedBase):
    """Streams text, then tool calls WHOLE (`AIMessageChunk(tool_calls=...)`),
    the way providers that don't stream argument deltas surface in LangChain.
    `AIMessageChunk.init_tool_calls` turns those into `tool_call_chunks` with
    `index=None`. `calls_per_chunk=0` puts every call in one chunk."""

    calls_per_chunk: int = 1

    async def _astream(self, messages: Any, stop: Any = None, run_manager: Any = None, **kw: Any):  # type: ignore[override]
        reply = self._next(messages)
        if reply.text:
            yield ChatGenerationChunk(message=AIMessageChunk(content=reply.text))
        calls = [{"id": i, "name": n, "args": a, "type": "tool_call"} for i, n, a in reply.calls]
        step = self.calls_per_chunk or max(len(calls), 1)
        for start in range(0, len(calls), step):
            yield ChatGenerationChunk(
                message=AIMessageChunk(content="", tool_calls=cast(Any, calls[start : start + step]))
            )
        yield ChatGenerationChunk(message=AIMessageChunk(content="", chunk_position="last"))


def model(*replies: Reply) -> ScriptedChatModel:
    return ScriptedChatModel(replies=deque(replies), seen=[])


# ---- sectr tools (framework-free) + execution log --------------------------

EXECUTED: list[tuple[str, dict[str, Any]]] = []


async def _lookup_order(order_id: str) -> dict[str, str]:
    EXECUTED.append(("lookup_order", {"order_id": order_id}))
    return {"order_id": order_id, "status": "shipped"}


async def _refund(order_id: str) -> str:
    EXECUTED.append(("refund", {"order_id": order_id}))
    return f"Refunded {order_id}"


async def _todo_write(items: list[str]) -> str:
    EXECUTED.append(("todo_write", {"items": items}))
    return "ok"


LOOKUP = Tool("lookup_order", _lookup_order, "Look up an order.")
REFUND = Tool("refund", _refund, "Refund an order.", needs_approval=True)
TODO = Tool("todo_write", _todo_write, "Replace the todo list.")


@pytest.fixture(autouse=True)
def clean(monkeypatch: pytest.MonkeyPatch):
    monkeypatch.delenv("SECTR_APPROVAL_POLICY", raising=False)
    EXECUTED.clear()
    yield
    EXECUTED.clear()


def executed(name: str) -> int:
    return sum(1 for n, _ in EXECUTED if n == name)


# ---- graph builders ------------------------------------------------------------


def react_graph(chat: BaseChatModel, tools: list[Tool], **tool_node: Any):
    """W1: canonical ReAct StateGraph(MessagesState) + agent node + ToolNode."""
    return tools_graph(chat, [as_langgraph_tool(t) for t in tools], **tool_node)


def tools_graph(chat: BaseChatModel, tools: list[BaseTool], **tool_node: Any):
    """`react_graph` over LangChain tools as given."""

    async def agent(state: MessagesState) -> dict[str, list]:
        return {"messages": [await chat.ainvoke(state["messages"])]}

    builder = StateGraph(MessagesState)
    builder.add_node("agent", agent)
    builder.add_node("tools", ToolNode(tools, **tool_node))
    builder.add_conditional_edges(
        "agent",
        lambda s: "tools" if getattr(s["messages"][-1], "tool_calls", None) else END,
        ["tools", END],
    )
    builder.add_edge("tools", "agent")
    builder.set_entry_point("agent")
    return builder.compile()


def prebuilt_graph(chat: BaseChatModel, tools: list[Tool]):
    """W2: create_react_agent (deprecated in langgraph 1.x) with sectr-converted tools."""
    return create_react_agent(chat, [as_langgraph_tool(t) for t in tools])


def agent_graph(chat: BaseChatModel, tools: list[Tool]):
    """W2b: langchain.agents.create_agent, the current prebuilt agent, with
    sectr-converted tools."""
    return create_agent(chat, [as_langgraph_tool(t) for t in tools])


BUILDERS: dict[str, Callable[..., Any]] = {
    "state_graph": react_graph,
    "prebuilt": prebuilt_graph,
    "create_agent": agent_graph,
}


@pytest.fixture(params=list(BUILDERS))
def build_graph(request: pytest.FixtureRequest) -> Callable[..., Any]:
    return BUILDERS[request.param]


# ---- journal harness -----------------------------------------------------------


def journaled(payload: dict[str, Any]) -> dict[str, Any]:
    """`payload` as the journal gives it back: by value, an object's keys
    sorted (serde_json, Postgres jsonb)."""
    return json.loads(json.dumps(payload, sort_keys=True))


class Session:
    """Runs turns through `drive` and journals the frames back as envelopes."""

    def __init__(self, graph: Any) -> None:
        self.handler = langgraph(graph)
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


def types(frames: list[dict[str, Any]]) -> list[str]:
    return [f["type"] for f in frames]


def shape(frames: list[dict[str, Any]]) -> list[tuple[Any, ...]]:
    """Frames with message ids dropped and consecutive text deltas merged."""
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
        elif t == "MESSAGE_END":
            out.append((t,))
        elif t == "NO_MORE_ACTIONS":
            out.append((t,))
        else:
            out.append((t, p))
    return out


def last_messages(chat: ScriptedChatModel) -> list[BaseMessage]:
    return chat.seen[-1]


def message_ids_consistent(frames: list[dict[str, Any]]) -> bool:
    open_id: str | None = None
    for f in frames:
        if f["type"] == "MESSAGE_START":
            open_id = f["payload"]["message_id"]
        elif f["type"] in ("MESSAGE_DELTA", "MESSAGE_END"):
            if f["payload"]["message_id"] != open_id:
                return False
            if f["type"] == "MESSAGE_END":
                open_id = None
    return open_id is None


LOOKUP_CALL = ("c1", "lookup_order", {"order_id": "123"})
REFUND_CALL = ("c2", "refund", {"order_id": "123"})


# ======================================================================
# W9 — as_langgraph_tool schema goldens
# ======================================================================


def schema(tool: Tool) -> dict[str, Any]:
    return as_langgraph_tool(tool).tool_call_schema.model_json_schema()  # type: ignore[union-attr]


def test_schema_required_and_typed_params():
    assert schema(LOOKUP) == {
        "description": "Look up an order.",
        "properties": {"order_id": {"title": "Order Id", "type": "string"}},
        "required": ["order_id"],
        "title": "lookup_order",
        "type": "object",
    }


def test_schema_defaulted_params_are_optional():
    async def f(order_id: str, note: str = "n", count: int = 2) -> str:
        return ""

    out = schema(Tool("f", f, "d"))
    assert out["required"] == ["order_id"]
    assert out["properties"]["note"] == {"default": "n", "title": "Note", "type": "string"}
    assert out["properties"]["count"] == {"default": 2, "title": "Count", "type": "integer"}


def test_schema_untyped_param_is_a_string():
    def f(thing):  # type: ignore[no-untyped-def]
        return thing

    out = schema(Tool("f", f, "d"))
    assert out["properties"]["thing"]["type"] == "string"
    assert out["required"] == ["thing"]


def test_schema_container_types_and_hidden_call_id():
    out = schema(TODO)
    assert out["properties"] == {
        "items": {"items": {"type": "string"}, "title": "Items", "type": "array"}
    }
    # the injected call id never reaches the model
    assert "tool_call_id" not in out["properties"]
    assert "tool_call_id" in as_langgraph_tool(TODO).args_schema.model_json_schema()["properties"]  # type: ignore[union-attr]


def test_missing_docstring_is_a_value_error():
    with pytest.raises(ValueError, match="docstring"):
        as_langgraph_tool(Tool("f", lambda: None, None))


def test_graph_injected_params_are_rejected():
    async def f(order_id: str, state: Annotated[dict, InjectedState]) -> str:
        return ""

    with pytest.raises(ValueError, match="injected by the graph"):
        as_langgraph_tool(Tool("f", f, "d"))


def test_reserved_param_name_is_rejected():
    async def f(tool_call_id: str) -> str:
        return ""

    with pytest.raises(ValueError, match="reserved"):
        as_langgraph_tool(Tool("f", f, "d"))


# ======================================================================
# gating behavior of the converted tool
# ======================================================================


def tool_call(call_id: str, name: str, args: dict[str, Any]) -> dict[str, Any]:
    return {"name": name, "args": args, "id": call_id, "type": "tool_call"}


async def test_gated_tool_pause_closes_its_callback_run():
    """BaseTool.arun only reports `Exception`s to callbacks; the BaseException
    pause must still close the tool run (as an error) for tracers."""

    class Recorder(BaseCallbackHandler):
        def __init__(self) -> None:
            self.started: list[Any] = []
            self.errored: list[tuple[Any, BaseException]] = []

        def on_tool_start(self, serialized: Any, input_str: str, *, run_id: Any, **kw: Any) -> None:
            self.started.append(run_id)

        def on_tool_error(self, error: BaseException, *, run_id: Any, **kw: Any) -> None:
            self.errored.append((run_id, error))

    recorder = Recorder()
    with pytest.raises(ApprovalRequired):
        await as_langgraph_tool(REFUND).ainvoke(
            tool_call("c2", "refund", {"order_id": "123"}), config={"callbacks": [recorder]}
        )
    assert len(recorder.started) == 1
    assert [run_id for run_id, _ in recorder.errored] == recorder.started
    assert isinstance(recorder.errored[0][1], ApprovalRequired)


async def test_approval_required_is_control_flow_not_an_error():
    """Rebased on BaseException so frameworks' `except Exception` can't
    swallow the pause; ToolNode(handle_tool_errors=True) is where an
    Exception-based pause was actually lost."""
    assert not issubclass(ApprovalRequired, Exception)

    async def boom(x: str) -> str:
        raise RuntimeError("boom")

    chat = model(
        Reply(calls=(("c1", "boom", {"x": "1"}),)),
        Reply("recovered"),
    )
    boom_tool = Tool("boom", boom, "d")
    ordinary = react_graph(chat, [boom_tool], handle_tool_errors=True)
    out = [f async for f in drive(langgraph(ordinary), SessionContext([], {"kind": "user_message", "input": "x"}))]
    # an ordinary tool error IS swallowed into a model-visible ToolMessage ...
    assert [m for m in last_messages(chat) if isinstance(m, ToolMessage)][0].content.startswith("Error")  # type: ignore[union-attr]
    assert types(out)[-1] == "NO_MORE_ACTIONS"

    # ... the approval pause is not
    chat2 = model(Reply(calls=(REFUND_CALL,)))
    out = [
        f
        async for f in drive(
            langgraph(react_graph(chat2, [REFUND], handle_tool_errors=True)),
            SessionContext([], {"kind": "user_message", "input": "x"}),
        )
    ]
    assert types(out)[-1] == "TOOL_APPROVAL_REQUIRED"


# ======================================================================
# W3 / W1 / W2 / W2b — turns
# ======================================================================


async def test_w1_w2_lookup_turn(build_graph):
    chat = model(Reply("Let me check.", (LOOKUP_CALL,)), Reply("It shipped!"))
    out = await Session(build_graph(chat, [LOOKUP, REFUND])).send("where is 123?")
    assert shape(out) == [
        ("MESSAGE_START", "assistant"),
        ("MESSAGE_DELTA", "text", "Let me check."),
        ("MESSAGE_END",),
        ("TOOL_CALL", {"tool_call_id": "c1", "name": "lookup_order", "arguments": {"order_id": "123"}}),
        ("TOOL_RESULT", {"tool_call_id": "c1", "result": '{"order_id": "123", "status": "shipped"}'}),
        ("MESSAGE_START", "assistant"),
        ("MESSAGE_DELTA", "text", "It shipped!"),
        ("MESSAGE_END",),
        ("NO_MORE_ACTIONS",),
    ]
    assert message_ids_consistent(out)
    assert executed("lookup_order") == 1


async def test_non_streaming_model_whole_messages_map_too():
    class Whole(_ScriptedBase):
        pass

    chat = Whole(replies=deque([Reply("Checking.", (LOOKUP_CALL,)), Reply("Done.")]), seen=[])
    out = await Session(react_graph(chat, [LOOKUP])).send("x")
    assert types(out) == [
        "MESSAGE_START", "MESSAGE_DELTA", "MESSAGE_END", "TOOL_CALL", "TOOL_RESULT",
        "MESSAGE_START", "MESSAGE_DELTA", "MESSAGE_END", "NO_MORE_ACTIONS",
    ]  # fmt: skip
    assert out[3]["payload"]["arguments"] == {"order_id": "123"}


@pytest.mark.parametrize("calls_per_chunk", [1, 0], ids=["chunk_per_call", "one_chunk"])
async def test_parallel_whole_tool_calls_without_index_stay_separate(calls_per_chunk: int):
    """Tool-call chunks with `index=None` are separate calls, not fragments
    of one call: grouping them under one slot concatenates names and
    arguments and journals a single bogus call."""
    first, second = ("c1", "lookup_order", {"order_id": "1"}), ("c2", "lookup_order", {"order_id": "2"})
    chat = WholeCallChunksModel(
        replies=deque([Reply("Checking both.", (first, second)), Reply("Both shipped."), Reply("You're welcome.")]),
        seen=[],
        calls_per_chunk=calls_per_chunk,
    )
    session = Session(react_graph(chat, [LOOKUP]))
    out = await session.send("where are 1 and 2?")
    assert shape(out) == [
        ("MESSAGE_START", "assistant"),
        ("MESSAGE_DELTA", "text", "Checking both."),
        ("MESSAGE_END",),
        ("TOOL_CALL", {"tool_call_id": "c1", "name": "lookup_order", "arguments": {"order_id": "1"}}),
        ("TOOL_CALL", {"tool_call_id": "c2", "name": "lookup_order", "arguments": {"order_id": "2"}}),
        ("TOOL_RESULT", {"tool_call_id": "c1", "result": '{"order_id": "1", "status": "shipped"}'}),
        ("TOOL_RESULT", {"tool_call_id": "c2", "result": '{"order_id": "2", "status": "shipped"}'}),
        ("MESSAGE_START", "assistant"),
        ("MESSAGE_DELTA", "text", "Both shipped."),
        ("MESSAGE_END",),
        ("NO_MORE_ACTIONS",),
    ]
    # the journal round-trips into a conversation a strict provider accepts
    assert types(await session.send("thanks"))[-1] == "NO_MORE_ACTIONS"


# ======================================================================
# pause + resume (W1/W2/W2b)
# ======================================================================


async def test_approval_pause_is_the_final_frame_without_sentinel(build_graph):
    chat = model(Reply("Refunding now.", (REFUND_CALL,)))
    out = await Session(build_graph(chat, [LOOKUP, REFUND])).send("refund it")
    assert shape(out) == [
        ("MESSAGE_START", "assistant"),
        ("MESSAGE_DELTA", "text", "Refunding now."),
        ("MESSAGE_END",),
        ("TOOL_CALL", {"tool_call_id": "c2", "name": "refund", "arguments": {"order_id": "123"}}),
        ("TOOL_APPROVAL_REQUIRED", {"tool_call_id": "c2", "name": "refund", "arguments": {"order_id": "123"}}),
    ]
    assert EXECUTED == []  # never executed before the decision


async def test_resume_approved_executes_exactly_once_then_continues(build_graph):
    chat = model(Reply("Refunding now.", (REFUND_CALL,)), Reply("Refund processed!"))
    session = Session(build_graph(chat, [LOOKUP, REFUND]))
    await session.send("refund it")
    out = await session.decide(True)

    assert shape(out) == [
        ("TOOL_RESULT", {"tool_call_id": "c2", "result": "Refunded 123"}),
        ("MESSAGE_START", "assistant"),
        ("MESSAGE_DELTA", "text", "Refund processed!"),
        ("MESSAGE_END",),
        ("NO_MORE_ACTIONS",),
    ]
    # the adapter runs the tool once; the graph does not run it again
    assert executed("refund") == 1
    # the model generated only the continuation, on an ANSWERED conversation
    msgs = last_messages(chat)
    assert isinstance(msgs[-1], ToolMessage) and msgs[-1].content == "Refunded 123"
    assert msgs[-1].tool_call_id == "c2"
    assert [type(m).__name__ for m in msgs] == ["HumanMessage", "AIMessage", "ToolMessage"]
    assert len(chat.seen) == 2  # one call per generation, none on a dangling call


async def test_resume_denied_returns_the_rejection_message(build_graph):
    chat = model(Reply("Refunding now.", (REFUND_CALL,)), Reply("Understood, no refund."))
    session = Session(build_graph(chat, [LOOKUP, REFUND]))
    await session.send("refund it")
    out = await session.decide(False)

    assert out[0] == {
        "type": "TOOL_RESULT",
        "payload": {"tool_call_id": "c2", "result": REJECTION_MESSAGE},
    }
    assert types(out)[-1] == "NO_MORE_ACTIONS"
    assert EXECUTED == []
    assert last_messages(chat)[-1].content == REJECTION_MESSAGE


async def test_strict_model_rejects_rerunning_from_entry_on_a_dangling_call():
    """Regression for the withdrawn resume design (re-invoke the graph from
    its entry node on the rebuilt, unanswered state): the entry node calls
    the model on a dangling tool call, which the strict model — like a real
    provider — rejects. A lenient fake would have re-emitted the call and
    passed."""
    chat = model(Reply("Refunding.", (REFUND_CALL,)), Reply("never reached"))
    session = Session(react_graph(chat, [REFUND]))
    await session.send("refund it")
    ctx = SessionContext(
        transcript=list(session.transcript),
        reason={"kind": "approval_decision", "decision": {"approved": True}},
    )
    dangling = _transcript_messages(ctx)
    assert isinstance(dangling[-1], AIMessage) and dangling[-1].tool_calls
    with pytest.raises(ValueError, match="unanswered"):
        async for _ in react_graph(chat, [REFUND]).astream({"messages": dangling}):
            pass


# ======================================================================
# W8 — multi-turn memory
# ======================================================================


async def test_w8_turn_two_sees_turn_one_tool_results(build_graph):
    chat = model(
        Reply("Checking.", (LOOKUP_CALL,)),
        Reply("Order 123 shipped."),
        Reply("Noted.", (("c3", "todo_write", {"items": ["follow up on 123"]}),)),
        Reply("Added to the list."),
    )
    session = Session(build_graph(chat, [LOOKUP, TODO]))
    await session.send("where is 123?")
    out = await session.send("add a todo for it")

    assert types(out)[-1] == "NO_MORE_ACTIONS"
    first_turn_result = json.dumps({"order_id": "123", "status": "shipped"})
    seen = [(type(m).__name__, m.content) for m in chat.seen[2]]
    assert ("ToolMessage", first_turn_result) in seen
    assert ("HumanMessage", "where is 123?") in seen
    assert seen[-1] == ("HumanMessage", "add a todo for it")
    assert executed("todo_write") == 1


# ======================================================================
# W7 — several calls in one AIMessage
# ======================================================================


async def test_w7_a_sibling_of_a_gated_call_runs_once_in_total():
    """The instant sibling finishes on the pause turn, before the gated
    call's pause aborts the ToolNode. Its result is journaled before the
    pause, so it doesn't run again on resume."""
    chat = model(
        Reply("On it.", (LOOKUP_CALL, REFUND_CALL)),
        Reply("All done."),
    )
    session = Session(react_graph(chat, [LOOKUP, REFUND]))
    paused = await session.send("check and refund")
    assert types(paused)[-2:] == ["TOOL_RESULT", "TOOL_APPROVAL_REQUIRED"]
    assert paused[-2]["payload"]["tool_call_id"] == "c1"
    assert paused[-1]["payload"]["tool_call_id"] == "c2"

    out = await session.decide(True)
    assert types(out)[-1] == "NO_MORE_ACTIONS"
    assert executed("refund") == 1
    tool_msgs = [m for m in last_messages(chat) if isinstance(m, ToolMessage)]
    assert {m.tool_call_id for m in tool_msgs} == {"c1", "c2"}
    assert executed("lookup_order") == 1  # once across the pause turn and the resume


# ======================================================================
# S2 — non-message state is lost at the turn boundary (documented)
# ======================================================================


async def test_s2_non_message_state_does_not_survive_the_turn_boundary():
    class State(MessagesState):
        scratch: str

    seen_scratch: list[str | None] = []

    async def agent(state: State) -> dict[str, Any]:
        seen_scratch.append(state.get("scratch"))
        return {"messages": [AIMessage("ok")], "scratch": "written"}

    builder = StateGraph(State)
    builder.add_node("agent", agent)
    builder.set_entry_point("agent")
    builder.add_edge("agent", END)
    session = Session(builder.compile())
    await session.send("one")
    await session.send("two")
    # written within turn 1, gone in turn 2: the rebuild is conversation-only
    assert seen_scratch == [None, None]


# ======================================================================
# F — loud failures
# ======================================================================


def interrupt_graph():
    async def agent(state: MessagesState) -> dict[str, list]:
        if isinstance(state["messages"][-1], ToolMessage):
            return {"messages": [AIMessage("after")]}
        return {"messages": [AIMessage("", tool_calls=[{"id": "c1", "name": "ask", "args": {"q": "ok?"}}])]}

    async def ask(q: str) -> str:
        interrupt({"question": q})
        return "unreachable"

    from langchain_core.tools import StructuredTool

    builder = StateGraph(MessagesState)
    builder.add_node("agent", agent)
    builder.add_node("tools", ToolNode([StructuredTool.from_function(coroutine=ask, name="ask", description="d")]))
    builder.add_conditional_edges(
        "agent", lambda s: "tools" if getattr(s["messages"][-1], "tool_calls", None) else END, ["tools", END]
    )
    builder.add_edge("tools", "agent")
    builder.set_entry_point("agent")
    return builder.compile()


async def test_f1_f3_native_interrupt_fails_loudly_never_as_complete():
    out = await Session(interrupt_graph()).send("go")
    assert types(out)[-2:] == ["ERROR", "NO_MORE_ACTIONS"]
    error = out[-2]["payload"]
    assert error["code"] == "langgraph_interrupt"
    assert "as_langgraph_tool" in error["message"] and "Checkpointer-backed resume" in error["message"]
    assert "TOOL_APPROVAL_REQUIRED" not in types(out)


async def test_f2_command_resume_without_checkpointer_is_translated():
    inner = StateGraph(MessagesState)
    inner.add_node("a", lambda s: {})
    inner.set_entry_point("a")
    inner_graph = inner.compile()

    async def agent(state: MessagesState) -> dict[str, list]:
        await inner_graph.ainvoke(Command(resume=1))
        return {"messages": []}

    builder = StateGraph(MessagesState)
    builder.add_node("agent", agent)
    builder.set_entry_point("agent")
    builder.add_edge("agent", END)
    out = await Session(builder.compile()).send("go")
    assert types(out)[-2:] == ["ERROR", "NO_MORE_ACTIONS"]
    assert out[-2]["payload"]["code"] == "langgraph_resume"
    assert "without a checkpointer" in out[-2]["payload"]["message"]


async def test_other_runtime_errors_propagate_as_errors():
    async def agent(state: MessagesState) -> dict[str, list]:
        raise RuntimeError("model exploded")

    builder = StateGraph(MessagesState)
    builder.add_node("agent", agent)
    builder.set_entry_point("agent")
    builder.add_edge("agent", END)
    out = await Session(builder.compile()).send("go")
    assert [f["type"] for f in out] == ["ERROR", "NO_MORE_ACTIONS"]
    assert out[0]["payload"]["message"] == "model exploded"


async def test_f4_approval_from_a_bare_node_is_a_loud_error():
    async def agent(state: MessagesState) -> dict[str, list]:
        raise ApprovalRequired("deploy", {"env": "prod"})

    builder = StateGraph(MessagesState)
    builder.add_node("agent", agent)
    builder.set_entry_point("agent")
    builder.add_edge("agent", END)
    out = await Session(builder.compile()).send("go")
    assert types(out) == ["ERROR", "NO_MORE_ACTIONS"]
    assert out[0]["payload"]["code"] == "langgraph_unmatched_approval"


async def test_f4_hand_rolled_tool_node_with_a_foreign_call_id_is_loud():
    async def agent(state: MessagesState) -> dict[str, list]:
        return {"messages": [AIMessage("", tool_calls=[{"id": "c1", "name": "x", "args": {}}])]}

    async def tools(state: MessagesState) -> dict[str, list]:
        raise ApprovalRequired("x", {}, call_id="not-the-models-id")

    builder = StateGraph(MessagesState)
    builder.add_node("agent", agent)
    builder.add_node("tools", tools)
    builder.set_entry_point("agent")
    builder.add_edge("agent", "tools")
    out = await Session(builder.compile()).send("go")
    assert types(out)[-2:] == ["ERROR", "NO_MORE_ACTIONS"]
    assert out[-2]["payload"]["code"] == "langgraph_unmatched_approval"


async def test_f4_hand_rolled_tool_node_fails_before_anyone_is_asked_to_approve():
    """A hand-rolled node passing the model's call through gives the gated
    tool the real call id, so the pause itself would succeed — but resume
    only runs the ToolNode's tools. It must fail at the pause, not after
    the human approves."""
    refund = as_langgraph_tool(REFUND)
    chat = model(Reply(calls=(REFUND_CALL,)))

    async def agent(state: MessagesState) -> dict[str, list]:
        return {"messages": [await chat.ainvoke(state["messages"])]}

    async def tools(state: MessagesState) -> dict[str, list]:
        call = state["messages"][-1].tool_calls[0]
        return {"messages": [await refund.ainvoke(call)]}

    builder = StateGraph(MessagesState)
    builder.add_node("agent", agent)
    builder.add_node("tools", tools)
    builder.set_entry_point("agent")
    builder.add_edge("agent", "tools")
    builder.add_edge("tools", END)
    out = await Session(builder.compile()).send("refund it")
    assert types(out)[-2:] == ["ERROR", "NO_MORE_ACTIONS"]
    assert "TOOL_APPROVAL_REQUIRED" not in types(out)
    assert out[-2]["payload"]["code"] == "langgraph_unmatched_approval"
    assert "ToolNode" in out[-2]["payload"]["message"]
    assert EXECUTED == []


def test_f5_checkpointed_graph_is_rejected_at_construction():
    builder = StateGraph(MessagesState)
    builder.add_node("agent", lambda s: {})
    builder.set_entry_point("agent")
    with pytest.raises(ValueError, match="checkpointer") as excinfo:
        langgraph(builder.compile(checkpointer=InMemorySaver()))
    assert "Checkpointer-backed resume" in str(excinfo.value)


def test_f6_sectr_tools_in_several_tool_nodes_are_rejected():
    builder = StateGraph(MessagesState)
    builder.add_node("agent", lambda s: {})
    builder.add_node("tools_a", ToolNode([as_langgraph_tool(LOOKUP)]))
    builder.add_node("tools_b", ToolNode([as_langgraph_tool(REFUND)]))
    builder.set_entry_point("agent")
    with pytest.raises(ValueError, match="several ToolNodes") as excinfo:
        langgraph(builder.compile())
    assert "Checkpointer-backed resume" in str(excinfo.value)


def test_f6_tool_node_nested_in_a_subgraph_is_rejected():
    inner = react_graph(model(), [REFUND])
    outer = StateGraph(MessagesState)
    outer.add_node("worker", inner)
    outer.set_entry_point("worker")
    with pytest.raises(ValueError, match="nested inside a subgraph") as excinfo:
        langgraph(outer.compile())
    assert "Checkpointer-backed resume" in str(excinfo.value)


def test_graphs_without_sectr_tools_are_accepted_even_when_nested():
    inner = StateGraph(MessagesState)
    inner.add_node("a", lambda s: {})
    inner.set_entry_point("a")
    outer = StateGraph(MessagesState)
    outer.add_node("sub", inner.compile())
    outer.set_entry_point("sub")
    handler = langgraph(outer.compile())
    assert handler.sectr_framework == "langgraph"  # type: ignore[attr-defined]


def test_handler_carries_the_framework_marker():
    assert langgraph(react_graph(model(), [LOOKUP])).sectr_framework == "langgraph"  # type: ignore[attr-defined]


# ======================================================================
# resume robustness
# ======================================================================


async def test_resume_with_a_journal_that_lacks_the_attached_call():
    """Only the approval event survived (no TOOL_CALL): the pending call is
    anchored from its journaled name + arguments, like openai-agents."""
    chat = model(Reply("Refunded."))
    session = Session(react_graph(chat, [REFUND]))
    session._append("INVOCATION_STARTED", {"reason": {"kind": "user_message", "input": "refund 123"}})
    session._append(
        "TOOL_APPROVAL_REQUIRED",
        {"tool_call_id": "c9", "name": "refund", "arguments": {"order_id": "123"}},
    )
    out = await session.decide(True)
    assert out[0]["payload"] == {"tool_call_id": "c9", "result": "Refunded 123"}
    msgs = last_messages(chat)
    assert [type(m).__name__ for m in msgs] == ["HumanMessage", "AIMessage", "ToolMessage"]
    assert msgs[1].tool_calls[0]["id"] == "c9"  # type: ignore[union-attr]


async def test_approved_tool_missing_from_the_graph_is_a_clear_error():
    chat = model(Reply("x", (REFUND_CALL,)))
    session = Session(react_graph(chat, [REFUND]))
    await session.send("refund")
    # redeployed graph no longer has the tool
    session.handler = langgraph(react_graph(model(), [LOOKUP]))
    out = await session.decide(True)
    assert types(out) == ["ERROR", "NO_MORE_ACTIONS"]
    assert "not in the graph's ToolNode" in out[0]["payload"]["message"]


@pytest.mark.parametrize("how", ["per_node", "graph_default"])
async def test_an_error_handler_on_the_tools_node_does_not_swallow_the_pause(how: str):
    """Checked: LangGraph does route the pause to a node error
    handler (it runs), but the adapter has already recorded the pause, so
    the turn still suspends, nothing runs early, the handler's output is
    dropped, and the resume works."""
    handled: list[bool] = []

    async def handler(state: MessagesState) -> dict[str, list]:
        handled.append(True)
        return {"messages": [AIMessage("Something went wrong.")]}

    chat = model(Reply("Refunding.", (REFUND_CALL,)), Reply("Done."))

    async def agent(state: MessagesState) -> dict[str, list]:
        return {"messages": [await chat.ainvoke(state["messages"])]}

    builder = StateGraph(MessagesState)
    if how == "graph_default":
        builder.set_node_defaults(error_handler=handler)
    builder.add_node("agent", agent)
    tools = ToolNode([as_langgraph_tool(REFUND)])
    if how == "per_node":
        builder.add_node("tools", tools, error_handler=handler)
    else:
        builder.add_node("tools", tools)
    builder.add_conditional_edges(
        "agent",
        lambda s: "tools" if getattr(s["messages"][-1], "tool_calls", None) else END,
        ["tools", END],
    )
    builder.add_edge("tools", "agent")
    builder.set_entry_point("agent")
    session = Session(builder.compile())

    paused = await session.send("refund it")
    assert handled  # the pause reached the handler...
    assert types(paused)[-1] == "TOOL_APPROVAL_REQUIRED"  # ...and the turn still suspended
    assert executed("refund") == 0
    assert len(chat.seen) == 1  # the graph didn't go on after the handler

    out = await session.decide(True)
    assert types(out) == ["TOOL_RESULT", "MESSAGE_START", "MESSAGE_DELTA", "MESSAGE_END", "NO_MORE_ACTIONS"]
    assert executed("refund") == 1


async def test_platform_policy_gates_a_plain_langchain_tool(monkeypatch: pytest.MonkeyPatch):
    """A tool that isn't a Sectr tool (a plain LangChain tool, or an MCP
    tool from langchain-mcp-adapters) named in the policy pauses too, and
    approving runs it once."""
    monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["lookup_order"]')
    ran: list[str] = []

    @langchain_tool
    def lookup_order(order_id: str) -> str:
        """Look up an order."""
        ran.append(order_id)
        return "shipped"

    chat = model(Reply(calls=(("c1", "lookup_order", {"order_id": "42"}),)), Reply("It shipped."))

    async def agent(state: MessagesState) -> dict[str, list]:
        return {"messages": [await chat.ainvoke(state["messages"])]}

    builder = StateGraph(MessagesState)
    builder.add_node("agent", agent)
    builder.add_node("tools", ToolNode([lookup_order]))
    builder.add_conditional_edges(
        "agent",
        lambda s: "tools" if getattr(s["messages"][-1], "tool_calls", None) else END,
        ["tools", END],
    )
    builder.add_edge("tools", "agent")
    builder.set_entry_point("agent")
    session = Session(builder.compile())
    out = await session.send("where is 42?")
    assert types(out)[-1] == "TOOL_APPROVAL_REQUIRED"
    assert ran == []
    out = await session.decide(True)
    assert types(out)[0] == "TOOL_RESULT" and types(out)[-1] == "NO_MORE_ACTIONS"
    assert ran == ["42"]


async def _refund_fails(order_id: str) -> str:
    raise RuntimeError("payment provider down")


async def test_handle_tool_errors_applies_to_an_approved_tool_on_resume():
    """With `handle_tool_errors=True`, a tool that raises becomes an error
    `ToolMessage` and the turn goes on, on resume as on any turn."""
    failing = Tool("refund", _refund_fails, "Refund an order.", needs_approval=True)
    chat = model(Reply("Refunding.", (REFUND_CALL,)), Reply("The refund failed."))
    session = Session(react_graph(chat, [failing], handle_tool_errors=True))
    await session.send("refund it")
    out = await session.decide(True)
    assert "ERROR" not in types(out)
    assert out[0]["type"] == "TOOL_RESULT"
    assert str(out[0]["payload"]["result"]).startswith("Error")
    assert types(out)[-1] == "NO_MORE_ACTIONS"


async def test_create_agent_tool_middleware_runs_for_an_approved_tool_on_resume():
    wrapped: list[str] = []

    class Record(AgentMiddleware):
        def wrap_tool_call(self, request: Any, handler: Any) -> Any:
            wrapped.append(request.tool_call["name"])
            return handler(request)

        async def awrap_tool_call(self, request: Any, handler: Any) -> Any:
            wrapped.append(request.tool_call["name"])
            return await handler(request)

    chat = model(Reply("Refunding.", (REFUND_CALL,)), Reply("Done."))
    session = Session(create_agent(chat, [as_langgraph_tool(REFUND)], middleware=[Record()]))
    await session.send("refund it")
    wrapped.clear()  # the pause turn wraps the call that pauses; count the resume only
    out = await session.decide(True)
    assert types(out)[-1] == "NO_MORE_ACTIONS"
    assert executed("refund") == 1
    assert wrapped == ["refund"]


def texts(frames: list[dict[str, Any]]) -> list[str]:
    return [f["payload"]["delta"]["text"] for f in frames if f["type"] == "MESSAGE_DELTA"]


def routed_graph(chat: BaseChatModel, tools: list[BaseTool], route: Callable[..., str], **extra: Any):
    """`tools_graph`, but the tools node routes by `route` (to `agent` or to
    one of `extra`, each a node ending the run)."""

    async def agent(state: MessagesState) -> dict[str, list]:
        return {"messages": [await chat.ainvoke(state["messages"])]}

    builder = StateGraph(MessagesState)
    builder.add_node("agent", agent)
    builder.add_node("tools", ToolNode(tools))
    for name, node in extra.items():
        builder.add_node(name, node)
        builder.add_edge(name, END)
    builder.add_conditional_edges(
        "agent",
        lambda s: "tools" if getattr(s["messages"][-1], "tool_calls", None) else END,
        ["tools", END],
    )
    builder.add_conditional_edges("tools", route, ["agent", *extra])
    builder.set_entry_point("agent")
    return builder.compile()


async def test_a_command_from_an_approved_tool_is_applied(monkeypatch: pytest.MonkeyPatch):
    """The approved call runs in the graph's ToolNode, so a `Command` result
    updates the state and its `goto` is followed."""
    monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["escalate"]')

    @langchain_tool
    def escalate(reason: str, tool_call_id: Annotated[str, InjectedToolCallId]) -> Command:
        """Hand the conversation to a human."""
        return Command(
            update={"messages": [ToolMessage("Escalated.", tool_call_id=tool_call_id)]},
            goto="human",
        )

    async def human(state: MessagesState) -> dict[str, list]:
        return {"messages": [AIMessage("A human will contact you.")]}

    chat = model(Reply("Escalating.", (("c1", "escalate", {"reason": "angry"}),)))
    session = Session(routed_graph(chat, [escalate], lambda s: "agent", human=human))
    assert types(await session.send("help"))[-1] == "TOOL_APPROVAL_REQUIRED"
    out = await session.decide(True)
    assert out[0]["payload"] == {"tool_call_id": "c1", "result": "Escalated."}
    assert texts(out) == ["A human will contact you."]


async def test_a_conditional_edge_out_of_the_tools_node_is_followed():
    async def audit(state: MessagesState) -> dict[str, list]:
        return {"messages": [AIMessage("Audited.")]}

    chat = model(Reply("Refunding.", (REFUND_CALL,)))
    graph = routed_graph(chat, [as_langgraph_tool(REFUND)], lambda s: "audit", audit=audit)
    session = Session(graph)
    await session.send("refund it")
    out = await session.decide(True)
    assert types(out)[0] == "TOOL_RESULT"
    assert texts(out) == ["Audited."]
    assert executed("refund") == 1


@pytest.mark.parametrize("injected", ["state", "store", "runtime"])
async def test_an_approved_tool_gets_its_graph_injected_args(
    injected: str, monkeypatch: pytest.MonkeyPatch
):
    """A policy-gated tool taking `InjectedState`, `InjectedStore` or
    `ToolRuntime` runs on approval: the graph injects them as on any turn."""
    from langgraph.store.memory import InMemoryStore

    monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["refund"]')
    chat = model(Reply("Refunding.", (REFUND_CALL,)), Reply("Done."))
    session = Session(create_agent(chat, [_injected_tools()[injected]], store=InMemoryStore()))
    await session.send("refund it")
    out = await session.decide(True)
    assert types(out)[0] == "TOOL_RESULT" and types(out)[-1] == "NO_MORE_ACTIONS"
    assert executed("refund") == 1


async def test_an_injected_state_sibling_of_an_approved_call_gets_the_state():
    seen: list[int] = []

    @langchain_tool
    def count(state: Annotated[dict, InjectedState]) -> str:
        """Count the messages."""
        seen.append(len(state["messages"]))
        return "ok"

    chat = model(Reply("", (("c1", "count", {}), REFUND_CALL)), Reply("Done."))
    session = Session(tools_graph(chat, [count, as_langgraph_tool(REFUND)]))
    await session.send("count and refund")
    out = await session.decide(True)
    assert types(out)[-1] == "NO_MORE_ACTIONS"
    assert seen and all(n == 2 for n in seen)  # the user message and the AIMessage
    assert executed("refund") == 1


async def test_after_model_middleware_does_not_run_again_for_the_paused_response():
    """The resume starts at the tools node: the model's paused response
    isn't post-processed a second time."""
    seen: list[Any] = []

    class AfterModel(AgentMiddleware):
        async def aafter_model(self, state: Any, runtime: Any) -> None:
            seen.append(state["messages"][-1].content)

    chat = model(Reply("Refunding.", (REFUND_CALL,)), Reply("Done."))
    session = Session(create_agent(chat, [as_langgraph_tool(REFUND)], middleware=[AfterModel()]))
    await session.send("refund it")
    await session.decide(True)
    assert seen == ["Refunding.", "Done."]


@pytest.mark.parametrize("builder", list(BUILDERS))
async def test_two_gated_calls_approve_then_deny_each_answered_once(builder: str):
    """The second gated call pauses again after the first is approved; the
    approved result is journaled before that pause, it never runs again, and
    the model gets both results in call order."""
    calls = (("c1", "refund", {"order_id": "1"}), ("c2", "refund", {"order_id": "2"}))
    chat = model(Reply("", calls), Reply("Done."))
    session = Session(BUILDERS[builder](chat, [REFUND]))
    await session.send("refund both")
    first = await session.decide(True)
    assert types(first) == ["TOOL_RESULT", "TOOL_APPROVAL_REQUIRED"]
    second = await session.decide(False)
    assert second[0]["payload"]["result"] == REJECTION_MESSAGE
    assert types(second)[-1] == "NO_MORE_ACTIONS"
    assert executed("refund") == 1
    tool_msgs = [m for m in last_messages(chat) if isinstance(m, ToolMessage)]
    assert [m.tool_call_id for m in tool_msgs] == ["c1", "c2"]


@pytest.mark.parametrize("builder", list(BUILDERS))
async def test_a_sibling_finishes_before_a_gated_sibling_pauses_again(builder: str):
    """Approving one of two gated calls: the ungated sibling runs once in
    total — on the pause turn, journaled before the pause — and the other
    gated call pauses again after the approved one."""
    calls = (LOOKUP_CALL, ("c2", "refund", {"order_id": "1"}), ("c3", "refund", {"order_id": "2"}))
    chat = model(Reply("", calls), Reply("Done."))
    session = Session(BUILDERS[builder](chat, [LOOKUP, REFUND]))
    paused = await session.send("check and refund both")
    first = paused[-1]["payload"]["tool_call_id"]
    on_pause = [f["payload"]["tool_call_id"] for f in paused if f["type"] == "TOOL_RESULT"]
    out = await session.decide(True)
    results = [f["payload"]["tool_call_id"] for f in out if f["type"] == "TOOL_RESULT"]
    assert results[0] == first
    assert sorted(on_pause + results[1:]) == ["c1"]
    assert types(out)[-1] == "TOOL_APPROVAL_REQUIRED"
    await session.decide(True)
    assert executed("lookup_order") == 1
    assert executed("refund") == 2


@pytest.mark.parametrize("builder", list(BUILDERS))
async def test_the_pause_waits_for_a_slow_sibling(builder: str):
    """A sibling still running when the gated call pauses: the pause waits
    for it, journals its result, and the resume doesn't run it again."""
    started: list[str] = []

    async def _slow_lookup(order_id: str) -> str:
        started.append(order_id)
        await asyncio.sleep(0.05)
        return "shipped"

    slow = Tool("lookup_order", _slow_lookup, "Look up an order.")
    chat = model(Reply("", (LOOKUP_CALL, REFUND_CALL)), Reply("Done."))
    session = Session(BUILDERS[builder](chat, [slow, REFUND]))
    paused = await session.send("check and refund")
    assert types(paused)[-2:] == ["TOOL_RESULT", "TOOL_APPROVAL_REQUIRED"]
    assert paused[-2]["payload"] == {"tool_call_id": "c1", "result": "shipped"}
    out = await session.decide(True)
    assert types(out)[-1] == "NO_MORE_ACTIONS"
    assert started == ["123"]
    assert executed("refund") == 1


async def test_a_tools_node_reached_from_several_nodes_fails_at_the_pause():
    """A resume seeds the state as the tools node's single predecessor; with
    two, the pause fails before anyone is asked to approve."""
    chat = model(Reply("Refunding.", (REFUND_CALL,)))

    async def agent(state: MessagesState) -> dict[str, list]:
        return {"messages": [await chat.ainvoke(state["messages"])]}

    async def other(state: MessagesState) -> dict[str, list]:
        return {}

    builder = StateGraph(MessagesState)
    builder.add_node("agent", agent)
    builder.add_node("other", other)
    builder.add_node("tools", ToolNode([as_langgraph_tool(REFUND)]))
    builder.add_conditional_edges("agent", lambda s: "tools", ["tools", "other"])
    builder.add_edge("other", "tools")
    builder.add_edge("tools", END)
    builder.set_entry_point("agent")
    out = await Session(builder.compile()).send("refund it")
    assert types(out) == ["MESSAGE_START", "MESSAGE_DELTA", "MESSAGE_END", "TOOL_CALL", "ERROR", "NO_MORE_ACTIONS"]
    assert out[-2]["payload"]["code"] == "langgraph_unmatched_approval"
    assert executed("refund") == 0


# ======================================================================
# transcript projection (§1.4) through the real LangChain message types
# ======================================================================


def envelope(seq: int, type: str, payload: dict[str, Any]) -> Envelope:  # noqa: A002
    return Envelope(
        id=f"id-{seq}",
        session_id="s",
        deployment_id="d",
        seq=seq,
        type=type,
        payload=payload,
        created_at="2026-01-01T00:00:00Z",
    )


def project(transcript: list[Envelope], reason: dict[str, Any]) -> list[BaseMessage]:
    return _transcript_messages(SessionContext(transcript=transcript, reason=reason))


def test_projection_drops_reasoning():
    transcript = [
        envelope(0, "INVOCATION_STARTED", {"reason": {"kind": "user_message", "input": "hi"}}),
        envelope(1, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        envelope(2, "MESSAGE_DELTA", {"message_id": "m1", "delta": {"type": "reasoning", "text": "hmm"}}),
        envelope(3, "MESSAGE_DELTA", {"message_id": "m1", "delta": {"type": "text", "text": "Hello"}}),
        envelope(4, "MESSAGE_END", {"message_id": "m1"}),
    ]
    msgs = project(transcript, {"kind": "user_message", "input": "hi"})
    assert msgs[0].content == "hi"
    assert msgs[1].content == "Hello" and "reasoning_content" not in msgs[1].additional_kwargs


THINKING = '{"type": "thinking", "thinking": "hmm", "signature": "sig"}'


def first_ai(messages: list[BaseMessage]) -> tuple[Any, dict[str, Any]]:
    """The first AIMessage's content and `additional_kwargs`."""
    m = next(m for m in messages if isinstance(m, AIMessage))
    return m.content, m.additional_kwargs


@pytest.mark.parametrize("blocks", [False, True], ids=["additional_kwargs", "content_blocks"])
async def test_reasoning_is_sent_back_on_the_next_turn_as_in_the_tool_loop(build_graph, blocks: bool):
    chat = model(
        Reply(
            reasoning="They want 42.",
            signature="sig-1",
            blocks=blocks,
            calls=(("c1", "lookup_order", {"order_id": "42"}),),
        ),
        Reply(text="It shipped."),
        Reply(text="Bye.", reasoning="Be polite.", signature="sig-2", blocks=blocks),
    )
    session = Session(build_graph(chat, [LOOKUP]))
    first = await session.send("Where is 42?")
    state, answer = [f["payload"].get("framework_state") for f in first if f["type"] == "MESSAGE_END"]
    assert (state["adapter"], state["v"]) == ("langgraph", 1)
    data = state["data"]
    if blocks:
        assert len(data["content"]) == 1 and "additional_kwargs" not in data
    else:
        assert "content" not in data and "sig-1" in data["additional_kwargs"]
    # Each response's own reasoning only: none for one without, and a later
    # one's without the earlier ones'.
    assert answer is None
    second = await session.send("Thanks!")
    (later,) = [f["payload"].get("framework_state") for f in second if f["type"] == "MESSAGE_END"]
    assert "sig-2" in json.dumps(later) and "sig-1" not in json.dumps(later)
    in_loop, next_turn = first_ai(chat.seen[1]), first_ai(chat.seen[2])
    assert "sig-1" in json.dumps(in_loop)
    assert next_turn == in_loop
    # Keys in their original order, which the journal sorts (LangChain
    # forwards these to the provider as they are).
    assert json.dumps(in_loop) != json.dumps(in_loop, sort_keys=True)
    assert json.dumps(next_turn) == json.dumps(in_loop)


async def test_an_approval_resume_sends_the_reasoning_back(build_graph):
    chat = model(
        Reply(reasoning="Refund it.", signature="sig-1", calls=(("c1", "refund", {"order_id": "42"}),)),
        Reply(text="Done."),
    )
    session = Session(build_graph(chat, [REFUND]))
    await session.send("Refund 42.")
    await session.decide(True)
    _, kwargs = first_ai(last_messages(chat))
    assert [d["signature"] for d in kwargs["reasoning_details"]] == ["sig-1"]


async def test_a_named_agents_messages_keep_their_name_on_the_next_turn():
    chat = model(
        Reply(text="Checking.", calls=(("c1", "lookup_order", {"order_id": "42"}),)),
        Reply(text="It shipped."),
        Reply(text="Bye."),
    )
    session = Session(create_agent(chat, [as_langgraph_tool(LOOKUP)], name="support"))
    await session.send("Where is 42?")
    await session.send("Thanks!")

    def names(messages: list[BaseMessage]) -> list[str | None]:
        return [m.name for m in messages if isinstance(m, AIMessage)]

    assert names(chat.seen[1]) == ["support"]
    assert names(chat.seen[2]) == ["support", "support"]


async def test_a_named_agents_empty_reply_gets_no_message():
    chat = model(Reply(), Reply(text="Hi!"))
    session = Session(create_agent(chat, [as_langgraph_tool(LOOKUP)], name="support"))
    assert types(await session.send("Hello?")) == ["NO_MORE_ACTIONS"]


def test_projection_reads_its_framework_state():
    state = {"adapter": "langgraph", "v": 1, "data": {"content": [THINKING]}}
    transcript = [
        envelope(0, "INVOCATION_STARTED", {"reason": {"kind": "user_message", "input": "hi"}}),
        envelope(1, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        envelope(2, "MESSAGE_DELTA", {"message_id": "m1", "delta": {"type": "text", "text": "Hello"}}),
        envelope(3, "MESSAGE_END", {"message_id": "m1", "framework_state": state}),
    ]
    msgs = project(transcript, {"kind": "user_message", "input": "hi"})
    assert first_ai(msgs) == ([json.loads(THINKING), {"type": "text", "text": "Hello"}], {})


@pytest.mark.parametrize(
    "state",
    [
        {"adapter": "pydantic_ai", "v": 1, "data": {"content": [THINKING]}},
        {"adapter": "langgraph", "v": 2, "data": {"content": [THINKING]}},
        {"adapter": "langgraph", "v": 1, "data": {"content": [{"type": "thinking", "thinking": "hmm"}]}},
        {"adapter": "langgraph", "v": 1, "data": {"content": ["{not json"]}},
        {"adapter": "langgraph", "v": 1, "data": {"content": ['["hmm"]']}},
        {"adapter": "langgraph", "v": 1, "data": {"content": ['{"type": "text", "text": "hmm"}']}},
        {"adapter": "langgraph", "v": 1, "data": {"additional_kwargs": '{"refusal": "no"}'}},
        {"adapter": "langgraph", "v": 1, "data": {"content": [THINKING], "extra": 1}},
        {"adapter": "langgraph", "v": 1, "data": {"name": 1}},
    ],
    ids=[
        "foreign",
        "newer_version",
        "not_a_string",
        "not_json",
        "not_an_object",
        "not_reasoning",
        "other_kwargs",
        "unknown_field",
        "name_not_a_string",
    ],
)
def test_projection_drops_reasoning_from_framework_state_it_cant_read(
    state: dict[str, Any], caplog: pytest.LogCaptureFixture
):
    transcript = [
        envelope(0, "INVOCATION_STARTED", {"reason": {"kind": "user_message", "input": "hi"}}),
        envelope(1, "MESSAGE_START", {"role": "assistant", "message_id": "m1"}),
        envelope(2, "MESSAGE_DELTA", {"message_id": "m1", "delta": {"type": "text", "text": "Hello"}}),
        envelope(3, "MESSAGE_END", {"message_id": "m1", "framework_state": state}),
    ]
    with caplog.at_level(logging.WARNING):
        msgs = project(transcript, {"kind": "user_message", "input": "hi"})
    assert first_ai(msgs) == ("Hello", {})
    assert msgs[1].name is None
    # Its own state it can't read warns; another adapter's is ignored quietly.
    assert ("langgraph framework state" in caplog.text) == (state["adapter"] == "langgraph")


def test_projection_appends_the_input_when_absent_from_the_transcript():
    msgs = project([], {"kind": "user_message", "input": "hello"})
    assert [(type(m), m.content) for m in msgs] == [(HumanMessage, "hello")]


# ======================================================================
# static wrong-import detection (`sectr inspect`)
# ======================================================================


@pytest.mark.parametrize(
    "source",
    [
        "from langgraph.types import interrupt\n",
        "from langgraph.types import Command, interrupt as ask\n",
        "from langchain.agents import interrupt\n",
        "import langgraph.types\n\nlanggraph.types.interrupt({})\n",
        "import langgraph.types as t\n\nt.interrupt({})\n",
        "from langgraph import types\n\ntypes.interrupt({})\n",
    ],
)
def test_static_scan_flags_native_interrupt_imports(source: str):
    warnings = scan_interrupt_imports(source, "main.py")
    assert len(warnings) == 1
    assert "main.py:" in warnings[0] and "as_langgraph_tool" in warnings[0]


@pytest.mark.parametrize(
    "source",
    [
        "from langgraph.types import Command\n",
        "import langgraph.types\n",
        "from langgraph.graph import StateGraph\n",
        "def interrupt(): ...\ninterrupt()\n",
    ],
)
def test_static_scan_ignores_unrelated_code(source: str):
    assert scan_interrupt_imports(source, "main.py") == []


def test_inspect_manifest_reports_the_wrong_import(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    (tmp_path / "wrongimp.py").write_text(
        textwrap.dedent(
            """
            from langgraph.types import interrupt
            from sectr import AgentApp

            app = AgentApp(name="a")

            @app.agent_route("/chat")
            async def chat(ctx):
                yield []
            """
        )
    )
    monkeypatch.syspath_prepend(str(tmp_path))
    importlib.invalidate_caches()
    try:
        manifest = build_manifest("wrongimp:app")
    finally:
        sys.modules.pop("wrongimp", None)
    assert len(manifest["warnings"]) == 1
    assert "wrongimp.py:2" in manifest["warnings"][0]


def test_inspect_manifest_has_no_warnings_key_when_clean(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    (tmp_path / "cleanimp.py").write_text(
        "from sectr import AgentApp\napp = AgentApp(name='a')\n"
    )
    monkeypatch.syspath_prepend(str(tmp_path))
    importlib.invalidate_caches()
    try:
        manifest = build_manifest("cleanimp:app")
    finally:
        sys.modules.pop("cleanimp", None)
    assert "warnings" not in manifest


# ---- runtime context ---------------------------------------------------------


@dataclass
class Ctx:
    user: str


async def test_a_context_factory_reaches_the_tools():
    seen: list[object] = []

    @langchain_tool
    def whoami(runtime: ToolRuntime[Ctx]) -> str:
        """Who the user is."""
        seen.append(runtime.context)
        return "ok"

    async def context(session: SessionContext) -> Ctx:
        return Ctx(user="u1")

    chat = model(Reply("", (("c1", "whoami", {}),)), Reply("done"))
    graph = create_agent(chat, [whoami], context_schema=Ctx)
    handler = langgraph(graph, context=context)
    ctx = SessionContext(transcript=[], reason={"kind": "user_message", "input": "who am I?"})
    [frame async for frame in drive(handler, ctx)]
    assert seen == [Ctx(user="u1")]


async def test_the_context_reaches_an_approved_tool_on_resume(monkeypatch: pytest.MonkeyPatch):
    """The resume runs the graph too, with the context, so the approved
    tool reads it as on any turn. The factory is called once per
    invocation, with that invocation's `SessionContext`, and may be sync."""
    monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["refund"]')
    seen: list[object] = []
    reasons: list[str] = []

    @langchain_tool
    def refund(order_id: str, runtime: ToolRuntime[Ctx]) -> str:
        """Refund an order."""
        seen.append(runtime.context)
        return f"Refunded {order_id}"

    def context(session: SessionContext) -> Ctx:
        reasons.append(session.reason["kind"])
        return Ctx(user="u1")

    chat = model(Reply("Refunding.", (REFUND_CALL,)), Reply("Done."))
    session = Session(create_agent(chat, [refund], context_schema=Ctx))
    session.handler = langgraph(create_agent(chat, [refund], context_schema=Ctx), context=context)
    assert types(await session.send("refund it"))[-1] == "TOOL_APPROVAL_REQUIRED"
    out = await session.decide(True)
    assert types(out)[0] == "TOOL_RESULT" and types(out)[-1] == "NO_MORE_ACTIONS"
    assert seen == [Ctx(user="u1")]
    assert reasons == ["user_message", "approval_decision"]


async def test_without_a_context_factory_the_runtime_context_is_none():
    seen: list[object] = []

    @langchain_tool
    def whoami(runtime: ToolRuntime) -> str:
        """Who the user is."""
        seen.append(runtime.context)
        return "ok"

    chat = model(Reply("", (("c1", "whoami", {}),)), Reply("done"))
    await Session(create_agent(chat, [whoami])).send("who am I?")
    assert seen == [None]


# ---- structured output (not supported yet) ----------------------------------------------


class _Out(BaseModel):
    answer: str


@pytest.mark.parametrize(
    "build",
    [
        lambda chat: create_agent(chat, [], response_format=_Out),
        lambda chat: create_react_agent(chat, [], response_format=_Out),
    ],
    ids=["create_agent", "create_react_agent"],
)
def test_structured_output_is_rejected_at_construction(build: Callable[[Any], Any]) -> None:
    with pytest.raises(ValueError, match="structured output is not supported on Sectr yet"):
        langgraph(build(model()))


def test_agents_without_response_format_are_accepted() -> None:
    langgraph(create_agent(model(), []))
    langgraph(create_react_agent(model(), []))


# ---- subagents -----------------------------------------------------
#
# Each shape users write subagents in, against the beta contract
# (`tests/subagent_contract.py`): opaque when its tools are ungated, and a
# gated Sectr tool inside it (in code, or named by the policy) fails loudly.
# (A sync tool running the subagent with `invoke()` can't work: Sectr tools
# are async-only `StructuredTool`s.)

INNER_TEXT = "Let me check with the carrier."
ASK = {"input": "Cancel order 42."}


def _specialist(tool: Tool, call_id: str = "i1") -> Any:
    """A `create_agent` subagent whose model says `INNER_TEXT`, calls `tool`
    on order 42 (as `call_id`), then answers."""
    chat = model(
        Reply(text=INNER_TEXT, calls=((call_id, tool.name, {"order_id": "42"}),)),
        Reply(text="Done."),
    )
    return create_agent(chat, [as_langgraph_tool(tool)], name="specialist")


def _outer_model() -> ScriptedChatModel:
    return model(Reply(calls=(("c1", "ask_specialist", ASK),)), Reply(text="All done."))


def _messages(input: str) -> dict[str, Any]:  # noqa: A002
    return {"messages": [{"role": "user", "content": input}]}


def _sectr_agent_tool(specialist: Any) -> Any:
    """A Sectr `@tool` awaiting the subagent: the config reaches it
    implicitly (langchain's context propagation)."""

    async def ask_specialist(input: str) -> str:  # noqa: A002
        result = await specialist.ainvoke(_messages(input))
        return result["messages"][-1].text

    return create_agent(_outer_model(), [as_langgraph_tool(Tool("ask_specialist", ask_specialist, "Ask."))])


def _langchain_agent_tool(specialist: Any) -> Any:
    """A plain LangChain `@tool` passing its config on to the subagent."""

    @langchain_tool
    async def ask_specialist(input: str, config: RunnableConfig) -> str:  # noqa: A002
        """Ask the specialist."""
        result = await specialist.ainvoke(_messages(input), config)
        return result["messages"][-1].text

    return create_agent(_outer_model(), [ask_specialist])


def _subgraph(specialist: Any) -> Any:
    """The subagent graph as a node of the outer graph."""
    builder = StateGraph(MessagesState)
    builder.add_node("specialist", specialist)
    builder.set_entry_point("specialist")
    builder.add_edge("specialist", END)
    return builder.compile()


SHAPES: dict[str, Callable[[Any], Any]] = {
    "sectr_agent_tool": _sectr_agent_tool,
    "langchain_agent_tool": _langchain_agent_tool,
}


async def _track(order_id: str) -> str:
    EXECUTED.append(("track_parcel", {"order_id": order_id}))
    return "In transit"


TRACK = Tool("track_parcel", _track, "Track an order's parcel.")


@pytest.fixture(params=["code", "policy"])
def gated(request: pytest.FixtureRequest, monkeypatch: pytest.MonkeyPatch) -> tuple[Tool, list[Any]]:
    """`cancel_order`, gated in code or named by `SECTR_APPROVAL_POLICY`, and
    what it ran for."""
    ran: list[Any] = []
    if request.param == "policy":
        monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["cancel_order"]')

    async def cancel_order(order_id: str) -> str:
        ran.append(order_id)
        return "Cancelled"

    return Tool("cancel_order", cancel_order, "Cancel an order.", needs_approval=request.param == "code"), ran


@pytest.mark.parametrize("shape", list(SHAPES))
async def test_an_agent_tool_subagent_is_opaque(shape: str):
    graph = SHAPES[shape](_specialist(TRACK))
    frames = await run_turn(langgraph(graph), "Where is 42?")
    assert_opaque(frames, "ask_specialist", INNER_TEXT)


@pytest.mark.parametrize("shape", list(SHAPES))
async def test_a_gated_tool_in_an_agent_tool_subagent_fails_loudly(shape: str, gated: tuple[Tool, list[Any]]):
    tool, ran = gated
    await assert_fails_loudly(lambda: langgraph(SHAPES[shape](_specialist(tool))), "Cancel 42.", ran)


@pytest.mark.parametrize("shape", list(SHAPES))
async def test_approving_the_subagent_tool_doesnt_approve_an_inner_call_with_its_id(
    shape: str, gated: tuple[Tool, list[Any]], monkeypatch: pytest.MonkeyPatch
):
    """A subagent's model can reuse the id of the top-level call that ran it
    (a provider numbering its call ids per response). Approving that call
    approves only it: the gated tool inside still fails loudly."""
    tool, ran = gated
    policy = ["ask_specialist", *([tool.name] if not tool.needs_approval else [])]
    monkeypatch.setenv("SECTR_APPROVAL_POLICY", json.dumps(policy))
    session = Session(SHAPES[shape](_specialist(tool, call_id="c1")))
    assert types(await session.send("Cancel 42."))[-1] == "TOOL_APPROVAL_REQUIRED"
    out = await session.decide(True)
    assert [f["payload"].get("code") for f in out if f["type"] == "ERROR"] == [SUBAGENT_GATED_TOOL]
    assert ran == []


async def test_a_gated_tool_in_a_subgraph_fails_loudly(gated: tuple[Tool, list[Any]]):
    tool, ran = gated
    await assert_fails_loudly(lambda: langgraph(_subgraph(_specialist(tool))), "Cancel 42.", ran)


# ---- gating any tool in code ----------------------------------------
#
# A tool that isn't a Sectr tool (a plain LangChain `@tool`, a
# `StructuredTool`, a `BaseTool` subclass, an MCP tool) can be gated in code
# two ways: `create_agent`'s `HumanInTheLoopMiddleware(interrupt_on=...)`,
# which the adapter honors with Sectr's own pause and resume, and
# `as_langgraph_tool(tool, needs_approval=True)` on any graph. Every kind of
# tool, through each, pauses without running; an approval runs it once and
# a denial never does. The conformance harness runs its approval scenarios
# through both as well, on real model clients.


def _plain_tool() -> BaseTool:
    @langchain_tool
    async def refund(order_id: str) -> str:
        """Refund an order."""
        return await _refund(order_id)

    return refund


def _structured_tool() -> BaseTool:
    return StructuredTool.from_function(coroutine=_refund, name="refund", description="Refund an order.")


class _RefundArgs(BaseModel):
    order_id: str


class _RefundTool(BaseTool):
    """A hand-written `BaseTool` subclass, the way community tools are."""

    name: str = "refund"
    description: str = "Refund an order."
    args_schema: type[BaseModel] = _RefundArgs

    def _run(self, order_id: str) -> str:
        raise NotImplementedError("async only")

    async def _arun(self, order_id: str) -> str:
        return await _refund(order_id)


def _mcp_shaped_tool() -> BaseTool:
    """The shape langchain-mcp-adapters builds: a JSON Schema `args_schema`
    and a `content_and_artifact` result. (The real thing is in the
    conformance harness, against its MCP server.)"""

    async def call(**arguments: Any) -> tuple[str, Any]:
        return await _refund(arguments["order_id"]), None

    return StructuredTool(
        name="refund",
        description="Refund an order.",
        args_schema={
            "type": "object",
            "properties": {"order_id": {"type": "string"}},
            "required": ["order_id"],
        },
        coroutine=call,
        response_format="content_and_artifact",
    )


KINDS: dict[str, Callable[[], BaseTool]] = {
    "plain_tool": _plain_tool,
    "structured_tool": _structured_tool,
    "base_tool": _RefundTool,
    "mcp_shaped": _mcp_shaped_tool,
}


def _hitl(**interrupt_on: Any) -> HumanInTheLoopMiddleware:
    return HumanInTheLoopMiddleware(interrupt_on=interrupt_on)


GATES: dict[str, Callable[[BaseChatModel, BaseTool], Any]] = {
    "middleware": lambda chat, t: create_agent(chat, [t], middleware=[_hitl(refund=True)]),
    "wrapped_create_agent": lambda chat, t: create_agent(chat, [as_langgraph_tool(t, needs_approval=True)]),
    "wrapped_state_graph": lambda chat, t: tools_graph(chat, [as_langgraph_tool(t, needs_approval=True)]),
}

REFUND_PAUSE = [
    ("TOOL_CALL", {"tool_call_id": "c2", "name": "refund", "arguments": {"order_id": "123"}}),
    ("TOOL_APPROVAL_REQUIRED", {"tool_call_id": "c2", "name": "refund", "arguments": {"order_id": "123"}}),
]


async def _approve_runs_once(graph: Any, chat: ScriptedChatModel) -> None:
    """`refund` pauses without running (and without the middleware's own
    `interrupt()`); approving runs it once and the turn goes on."""
    session = Session(graph)
    out = await session.send("refund it")
    assert "ERROR" not in types(out), out
    assert shape(out)[-2:] == REFUND_PAUSE
    assert EXECUTED == []
    out = await session.decide(True)
    assert shape(out)[0] == ("TOOL_RESULT", {"tool_call_id": "c2", "result": "Refunded 123"})
    assert types(out)[-1] == "NO_MORE_ACTIONS"
    assert executed("refund") == 1
    assert len(chat.seen) == 2


def _refund_chat() -> ScriptedChatModel:
    return model(Reply("Refunding now.", (REFUND_CALL,)), Reply("Refund processed!"))


@pytest.mark.parametrize("gate", list(GATES))
@pytest.mark.parametrize("kind", list(KINDS))
async def test_any_tool_gated_in_code_pauses_and_an_approval_runs_it_once(kind: str, gate: str):
    chat = _refund_chat()
    await _approve_runs_once(GATES[gate](chat, KINDS[kind]()), chat)


@pytest.mark.parametrize("gate", list(GATES))
@pytest.mark.parametrize("kind", list(KINDS))
async def test_any_tool_gated_in_code_never_runs_when_denied(kind: str, gate: str):
    chat = model(Reply("Refunding now.", (REFUND_CALL,)), Reply("Understood, no refund."))
    session = Session(GATES[gate](chat, KINDS[kind]()))
    out = await session.send("refund it")
    assert shape(out)[-2:] == REFUND_PAUSE
    out = await session.decide(False)
    assert out[0] == {"type": "TOOL_RESULT", "payload": {"tool_call_id": "c2", "result": REJECTION_MESSAGE}}
    assert types(out)[-1] == "NO_MORE_ACTIONS"
    assert EXECUTED == []


UNGATED_REFUND = Tool("refund", _refund, "Refund an order.")


async def test_interrupt_on_gates_a_sectr_tool():
    chat = _refund_chat()
    graph = create_agent(chat, [as_langgraph_tool(UNGATED_REFUND)], middleware=[_hitl(refund=True)])
    await _approve_runs_once(graph, chat)


async def test_needs_approval_argument_gates_a_sectr_tool():
    chat = _refund_chat()
    await _approve_runs_once(tools_graph(chat, [as_langgraph_tool(UNGATED_REFUND, needs_approval=True)]), chat)


async def test_a_wrapped_tool_without_needs_approval_runs_ungated():
    chat = model(Reply("Refunding now.", (REFUND_CALL,)), Reply("Refund processed!"))
    out = await Session(tools_graph(chat, [as_langgraph_tool(_plain_tool())])).send("refund it")
    assert "TOOL_APPROVAL_REQUIRED" not in types(out)
    assert types(out)[-1] == "NO_MORE_ACTIONS"
    assert executed("refund") == 1


async def test_interrupt_on_with_approve_and_reject_decisions_gates():
    chat = _refund_chat()
    decisions = {"allowed_decisions": ["approve", "reject"], "description": "Refund this order?"}
    graph = create_agent(chat, [_plain_tool()], middleware=[_hitl(refund=decisions)])
    await _approve_runs_once(graph, chat)


@pytest.mark.parametrize("when", [True, False])
async def test_interrupt_on_honors_a_when_predicate(when: bool):
    """`when` decides per call, as in the middleware: a call it lets through
    runs without a pause."""
    chat = _refund_chat()
    config = {"allowed_decisions": ["approve", "reject"], "when": lambda request: when}
    graph = create_agent(chat, [_plain_tool()], middleware=[_hitl(refund=config)])
    if when:
        await _approve_runs_once(graph, chat)
    else:
        out = await Session(graph).send("refund it")
        assert "TOOL_APPROVAL_REQUIRED" not in types(out)
        assert executed("refund") == 1


async def test_one_middleware_in_two_agents_gates_both():
    """The adapter empties the middleware's `interrupt_on` so it never calls
    `interrupt()`; an agent sharing the same middleware instance, wrapped
    afterwards, still gets its tools gated."""
    hitl = _hitl(refund=True)
    langgraph(create_agent(model(), [_plain_tool()], middleware=[hitl]))
    chat = _refund_chat()
    await _approve_runs_once(create_agent(chat, [_plain_tool()], middleware=[hitl]), chat)


async def test_interrupt_on_false_leaves_the_tool_ungated():
    """A guard: only the tools `interrupt_on` gates pause."""
    chat = model(Reply("Refunding now.", (REFUND_CALL,)), Reply("Refund processed!"))
    graph = create_agent(chat, [_plain_tool()], middleware=[_hitl(refund=False)])
    out = await Session(graph).send("refund it")
    assert "TOOL_APPROVAL_REQUIRED" not in types(out)
    assert types(out)[-1] == "NO_MORE_ACTIONS"
    assert executed("refund") == 1


@pytest.mark.parametrize(
    "decisions",
    [["approve", "edit"], ["reject", "respond"]],
    ids=["no_reject", "no_approve"],
)
def test_interrupt_on_without_both_approve_and_reject_is_rejected(decisions: list[str]):
    """Sectr's approvals are approve or reject: an entry that doesn't allow
    both can't be honored. (`True` allows every decision, edit and respond
    too, and is accepted: Sectr offers approve and reject.)"""
    graph = create_agent(model(), [_plain_tool()], middleware=[_hitl(refund={"allowed_decisions": decisions})])
    with pytest.raises(ValueError, match="allowed_decisions"):
        langgraph(graph)


@pytest.mark.parametrize(
    "how",
    ["interrupt_on_and_policy", "interrupt_on_and_needs_approval", "wrapped_and_policy"],
)
async def test_a_tool_gated_two_ways_pauses_once(how: str, monkeypatch: pytest.MonkeyPatch):
    chat = _refund_chat()
    if how == "interrupt_on_and_policy":
        monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["refund"]')
        graph = create_agent(chat, [_plain_tool()], middleware=[_hitl(refund=True)])
    elif how == "interrupt_on_and_needs_approval":
        graph = create_agent(chat, [as_langgraph_tool(REFUND)], middleware=[_hitl(refund=True)])
    else:
        monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["refund"]')
        graph = tools_graph(chat, [as_langgraph_tool(_plain_tool(), needs_approval=True)])
    await _approve_runs_once(graph, chat)


async def test_calls_gated_different_ways_in_one_message_are_approved_in_turn():
    """`refund` is a Sectr tool gated in code, `cancel_order` a plain tool
    gated by `interrupt_on`: one pause each, and each runs once."""

    @langchain_tool
    async def cancel_order(order_id: str) -> str:
        """Cancel an order."""
        EXECUTED.append(("cancel_order", {"order_id": order_id}))
        return f"Cancelled {order_id}"

    calls = (("c1", "refund", {"order_id": "1"}), ("c2", "cancel_order", {"order_id": "2"}))
    chat = model(Reply("On it.", calls), Reply("Both done."))
    graph = create_agent(chat, [as_langgraph_tool(REFUND), cancel_order], middleware=[_hitl(cancel_order=True)])
    session = Session(graph)
    out = await session.send("refund 1, cancel 2")
    assert types(out)[-1] == "TOOL_APPROVAL_REQUIRED"
    paused = [out[-1]["payload"]["name"]]
    out = await session.decide(True)
    assert types(out)[-1] == "TOOL_APPROVAL_REQUIRED"
    paused.append(out[-1]["payload"]["name"])
    out = await session.decide(True)
    assert types(out)[-1] == "NO_MORE_ACTIONS"
    assert sorted(paused) == ["cancel_order", "refund"]
    assert executed("refund") == 1 and executed("cancel_order") == 1


def _injected_tools() -> dict[str, BaseTool]:
    """`refund`, taking an argument the graph injects, three ways."""

    @langchain_tool("refund")
    async def with_state(order_id: str, state: Annotated[dict, InjectedState]) -> str:
        """Refund an order."""
        return await _refund(order_id)

    @langchain_tool("refund")
    async def with_store(order_id: str, store: Annotated[Any, InjectedStore()]) -> str:
        """Refund an order."""
        return await _refund(order_id)

    @langchain_tool("refund")
    async def with_runtime(order_id: str, runtime: ToolRuntime) -> str:
        """Refund an order."""
        return await _refund(order_id)

    return {"state": with_state, "store": with_store, "runtime": with_runtime}


INJECTED = ["state", "store", "runtime"]


@pytest.mark.parametrize("injected", INJECTED)
@pytest.mark.parametrize("gate", ["middleware", "wrapped"])
async def test_a_gated_tool_with_graph_injected_args_runs_on_approval(injected: str, gate: str):
    """Accepted, either way of gating: the approved call runs in the graph's
    ToolNode on resume, which injects the state, store or runtime
    as on any turn."""
    from langgraph.store.memory import InMemoryStore

    refund = _injected_tools()[injected]
    chat = model(Reply("Refunding.", (REFUND_CALL,)), Reply("Done."))
    if gate == "middleware":
        graph = create_agent(chat, [refund], middleware=[_hitl(refund=True)], store=InMemoryStore())
    else:
        graph = create_agent(chat, [as_langgraph_tool(refund, needs_approval=True)], store=InMemoryStore())
    session = Session(graph)
    assert types(await session.send("refund it"))[-1] == "TOOL_APPROVAL_REQUIRED"
    assert executed("refund") == 0
    out = await session.decide(True)
    assert types(out)[0] == "TOOL_RESULT" and types(out)[-1] == "NO_MORE_ACTIONS"
    assert executed("refund") == 1


@pytest.mark.parametrize("injected", INJECTED)
def test_an_ungated_tool_with_graph_injected_args_is_accepted_next_to_interrupt_on(injected: str):
    """A guard: `interrupt_on` gating one tool doesn't affect an injected
    tool next to it."""
    lookup = _injected_tools()[injected].model_copy(update={"name": "lookup_order"})
    graph = create_agent(model(), [_plain_tool(), lookup], middleware=[_hitl(refund=True)])
    langgraph(graph)


@pytest.mark.parametrize("shape", list(SHAPES))
async def test_interrupt_on_inside_a_subagent_fails_loudly(shape: str):
    """`HumanInTheLoopMiddleware` on a subagent gates a tool inside it, which
    the beta subagent contract doesn't support. The subagent
    isn't part of the graph the adapter sees, so its middleware isn't
    honored: its `interrupt()` fails the turn."""
    ran: list[str] = []

    @langchain_tool
    async def cancel_order(order_id: str) -> str:
        """Cancel an order."""
        ran.append(order_id)
        return "Cancelled"

    chat = model(
        Reply(text=INNER_TEXT, calls=(("i1", "cancel_order", {"order_id": "42"}),)),
        Reply(text="Done."),
    )
    specialist = create_agent(chat, [cancel_order], middleware=[_hitl(cancel_order=True)], name="specialist")
    await assert_fails_loudly(lambda: langgraph(SHAPES[shape](specialist)), "Cancel 42.", ran)


@pytest.mark.parametrize("gate", ["interrupt_on", "policy"])
async def test_a_gated_plain_tool_in_a_subgraph_fails_loudly(gate: str, monkeypatch: pytest.MonkeyPatch):
    """A subgraph without Sectr tools is accepted, and the adapter gates
    its ToolNode's calls: a gated one (by the subgraph's own middleware, or
    named by the policy) is inside a subagent, so it fails the turn."""
    ran: list[str] = []

    @langchain_tool
    async def cancel_order(order_id: str) -> str:
        """Cancel an order."""
        ran.append(order_id)
        return "Cancelled"

    chat = model(
        Reply(text=INNER_TEXT, calls=(("i1", "cancel_order", {"order_id": "42"}),)),
        Reply(text="Done."),
    )
    if gate == "policy":
        monkeypatch.setenv("SECTR_APPROVAL_POLICY", '["cancel_order"]')
        specialist = create_agent(chat, [cancel_order], name="specialist")
    else:
        specialist = create_agent(chat, [cancel_order], middleware=[_hitl(cancel_order=True)], name="specialist")
    await assert_fails_loudly(lambda: langgraph(_subgraph(specialist)), "Cancel 42.", ran)

