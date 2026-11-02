"""LangGraph adapter (`sectr[langgraph]` extra; floor = tested versions:
`langgraph>=1.2.11`, `langgraph-prebuilt>=1.1.0`, `langchain-core>=1.6.6`).

`langgraph(graph, *, context=None)` returns a handler mapping a compiled
LangGraph onto the platform contract (example:
`examples/langgraph/main.py`). `context` is the graph's runtime context,
a factory called once per invocation, as Pydantic AI's `deps`. There is NO
LangGraph checkpointer — each platform turn is a fresh `graph.astream`
whose `{"messages": [...]}` state is rebuilt from `ctx.messages`. The
journal is the checkpoint.

- normal turn: transcript → LangChain messages; `astream(stream_mode=
  ["messages", "updates"])`; text/reasoning chunks → MESSAGE_START / DELTA /
  END, accumulated `tool_call_chunks` → TOOL_CALL (id verbatim from the
  model), node-emitted `ToolMessage` → TOOL_RESULT; stream end →
  NO_MORE_ACTIONS (appended by `drive`).
- pause: a tool converted with `as_langgraph_tool` that is gated
  (`needs_approval` / `SECTR_APPROVAL_POLICY`) raises `ApprovalRequired`
  (a `BaseException`, so it escapes `ToolNode`'s error handling) carrying
  the model's call id (`InjectedToolCallId`). The other gated calls are
  gated by the adapter's own `ToolNode` tool-call wrapper, installed at
  construction around the graph's (and its middleware's), which raises
  the same way before the tool runs: any tool a `HumanInTheLoopMiddleware`
  names in `interrupt_on` (the adapter empties it, so the middleware never
  calls `interrupt()`), and any other tool (a plain LangChain tool, an MCP
  tool from `langchain-mcp-adapters`) that `as_langgraph_tool(...,
  needs_approval=True)` marked or the policy names. A pausing call first
  waits for the other calls of its message that are running (parallel
  siblings) to finish, and their results are journaled before the pause,
  even those the aborted `ToolNode` never streamed — so a sibling runs
  once, on the pause turn, and never again on resume (`_Run`, read by the
  gates). The adapter closes any open message and yields
  TOOL_APPROVAL_REQUIRED as the final frame. One approval per turn: the
  first escaping `ApprovalRequired` wins.
- resume (`approval_decision`): the graph runs the decided call itself,
  from its tools node. The pause state is rebuilt from the journal: the
  graph is copied with a throwaway checkpointer for this invocation, the
  rebuilt messages (up to the AIMessage with the calls) are seeded as the
  output of the tools node's predecessor (`aupdate_state(as_node=…)`),
  and `astream(None)` continues from there — the graph's own `ToolNode`
  (its wrappers and middleware, injected arguments, error handling,
  `Command` results) and its routing out of the tools node apply to the
  resumed calls as on any turn. The decision travels in the run's config
  (`_Run`), read by the gates: the approved call passes its gate, a
  denied one gets the standard rejection text without running, and a
  call answered on an earlier invocation (a sibling that ran on the pause
  turn) gets its journaled result back without running again. Other
  unanswered calls in the same AIMessage wait for the decided call, then
  run normally; a gated sibling that pauses again waits for them as on
  the pause turn. Several gated calls in one AIMessage become sequential
  approvals, one per turn.

Supported graphs (checked at `langgraph(graph)` construction, failing
before the first turn): ReAct-shaped graphs — exactly one top-level
`ToolNode` holding the sectr tools, or, with none, the tools the policy
may gate (`StateGraph(MessagesState)` + agent
node + `ToolNode`, `langchain.agents.create_agent`, or the deprecated
`create_react_agent`). A resume seeds the state as the tools node's single
predecessor; a graph whose tools node is reached from several nodes fails
at the pause, before anyone is asked to approve. Checkpointed graphs, sectr
tools in several ToolNodes, and ToolNodes nested in subgraphs are rejected:
they need checkpointer-backed resume, which isn't supported yet and would
also bring native `interrupt()`, multi-agent graphs, and non-message state.
Until then non-message state channels do not survive the turn boundary — keep
state in the conversation (`todo_write` as an ordinary tool).

Subagents (an agent awaited inside a tool, with the graph's config, which
langchain passes on implicitly) are opaque: only the outer tool's call and
result are journaled. A gated call inside one (a Sectr tool gated in code
or named by the policy, a gated tool in a subgraph, or a subagent's own
`HumanInTheLoopMiddleware`) fails the turn with `subagent_gated_tool`,
before the tool runs: the adapter can't pause and resume inside a
subagent. A run is a subagent's when its checkpoint namespace is nested.
Sectr tools in a subgraph are rejected at construction, with the code.

Native `langgraph.types.interrupt()` is not supported: with no
checkpointer it surfaces only as `__interrupt__` in the `updates` stream
(the turn would otherwise end as if complete) — the adapter fails loudly
instead. Gate tools with `as_langgraph_tool` or `HumanInTheLoopMiddleware`
instead.

Framework state: what the graph stores on a model response's `AIMessage`
beyond its text and calls is journaled on its MESSAGE_END, read from the
model node's update, and put back on the rebuilt `AIMessage`, so later
requests send it as the graph does natively: its reasoning
(reasoning/thinking content blocks, signatures included, and
`additional_kwargs` `reasoning_content` / `reasoning_details`; its text is
also streamed as `reasoning` deltas), and its `name` (a named agent's,
which Chat Completions sends with the message).
"""

from __future__ import annotations

import asyncio
import inspect
import json
import logging
import typing
import uuid
from collections.abc import AsyncIterator, Awaitable, Callable, Iterable, Iterator
from dataclasses import dataclass, field
from typing import Annotated, Any, cast

from langchain_core.callbacks import AsyncCallbackManagerForToolRun
from langchain_core.messages import (
    AIMessage,
    AIMessageChunk,
    BaseMessage,
    HumanMessage,
    ToolMessage,
)
from langchain_core.runnables import RunnableConfig
from langchain_core.tools import BaseTool, InjectedToolArg, InjectedToolCallId, StructuredTool
from langgraph.checkpoint.memory import InMemorySaver
from langgraph.config import get_config
from langgraph.pregel import Pregel
from langgraph.prebuilt import ToolNode
from pydantic import create_model

from ..app import Handler
from ..context import SessionContext
from ..events import (
    Error,
    MessageDelta,
    MessageEnd,
    MessageStart,
    RunnerEvent,
    ToolApprovalRequired,
    ToolCall,
    ToolResult,
)
from ..tools import ApprovalRequired, Tool, platform_approval_policy
from . import SUBAGENT_GATED_TOOL
from ._framework_state import (
    FrameworkData,
    exact,
    framework_state,
    journaled_state,
    parsed,
    unreadable,
)

__all__ = ["langgraph", "as_langgraph_tool"]

# Same model-visible text as the openai-agents adapter (and the SDK default).
REJECTION_MESSAGE = "Tool execution was not approved."

logger = logging.getLogger(__name__)

# The content blocks and `additional_kwargs` that hold an AIMessage's
# reasoning, across LangChain's provider integrations.
_REASONING_BLOCKS = frozenset({"thinking", "redacted_thinking", "reasoning"})
_REASONING_KWARGS = ("reasoning_content", "reasoning_details")


class _State(FrameworkData):
    """A stored AIMessage's framework state: its reasoning, which its
    `reasoning` deltas carry only the text of, and its `name`. LangChain's
    provider integrations forward the reasoning to the provider as it is, so
    each piece is kept as its exact JSON (the journal would re-sort an
    object's keys)."""

    adapter = "langgraph"
    version = 1

    content: tuple[str, ...] = ()
    """The reasoning content blocks (thinking, redacted_thinking,
    reasoning), in order, each as its exact JSON."""
    additional_kwargs: str | None = None
    """The reasoning `additional_kwargs` (`reasoning_content`,
    `reasoning_details`), as their exact JSON."""
    name: str | None = None
    """The message's `name`: a named agent's, which Chat Completions sends."""

    def has_reasoning(self) -> bool:
        return bool(self.content) or self.additional_kwargs is not None

_SECTR_TOOL_KEY = "sectr.tool"
# Marks a LangChain tool `as_langgraph_tool(tool, needs_approval=True)`
# gated: the adapter's ToolNode gate pauses it (it doesn't gate itself).
_SECTR_GATED_KEY = "sectr.needs_approval"
# Where `_honor_interrupt_on` keeps a `HumanInTheLoopMiddleware`'s original
# `interrupt_on`, once it has emptied it (so a second `langgraph(graph)`
# still finds it).
_INTERRUPT_ON_ATTR = "_sectr_interrupt_on"
_CHECKPOINT_POINTER = (
    "Checkpointer-backed resume (native interrupt(), multi-agent graphs, subgraphs, "
    "non-message state) isn't supported yet."
)
_NATIVE_INTERRUPT_MESSAGE = (
    "langgraph interrupt() was called, but sectr runs graphs without a checkpointer, so the "
    "pause cannot be resumed. Gate tools with HumanInTheLoopMiddleware on create_agent, or "
    "sectr.adapters.langgraph.as_langgraph_tool(tool, needs_approval=True), instead. "
    + _CHECKPOINT_POINTER
)
# LangGraph's checkpoint namespace separator (`langgraph._internal.
# _constants.NS_SEP`): a graph run inside another's node or tool runs in
# `parent|child`, a top-level node in one segment.
_NS_SEP = "|"
_NO_CHECKPOINTER_MESSAGE = (
    "Command(resume=...) is not supported: sectr runs graphs without a checkpointer. "
    "Gate tools with HumanInTheLoopMiddleware on create_agent, or "
    "sectr.adapters.langgraph.as_langgraph_tool(tool, needs_approval=True), instead. "
    + _CHECKPOINT_POINTER
)

# Where a run's `_Run` lives in its config (`configurable`). The config is
# the run's own, so the state is scoped to that run.
_RUN_KEY = "__sectr_run"


@dataclass
class _Run:
    """One invocation's tool-call state, applied by the gates to each call
    as the graph's ToolNode runs it — on every run, and on an approval
    resume with the decision.

    A call that pauses waits until every other running call has finished
    before its pause aborts the node, so their results are journaled (even
    those that never streamed) and none of them runs again on the resume.
    On a resume the decided call runs first, and the others wait for it."""

    # The decided call (approval resume only), and the decision.
    call_id: str | None = None
    approved: bool = False
    # Calls answered on an earlier invocation: their journaled result is
    # given back, and they never run again.
    journaled: dict[str, ToolMessage] = field(default_factory=dict)
    decided: asyncio.Event = field(default_factory=asyncio.Event)
    # Calls running now (past the gate, not paused), and set when none are.
    running: int = 0
    idle: asyncio.Event = field(default_factory=asyncio.Event)
    # Results of the calls run in this invocation, by call id: journaled
    # before a pause in case the node aborted before they streamed.
    results: dict[str, ToolMessage] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if self.call_id is None:
            self.decided.set()  # nothing to wait for

    def settled(self, call: dict[str, Any]) -> ToolMessage | None:
        """A call's answer that doesn't run it: its journaled result, or the
        rejection of the denied call."""
        if call["id"] in self.journaled:
            return self.journaled[call["id"]]
        if call["id"] == self.call_id and not self.approved:
            return ToolMessage(content=REJECTION_MESSAGE, tool_call_id=call["id"], name=call["name"])
        return None

    async def answer(self, call: dict[str, Any], run: Callable[[], Awaitable[Any]]) -> Any:
        settled = self.settled(call)
        if settled is not None:
            if call["id"] == self.call_id:
                self.decided.set()
            return settled
        self._enter()
        try:
            if call["id"] != self.call_id:
                await self.decided.wait()
            result = await run()
        except ApprovalRequired:
            self._leave()
            # Let the node's other calls reach their gates, then let them
            # finish before pausing.
            await asyncio.sleep(0)
            await self.idle.wait()
            raise
        except BaseException:
            self._leave()
            raise
        finally:
            if call["id"] == self.call_id:
                self.decided.set()
        self._leave()
        if isinstance(result, ToolMessage):
            self.results[call["id"]] = result
        return result

    def answer_sync(self, call: dict[str, Any], run: Callable[[], Any]) -> Any:
        """A sync tool-call wrapper runs its calls inline, one at a time:
        nothing to wait for."""
        settled = self.settled(call)
        if settled is not None:
            return settled
        result = run()
        if isinstance(result, ToolMessage):
            self.results[call["id"]] = result
        return result

    def _enter(self) -> None:
        self.running += 1
        self.idle.clear()

    def _leave(self) -> None:
        self.running -= 1
        if self.running == 0:
            self.idle.set()


def _current_run() -> _Run | None:
    """The top-level run's state, read by the gates. None inside a
    subagent: its calls are none of the run's (a subagent's model can reuse
    a top-level call id), so none is approved, denied or answered there."""
    try:
        config = get_config()
    except RuntimeError:  # outside a graph run
        return None
    if _in_subagent():
        return None
    return (config.get("configurable") or {}).get(_RUN_KEY)


def _approved(call_id: str | None) -> bool:
    state = _current_run()
    return state is not None and state.approved and state.call_id == call_id


class _GatedInSubagent(ApprovalRequired):
    """A gated call inside a subagent: a graph run inside a tool or node of
    the graph (`create_agent` awaited in a tool, a subgraph). The adapter
    can't pause and resume inside one, so the turn fails with
    `subagent_gated_tool`. An `ApprovalRequired`, so it escapes the tool
    nodes and graphs in between as a pause does."""

    def __init__(self, name: str, arguments: Any, gate: str) -> None:
        super().__init__(name, arguments)
        self.gate = gate


def _in_subagent() -> bool:
    """Whether this runs inside a graph run nested in the graph's own (its
    checkpoint namespace has more than one segment). A subagent run that
    doesn't inherit the graph's config looks top-level: its gated call
    fails as an approval outside a model tool call instead."""
    try:
        config = get_config()
    except RuntimeError:  # outside a graph run
        return False
    return _NS_SEP in ((config.get("configurable") or {}).get("checkpoint_ns") or "")


def _gate_call(name: str, arguments: Any, call_id: str | None, gate: str) -> None:
    """Pause on a gated call, or, inside a subagent, fail it."""
    if _in_subagent():
        raise _GatedInSubagent(name, arguments, gate)
    raise ApprovalRequired(name, arguments, call_id=call_id)


def _subagent_gated_message(exc: _GatedInSubagent) -> str:
    what = "needs approval" if exc.gate == "code" else "is in the deployment's approval policy ([deploy] approval_tools)"
    return (
        f"tool {exc.name!r} {what}, but it was called inside a subagent (an agent run inside "
        f"a tool, or a subgraph), and a gated tool inside a subagent is not supported on Sectr "
        f"yet ({SUBAGENT_GATED_TOOL}). Gate the tool that runs the subagent instead, or move "
        "the tool to the graph's top-level ToolNode."
    )


# ----------------------------------------------------------------------
# handler
# ----------------------------------------------------------------------


def langgraph(
    graph: Any,
    *,
    context: Callable[[SessionContext], Any | Awaitable[Any]] | None = None,
) -> Handler:
    """Handler factory wrapping a compiled LangGraph (see module docstring).

    `context` is a factory called once per invocation with the
    `SessionContext` (a static value is `lambda ctx: v`); its value is the
    graph's runtime context (`astream(context=…)`), which tools read as
    `runtime.context` (`ToolRuntime`) and nodes and middleware as
    `Runtime.context` — on every run, an approval resume included.

    Raises `ValueError` at construction for graphs outside the supported
    ReAct shape, or with structured output — before the first turn, never
    mid-conversation."""
    _reject_structured_output(graph)
    tools = _supported_tools(graph)
    _install_gates(graph)
    resume_node = _resume_node(graph, tools)

    async def handler(ctx: SessionContext) -> AsyncIterator[RunnerEvent]:
        messages = _transcript_messages(ctx)
        runtime_context = await _context(ctx, context)
        if ctx.reason.get("kind") == "approval_decision" and ctx.pending_tool_call is not None:
            run = _resume(graph, ctx, messages, tools, resume_node, runtime_context)
        else:
            run = _stream_graph(graph, {"messages": messages}, tools, context=runtime_context)
        async for event in run:
            if isinstance(event, ToolApprovalRequired) and resume_node is None:
                # Fail now, before a human is asked to approve a call the
                # resume couldn't run.
                yield Error(
                    message=(
                        "the graph's tools node is reached from several nodes, so an "
                        "approval can't be resumed (a resume seeds the state as the "
                        "tools node's single predecessor). " + _CHECKPOINT_POINTER
                    ),
                    code="langgraph_unmatched_approval",
                )
                return
            yield event

    handler.sectr_framework = "langgraph"  # type: ignore[attr-defined]
    return handler


# ----------------------------------------------------------------------
# graph validation / tool discovery
# ----------------------------------------------------------------------


def _tool_nodes(graph: Any, nested: bool = False) -> Iterator[tuple[ToolNode, bool]]:
    """(ToolNode, lives-in-a-subgraph) for every ToolNode reachable through
    `graph.nodes`, recursing into compiled subgraphs."""
    for node in (getattr(graph, "nodes", None) or {}).values():
        bound = getattr(node, "bound", None)
        if isinstance(bound, ToolNode):
            yield bound, nested
        elif isinstance(bound, Pregel):
            yield from _tool_nodes(bound, True)


def _reject_structured_output(graph: Any) -> None:
    """Structured output isn't supported yet. `create_agent(...,
    response_format=...)` keeps the format in its model node's closure
    (`initial_response_format`, langchain 1.x; the conformance case
    `structured_output` catches a change), and `create_react_agent`'s adds a
    `generate_structured_response` node. A custom node calling
    `with_structured_output` can't be detected."""
    rejected = ValueError(
        "langgraph(graph): the agent has a response_format, but structured output is not "
        "supported on Sectr yet. Remove response_format: the agent's answer is journaled "
        "and shown as text."
    )
    for name, node in (getattr(graph, "nodes", None) or {}).items():
        if name == "generate_structured_response":
            raise rejected
        bound = getattr(node, "bound", None)
        if isinstance(bound, Pregel):
            _reject_structured_output(bound)
            continue
        for fn in (getattr(bound, "afunc", None), getattr(bound, "func", None)):
            code, closure = getattr(fn, "__code__", None), getattr(fn, "__closure__", None)
            if code is None or not closure:
                continue
            cells = dict(zip(code.co_freevars, closure, strict=True))
            cell = cells.get("initial_response_format")
            if cell is not None and cell.cell_contents is not None:
                raise rejected


def _is_sectr_tool(tool: BaseTool) -> bool:
    return bool((tool.metadata or {}).get(_SECTR_TOOL_KEY))


def _wrapped_gated(tool: BaseTool) -> bool:
    return bool((tool.metadata or {}).get(_SECTR_GATED_KEY))


def _supported_tools(graph: Any) -> dict[str, BaseTool]:
    """Validate the graph shape and return the tools resume may execute:
    the single top-level ToolNode holding sectr-converted tools, else the
    graph's only top-level ToolNode (whose tools the policy may gate). A
    policy-gated call in another top-level ToolNode fails closed, at the
    pause (`langgraph_unmatched_approval`); in a subgraph's, with
    `subagent_gated_tool`."""
    if getattr(graph, "checkpointer", None):
        raise ValueError(
            "langgraph(graph): the graph was compiled with a checkpointer. A checkpointer "
            "is a second source of truth the stateless sectr runner cannot use — compile "
            "without one (the journal is the checkpoint). " + _CHECKPOINT_POINTER
        )

    # Any sectr-converted tool may be gated by deployment policy, so shape
    # validation applies to all of them, not only needs_approval=True ones,
    # and to every tool `as_langgraph_tool` gated.
    holders = [
        (node, nested)
        for node, nested in _tool_nodes(graph)
        if any(_is_sectr_tool(t) or _wrapped_gated(t) for t in node.tools_by_name.values())
    ]
    if any(nested for _, nested in holders):
        raise ValueError(
            "langgraph(graph): a ToolNode holding sectr tools is nested inside a subgraph. A "
            "Sectr tool can be gated (needs_approval, or [deploy] approval_tools), and a gated "
            f"tool inside a subagent is not supported on Sectr yet ({SUBAGENT_GATED_TOOL}): "
            "only a single top-level ToolNode is supported. " + _CHECKPOINT_POINTER
        )
    if len(holders) > 1:
        raise ValueError(
            "langgraph(graph): sectr tools appear in several ToolNodes; only one top-level "
            "ToolNode is supported (multi-agent / supervisor graphs). " + _CHECKPOINT_POINTER
        )
    if holders:
        return dict(holders[0][0].tools_by_name)
    top = [node for node, nested in _tool_nodes(graph) if not nested]
    return dict(top[0].tools_by_name) if len(top) == 1 else {}


async def _context(
    ctx: SessionContext, factory: Callable[[SessionContext], Any | Awaitable[Any]] | None
) -> Any:
    if factory is None:
        return None
    value = factory(ctx)
    return await value if inspect.isawaitable(value) else value


def _resume_node(graph: Any, tools: dict[str, BaseTool]) -> str | None:
    """The node a resume seeds the rebuilt state as: the single predecessor
    of the top-level ToolNode running `tools` (the model node, or the last
    after-model middleware node), so the graph routes on to its tools node
    as it did on the pause turn. None when there's no such single node."""
    names = [
        name
        for name, node in (getattr(graph, "nodes", None) or {}).items()
        if isinstance(getattr(node, "bound", None), ToolNode) and node.bound.tools_by_name == tools
    ]
    if len(names) != 1:
        return None
    try:
        edges = graph.get_graph().edges
    except Exception:  # a graph that can't be drawn
        return None
    preds = {e.source for e in edges if e.target == names[0] and e.source != names[0]}
    return preds.pop() if len(preds) == 1 else None


def _install_gates(graph: Any) -> None:
    """Gate the tool calls of every ToolNode in the graph and its compiled
    subgraphs: the tools a `HumanInTheLoopMiddleware` at the same level
    names in `interrupt_on`, and, by name, non-Sectr tools the policy
    names or `as_langgraph_tool` gated."""
    interrupt_on = _honor_interrupt_on(graph)
    for node in (getattr(graph, "nodes", None) or {}).values():
        bound = getattr(node, "bound", None)
        if isinstance(bound, ToolNode):
            _gate_tool_calls(bound, interrupt_on)
        elif isinstance(bound, Pregel):
            _install_gates(bound)


def _honor_interrupt_on(graph: Any) -> dict[str, Any]:
    """The tools the graph's `HumanInTheLoopMiddleware` gates
    (`create_agent(middleware=[...])`), by name, with their config.

    The middleware itself would gate them with `interrupt()`, which needs a
    checkpointer: its `interrupt_on` is emptied, so its after-model hook
    never interrupts, and the adapter's ToolNode gate pauses those calls
    with Sectr's own pause and resume instead. Sectr's decisions are
    approve and reject, so an entry that doesn't allow both is rejected
    (`True` allows every decision, and is accepted). `edit` and `respond`
    aren't offered."""
    try:
        from langchain.agents.middleware import HumanInTheLoopMiddleware
    except ImportError:  # langchain isn't installed: no `create_agent`
        return {}
    found: list[HumanInTheLoopMiddleware] = []
    for node in (getattr(graph, "nodes", None) or {}).values():
        bound = getattr(node, "bound", None)
        for fn in (getattr(bound, "afunc", None), getattr(bound, "func", None)):
            owner = getattr(fn, "__self__", None)
            if isinstance(owner, HumanInTheLoopMiddleware) and all(owner is not m for m in found):
                found.append(owner)
    gated: dict[str, Any] = {}
    for middleware in found:
        interrupt_on = getattr(middleware, _INTERRUPT_ON_ATTR, None)
        if interrupt_on is None:
            interrupt_on = dict(middleware.interrupt_on)
            for name, config in interrupt_on.items():
                decisions = config.get("allowed_decisions") or []
                if not {"approve", "reject"} <= set(decisions):
                    raise ValueError(
                        f"langgraph(graph): HumanInTheLoopMiddleware's interrupt_on[{name!r}] "
                        f"has allowed_decisions={decisions!r}, but a Sectr approval is approve "
                        f"or reject: allow both (interrupt_on={{{name!r}: True}} does)."
                    )
            setattr(middleware, _INTERRUPT_ON_ATTR, interrupt_on)
            middleware.interrupt_on = {}
        gated.update(interrupt_on)
    return gated


def _adapter_gate(request: Any, interrupt_on: dict[str, Any]) -> str | None:
    """How the adapter gates this call, if it does: `"code"` for a tool
    `interrupt_on` names (subject to its `when`, if any) or, if it isn't a
    Sectr tool (those gate themselves, per call), one `as_langgraph_tool`
    gated; `"policy"` for one that isn't a Sectr tool and that
    `SECTR_APPROVAL_POLICY` names. A name that isn't one of the node's
    tools is ignored, as the middleware does."""
    tool, name = request.tool, request.tool_call["name"]
    if tool is None:
        return None
    config = interrupt_on.get(name)
    if config is not None:
        when = config.get("when")
        if when is None or when(request):
            return "code"
    if _is_sectr_tool(tool):
        return None
    if _wrapped_gated(tool):
        return "code"
    return "policy" if name in platform_approval_policy() else None


def _check_gate(request: Any, interrupt_on: dict[str, Any]) -> None:
    call = request.tool_call
    if _approved(call["id"]):
        return
    gate = _adapter_gate(request, interrupt_on)
    if gate is not None:
        _gate_call(call["name"], call["args"], call["id"], gate)


def _gate_tool_calls(node: ToolNode, interrupt_on: dict[str, Any]) -> None:
    """Gate the node's calls the adapter gates ([`_adapter_gate`]; the
    policy is read per call, as for Sectr tools): a tool-call wrapper
    (`ToolNode`'s `awrap_tool_call` / `wrap_tool_call`, which `create_agent`
    fills from middleware) that pauses before the call reaches the tool or
    the graph's own wrapper. Tools, their schemas and injected arguments
    are untouched. Idempotent: a graph may be wrapped by several handlers."""
    if getattr(node, "_sectr_policy_gate", False):
        return
    if not (hasattr(node, "_awrap_tool_call") and hasattr(node, "_wrap_tool_call")):
        raise ValueError(
            "langgraph(graph): this langgraph-prebuilt ToolNode has no tool-call wrapper "
            "hooks, so the deployment's approval policy ([deploy] approval_tools) can't be "
            "enforced on its tools; install "
            "langgraph-prebuilt>=1.1.0."
        )
    inner_async, inner_sync = node._awrap_tool_call, node._wrap_tool_call

    if inner_sync is not None:

        def sync_gate(request: Any, execute: Any) -> Any:
            def run() -> Any:
                _check_gate(request, interrupt_on)
                return inner_sync(request, execute)

            state = _current_run()
            return run() if state is None else state.answer_sync(request.tool_call, run)

        node._wrap_tool_call = sync_gate
    if inner_async is not None or inner_sync is None:
        # Without a wrapper of its own, the node calls the async one (or,
        # with only a sync one, that one) in its async path.

        async def async_gate(request: Any, execute: Any) -> Any:
            async def run() -> Any:
                _check_gate(request, interrupt_on)
                if inner_async is not None:
                    return await inner_async(request, execute)
                return await execute(request)

            state = _current_run()
            return await (run() if state is None else state.answer(request.tool_call, run))

        node._awrap_tool_call = async_gate
    node._sectr_policy_gate = True  # type: ignore[attr-defined]


# ----------------------------------------------------------------------
# as_langgraph_tool
# ----------------------------------------------------------------------


def _gated(t: Tool) -> bool:
    return t.needs_approval or t.name in platform_approval_policy()


def _reject_injection(t: Tool, hints: dict[str, Any]) -> None:
    """Graph-injected arguments (state, store, runtime) aren't passed
    through a sectr tool's function yet."""
    from langgraph.prebuilt import ToolRuntime

    for pname, hint in hints.items():
        metadata = typing.get_args(hint)[1:] if typing.get_origin(hint) is Annotated else ()
        base = typing.get_args(hint)[0] if metadata else hint
        injected = any(
            isinstance(m, InjectedToolArg)
            or (inspect.isclass(m) and issubclass(m, InjectedToolArg))
            for m in metadata
        )
        if injected or base is ToolRuntime or typing.get_origin(base) is ToolRuntime:
            raise ValueError(
                f"tool {t.name!r}: parameter {pname!r} is injected by the graph "
                "(InjectedState / InjectedStore / ToolRuntime), which sectr tools don't "
                "support yet; take plain arguments."
            )


class _SectrTool(StructuredTool):
    """A `StructuredTool` that closes its callback run when it pauses.

    `BaseTool.arun` reports failures to callbacks only for `Exception`, so a
    `BaseException` pause would leave the tool run open forever in LangSmith
    and other tracers. The pause is reported as a tool error
    (`ApprovalRequired`), then re-raised unchanged."""

    async def _arun(
        self,
        *args: Any,
        config: RunnableConfig,
        run_manager: AsyncCallbackManagerForToolRun | None = None,
        **kwargs: Any,
    ) -> Any:
        try:
            return await super()._arun(*args, config=config, run_manager=run_manager, **kwargs)
        except ApprovalRequired as exc:
            if run_manager is not None:
                await run_manager.on_tool_error(exc)
            raise


def as_langgraph_tool(t: Tool | BaseTool, *, needs_approval: bool = False) -> BaseTool:
    """Convert a sectr `Tool` into a LangChain `StructuredTool` for a
    `ToolNode`, preserving approval gating; or, given a LangChain tool (any
    `BaseTool`: a plain `@tool`, an MCP tool from `langchain-mcp-adapters`, a
    community tool), gate it.

    A LangChain tool with `needs_approval=True` comes back as a copy marked
    gated, otherwise unchanged (schema, injected arguments, results): the
    adapter's ToolNode gate pauses its calls, as it does for the tools the
    policy names. Without `needs_approval` it's returned as it is.

    The model-facing schema is derived from the function signature
    (required vs defaulted parameters; untyped parameters are `str`). A
    hidden `InjectedToolCallId` argument gives the wrapper the model's call
    id, so a gated call raises `ApprovalRequired(call_id=<that id>)` — it
    always matches the journaled TOOL_CALL. The wrapper raises instead of
    executing while the tool is gated (`needs_approval` or
    `SECTR_APPROVAL_POLICY`, evaluated per call); the adapter bypasses the
    gate only for the pending call after approval. `needs_approval=True`
    gates a sectr `Tool` as its own `needs_approval` does.
    """
    if isinstance(t, BaseTool):
        if not needs_approval:
            return t
        return t.model_copy(update={"metadata": {**(t.metadata or {}), _SECTR_GATED_KEY: True}})
    if t.description is None:
        raise ValueError(f"tool {t.name!r} needs a docstring (used as the LLM description)")

    hints = typing.get_type_hints(t.fn, include_extras=True)
    _reject_injection(t, hints)

    params = inspect.signature(t.fn).parameters
    if "tool_call_id" in params:
        raise ValueError(f"tool {t.name!r}: 'tool_call_id' is reserved by as_langgraph_tool")
    fields: dict[str, Any] = {
        pname: (
            hints.get(pname, str),
            ... if p.default is inspect.Parameter.empty else p.default,
        )
        for pname, p in params.items()
        if p.kind not in (inspect.Parameter.VAR_POSITIONAL, inspect.Parameter.VAR_KEYWORD)
    }
    # Hidden from the model (tool_call_schema), filled in by ToolNode.
    fields["tool_call_id"] = (Annotated[str, InjectedToolCallId], "")
    # pydantic's create_model overloads don't type a heterogeneous
    # field-definition dict; the schema is pinned by golden tests instead.
    args_schema = create_model(f"{t.name}_args", **cast(Any, fields))

    async def _run(*, tool_call_id: str = "", **kwargs: Any) -> Any:
        call_id = tool_call_id or None
        if (needs_approval or _gated(t)) and not _approved(call_id):
            gate = "code" if needs_approval or t.needs_approval else "policy"
            _gate_call(t.name, kwargs, call_id, gate)
        return await t(**kwargs)

    return _SectrTool.from_function(
        coroutine=_run,
        name=t.name,
        description=t.description,
        args_schema=args_schema,
        metadata={_SECTR_TOOL_KEY: t.name},
    )


# ----------------------------------------------------------------------
# transcript → LangChain messages
# ----------------------------------------------------------------------


def _parse_args(arguments: Any) -> dict[str, Any]:
    if isinstance(arguments, str):
        try:
            arguments = json.loads(arguments)
        except json.JSONDecodeError:
            return {}
    return arguments if isinstance(arguments, dict) else {}


def _transcript_messages(ctx: SessionContext) -> list[BaseMessage]:
    """Project the canonical transcript view (`ctx.messages`) into LangChain
    messages. Used by every turn kind:

    - user → `HumanMessage` (dict input stringified — dict content explodes
      provider converters)
    - assistant → one `AIMessage` carrying its text and its `tool_calls`,
      call ids verbatim (the answered `ToolMessage` must validate), and its
      reasoning and `name` from the journaled framework state (see module
      docstring)
    - tool → `ToolMessage`, its content the journaled result: content
      blocks stay blocks (the canonical view holds them as a JSON string,
      which the model would see instead)
    - an unanswered trailing call is preserved as-is; an unanswered call
      followed by more conversation (a tool raised, or a crash) arrives
      already answered by the canonical fold (`INTERRUPTED_TOOL_RESULT`);
      a partial assistant message (crash mid-turn) stays partial — the
      fold already appends the synthesized continuation prompt
    """
    results = {
        e.payload["tool_call_id"]: e.payload["result"]
        for e in ctx.transcript
        if e.type == "TOOL_RESULT"
    }
    out: list[BaseMessage] = []
    for msg in ctx.messages:
        role = msg["role"]
        if role == "user":
            out.append(HumanMessage(content=msg["content"]))
        elif role == "tool":
            result = results.get(msg["tool_call_id"])
            content = result if isinstance(result, list) else msg["content"]
            out.append(ToolMessage(content=content, tool_call_id=msg["tool_call_id"]))
        else:  # assistant
            calls = [
                {
                    "id": call["id"],
                    "name": call["function"]["name"],
                    "args": _parse_args(call["function"]["arguments"]),
                    "type": "tool_call",
                }
                for call in msg.get("tool_calls") or []
            ]
            text = msg.get("content") or ""
            blocks, kwargs, name = _journaled(msg)
            if text or calls or blocks or kwargs:
                out.append(
                    AIMessage(
                        content=[*blocks, *([{"type": "text", "text": text}] if text else [])]
                        if blocks
                        else text,
                        tool_calls=cast(Any, calls),
                        additional_kwargs=kwargs,
                        name=name,
                    )
                )

    # A partial transcript (tests, harnesses) falls back to appending the
    # current input — a turn never runs without its user message.
    if ctx.reason.get("kind") == "user_message":
        raw = ctx.reason["input"]
        if not any(m["role"] == "user" and m["content"] == raw for m in ctx.messages):
            out.append(HumanMessage(content=raw))
    return _in_call_order(out)


def _journaled(
    msg: dict[str, Any],
) -> tuple[list[dict[str, Any]], dict[str, Any], str | None]:
    """The reasoning content blocks and `additional_kwargs` (keys in their
    original order) and the `name` journaled with an assistant message;
    none for a state this adapter didn't write or can't read (they're then
    dropped)."""
    state = journaled_state(msg, _State)
    if state is None:
        return [], {}, None
    try:
        blocks = [parsed(b) for b in state.content]
        kwargs = parsed(state.additional_kwargs) if state.additional_kwargs is not None else {}
    except ValueError as exc:
        unreadable(_State, f"a piece isn't a JSON object: {exc}")
        return [], {}, None
    if any(b.get("type") not in _REASONING_BLOCKS for b in blocks) or not set(kwargs) <= set(
        _REASONING_KWARGS
    ):
        unreadable(_State, "a block or additional_kwargs key that isn't reasoning")
        return [], {}, None
    return blocks, kwargs, state.name


def _message_state(message: AIMessage) -> _State | None:
    """A stored AIMessage's framework state."""
    blocks = []
    if isinstance(message.content, list):
        blocks = [
            b for b in message.content if isinstance(b, dict) and b.get("type") in _REASONING_BLOCKS
        ]
    extra = message.additional_kwargs
    kwargs = {k: extra[k] for k in _REASONING_KWARGS if k in extra}
    try:
        state = _State(
            content=tuple(exact(b) for b in blocks),
            additional_kwargs=exact(kwargs) if kwargs else None,
            name=message.name,
        )
    except (TypeError, ValueError) as exc:  # a block that isn't JSON
        logger.warning("sectr: langgraph reasoning isn't JSON, not journaled: %s", exc)
        return None
    return state


def _updated_messages(update: Any) -> Iterator[BaseMessage]:
    """The messages an `updates` stream item writes, per node."""
    if not isinstance(update, dict):
        return
    for value in update.values():
        written = value.get("messages") if isinstance(value, dict) else None
        if isinstance(written, BaseMessage):
            yield written
        elif isinstance(written, list):
            yield from (m for m in written if isinstance(m, BaseMessage))


def _in_call_order(messages: list[BaseMessage]) -> list[BaseMessage]:
    """`messages` with each AIMessage's ToolMessages in the order of its
    `tool_calls`, as `ToolNode` sends them. The journal holds results in
    completion order, and a resume answers the decided call before its
    siblings; the model gets call order either way."""
    out: list[BaseMessage] = []
    position: dict[str | None, int] = {}
    run: list[ToolMessage] = []
    for m in [*messages, None]:
        if isinstance(m, ToolMessage):
            run.append(m)
            continue
        # Stable: a result without a matching call keeps its place at the end.
        run.sort(key=lambda t: position.get(t.tool_call_id, len(position)))
        out.extend(run)
        run = []
        if m is None:
            break
        if isinstance(m, AIMessage):
            position = {c["id"]: i for i, c in enumerate(m.tool_calls)}
        out.append(m)
    return out


# ----------------------------------------------------------------------
# resume: rebuild the pause, run the graph from its tools node
# ----------------------------------------------------------------------


def _unanswered_calls(messages: list[BaseMessage]) -> list[dict[str, Any]]:
    """Tool calls of the last tool-calling AIMessage that have no ToolMessage."""
    answered = {m.tool_call_id for m in messages if isinstance(m, ToolMessage)}
    for m in reversed(messages):
        if isinstance(m, AIMessage) and m.tool_calls:
            return [dict(c) for c in m.tool_calls if c["id"] not in answered]
    return []


def _result_payload(message: ToolMessage) -> Any:
    """TOOL_RESULT `result`: what the graph stores, `ToolMessage.content`,
    so a later turn sends the model what this one did (rule of 2026-10-12).
    A string as is: `ToolNode`'s `json.dumps` of a dict stays that text
. Content blocks (an MCP tool's, from `langchain-mcp-adapters`)
    stay blocks, without LangChain's random block `id`s; the
    journal re-sorting a block's keys is harmless, as providers read its
    fields."""
    content = message.content
    if isinstance(content, str):
        return content
    return [
        {k: v for k, v in block.items() if k != "id"} if isinstance(block, dict) else block
        for block in content
    ]


async def _resume(
    graph: Any,
    ctx: SessionContext,
    messages: list[BaseMessage],
    tools: dict[str, BaseTool],
    resume_node: str | None,
    runtime_context: Any,
) -> AsyncIterator[RunnerEvent]:
    """Rebuild the pause from the journal and let the graph run the decided
    call from its tools node (see module docstring)."""
    pending = cast(dict[str, Any], ctx.pending_tool_call)
    approved = bool(ctx.decision["approved"])
    if approved and pending["name"] not in tools:
        raise RuntimeError(
            f"approved tool {pending['name']!r} is not in the graph's ToolNode — cannot execute it"
        )
    if resume_node is None:
        raise RuntimeError(
            "the graph's tools node is reached from several nodes, so the approval can't be "
            "resumed. " + _CHECKPOINT_POINTER
        )
    if not any(c["id"] == pending["tool_call_id"] for c in _unanswered_calls(messages)):
        # Journal without the attached call (defensive, like openai-agents):
        # anchor the pending call from its journaled name + arguments.
        call = {
            "id": pending["tool_call_id"],
            "name": pending["name"],
            "args": _parse_args(pending["arguments"]),
            "type": "tool_call",
        }
        messages.append(AIMessage(content="", tool_calls=cast(Any, [call])))

    # The state as the tools node's predecessor left it: ending at the
    # AIMessage with the calls, which the graph's router looks for last.
    # Results journaled since are given back by the gates instead.
    last = max(i for i, m in enumerate(messages) if isinstance(m, AIMessage) and m.tool_calls)
    state = _Run(
        call_id=pending["tool_call_id"],
        approved=approved,
        journaled={m.tool_call_id: m for m in messages[last + 1 :] if isinstance(m, ToolMessage)},
    )
    run = graph.copy(update={"checkpointer": InMemorySaver()})
    config: RunnableConfig = {"configurable": {"thread_id": "resume", _RUN_KEY: state}}
    await run.aupdate_state(config, {"messages": messages[: last + 1]}, as_node=resume_node)
    # Rebuilt from the journal, every call has its id.
    calls = {
        c["id"] or "": (c["name"], c["args"]) for c in cast(AIMessage, messages[last]).tool_calls
    }
    async for event in _stream_graph(
        run, None, tools, config=config, context=runtime_context, state=state, calls=calls
    ):
        yield event


# ----------------------------------------------------------------------
# astream → runner events
# ----------------------------------------------------------------------


def _deltas(message: AIMessage) -> Iterator[tuple[str, str]]:
    """(kind, text) deltas of one AI message/chunk; kind is `text` or
    `reasoning` (the closed delta vocabulary)."""
    content = message.content
    if isinstance(content, str):
        if content:
            yield "text", content
    else:
        for block in content:
            if isinstance(block, str):
                if block:
                    yield "text", block
            elif isinstance(block, dict):
                kind = block.get("type")
                if kind == "text" and block.get("text"):
                    yield "text", block["text"]
                elif kind in ("thinking", "reasoning"):
                    text = block.get("thinking") or block.get("reasoning") or block.get("text")
                    if isinstance(text, str) and text:
                        yield "reasoning", text
                    # The Responses API's reasoning summary (`ChatOpenAI(
                    # use_responses_api=True)`), nested in parts.
                    for part in block.get("summary") or ():
                        text = part.get("text") if isinstance(part, dict) else None
                        if isinstance(text, str) and text:
                            yield "reasoning", text
    reasoning = message.additional_kwargs.get("reasoning_content")
    if isinstance(reasoning, str) and reasoning:
        yield "reasoning", reasoning


class _StreamMapper:
    """Stateful mapping of `stream_mode="messages"` items onto runner events."""

    def __init__(
        self, calls: dict[str, tuple[str, Any]] | None = None, journaled: Iterable[str] = ()
    ) -> None:
        self.open_message: str | None = None
        self._message_in_response = False
        # this response's message ids (LangChain's run id, then the
        # provider's, which the stored message keeps), whether its last
        # chunk came, and its framework state once known: a whole message's
        # at once, a streamed one's from its node's update
        self._ids: set[str] = set()
        self._ended = False
        self._state: _State | None = None
        # this response's tool-call chunks, merged by LangChain's own `+`
        self._tool_chunks: AIMessageChunk | None = None
        self._whole_calls: list[dict[str, Any]] = []
        # every call this run may pause on: call id → (name, arguments).
        # Those it emits as TOOL_CALL, and on a resume the rebuilt ones.
        self.calls: dict[str, tuple[str, Any]] = dict(calls or {})
        # results already in the journal (given back on a resume): not again
        self._journaled = set(journaled)
        self.results: set[str] = set()

    def on_message(self, message: BaseMessage) -> Iterator[RunnerEvent]:
        if isinstance(message, ToolMessage):
            yield from self.flush()
            if message.tool_call_id in self._journaled:
                return
            self.results.add(message.tool_call_id)
            yield ToolResult(tool_call_id=message.tool_call_id, result=_result_payload(message))
            return
        if not isinstance(message, AIMessage):
            return
        if self._ended:
            # The next response, with no node update since the last one's
            # last chunk.
            yield from self.flush()
        if message.id:
            self._ids.add(message.id)
        if not isinstance(message, AIMessageChunk):
            self._state = _message_state(message)

        for kind, text in _deltas(message):
            if self.open_message is None:
                self.open_message = uuid.uuid4().hex[:16]
                self._message_in_response = True
                yield MessageStart(role="assistant", message_id=self.open_message)
            yield MessageDelta(message_id=self.open_message, delta={"type": kind, "text": text})

        if isinstance(message, AIMessageChunk) and message.tool_call_chunks:
            # LangChain decides which fragments form one call: same `index`
            # merges, while index-less chunks (providers that send calls
            # whole) stay separate calls, exactly as the graph's own
            # aggregated message sees them.
            part = AIMessageChunk(content="", tool_call_chunks=message.tool_call_chunks)
            self._tool_chunks = part if self._tool_chunks is None else self._tool_chunks + part
        elif message.tool_calls:
            # a complete (non-streamed) message: calls arrive whole
            self._whole_calls.extend(dict(c) for c in message.tool_calls)

        if not isinstance(message, AIMessageChunk):
            yield from self.flush()
        elif message.chunk_position == "last":
            # Closed by its node's update, which holds the message as the
            # graph stores it, for its framework state.
            self._ended = True

    def flush(self, update: Any = None) -> Iterator[RunnerEvent]:
        """End of one model response: close the message, with its framework
        state (from `update`, a node update holding the stored message, for
        a streamed one), then journal its tool calls. Idempotent; also
        called at every tool message, the next response, and stream end."""
        calls: list[tuple[str, str, Any]] = []
        for chunk in self._tool_chunks.tool_call_chunks if self._tool_chunks else []:
            raw = chunk.get("args") or ""
            try:
                args: Any = json.loads(raw) if raw else {}
            except json.JSONDecodeError:
                args = raw
            calls.append((chunk.get("id") or uuid.uuid4().hex[:16], chunk.get("name") or "", args))
        calls.extend((c["id"], c["name"], c["args"]) for c in self._whole_calls)
        self._tool_chunks = None
        self._whole_calls.clear()
        state = self._state
        if state is None and self._ids:
            stored = next(
                (
                    m
                    for m in _updated_messages(update)
                    if isinstance(m, AIMessage) and m.id in self._ids
                ),
                None,
            )
            state = _message_state(stored) if stored is not None else None
        self._ids = set()
        self._ended = False
        self._state = None

        reasoning = state is not None and state.has_reasoning()
        journaled = framework_state(state) if state is not None else None
        if self.open_message is not None:
            yield MessageEnd(message_id=self.open_message, framework_state=journaled)
            self.open_message = None
        elif (calls or reasoning) and not self._message_in_response:
            # A tool-only response still gets a (empty) message so the
            # transcript fold attaches its calls to THIS response and not to
            # an earlier assistant message; so does reasoning with no text.
            # (A name alone is no content: an empty reply stays empty.)
            message_id = uuid.uuid4().hex[:16]
            yield MessageStart(role="assistant", message_id=message_id)
            yield MessageEnd(message_id=message_id, framework_state=journaled)
        for call_id, name, args in calls:
            self.calls[call_id] = (name, args)
            yield ToolCall(tool_call_id=call_id, name=name, arguments=args)
        self._message_in_response = False


def _interrupt_error(interrupts: Iterable[Any]) -> Error:
    """The turn's error for an `interrupt()`. The adapter empties the
    `interrupt_on` of every `HumanInTheLoopMiddleware` in the graph, so one
    that still interrupts belongs to an agent the graph runs inside a tool:
    a subagent, whose gated tool fails with `subagent_gated_tool`. Any
    other `interrupt()` is native, and unsupported."""
    for interrupt in interrupts:
        value = getattr(interrupt, "value", None)
        requests = value.get("action_requests") if isinstance(value, dict) else None
        if requests:
            name = requests[0].get("name") if isinstance(requests[0], dict) else None
            return Error(
                message=_subagent_gated_message(_GatedInSubagent(str(name), None, "code")),
                code=SUBAGENT_GATED_TOOL,
            )
    return Error(message=_NATIVE_INTERRUPT_MESSAGE, code="langgraph_interrupt")


async def _stream_graph(
    graph: Any,
    input: Any,  # noqa: A002 — astream's own name
    tools: dict[str, BaseTool],
    *,
    config: RunnableConfig | None = None,
    context: Any = None,
    state: _Run | None = None,
    calls: dict[str, tuple[str, Any]] | None = None,
) -> AsyncIterator[RunnerEvent]:
    if state is None:  # a turn: the gates still coordinate its tool calls
        state = _Run()
        config = {"configurable": {_RUN_KEY: state}}
    mapper = _StreamMapper(calls, state.journaled)
    stream = graph.astream(input, config, context=context, stream_mode=["messages", "updates"])
    try:
        async for mode, data in stream:
            if mode == "messages":
                message, _metadata = data
                for event in mapper.on_message(message):
                    yield event
                continue
            # mode == "updates"
            for event in mapper.flush(data):
                yield event
            if "__interrupt__" in data:
                # Pregel swallows GraphInterrupt into this update; without
                # the guard the turn would end as if complete.
                yield _interrupt_error(data["__interrupt__"])
                return
    except _GatedInSubagent as exc:
        for event in mapper.flush():
            yield event
        yield Error(message=_subagent_gated_message(exc), code=SUBAGENT_GATED_TOOL)
        return
    except ApprovalRequired as exc:
        for event in mapper.flush():
            yield event
        emitted = mapper.calls.get(exc.call_id) if exc.call_id else None
        if emitted is None:
            yield Error(
                message=(
                    f"tool {exc.name!r} requested approval outside a model tool call "
                    "(no matching call id) — only tools converted with as_langgraph_tool "
                    "and run by a ToolNode can pause a turn. " + _CHECKPOINT_POINTER
                ),
                code="langgraph_unmatched_approval",
            )
            return
        if exc.name not in tools:
            # e.g. a hand-rolled tool node calling a gated tool itself, or a
            # policy-gated tool in a second or nested ToolNode: the pause
            # would work, but resume only executes the one top-level
            # ToolNode's tools, so fail now — before a human is asked to
            # approve.
            yield Error(
                message=(
                    f"gated tool {exc.name!r} ran outside the graph's top-level ToolNode (a "
                    "hand-rolled tool node, or a subgraph's?) — the adapter could not execute "
                    "it after approval. Put gated tools in the graph's single top-level "
                    "langgraph.prebuilt.ToolNode. " + _CHECKPOINT_POINTER
                ),
                code="langgraph_unmatched_approval",
            )
            return
        name, arguments = emitted
        for call_id, result in state.results.items():
            if call_id not in mapper.results:
                # It finished, but this pause aborted the node before its
                # result streamed.
                yield ToolResult(tool_call_id=call_id, result=_result_payload(result))
        yield ToolApprovalRequired(
            tool_call_id=cast(str, exc.call_id), name=name, arguments=arguments, reason=exc.reason
        )
        return
    except RuntimeError as exc:
        if "without checkpointer" not in str(exc):
            raise
        for event in mapper.flush():
            yield event
        yield Error(message=_NO_CHECKPOINTER_MESSAGE, code="langgraph_resume")
        return
    finally:
        await stream.aclose()

    for event in mapper.flush():
        yield event
