"""openai-agents adapter (`sectr[openai-agents]` extra).

`openai_agents(agent, *, model=None, run_config=None, context=None)` returns a handler
mapping the SDK's native streaming + HITL onto the platform contract:

- normal turn: transcript → input items; `Runner.run_streamed`; text deltas
  → MESSAGE_DELTA; `tool_called` → TOOL_CALL; `tool_output` → TOOL_RESULT;
  exhaustion → NO_MORE_ACTIONS (appended by `drive`). Each model response
  is one MESSAGE_START/END bracket, opened lazily on its first text or
  reasoning delta and closed at its `response.completed`, followed by its
  TOOL_CALLs; a tool-only response gets an empty bracket (the SDK streams a
  response's run items after its `response.completed`).
- pause: the framework's native interruption path — approval-gated tools
  (`needs_approval` ∪ platform `SECTR_APPROVAL_POLICY`) produce
  `result.interruptions` after the stream drains. Verified against SDK 0.22:
  the gated call DOES stream a `tool_called` item before the interruption
  (only the `ToolApprovalItem` itself is suppressed from the item stream),
  so TOOL_CALL is journaled from the stream; the interruption path journals
  it too only if the stream never emitted it (defensive against SDK
  changes), then TOOL_APPROVAL_REQUIRED as the final frame (turn suspended).
  Several interruptions in one turn become sequential approvals (the
  contract has one decision per resume; batched approvals aren't
  supported yet): the turn pauses on the first, and each resume lists the
  response's unanswered calls, so the SDK pauses on the next by itself.
- resume (`reason.kind == "approval_decision"`): the SDK's native resume.
  The runner is stateless and the journal stores no SDK state, so the
  paused `RunState` is rebuilt from the journal (the conversation and the
  pending call), the decision applied (`approve` / `reject`), and
  `Runner.run_streamed(agent, state)` runs or rejects the call itself —
  with the run's context, config, hooks, guardrails and tracing, as a call
  that needs no approval — then continues the run. Its TOOL_RESULT streams
  like any other.
- handoffs: the conversation stays with the agent it was handed off to.
  The transfer call and its output are journaled as TOOL_CALL/TOOL_RESULT,
  as the SDK stores them, and each turn (an approval resume's included)
  runs the agent the journaled transfers lead to (`_current_agent`). A
  handoff that rewrites the next agent's history (`input_filter`,
  `nest_handoff_history`) or hides its agent (a `Handoff` built by hand) is
  rejected at construction: a later turn couldn't replay what the agent
  saw, or find it.
- subagents (`as_tool()` agents and handoff agents, recursively) follow the
  beta subagent contract: a tool gated in code inside one is rejected at
  construction with `SUBAGENT_GATED_TOOL`; one `SECTR_APPROVAL_POLICY`
  names (known only once the runner starts) ends each turn in that `ERROR`
  instead, before the model runs.
- reasoning: a model response's reasoning items, as the SDK keeps them
  in its history (`to_input_list()`: encrypted content, provider data), are
  journaled on its MESSAGE_END as framework state, and put back in front of
  the response's other items when the input is rebuilt: later requests send
  them back as the SDK does natively. Their text is also streamed as
  `reasoning` deltas.
- MCP servers (`mcp_servers`, on the agent and on its subagents) are
  connected for each invocation and cleaned up after it; invocations
  running at once share one connection per server. The agent's MCP tools are
  listed once per invocation into its tools, where `SECTR_APPROVAL_POLICY`
  gates them and an approval resume finds them.
- `context` (a factory, like Pydantic AI's `deps`) is the SDK's run
  context: `Runner.run_streamed(context=…)` on every run, an approval
  resume's included, and the MCP tool listing's `RunContextWrapper`.
- tracing is disabled by default (dev runs have no OpenAI tracing sink);
  pass your own `run_config` to override any of this.

The sectr tool registry (`sectr.tools`) is NOT involved — openai-agents has
native HITL, so tools stay 100% framework code (zero sectr imports in the
user's tool module).
"""

from __future__ import annotations

import asyncio
import inspect
import json
import logging
import uuid
import weakref
from collections.abc import AsyncIterator, Awaitable, Callable
from contextlib import AsyncExitStack, asynccontextmanager
from dataclasses import replace
from typing import Any, cast

from agents import Agent as OAgent  # noqa: N811 — disambiguate from sectr
from agents import Model, RunConfig, RunContextWrapper, Runner, RunState
from agents.handoffs import Handoff
from agents.items import (
    HandoffCallItem,
    HandoffOutputItem,
    MessageOutputItem,
    ToolApprovalItem,
    ToolCallItem,
    ToolCallOutputItem,
)
from agents.mcp import MCPServer
from agents.models.fake_id import FAKE_RESPONSES_ID
from agents.run import DEFAULT_MAX_TURNS
from agents.stream_events import RawResponsesStreamEvent, RunItemStreamEvent
from agents.tool import FunctionTool

from ..app import Handler
from ..context import SessionContext
from ..events import (
    Error,
    MessageDelta,
    MessageEnd,
    MessageStart,
    RunnerEvent,
    ToolCall,
    ToolApprovalRequired,
    ToolResult,
)
from ..tools import platform_approval_policy
from . import SUBAGENT_GATED_TOOL
from ._framework_state import (
    FrameworkData,
    exact,
    framework_state,
    journaled_state,
    parsed,
    unreadable,
)

__all__ = ["openai_agents"]

logger = logging.getLogger(__name__)



class _State(FrameworkData):
    """A model response's framework state: its reasoning items, which its
    `reasoning` deltas carry only the text of. They go back to the SDK as
    input items, which it forwards to the provider as they are, so each is
    kept as its exact JSON (the journal would re-sort an object's keys)."""

    adapter = "openai_agents"
    version = 1

    items: tuple[str, ...]
    """The reasoning items, in order, each as `ReasoningItem.to_input_item()`
    holds it, as its exact JSON."""

# The `RunState` schema version an approval resume's rebuilt snapshot is
# written in ([`_resumed_state`]): the fields it fills mean the same in
# every SDK that reads this version, and the tests check the installed SDK
# still reads it.
_RUN_STATE_SCHEMA = "1.16"

# The error code a turn fails with when the journal hands off through a
# handoff the agent no longer has (`_current_agent`).
UNKNOWN_HANDOFF = "openai_agents_unknown_handoff"


def openai_agents(
    agent: OAgent,
    *,
    model: Model | None = None,
    run_config: RunConfig | None = None,
    context: Callable[[SessionContext], Any | Awaitable[Any]] | None = None,
) -> Handler:
    """Handler factory wrapping an `agents.Agent`.

    `model` / `run_config` pass through to `Runner.run_streamed` — used for
    deterministic fakes in CI (`SECTR_FAKE=1` in the examples) and power-user
    config. Tracing is disabled unless the caller supplies a run_config.

    `context` is a factory called once per invocation with the
    `SessionContext` (a static value is `lambda ctx: v`); its value is the
    SDK's run context (`Runner.run_streamed(context=…)`), which tools,
    hooks, guardrails, dynamic instructions and MCP tool filters read as
    `RunContextWrapper.context`.

    Raises `ValueError` for structured output (an `output_type` on the agent
    or on an agent it hands off to), for a handoff Sectr can't continue a
    conversation through (`_reject_unsupported_handoffs`), and with
    `SUBAGENT_GATED_TOOL` in the message for a tool gated in code inside a
    subagent (`_reject_gated_subagents`).
    """
    _reject_structured_output(agent)
    _reject_unsupported_handoffs(agent, run_config)
    subagents = _subagents(agent)
    _reject_gated_subagents(agent, subagents)

    async def handler(ctx: SessionContext) -> AsyncIterator[RunnerEvent]:
        run_context = await _context(ctx, context)
        async with _connected(agent, subagents):
            gated = await _policy_named_subagent_tool(subagents, run_context)
            if gated is not None:
                tool, owner = gated
                yield Error(message=_subagent_gated_message(agent, owner, tool, "policy"), code=SUBAGENT_GATED_TOOL)
                return
            try:
                current = _current_agent(agent, ctx)
            except _UnknownHandoff as exc:
                yield Error(message=str(exc), code=UNKNOWN_HANDOFF)
                return
            run_agents = await _run_agents(agent, run_context)
            async for event in _turn(ctx, run_agents[id(current)], run_context):
                yield event

    async def _turn(
        ctx: SessionContext, run_agent: OAgent, run_context: Any
    ) -> AsyncIterator[RunnerEvent]:
        config = run_config or RunConfig(model=model, tracing_disabled=True)

        input_items: Any
        # call_ids already TOOL_CALL-journaled. On a resume, the paused
        # response's: a call the SDK pauses on again (the next of several
        # gated calls) isn't streamed again, and the interruption path below
        # must not journal it a second time either.
        journaled_calls: set[str] = set()
        if ctx.reason.get("kind") == "approval_decision" and ctx.pending_tool_call is not None:
            input_items, journaled_calls = await _resumed_state(ctx, run_agent, run_context)
        else:
            # EVERY turn continues the conversation: rebuild input items
            # from the canonical transcript (which already contains this
            # invocation's user message via its INVOCATION_STARTED).
            # Feeding only `ctx.input` here ran every user turn as a COLD
            # conversation — the model never saw prior turns (found live
            # in the M1 run: turn 2 forgot the order ID turn 1 had just
            # looked up; scripted fake-model tests used self-contained
            # turns and empty transcripts, so it was invisible).
            input_items = _transcript_items(ctx)

        result = Runner.run_streamed(
            run_agent, cast(Any, input_items), run_config=config, context=run_context
        )

        open_message: str | None = None  # synthesized id of the streaming message
        bracketed = False  # the latest model response has its message bracket
        async for event in result.stream_events():
            if isinstance(event, RawResponsesStreamEvent):
                raw = event.data  # an OpenAI response stream event (0.22: `data`)
                kind = {
                    "response.output_text.delta": "text",
                    "response.reasoning_summary_text.delta": "reasoning",
                    "response.reasoning_text.delta": "reasoning",
                }.get(raw.type)
                if kind is not None:
                    text = cast(Any, raw).delta  # a delta event, by `kind`
                    if not text:
                        # Chat Completions and LiteLLM streams carry empty
                        # deltas (one per tool-call chunk): nothing to
                        # journal, and no message to open.
                        continue
                    if open_message is None:
                        open_message = uuid.uuid4().hex[:16]
                        bracketed = True
                        yield MessageStart(role="assistant", message_id=open_message)
                    yield MessageDelta(
                        message_id=open_message, delta={"type": kind, "text": text}
                    )
                elif raw.type == "response.created":
                    bracketed = False
                elif raw.type == "response.completed":
                    state = _response_state(cast(Any, raw).response)
                    if open_message is None and state is not None:
                        # Reasoning with no text: its message still holds it.
                        open_message = uuid.uuid4().hex[:16]
                        bracketed = True
                        yield MessageStart(role="assistant", message_id=open_message)
                    if open_message is not None:
                        yield MessageEnd(message_id=open_message, framework_state=state)
                        open_message = None
                continue
            if isinstance(event, RunItemStreamEvent):
                if isinstance(event.item, MessageOutputItem):
                    if open_message is not None:
                        yield MessageEnd(message_id=open_message)
                        open_message = None
                elif isinstance(event.item, (ToolCallItem, HandoffCallItem)):
                    # A handoff's transfer call is journaled like a tool's:
                    # the SDK keeps it, and its output, in the history.
                    call: Any = event.item.raw_item
                    call_id = _call_id(call)
                    if call_id in journaled_calls:
                        continue
                    journaled_calls.add(call_id)
                    if not bracketed:
                        for bracket in _empty_message():
                            yield bracket
                        bracketed = True
                    yield ToolCall(
                        tool_call_id=call_id,
                        name=_name(call),
                        arguments=_loads(_arguments(call)),
                    )
                elif isinstance(event.item, (ToolCallOutputItem, HandoffOutputItem)):
                    # What the SDK stores and sends the model: its
                    # conversion of the return value (`str()`, or text parts
                    # for structured output), not the value itself.
                    yield ToolResult(
                        tool_call_id=_call_id(event.item.raw_item),
                        result=_output(event.item.raw_item),
                    )

        if not result.interruptions:
            return  # drive appends NO_MORE_ACTIONS

        # The contract suspends a turn on one decision: several gated calls
        # become sequential approvals. The turn pauses on the first; the
        # others are journaled as TOOL_CALLs, unanswered, and the resume
        # lists them in the rebuilt state, so the SDK pauses on the next
        # after running (or rejecting) the decided one.
        interruption: ToolApprovalItem = result.interruptions[0]
        call = interruption.raw_item  # typed `Any` at the ToolCallItem branch above
        call_id, name = _call_id(call), _name(call)
        if call_id not in journaled_calls:
            # Gated calls are normally journaled via the streamed tool_called
            # item; journal here too in case an SDK change stops emitting it —
            # name + arguments must suffice for a resumed runner (contract).
            if not bracketed:
                for bracket in _empty_message():
                    yield bracket
            yield ToolCall(tool_call_id=call_id, name=name, arguments=_loads(_arguments(call)))
        yield ToolApprovalRequired(
            tool_call_id=call_id,
            name=name,
            arguments=_loads(_arguments(call)),
        )

    # Framework marker — `sectr inspect` surfaces this in the manifest and
    # the dev gateway reports it via /dev/status (DevStatus.framework).
    handler.sectr_framework = "openai-agents"  # type: ignore[attr-defined]
    return handler


async def _context(
    ctx: SessionContext, factory: Callable[[SessionContext], Any | Awaitable[Any]] | None
) -> Any:
    if factory is None:
        return None
    value = factory(ctx)
    return await value if inspect.isawaitable(value) else value


def _empty_message() -> list[RunnerEvent]:
    """The bracket of a tool-only model response: the fold attaches the
    response's calls to it, not to an earlier assistant message."""
    message_id = uuid.uuid4().hex[:16]
    return [MessageStart(role="assistant", message_id=message_id), MessageEnd(message_id=message_id)]


def _reject_structured_output(agent: OAgent, seen: set[int] | None = None) -> None:
    """Structured output isn't supported yet: fail at construction, before
    the first turn. Handoffs given as `Agent`s are checked too (a `Handoff`
    object builds its agent at run time, so it can't be)."""
    seen = seen if seen is not None else set()
    if id(agent) in seen:
        return
    seen.add(id(agent))
    if agent.output_type not in (None, str):
        name = getattr(agent.output_type, "__name__", repr(agent.output_type))
        raise ValueError(
            f"openai_agents(agent): agent {agent.name!r} has output_type={name}, but "
            "structured output is not supported on Sectr yet. Remove output_type: the "
            "agent's answer is journaled and shown as text."
        )
    for handoff in agent.handoffs:
        if isinstance(handoff, OAgent):
            _reject_structured_output(handoff, seen)


# ----------------------------------------------------------------------
# Handoffs
# ----------------------------------------------------------------------


def _handoffs(agent: OAgent) -> list[tuple[str, OAgent | None]]:
    """`agent`'s handoffs: each one's tool name, and the agent it hands off
    to (None for a `Handoff` built by hand, which hides it: only
    `handoff()` keeps a reference)."""
    found: list[tuple[str, OAgent | None]] = []
    for item in agent.handoffs:
        if isinstance(item, OAgent):
            found.append((Handoff.default_tool_name(item), item))
        else:
            target = item._agent_ref() if item._agent_ref is not None else None
            found.append((item.tool_name, target if isinstance(target, OAgent) else None))
    return found


def _conversation_agents(agent: OAgent) -> list[OAgent]:
    """The agents a conversation can be with: `agent`, and those its
    handoffs reach, through handoffs alone (an `as_tool()` agent's
    handoffs run inside its own, opaque, run)."""
    found = [agent]
    for owner in found:
        for _, target in _handoffs(owner):
            if target is not None and all(target is not a for a in found):
                found.append(target)
    return found


def _reject_unsupported_handoffs(agent: OAgent, run_config: RunConfig | None) -> None:
    """A handoff Sectr can't continue a conversation through fails at
    construction:

    - one that rewrites the history the next agent sees (`input_filter`,
      `nest_handoff_history`, on the handoff or in the run config): the
      journal holds the conversation as it was, so a later turn couldn't
      replay what that agent saw;
    - a `Handoff` built by hand: it hides its agent, so a later turn
      couldn't find the agent the conversation is with (only calling it
      would tell, and that runs the user's `on_handoff`)."""
    any_handoffs = False
    for owner in _conversation_agents(agent):
        for item in owner.handoffs:
            any_handoffs = True
            if isinstance(item, OAgent):
                continue
            where = f"handoff {item.tool_name!r} on agent {owner.name!r}"
            if item._agent_ref is None:
                raise ValueError(
                    f"openai_agents(agent): {where} is a Handoff built by hand, so Sectr can't "
                    "tell which agent it hands off to, and the next turn has to continue with "
                    "that agent. Build it with handoff() instead."
                )
            if item.input_filter is not None or item.nest_handoff_history:
                setting = "input_filter" if item.input_filter is not None else "nest_handoff_history"
                raise ValueError(
                    f"openai_agents(agent): {where} sets {setting}, which rewrites the history "
                    "the next agent sees, and that isn't supported on Sectr yet: the next turn "
                    "replays the conversation as it was. Remove it."
                )
    if any_handoffs and run_config is not None:
        if run_config.handoff_input_filter is not None or run_config.nest_handoff_history:
            setting = (
                "handoff_input_filter"
                if run_config.handoff_input_filter is not None
                else "nest_handoff_history"
            )
            raise ValueError(
                f"openai_agents(agent): run_config sets {setting}, which rewrites the history "
                "an agent handed off to sees, and that isn't supported on Sectr yet: the next "
                "turn replays the conversation as it was. Remove it."
            )


class _UnknownHandoff(Exception):
    pass


def _current_agent(agent: OAgent, ctx: SessionContext) -> OAgent:
    """The agent the conversation is with: `agent`, then the target of each
    journaled handoff in turn. A handoff is a call to one of the current
    agent's handoff tools whose result is the SDK's transfer message
    (`Handoff.get_transfer_message`); other calls to them in the same
    response got "Multiple handoffs detected" instead, and changed nothing.

    A transfer the agent no longer has (a redeploy removed the handoff)
    raises `_UnknownHandoff`: continuing with another agent would answer
    from the wrong instructions and tools."""
    names: dict[str, str] = {}
    current = agent
    for event in ctx.transcript:
        if event.type == "TOOL_CALL":
            names[event.payload["tool_call_id"]] = event.payload["name"]
        elif event.type == "TOOL_RESULT":
            name = names.get(event.payload["tool_call_id"], "")
            handed_to = _transfer_target(event.payload.get("result"))
            if handed_to is None:
                continue
            target = dict(_handoffs(current)).get(name)
            if target is not None:
                current = target
            elif not _may_be_a_tool(current, name):
                raise _UnknownHandoff(
                    f"the conversation was handed off to agent {handed_to!r} with {name!r}, "
                    f"but agent {current.name!r} no longer has that handoff (was it removed "
                    "since?), so Sectr can't tell which agent the conversation is with. "
                    "Restore the handoff, or start a new session."
                )
    return current


def _transfer_target(result: Any) -> str | None:
    """The agent a handoff's output names (`{"assistant": name}`), or None
    for any other result."""
    if not isinstance(result, str):
        return None
    try:
        value = json.loads(result)
    except json.JSONDecodeError:
        return None
    if isinstance(value, dict) and list(value) == ["assistant"] and isinstance(value["assistant"], str):
        return value["assistant"]
    return None


def _may_be_a_tool(agent: OAgent, name: str) -> bool:
    """Whether a call to `name` may be a tool call of `agent` (whose result
    only looks like a transfer): one of its tools, or any name when its MCP
    servers' tools aren't known here."""
    return bool(agent.mcp_servers) or any(getattr(t, "name", None) == name for t in agent.tools)


async def _run_agents(agent: OAgent, run_context: Any) -> dict[int, OAgent]:
    """The conversation's agents (`_conversation_agents`) as this invocation
    runs them, by the original's `id`: `agent` as `_run_agent` prepares it,
    and each agent a clone whose handoffs lead to the others' clones. A
    handoff back to `agent` then runs it prepared too (its MCP tools
    listed, the policy applied), not as written."""
    run = await _run_agent(agent, run_context)
    if not agent.handoffs:
        return {id(agent): run}
    originals = _conversation_agents(agent)
    runs = {id(a): a.clone() for a in originals[1:]}
    runs[id(agent)] = run if run is not agent else agent.clone()
    for original in originals:
        runs[id(original)].handoffs = [_retargeted(item, runs) for item in original.handoffs]
    return runs


def _retargeted(item: OAgent | Handoff[Any, Any], runs: dict[int, OAgent]) -> Any:
    """A handoff leading to its target's clone in `runs`. A `handoff()`
    object still runs its `on_handoff` first."""
    if isinstance(item, OAgent):
        return runs[id(item)]
    target = runs[id(item._agent_ref())] if item._agent_ref is not None else None
    if target is None:  # unreachable: rejected at construction
        return item
    invoke = item.on_invoke_handoff

    async def on_invoke_handoff(ctx: RunContextWrapper[Any], arguments: str) -> OAgent:
        await invoke(ctx, arguments)
        return target

    retargeted = replace(item, on_invoke_handoff=on_invoke_handoff)
    retargeted._agent_ref = weakref.ref(target)
    return retargeted


# ----------------------------------------------------------------------
# Subagents
# ----------------------------------------------------------------------


def _subagents(agent: OAgent) -> list[OAgent]:
    """Every agent `agent` reaches, recursively: its `as_tool()` agents and
    the agents it hands off to (as `Agent`s or `handoff()` objects), and
    theirs. `agent` itself only when it runs inside an `as_tool()` agent's
    run: a handoff back to it, in its own run, continues the conversation
    with it, and its gated tools pause and resume as usual. A `Handoff`
    built by hand hides its agent, as does a `Runner.run` inside a tool."""
    found: list[OAgent] = []
    seen: set[int] = set()

    def visit(parent: OAgent, nested: bool) -> None:
        for child, as_tool in _reached(parent):
            inside = nested or as_tool
            if id(child) in seen or (child is agent and not inside):
                continue
            seen.add(id(child))
            found.append(child)
            visit(child, inside)

    visit(agent, nested=False)
    return found


def _reached(agent: OAgent) -> list[tuple[OAgent, bool]]:
    """The agents `agent` runs directly, each with whether it runs as a
    tool: `as_tool()` agents (the SDK keeps the agent on the tool), then
    handoffs (a `handoff()` object keeps a weak reference to it)."""
    reached = [
        (tool._agent_instance, True)
        for tool in agent.tools
        if isinstance(tool, FunctionTool) and isinstance(tool._agent_instance, OAgent)
    ]
    reached += [(target, False) for _, target in _handoffs(agent) if target is not None]
    return reached


def _reject_gated_subagents(agent: OAgent, subagents: list[OAgent]) -> None:
    """A gated tool inside a subagent isn't supported: the subagent's run is
    opaque, and an approval can't resume inside it (a handoff's resume
    looks the tool up on the starting agent). A tool gated in code
    (`needs_approval`, a callable included) or an MCP server that can
    require approval fails here, at construction, which `sectr dev` and a
    deploy's build both run. The deployment's policy reaches the runner
    only when it starts, and where a startup failure's reason isn't shown:
    a tool it names fails the turn instead (`_policy_named_subagent_tool`)."""
    for owner in subagents:
        for tool in owner.tools:
            if getattr(tool, "needs_approval", False) is not False:
                raise ValueError(
                    "openai_agents(agent): "
                    + _subagent_gated_message(agent, owner, getattr(tool, "name", None), "code")
                )
        for server in owner.mcp_servers:
            if _may_require_approval(server):
                raise ValueError(
                    "openai_agents(agent): " + _subagent_gated_message(agent, owner, None, server)
                )


def _may_require_approval(server: MCPServer) -> bool:
    """`require_approval`, as the SDK normalizes it: a bool, a per-tool
    map, or a callable deciding per call."""
    policy = server._needs_approval_policy
    if isinstance(policy, dict):
        return any(policy.values())
    return callable(policy) or policy is True


async def _policy_named_subagent_tool(
    subagents: list[OAgent], run_context: Any
) -> tuple[str, OAgent] | None:
    """The first tool of a subagent that `SECTR_APPROVAL_POLICY` names, and
    its agent: a tool on it, or one its MCP servers serve, listed as the
    SDK lists them for the subagent's run (they're connected for the
    invocation, `_connected`)."""
    policy = set(platform_approval_policy())
    if not policy:
        return None
    for owner in subagents:
        tools = [t.name for t in owner.tools if hasattr(t, "name")]
        if owner.mcp_servers:
            tools += [t.name for t in await owner.get_mcp_tools(RunContextWrapper(context=run_context))]
        for name in tools:
            if name in policy:
                return name, owner
    return None


def _subagent_gated_message(
    agent: OAgent, owner: OAgent, tool: str | None, gate: str | MCPServer
) -> str:
    if isinstance(gate, MCPServer):
        what = f"MCP server {gate.name!r} on agent {owner.name!r} can require approval (require_approval)"
    elif gate == "code":
        what = f"tool {tool!r} on agent {owner.name!r} needs approval (needs_approval)"
    else:
        what = (
            f"tool {tool!r} on agent {owner.name!r} is in the deployment's approval policy "
            "([deploy] approval_tools)"
        )
    return (
        f"{what}, but {owner.name!r} is a subagent of {agent.name!r} "
        f"(an as_tool() agent or a handoff), and a gated tool inside a subagent is not "
        f"supported on Sectr yet ({SUBAGENT_GATED_TOOL}). Gate the as_tool() tool that runs "
        f"the subagent instead, or move the tool to {agent.name!r}."
    )


# ----------------------------------------------------------------------
# MCP servers
# ----------------------------------------------------------------------


class _Connection:
    """An MCP server's session, shared by the invocations using it at once.
    One task owns it, connecting and cleaning up, as the MCP client's task
    groups require; it closes when the last of those invocations ends."""

    def __init__(self, server: MCPServer, after: asyncio.Task[None] | None) -> None:
        self.server = server
        self.users = 0
        self.closing = False
        self.loop = asyncio.get_running_loop()
        self.ready: asyncio.Future[None] = self.loop.create_future()
        self._release = asyncio.Event()
        self.task = asyncio.create_task(self._own(after))

    async def _own(self, after: asyncio.Task[None] | None) -> None:
        if after is not None:
            # The previous session's cleanup and this connect share the
            # server object's state.
            await asyncio.wait([after])
        try:
            await self.server.connect()
        except asyncio.CancelledError:
            self.ready.cancel()
            raise
        except Exception as exc:  # noqa: BLE001 — every user gets it
            self.ready.set_exception(exc)
            return
        self.ready.set_result(None)
        try:
            await self._release.wait()
        finally:
            try:
                await self.server.cleanup()
            except Exception:  # noqa: BLE001 — the turn is over; don't fail it
                logger.warning("cleaning up MCP server %r failed", self.server.name, exc_info=True)

    @property
    def failed(self) -> bool:
        return self.ready.done() and (self.ready.cancelled() or self.ready.exception() is not None)

    def release(self) -> None:
        self.closing = True
        self._release.set()


# Per server (by identity): its current or last connection.
_connections: dict[int, _Connection] = {}


@asynccontextmanager
async def _shared(server: MCPServer) -> AsyncIterator[None]:
    """`server` connected for the block, sharing the connection other
    invocations hold open."""
    previous = _connections.get(id(server))
    if previous is not None and previous.loop is not asyncio.get_running_loop():
        previous = None  # an earlier event loop's (tests)
    conn = previous
    if conn is None or conn.closing or conn.failed:
        conn = _Connection(server, after=previous.task if previous is not None else None)
        _connections[id(server)] = conn
    conn.users += 1
    try:
        await asyncio.shield(conn.ready)
        yield
    finally:
        conn.users -= 1
        if conn.users == 0:
            conn.release()
            await asyncio.shield(conn.task)


@asynccontextmanager
async def _connected(agent: OAgent, subagents: list[OAgent]) -> AsyncIterator[None]:
    """The MCP servers of `agent` and of its subagents (`_subagents`: the
    agents it hands off to, and its `as_tool()` agents, whose runs the SDK
    doesn't connect either), each connected once for the block."""
    servers: list[MCPServer] = []
    for owner in [agent, *subagents]:
        servers += [s for s in owner.mcp_servers if all(s is not t for t in servers)]
    async with AsyncExitStack() as stack:
        for server in servers:
            await stack.enter_async_context(_shared(server))
        yield


async def _run_agent(agent: OAgent, run_context: Any) -> OAgent:
    """The agent as this invocation runs it: its MCP servers' tools listed
    into its tools, first, as the SDK orders them, so the platform policy
    can gate them and an approval resume can run them; then the policy
    applied. The SDK lists them again for each model request; listing once
    per invocation is the difference, for a server whose tools change
    mid-turn."""
    if agent.mcp_servers:
        mcp_tools = await agent.get_mcp_tools(RunContextWrapper(context=run_context))
        agent = agent.clone(tools=[*mcp_tools, *agent.tools], mcp_servers=[])
    return _apply_platform_policy(agent)


# ----------------------------------------------------------------------
# Platform policy union
# ----------------------------------------------------------------------


def _apply_platform_policy(agent: OAgent) -> OAgent:
    """Mark tools named by `SECTR_APPROVAL_POLICY` as approval-gated. The
    framework then treats them exactly like code-marked `needs_approval`
    tools — deployment config alone can gate a tool the user forgot to
    mark. No-op when the policy is empty or already covers every name."""
    policy = set(platform_approval_policy())
    if not policy:
        return agent
    marked = {
        tool.name: replace(tool, needs_approval=True)  # type: ignore[arg-type]
        for tool in agent.tools
        if tool.name in policy
    }
    if not marked:
        return agent
    tools = [marked.get(tool.name, tool) for tool in agent.tools]
    return agent.clone(tools=tools)


# ----------------------------------------------------------------------
# Resume: transcript → input items
# ----------------------------------------------------------------------


from openai.types.responses import ResponseInputItemParam


def _transcript_items(ctx: SessionContext) -> list[ResponseInputItemParam]:
    """Convert the canonical transcript view (`ctx.messages`) into
    openai-agents input items. Used by BOTH turn kinds — a user_message
    turn continues the conversation (the transcript's last message is the
    new user input); the approval-resume path appends the answered pending
    call on top.

    - user messages pass through as-is
    - assistant text becomes an output message, as the SDK's own items
      hold it (`ResponseOutputMessage`, with the SDK's placeholder id, which
      the Responses model strips before sending): the Chat Completions
      converter then merges the calls that follow into that one assistant
      message, as on the turn the model wrote it. A plain `{role, content}`
      item would make it two messages, changing the prompt.
    - journaled TOOL_CALLs (chat-style `tool_calls`) become `function_call`
      items with their original call_ids
    - journaled TOOL_RESULTs become `function_call_output` items, their
      `output` the journaled result: text parts stay parts (the canonical
      view holds them as a JSON string, which the model would see instead)

    The return type is the SDK's item union (Literal-tagged TypedDicts) so
    mypy rejects unknown item type names / malformed shapes statically —
    `function_call_result` (the wrong vocabulary, three live failures ago)
    fails to typecheck here.
    """
    results = {
        e.payload["tool_call_id"]: e.payload["result"]
        for e in ctx.transcript
        if e.type == "TOOL_RESULT"
    }
    items: list[ResponseInputItemParam] = []
    for msg in ctx.messages:
        if msg["role"] == "user":
            items.append({"role": "user", "content": msg["content"]})
        elif msg["role"] == "tool":
            result = results.get(msg["tool_call_id"])
            items.append(
                {
                    "type": "function_call_output",
                    "call_id": msg["tool_call_id"],
                    "output": result if isinstance(result, list) else msg["content"],
                }
            )
        else:  # assistant
            items.extend(_reasoning_items(msg))
            if msg.get("content"):
                items.append(
                    {
                        "type": "message",
                        "id": FAKE_RESPONSES_ID,
                        "role": "assistant",
                        "content": [
                            {"type": "output_text", "text": msg["content"], "annotations": []}
                        ],
                        "status": "completed",
                    }
                )
            for call in msg.get("tool_calls") or []:
                items.append(
                    {
                        "type": "function_call",
                        "call_id": call["id"],
                        "name": call["function"]["name"],
                        "arguments": call["function"]["arguments"],
                    }
                )

    # The invoke contract guarantees the transcript contains this
    # invocation's INVOCATION_STARTED, so the fold above already emitted
    # the current user message. A partial transcript (tests, harnesses)
    # falls back to appending the current input — a turn never runs
    # without its user message.
    if ctx.reason.get("kind") == "user_message":
        raw = ctx.reason["input"]
        if not any(m["role"] == "user" and m["content"] == raw for m in ctx.messages):
            items.append({"role": "user", "content": raw})
    return items


async def _resumed_state(
    ctx: SessionContext, agent: OAgent, run_context: Any
) -> tuple[RunState, set[str]]:
    """The SDK's paused `RunState`, rebuilt from the journal, with the
    decision applied: `Runner.run_streamed(agent, state)` then runs (or
    rejects) the call itself, as a native resume does, with the run's
    context, config, hooks, guardrails and tracing.

    The journal has what the snapshot needs: the conversation
    ([`_transcript_items`]) and the pending call. Its paused model response
    (from the assistant output before the call to the end: the calls, and
    the outputs of siblings that ran on the pause turn) becomes the run's
    generated items, the rest its input, so the model sees the items in
    their original order. The response's functions to run are its calls
    without an output: the pending one, and other gated calls still
    unanswered, which the SDK pauses on again (sequential approvals); a
    sibling with an output isn't run again. A handoff in the response
    (its transfer call, without an output yet: the SDK hands off after the
    response's tools ran) is the run's pending handoff, which the SDK
    carries out once the calls are done. Also returns the response's call
    ids, all journaled already.
    """
    pending = ctx.pending_tool_call
    assert pending is not None
    call_id, name = pending["tool_call_id"], pending["name"]
    tools = {t.name: t for t in agent.tools if isinstance(t, FunctionTool)}
    if name not in tools:
        raise RuntimeError(f"approved tool {name!r} is not on the agent")

    items: list[dict[str, Any]] = [dict(item) for item in _transcript_items(ctx)]
    at = next(
        (
            i
            for i, item in enumerate(items)
            if item.get("type") == "function_call" and item.get("call_id") == call_id
        ),
        None,
    )
    if at is None:  # defensive: a journal without the TOOL_CALL
        at = len(items)
        items.append(
            {
                "type": "function_call",
                "call_id": call_id,
                "name": name,
                "arguments": _dumps(pending["arguments"]),
            }
        )
    start = at
    while start > 0 and (
        items[start - 1].get("type") in ("function_call", "reasoning")
        or (
            items[start - 1].get("type") == "message"
            and items[start - 1].get("role") == "assistant"
        )
    ):
        start -= 1

    who = {"name": agent.name}
    call = items[at]
    approval = {"type": "tool_approval_item", "raw_item": call, "agent": who, "tool_name": name}
    transfers = {tool_name for tool_name, _ in _handoffs(agent)}
    generated = [_run_item(item, who, transfers) for item in items[start:]]
    response = [item for item in items[start:] if item.get("type") != "function_call_output"]
    answered = {i["call_id"] for i in items[start:] if i.get("type") == "function_call_output"}
    handoffs = [
        i
        for i in response
        if i.get("type") == "function_call" and i["name"] in transfers and i["call_id"] not in answered
    ]
    # The pending call first, then the others, as the response has them.
    unanswered = [call] + [
        i
        for i in response
        if i.get("type") == "function_call"
        and i["call_id"] not in answered
        and i["call_id"] != call_id
        and i["name"] in tools
    ]
    state = await RunState.from_json(
        agent,
        {
            "$schemaVersion": _RUN_STATE_SCHEMA,
            "current_turn": 1,
            "current_agent": who,
            "original_input": items[:start],
            "model_responses": [{"usage": {}, "output": response, "response_id": None}],
            "context": {"usage": {}, "approvals": {}},
            "generated_items": generated,
            "current_step": {
                "type": "next_step_interruption",
                "data": {"interruptions": [approval]},
            },
            "last_processed_response": {
                "new_items": generated,
                "tools_used": sorted({i["name"] for i in [*unanswered, *handoffs]}),
                "functions": [{"tool_call": i, "tool": {"name": i["name"]}} for i in unanswered],
                "computer_actions": [],
                "local_shell_actions": [],
                "handoffs": [{"tool_call": i, "handoff": {"tool_name": i["name"]}} for i in handoffs],
                "mcp_approval_requests": [],
                "interruptions": [approval],
                # An MCP tool's recipient (server, tool name, index), which
                # SDK 0.23+ checks a resumed MCP call against. The journal
                # holds no binding: this invocation's listing is the one the
                # call runs on ([`_run_agent`]). Older SDKs ignore the field.
                **_mcp_bindings(unanswered, tools),
            },
            "max_turns": DEFAULT_MAX_TURNS,
        },
        context_override=RunContextWrapper(context=run_context),
    )
    for item in state.get_interruptions():
        if ctx.decision["approved"]:
            state.approve(item)
        else:
            state.reject(item)
    return state, {i["call_id"] for i in response if i.get("type") == "function_call"}


def _response_state(response: Any) -> dict[str, Any] | None:
    """A model response's framework state (`_State`), if it has reasoning."""
    items = tuple(
        exact(item.model_dump(mode="json", exclude_unset=True))
        for item in getattr(response, "output", None) or ()
        if getattr(item, "type", None) == "reasoning"
    )
    return framework_state(_State(items=items)) if items else None


def _reasoning_items(msg: dict[str, Any]) -> list[ResponseInputItemParam]:
    """The reasoning items journaled with an assistant message, keys in
    their original order; none for a state this adapter didn't write or
    can't read (reasoning is then dropped)."""
    state = journaled_state(msg, _State)
    if state is None:
        return []
    try:
        items = [parsed(i) for i in state.items]
    except ValueError as exc:
        unreadable(_State, f"an item isn't a JSON object: {exc}")
        return []
    if any(i.get("type") != "reasoning" for i in items):
        unreadable(_State, "an item that isn't a reasoning item")
        return []
    return cast(list[ResponseInputItemParam], items)


def _mcp_bindings(calls: list[dict[str, Any]], tools: dict[str, FunctionTool]) -> dict[str, Any]:
    bindings = {
        call["call_id"]: list(binding)
        for call in calls
        if (binding := getattr(tools[call["name"]], "_mcp_tool_binding", None)) is not None
    }
    return {"mcp_tool_bindings": bindings} if bindings else {}


def _run_item(item: dict[str, Any], who: dict[str, str], transfers: set[str]) -> dict[str, Any]:
    """An input item as the serialized run item the SDK generated it as: a
    call to one of `transfers` (the agent's handoff tools) as a handoff's."""
    kind = item.get("type")
    if kind == "function_call":
        call_type = "handoff_call_item" if item["name"] in transfers else "tool_call_item"
        return {"type": call_type, "raw_item": item, "agent": who}
    if kind == "reasoning":
        return {"type": "reasoning_item", "raw_item": item, "agent": who}
    if kind == "function_call_output":
        return {
            "type": "tool_call_output_item",
            "raw_item": item,
            "agent": who,
            "output": item["output"],
        }
    return {"type": "message_output_item", "raw_item": item, "agent": who}


# ----------------------------------------------------------------------
# helpers
# ----------------------------------------------------------------------


def _call_id(raw: Any) -> str:
    """Run-item raw payloads are pydantic objects or plain param dicts
    depending on path — read defensively (the union is wide; only function
    calls reach these attributes in practice)."""
    if isinstance(raw, dict):
        return raw.get("call_id") or ""
    return getattr(raw, "call_id", "") or ""


def _name(raw: Any) -> str:
    if isinstance(raw, dict):
        return raw.get("name") or ""
    return getattr(raw, "name", "") or ""


def _arguments(raw: Any) -> Any:
    if isinstance(raw, dict):
        return raw.get("arguments")
    return getattr(raw, "arguments", None)


def _loads(value: Any) -> Any:
    """Model-emitted arguments arrive as a JSON string; parse so the journal
    stores structured data (the schema wants `arguments: any`)."""
    if isinstance(value, str):
        try:
            return json.loads(value)
        except json.JSONDecodeError:
            return value
    return value


def _output(raw: Any) -> Any:
    """A `function_call_output` item's `output`, read like [`_call_id`]."""
    if isinstance(raw, dict):
        return raw.get("output")
    return getattr(raw, "output", None)


def _dumps(value: Any) -> str:
    if isinstance(value, str):
        return value
    return json.dumps(value, default=str)