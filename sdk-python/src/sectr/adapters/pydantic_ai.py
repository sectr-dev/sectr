"""Pydantic AI adapter (`sectr[pydantic-ai]` extra; floor = tested version
`pydantic-ai-slim>=2.54`).

`pydantic_ai(agent, *, model, deps, model_settings, usage_limits)` returns a
handler mapping a `pydantic_ai.Agent` onto the platform contract (example:
`examples/pydantic-ai/main.py`). Pydantic AI
resumes a deferred-tool run from message history alone, which is exactly
Sectr's "transcript in, events out" model: no serialized run state, no
adapter-side tool execution.

- normal turn: `ctx.messages` → `message_history`; `agent.iter(None, ...)`
  with the current user message as the history's tail (D6). One
  MESSAGE_START/END bracket per model response; TOOL_CALLs are journaled
  from the model stream (never from `FunctionToolCallEvent`, which Pydantic
  AI re-emits on resume — D3) after MESSAGE_END; tool results from the
  tool-execution events.
- tool results: `_SectrGate` replaces each result with its
  `model_response_str()` before Pydantic AI stores it, so every provider
  gets that string, the journal holds it, and a later turn replays it
  unchanged. `None` is `""` everywhere. The agent's own
  `after_tool_execute` hooks see the raw value; its `wrap_tool_execute`
  hooks run outside the adapter's and see the string.
- pause: natively gated tools (`requires_approval=True`,
  `ApprovalRequired`) end the run with `DeferredToolRequests`. The adapter
  yields TOOL_APPROVAL_REQUIRED for the first approval (model order) as the
  final frame. `DeferredToolRequests` is added to the run's output type
  unless the agent already allows it (D7).
- resume (`approval_decision`): Pydantic AI's native deferred-tool resume —
  `message_history` + `DeferredToolResults`. Pydantic AI executes the
  approved tool itself (validation, `RunContext`, `deps`, hooks); the model
  generates only the continuation (D1). A denial passes
  `ToolDenied(REJECTION_MESSAGE)` (D9).
- several gated calls in one response: Pydantic AI needs a result for every
  open call, so the adapter passes a placeholder `ToolApproved()` for each
  undecided sibling and `_SectrGate` re-defers them before their function
  runs — sequential approvals, one per turn (D2). Batched approvals aren't supported yet.
- `SECTR_APPROVAL_POLICY` is enforced by the same gate (D8); it also covers
  toolset (MCP) tools, and holds under a `HandleDeferredToolCalls`
  auto-approver. A user's own auto-approver does bypass approval for their
  natively gated tools: that is their explicit choice.
- external deferred calls (`CallDeferred`, `kind='external'`) fail loudly
  (D10): Sectr has no external tool-result path yet.
- `system_prompt=` parts are restored with `ReinjectSystemPrompt()` (D5): a
  rebuilt history is never empty, so Pydantic AI would otherwise drop them.
- only `pydantic_ai.Agent` is accepted (D12): durable-execution wrappers
  own a second durable store, which conflicts with "the journal is the
  checkpoint".

- reasoning: a response's `ThinkingPart`s (signatures included) are
  journaled on its MESSAGE_END as framework state, and put back in front of
  its text and calls when the history is rebuilt, so later requests send
  them back as Pydantic AI does natively (only to the provider that issued
  them). Its text is also streamed as `reasoning` deltas.

Not journaled in v1: provider-native tool calls (`NativeToolCallPart`),
multimodal content.
"""

from __future__ import annotations

import inspect
import json
import types
import typing
import uuid
from collections.abc import AsyncIterator, Awaitable, Callable, Sequence
from dataclasses import dataclass, field, replace
from typing import Any, Union, get_args, get_origin

import pydantic_ai as _framework
from pydantic_ai import Agent, DeferredToolRequests, DeferredToolResults
from pydantic_ai.capabilities import AbstractCapability, ReinjectSystemPrompt
from pydantic_ai.exceptions import ApprovalRequired as PydanticApprovalRequired
from pydantic_ai.exceptions import UserError
from pydantic_ai.messages import (
    FunctionToolResultEvent,
    ModelMessage,
    ModelRequest,
    ModelRequestPart,
    ModelResponse,
    ModelResponsePart,
    OutputToolResultEvent,
    PartDeltaEvent,
    PartEndEvent,
    PartStartEvent,
    RetryPromptPart,
    TextPart,
    TextPartDelta,
    ThinkingPart,
    ThinkingPartDelta,
    ToolCallPart,
    ToolReturn,
    ToolReturnPart,
    UserPromptPart,
)
from pydantic_ai.models import Model
from pydantic_ai.settings import ModelSettings
from pydantic_ai.tools import ToolApproved, ToolDenied
from pydantic_ai.usage import UsageLimits
from pydantic_core import to_jsonable_python

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
from ..tools import platform_approval_policy
from . import SUBAGENT_GATED_TOOL
from ._framework_state import FrameworkData, framework_state, journaled_state

__all__ = ["pydantic_ai"]

# Pydantic AI prints a first-run banner on stderr when it detects a coding
# agent (even with a piped stderr), which would land in every runner's log.
_framework.BANNER_ENABLED = False


class _State(FrameworkData):
    """A response's framework state: its `ThinkingPart`s, which carry the
    signatures its `reasoning` deltas don't. Rebuilt as typed parts, which
    Pydantic AI serializes for the provider itself, so the journal's key
    order never reaches the request."""

    adapter = "pydantic_ai"
    version = 1

    thinking: tuple[ThinkingPart, ...]


# Same model-visible text as the openai-agents and langgraph adapters.
REJECTION_MESSAGE = "Tool execution was not approved."

# How Pydantic AI's error starts when a run ends with deferred tool requests
# its output type doesn't allow (`exceptions.UserError`, raised from two
# places with this prefix).
_DEFERRED_NOT_ALLOWED = "A deferred tool call was present, but `DeferredToolRequests` is not among"

_OUTPUT_VALIDATOR_MESSAGE = (
    "pydantic_ai(agent): the agent has output validators, so sectr cannot add "
    "DeferredToolRequests to its output type for a run. Declare it on the agent "
    "yourself: Agent(..., output_type=[str, DeferredToolRequests])."
)


def pydantic_ai(
    agent: Agent[Any, Any],
    *,
    model: Model | str | None = None,
    deps: Callable[[SessionContext], Any | Awaitable[Any]] | None = None,
    model_settings: ModelSettings | None = None,
    usage_limits: UsageLimits | None = None,
) -> Handler:
    """Handler factory wrapping a `pydantic_ai.Agent` (see module docstring).

    `model` / `model_settings` / `usage_limits` pass through to `agent.iter`;
    `model` is for deterministic fakes. `deps` is a factory called once per
    invocation with the `SessionContext` (a static value is `lambda ctx: v`).

    Raises `TypeError` for anything but a plain `Agent`, and `ValueError` for
    structured output (an `output_type` other than `str`) or for an agent
    whose output validators prevent adding `DeferredToolRequests`.
    """
    if not isinstance(agent, Agent):
        raise TypeError(
            f"pydantic_ai(agent): expected a pydantic_ai.Agent, got {type(agent).__name__}. "
            "Durable-execution wrappers (TemporalAgent, DBOSAgent, PrefectAgent) and other "
            "AbstractAgent subclasses own a second durable store, which conflicts with the "
            "journal being the checkpoint."
        )
    _reject_structured_output(agent)
    _run_output_type(agent)

    async def handler(ctx: SessionContext) -> AsyncIterator[RunnerEvent]:
        try:
            async for event in turn(ctx):
                yield event
        except _GatedInDelegate as exc:
            yield Error(message=exc.message, code=SUBAGENT_GATED_TOOL)

    async def turn(ctx: SessionContext) -> AsyncIterator[RunnerEvent]:
        output_type = _run_output_type(agent)  # validators can be added after construction
        history = _history(ctx)
        policy = frozenset(platform_approval_policy())

        results: DeferredToolResults | None = None
        if ctx.reason.get("kind") == "approval_decision":
            pending = ctx.pending_tool_call
            assert pending is not None  # drive guarantees it
            pending_id: str = pending["tool_call_id"]
            open_ids = _open_call_ids(history)
            if pending_id not in open_ids:
                yield Error(
                    message=(
                        f"pending tool call {pending_id!r} is not an open call of the "
                        "last model response in the rebuilt history"
                    ),
                    code="pydantic_ai_pending_mismatch",
                )
                return
            others = [i for i in open_ids if i != pending_id]
            approvals: dict[str, Any] = {i: ToolApproved() for i in others}
            approvals[pending_id] = (
                True if ctx.decision["approved"] else ToolDenied(REJECTION_MESSAGE)
            )
            results = DeferredToolResults(approvals=approvals)
            gate = _SectrGate(policy, decided=pending_id, placeholders=frozenset(others))
        else:
            gate = _SectrGate(policy)

        async with agent.iter(
            None,
            message_history=history,
            deferred_tool_results=results,
            output_type=output_type,
            model=model,
            deps=await _deps(ctx, deps),
            model_settings=model_settings,
            usage_limits=usage_limits,
            capabilities=[gate, ReinjectSystemPrompt()],
        ) as run:
            async for node in run:
                if Agent.is_model_request_node(node):
                    # Opened lazily on the first text/reasoning, like the
                    # other adapters: a request that fails before producing
                    # any (429, 400, a dropped stream) must not leave an
                    # unterminated message in the journal.
                    message_id: str | None = None
                    calls: list[ToolCallPart] = []
                    async with node.stream(run.ctx) as stream:
                        async for event in stream:
                            if isinstance(event, PartEndEvent) and isinstance(
                                event.part, ToolCallPart
                            ):
                                calls.append(event.part)
                                continue
                            content = _content(event)
                            if content is None:
                                continue
                            if message_id is None:
                                message_id = uuid.uuid4().hex[:16]
                                yield MessageStart(role="assistant", message_id=message_id)
                            yield _delta(message_id, *content)
                        state = _response_state(stream.response)
                    if message_id is None:
                        # A tool-only response still gets an (empty) bracket:
                        # the fold attaches its calls to THIS response (D4).
                        message_id = uuid.uuid4().hex[:16]
                        yield MessageStart(role="assistant", message_id=message_id)
                    yield MessageEnd(message_id=message_id, framework_state=state)
                    for call in calls:
                        yield ToolCall(
                            tool_call_id=call.tool_call_id,
                            name=call.tool_name,
                            arguments=_loads(call.args),
                        )
                elif Agent.is_call_tools_node(node):
                    async with node.stream(run.ctx) as stream:
                        async for event in stream:
                            if isinstance(event, (FunctionToolResultEvent, OutputToolResultEvent)):
                                yield ToolResult(
                                    tool_call_id=event.tool_call_id,
                                    result=_result_value(event.part),
                                )

        assert run.result is not None
        out = run.result.output
        if not isinstance(out, DeferredToolRequests):
            return  # drive appends NO_MORE_ACTIONS
        if out.calls:
            yield Error(
                message=(
                    "a tool deferred its execution to an external system (CallDeferred / "
                    "kind='external'); sectr has no external tool-result path yet. "
                    "See the Pydantic AI adapter reference in the docs."
                ),
                code="pydantic_ai_external_call",
            )
            return
        first = out.approvals[0]
        reason = (out.metadata.get(first.tool_call_id) or {}).get("reason")
        yield ToolApprovalRequired(
            tool_call_id=first.tool_call_id,
            name=first.tool_name,
            arguments=_loads(first.args),
            reason=reason if isinstance(reason, str) else None,
        )

    # Framework marker — `sectr inspect` surfaces this in the manifest and
    # the dev gateway reports it via /dev/status (DevStatus.framework).
    handler.sectr_framework = "pydantic-ai"  # type: ignore[attr-defined]
    return handler


# ----------------------------------------------------------------------
# D7: output type
# ----------------------------------------------------------------------


def _output_items(spec: Any) -> list[Any]:
    """An `output_type`'s alternatives: Pydantic AI accepts a list (or
    tuple) of them, a union (`X | Y`, `Union[X, Y]`), or any nesting of the
    two."""
    if isinstance(spec, (list, tuple)):
        return [item for member in spec for item in _output_items(member)]
    if isinstance(spec, typing.TypeAliasType):
        return _output_items(spec.__value__)
    if get_origin(spec) in (Union, types.UnionType):
        return [item for member in get_args(spec) for item in _output_items(member)]
    return [spec]


def _reject_structured_output(agent: Agent[Any, Any]) -> None:
    """Structured output isn't supported yet: fail at construction, before
    the first turn. Text (`str`) is the only output; `DeferredToolRequests`
    is the approval pause, not an output."""
    items = _output_items(agent.output_type)
    structured = [i for i in items if i is not str and i is not DeferredToolRequests]
    if structured:
        names = ", ".join(getattr(i, "__name__", repr(i)) for i in structured)
        raise ValueError(
            f"pydantic_ai(agent): the agent's output_type includes {names}, but structured "
            "output is not supported on Sectr yet. Use the default output_type=str: the "
            "agent's answer is journaled and shown as text."
        )


def _run_output_type(agent: Agent[Any, Any]) -> list[Any] | None:
    """The run-level `output_type` that lets the run pause for approval, or
    `None` when the agent already allows `DeferredToolRequests`."""
    items = _output_items(agent.output_type)
    if DeferredToolRequests in items:
        return None
    if agent._output_validators:  # noqa: SLF001 — no public accessor
        raise ValueError(_OUTPUT_VALIDATOR_MESSAGE)
    return [*items, DeferredToolRequests]


# ----------------------------------------------------------------------
# D2 / D8: the gate
# ----------------------------------------------------------------------


class _GatedInDelegate(Exception):  # noqa: N818 — becomes the turn's ERROR
    """A delegate run inside a tool ended on a gated call. The top-level run
    always allows `DeferredToolRequests`, and ends with them outside any
    tool's execution, so a run raising for them inside one is a delegate's
    (at any depth). Sectr can't pause and resume inside a delegate."""

    def __init__(self, tool: str) -> None:
        self.message = (
            f"a tool gated in code (requires_approval, or ApprovalRequired) was called inside "
            f"an agent that tool {tool!r} runs, and a gated tool inside a subagent is not "
            f"supported on Sectr yet ({SUBAGENT_GATED_TOOL}). Gate {tool!r} itself instead, or "
            "move the tool to the agent you serve."
        )
        super().__init__(self.message)


@dataclass
class _SectrGate(AbstractCapability[Any]):
    """Re-defers calls the adapter must not execute this turn: placeholders
    for undecided gated siblings (sequential approvals), and tools named by
    `SECTR_APPROVAL_POLICY` unless their decision is in hand. Also turns
    each tool result into the string that is journaled, and a delegate's
    gated call into `_GatedInDelegate`."""

    policy: frozenset[str] = frozenset()
    decided: str | None = None
    placeholders: frozenset[str] = field(default_factory=frozenset)

    @classmethod
    def get_serialization_name(cls) -> str | None:
        return None

    async def before_tool_execute(
        self, ctx: Any, *, call: ToolCallPart, tool_def: Any, args: Any
    ) -> Any:
        if call.tool_call_id in self.placeholders or (
            call.tool_name in self.policy and call.tool_call_id != self.decided
        ):
            raise PydanticApprovalRequired()
        return args

    async def wrap_tool_execute(
        self, ctx: Any, *, call: ToolCallPart, tool_def: Any, args: Any, handler: Any
    ) -> Any:
        """Store the result as its `model_response_str()`. Pydantic
        AI renders a raw value differently per provider (`None` is `""` on
        OpenAI, `"null"` on Anthropic; a list is one block per item there), so
        no journaled value would replay exactly. A string is sent as-is on
        every provider and survives the journal; the adapter journals it.

        A wrap, not `after_tool_execute`: run capabilities nest inside the
        agent's, so this wraps the lifecycle directly, after every
        `after_tool_execute` (the user's see the raw value, and theirs is
        what gets stringified)."""
        try:
            result = await handler(args)
        except UserError as exc:
            if str(exc).startswith(_DEFERRED_NOT_ALLOWED):
                raise _GatedInDelegate(call.tool_name) from exc
            raise
        if isinstance(result, ToolReturn):
            return replace(result, return_value=_response_str(call, result.return_value))
        return _response_str(call, result)


# ----------------------------------------------------------------------
# transcript → Pydantic AI messages
# ----------------------------------------------------------------------


def _loads(value: Any) -> Any:
    """Model-emitted arguments arrive as a JSON string; parse so the journal
    stores structured data. Malformed JSON stays a string."""
    if value is None:
        return {}
    if isinstance(value, str):
        try:
            return json.loads(value)
        except json.JSONDecodeError:
            return value
    return value


def _history(ctx: SessionContext) -> list[ModelMessage]:
    """Project the canonical transcript view (`ctx.messages`) into Pydantic
    AI messages. Consecutive user/tool messages merge into one
    `ModelRequest`; each assistant message is its own `ModelResponse`."""
    names: dict[str, str] = {}
    for msg in ctx.messages:
        if msg["role"] == "assistant":
            for call in msg.get("tool_calls") or []:
                names[call["id"]] = call["function"]["name"]

    history: list[ModelMessage] = []
    request_parts: list[ModelRequestPart] = []

    def flush() -> None:
        nonlocal request_parts
        if request_parts:
            history.append(ModelRequest(parts=request_parts))
            request_parts = []

    for msg in ctx.messages:
        role = msg["role"]
        if role == "user":
            request_parts.append(UserPromptPart(content=msg["content"]))
        elif role == "tool":
            request_parts.append(
                ToolReturnPart(
                    tool_name=names.get(msg["tool_call_id"], ""),
                    content=msg["content"],
                    tool_call_id=msg["tool_call_id"],
                )
            )
        else:  # assistant
            parts: list[ModelResponsePart] = []
            if msg.get("content"):
                parts.append(TextPart(content=msg["content"]))
            for call in msg.get("tool_calls") or []:
                parts.append(
                    ToolCallPart(
                        tool_name=call["function"]["name"],
                        args=call["function"]["arguments"],
                        tool_call_id=call["id"],
                    )
                )
            if not parts:
                continue  # nothing the model produced; providers reject empty responses
            flush()
            history.append(ModelResponse(parts=[*_thinking(msg), *parts]))
    flush()

    # The invoke contract guarantees the transcript holds this invocation's
    # INVOCATION_STARTED, so the fold already carries the current user
    # message. A partial transcript (tests, harnesses) falls back to
    # appending the current input — a turn never runs without its message.
    if ctx.reason.get("kind") == "user_message":
        raw = ctx.reason["input"]
        if not any(m["role"] == "user" and m["content"] == raw for m in ctx.messages):
            part = UserPromptPart(content=raw)
            if history and isinstance(history[-1], ModelRequest):
                history[-1].parts = [*history[-1].parts, part]
            else:
                history.append(ModelRequest(parts=[part]))
    return history


def _response_state(response: ModelResponse) -> dict[str, Any] | None:
    """A response's framework state, if it has thinking."""
    thinking = [p for p in response.parts if isinstance(p, ThinkingPart)]
    return framework_state(_State(thinking=tuple(thinking))) if thinking else None


def _thinking(msg: dict[str, Any]) -> list[ThinkingPart]:
    """The `ThinkingPart`s journaled with an assistant message; none for a
    state this adapter didn't write or can't read (reasoning is then
    dropped)."""
    state = journaled_state(msg, _State)
    return list(state.thinking) if state is not None else []


def _open_call_ids(history: Sequence[ModelMessage]) -> list[str]:
    """Tool-call ids of the last model response that have no
    `ToolReturnPart` in the request that follows it (model order)."""
    index = next(
        (i for i in range(len(history) - 1, -1, -1) if isinstance(history[i], ModelResponse)),
        None,
    )
    if index is None:
        return []
    response = history[index]
    assert isinstance(response, ModelResponse)
    answered: set[str] = set()
    if index + 1 < len(history):
        follower = history[index + 1]
        if isinstance(follower, ModelRequest):
            answered = {p.tool_call_id for p in follower.parts if isinstance(p, ToolReturnPart)}
    return [
        p.tool_call_id
        for p in response.parts
        if isinstance(p, ToolCallPart) and p.tool_call_id not in answered
    ]


# ----------------------------------------------------------------------
# helpers
# ----------------------------------------------------------------------


def _content(event: Any) -> tuple[str, str] | None:
    """The (delta type, text) a model-stream event carries, if any. The
    first chunk rides on `PartStartEvent`; signature-only thinking deltas
    carry none."""
    if isinstance(event, PartStartEvent):
        part = event.part
        if isinstance(part, TextPart) and part.content:
            return "text", part.content
        if isinstance(part, ThinkingPart) and part.content:
            return "reasoning", part.content
    elif isinstance(event, PartDeltaEvent):
        delta = event.delta
        if isinstance(delta, TextPartDelta) and delta.content_delta:
            return "text", delta.content_delta
        if isinstance(delta, ThinkingPartDelta) and delta.content_delta:
            return "reasoning", delta.content_delta
    return None


def _delta(message_id: str, kind: str, text: str) -> MessageDelta:
    return MessageDelta(message_id=message_id, delta={"type": kind, "text": text})


def _response_str(call: ToolCallPart, value: Any) -> Any:
    """`value` as Pydantic AI's own string for it. Multimodal content (v1:
    not journaled) is left alone: the string drops its files."""
    part = ToolReturnPart(tool_name=call.tool_name, content=value, tool_call_id=call.tool_call_id)
    if part.files:
        return value
    return part.model_response_str()


def _result_value(part: ToolReturnPart | RetryPromptPart) -> Any:
    if isinstance(part, RetryPromptPart):
        return part.model_response()
    return to_jsonable_python(part.content)


async def _deps(
    ctx: SessionContext, factory: Callable[[SessionContext], Any | Awaitable[Any]] | None
) -> Any:
    if factory is None:
        return None
    value = factory(ctx)
    return await value if inspect.isawaitable(value) else value
